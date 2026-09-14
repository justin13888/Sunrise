//! Review statistics per `docs/08-features/reviews-and-stats.md` §Stats:
//! completed-per-week and deferred-per-week trends, routine streaks, and
//! routine drift. Time-in-focus is *not* re-derived here — it is already
//! [`crate::focus::fold_focus_stats`], and a second implementation of the same
//! number is a second answer waiting to disagree.
//!
//! The spec pins the one definition that is easy to get wrong:
//!
//! > **Completed-per-week** = transitions to `done` whose `done_at` falls in
//! > the week. Re-opens that flip back to `pending` decrement the count for the
//! > week of the **original** completion, so the chart is stable.
//!
//! That is why the trend is folded from the **op log** and not from
//! `tasks.completed_at`: the materialized column is a last-writer-wins
//! register, so re-opening a task erases the very instant the chart needs to
//! decrement. The log keeps both facts, in order, forever.
//!
//! Everything in this module is pure. Time enters only as explicit parameters
//! ( `now_ms`, a [`WeekGrid`], a window) and a [`TimeZone`] the caller resolved
//! from the injected clock.

use crate::activity::{OpPayload, OpRecord};
use crate::routine::Routine;
use crate::routine_gen::{expand, occurrence_key_at, ExpandError};
use crate::rrule::Weekday;
use crate::task::TaskState;
use jiff::civil::Weekday as JiffWeekday;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

/// Trend length the spec asks for: the last 12 weeks.
pub const TREND_WEEKS: u32 = 12;

/// Drift window for routine tuning: the last 4 weeks.
pub const DRIFT_WINDOW_WEEKS: u32 = 4;

/// Default drift threshold: suggest tuning a Routine once more than 30% of its
/// occurrences in the window went uncompleted. Not user-facing (the spec puts
/// it behind the debug menu), hence a constant rather than a setting.
pub const DEFAULT_DRIFT_THRESHOLD: f64 = 0.30;

/// Milliseconds in a day.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Errors from building a [`WeekGrid`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StatsError {
    /// `now_ms` is not a representable instant.
    #[error("timestamp out of range: {0}")]
    TimestampRange(u64),
    /// Calendar arithmetic ran off the end of the supported range.
    #[error("week grid arithmetic overflowed")]
    Overflow,
}

/// The week buckets a trend is reported in.
///
/// Weeks are **civil** weeks in a real timezone, not fixed 604800-second
/// slices: a DST transition makes one week 23 or 25 hours longer, and a chart
/// whose Monday drifts an hour twice a year is a bug the user sees. The grid is
/// built once from a zone and a week-start weekday, then used as a pure
/// `instant → bucket` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekGrid {
    /// Ascending week-start instants (ms since epoch), one per bucket.
    starts: Vec<u64>,
    /// Exclusive end of the last bucket.
    end_ms: u64,
}

impl WeekGrid {
    /// Build directly from ascending week starts and an exclusive end.
    ///
    /// Used by tests and by callers that already know their boundaries; the
    /// normal constructor is [`WeekGrid::trailing`].
    #[must_use]
    pub fn from_starts(starts: Vec<u64>, end_ms: u64) -> Self {
        Self { starts, end_ms }
    }

    /// The `weeks` civil weeks ending with the one containing `now_ms`.
    ///
    /// `week_start` is the weekday a week begins on (Monday for ISO callers).
    /// The last bucket is the *current, partial* week — a review run on Friday
    /// must show the week it is in.
    pub fn trailing(
        now_ms: u64,
        weeks: u32,
        tz: &TimeZone,
        week_start: Weekday,
    ) -> Result<Self, StatsError> {
        let weeks = weeks.max(1);
        let now = to_timestamp(now_ms)?;
        let date = now.to_zoned(tz.clone()).date();
        let back = i64::from(date.weekday().since(to_jiff(week_start)).rem_euclid(7));
        let current = date
            .checked_sub(Span::new().days(back))
            .map_err(|_| StatsError::Overflow)?;

        let mut starts = Vec::with_capacity(weeks as usize);
        for i in (0..weeks).rev() {
            let d = current
                .checked_sub(Span::new().weeks(i64::from(i)))
                .map_err(|_| StatsError::Overflow)?;
            starts.push(zone_start_ms(d, tz)?);
        }
        let next = current
            .checked_add(Span::new().weeks(1))
            .map_err(|_| StatsError::Overflow)?;
        Ok(Self {
            starts,
            end_ms: zone_start_ms(next, tz)?,
        })
    }

    /// The bucket start instants, ascending.
    #[must_use]
    pub fn starts(&self) -> &[u64] {
        &self.starts
    }

    /// Number of buckets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    /// Whether the grid has no buckets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Exclusive end of the grid.
    #[must_use]
    pub const fn end_ms(&self) -> u64 {
        self.end_ms
    }

    /// Start of the *last* (current) bucket — the window a weekly review
    /// covers by default.
    #[must_use]
    pub fn last_start_ms(&self) -> u64 {
        self.starts.last().copied().unwrap_or(0)
    }

    /// Which bucket `at_ms` falls in, or `None` when it is outside the grid.
    ///
    /// `None` is a normal answer, not an error: a re-open that decrements a
    /// completion from before the window simply has nowhere to land, which is
    /// exactly the behaviour a 12-week chart wants.
    #[must_use]
    pub fn index_of(&self, at_ms: u64) -> Option<usize> {
        if at_ms >= self.end_ms {
            return None;
        }
        // `partition_point` gives the count of starts <= at_ms; the bucket is
        // one below that. Zero means `at_ms` precedes the grid.
        let n = self.starts.partition_point(|s| *s <= at_ms);
        n.checked_sub(1)
    }
}

/// One week of one trend line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeekBucket {
    /// Start of the week (ms since epoch, in the grid's zone).
    pub week_start_ms: u64,
    /// Net transitions to `done` credited to this week. Signed: a re-open
    /// decrements the week of the original completion, so a bucket can go
    /// negative if a task completed before the window is re-opened inside it.
    pub completed: i64,
    /// Defers recorded in this week.
    pub deferred: i64,
    /// Tasks created in this week.
    pub created: i64,
}

impl WeekBucket {
    /// An empty bucket for `week_start_ms`.
    #[must_use]
    pub const fn empty(week_start_ms: u64) -> Self {
        Self {
            week_start_ms,
            completed: 0,
            deferred: 0,
            created: 0,
        }
    }
}

/// One Stream's trend line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamTrend {
    /// Stream the line belongs to.
    pub stream: EntityRef,
    /// One bucket per grid week, ascending.
    pub weeks: Vec<WeekBucket>,
}

/// Completed / deferred / created trends, whole-vault and per Stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trends {
    /// Bucket starts, ascending — the x axis.
    pub week_starts: Vec<u64>,
    /// Whole-vault line.
    pub overall: Vec<WeekBucket>,
    /// Per-Stream lines, ordered by stream id.
    pub per_stream: Vec<StreamTrend>,
}

impl Trends {
    /// The line for one Stream, if it has any activity in the window.
    #[must_use]
    pub fn for_stream(&self, stream: EntityRef) -> Option<&[WeekBucket]> {
        self.per_stream
            .iter()
            .find(|t| t.stream == stream)
            .map(|t| t.weeks.as_slice())
    }
}

/// What we remember about a Task between ops while folding.
#[derive(Debug, Clone)]
struct TaskTrace {
    state: TaskState,
    deferred_count: i64,
    /// The instant and Stream a completion was *counted at*. Kept so a later
    /// re-open decrements the same bucket it incremented, which is the whole
    /// stability property the spec asks for.
    counted: Option<(u64, EntityRef)>,
}

/// Apply `f` to bucket `idx` of both the whole-vault line and `stream`'s line,
/// creating the Stream's line (zero-filled to the grid width) on first use.
fn bump(
    grid: &WeekGrid,
    overall: &mut [WeekBucket],
    per_stream: &mut BTreeMap<EntityRef, Vec<WeekBucket>>,
    stream: EntityRef,
    idx: usize,
    f: impl Fn(&mut WeekBucket),
) {
    f(&mut overall[idx]);
    let line = per_stream.entry(stream).or_insert_with(|| {
        grid.starts()
            .iter()
            .map(|s| WeekBucket::empty(*s))
            .collect()
    });
    f(&mut line[idx]);
}

/// Fold task ops into weekly trends.
///
/// `ops` must be in ascending `(at_ms, op_id)` order and should carry each
/// task's **full** history — a task whose create is outside the window still
/// needs its earlier state to tell a transition from a no-op write.
///
/// A completion is credited to the week of `completed_at` when the op set one,
/// falling back to the op's own timestamp; that is the spec's `done_at`.
#[must_use]
pub fn fold_trends(ops: &[OpRecord], grid: &WeekGrid) -> Trends {
    let mut overall: Vec<WeekBucket> = grid
        .starts()
        .iter()
        .map(|s| WeekBucket::empty(*s))
        .collect();
    let mut per_stream: BTreeMap<EntityRef, Vec<WeekBucket>> = BTreeMap::new();
    let mut trace: BTreeMap<EntityRef, TaskTrace> = BTreeMap::new();

    for op in ops {
        let (task, created) = match &op.payload {
            OpPayload::TaskCreated(t) => (t, true),
            OpPayload::TaskUpdated(t) => (t, false),
            _ => continue,
        };
        let prev = trace.get(&task.id).cloned();
        if created {
            if let Some(i) = grid.index_of(op.at_ms) {
                bump(
                    grid,
                    &mut overall,
                    &mut per_stream,
                    task.stream_id,
                    i,
                    |b| {
                        b.created += 1;
                    },
                );
            }
        }

        let was_done = prev.as_ref().is_some_and(|p| p.state == TaskState::Done);
        let is_done = task.state == TaskState::Done;
        let mut counted = prev.as_ref().and_then(|p| p.counted);

        // Baseline rule: the first row we see for a task establishes state.
        // Only a *create* may also be a completion (a task created done); an
        // update with no predecessor is not evidence of a transition.
        let transitioned_to_done = is_done && !was_done && (prev.is_some() || created);
        let transitioned_from_done = was_done && !is_done;

        if transitioned_to_done {
            let done_at = task
                .completed_at
                .as_ref()
                .and_then(|t| u64::try_from(t.index_ms()).ok())
                .unwrap_or(op.at_ms);
            if let Some(i) = grid.index_of(done_at) {
                bump(
                    grid,
                    &mut overall,
                    &mut per_stream,
                    task.stream_id,
                    i,
                    |b| {
                        b.completed += 1;
                    },
                );
            }
            counted = Some((done_at, task.stream_id));
        } else if transitioned_from_done {
            if let Some((at, stream)) = counted {
                if let Some(i) = grid.index_of(at) {
                    bump(grid, &mut overall, &mut per_stream, stream, i, |b| {
                        b.completed -= 1;
                    });
                }
            }
            counted = None;
        }

        if let Some(p) = &prev {
            let delta = task.deferred_count.saturating_sub(p.deferred_count);
            if delta > 0 {
                if let Some(i) = grid.index_of(op.at_ms) {
                    bump(
                        grid,
                        &mut overall,
                        &mut per_stream,
                        task.stream_id,
                        i,
                        |b| {
                            b.deferred += delta;
                        },
                    );
                }
            }
        }

        trace.insert(
            task.id,
            TaskTrace {
                state: task.state,
                deferred_count: task.deferred_count,
                counted,
            },
        );
    }

    Trends {
        week_starts: grid.starts().to_vec(),
        overall,
        per_stream: per_stream
            .into_iter()
            .map(|(stream, weeks)| StreamTrend { stream, weeks })
            .collect(),
    }
}

/// How far one Routine has drifted from its own cadence.
///
/// `missed` counts every occurrence in the window that was **not** completed,
/// whether it was explicitly skipped or simply never done — that is the "> 30%
/// skipped" the spec's tuning step keys on. `skipped` reports the explicit
/// subset separately, because "you skip this deliberately every Friday" and
/// "you keep forgetting this" want different suggestions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineDrift {
    /// Routine id.
    pub routine: EntityRef,
    /// Its template title, for display.
    pub title: String,
    /// Occurrences the rrule produced in the window.
    pub expected: u32,
    /// Of those, completed within the streak rules.
    pub completed: u32,
    /// Of those, explicitly skipped.
    pub skipped: u32,
    /// `expected - completed`.
    pub missed: u32,
    /// `missed / expected`, or `0.0` when the window held no occurrences.
    pub drift: f64,
    /// Whether `drift` exceeds the caller's threshold — the "suggest pausing /
    /// changing cadence" trigger.
    pub over_threshold: bool,
    /// Current streak, straight off the Routine (see [`crate::streak`]).
    pub streak: i64,
    /// Last completion, if any.
    pub last_completed_at_ms: Option<u64>,
    /// Whether the routine is already paused (a paused routine needs no pause
    /// suggestion).
    pub paused: bool,
}

/// Measure one Routine's drift over `window`.
///
/// Pure: the occurrence set comes from [`expand`], completion comes from the
/// Routine's own `streak_keys` (the idempotency-key set the streak mechanics
/// already maintain), and explicit skips from `skipped_keys` / `skip_dates`.
/// No database, no clock.
pub fn routine_drift(
    routine: &Routine,
    window: (Timestamp, Timestamp),
    threshold: f64,
) -> Result<RoutineDrift, ExpandError> {
    let tz = TimeZone::get(&routine.timezone)
        .map_err(|_| ExpandError::InvalidTimeZone(routine.timezone.clone()))?;
    // `expand` (not `Routine::occurrences_in`) on purpose: the drift measure
    // needs the *unfiltered* occurrence set, because an explicitly skipped
    // occurrence is a data point, and `occurrences_in` drops exactly those.
    let mut occurrences = expand(&routine.rrule, routine.starts_at, &tz, window)?;
    if let Some(end) = routine.ends_at {
        occurrences.retain(|o| o.at <= end);
    }
    // `skip_dates` is matched by civil minute in the routine's zone, the same
    // way `Routine::is_skipped` does it — a stored instant that a tzdb update
    // moved must still match the occurrence it was meant to skip.
    let skip_keys: BTreeSet<String> = routine
        .skip_dates
        .iter()
        .filter_map(|d| occurrence_key_at(&routine.timezone, *d).ok())
        .collect();

    let mut completed = 0u32;
    let mut skipped = 0u32;
    for occ in &occurrences {
        if routine.streak_key_seen(&occ.key) {
            completed = completed.saturating_add(1);
        } else if routine.skipped_keys.contains(&occ.key) || skip_keys.contains(&occ.key) {
            skipped = skipped.saturating_add(1);
        }
    }
    let expected = u32::try_from(occurrences.len()).unwrap_or(u32::MAX);
    let missed = expected.saturating_sub(completed);
    let drift = if expected == 0 {
        0.0
    } else {
        f64::from(missed) / f64::from(expected)
    };
    Ok(RoutineDrift {
        routine: routine.id,
        title: routine.template.title.clone(),
        expected,
        completed,
        skipped,
        missed,
        drift,
        over_threshold: expected > 0 && drift > threshold,
        streak: routine.streak_counter,
        last_completed_at_ms: routine
            .last_completed_at
            .and_then(|t| u64::try_from(t.as_millisecond()).ok()),
        paused: routine.paused,
    })
}

/// Convert epoch milliseconds into a jiff [`Timestamp`].
fn to_timestamp(ms: u64) -> Result<Timestamp, StatsError> {
    let signed = i64::try_from(ms).map_err(|_| StatsError::TimestampRange(ms))?;
    Timestamp::from_millisecond(signed).map_err(|_| StatsError::TimestampRange(ms))
}

/// Start-of-day instant for a civil date in `tz`, in epoch milliseconds.
fn zone_start_ms(date: jiff::civil::Date, tz: &TimeZone) -> Result<u64, StatsError> {
    let zoned = date
        .to_zoned(tz.clone())
        .map_err(|_| StatsError::Overflow)?;
    u64::try_from(zoned.timestamp().as_millisecond()).map_err(|_| StatsError::Overflow)
}

/// Domain weekday → jiff weekday.
const fn to_jiff(w: Weekday) -> JiffWeekday {
    match w {
        Weekday::Su => JiffWeekday::Sunday,
        Weekday::Mo => JiffWeekday::Monday,
        Weekday::Tu => JiffWeekday::Tuesday,
        Weekday::We => JiffWeekday::Wednesday,
        Weekday::Th => JiffWeekday::Thursday,
        Weekday::Fr => JiffWeekday::Friday,
        Weekday::Sa => JiffWeekday::Saturday,
    }
}

/// Whole days spanned by `[from, to)`, for callers sizing a review window.
#[must_use]
pub const fn days_between(from_ms: u64, to_ms: u64) -> u64 {
    to_ms.saturating_sub(from_ms) / DAY_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Energy;
    use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
    use crate::rrule::RRule;
    use crate::task::Task;
    use crate::unknown::Unknowns;
    use std::collections::BTreeSet;
    use sunrise_id::EntityKind;

    /// 2026-01-05T00:00:00Z is a **Monday**, which makes every week boundary in
    /// these tests checkable by hand.
    const MON: u64 = 1_767_571_200_000;
    const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

    fn eref(kind: EntityKind, b: u8) -> EntityRef {
        EntityRef::new(kind, [b; 16])
    }

    fn ts(ms: u64) -> Timestamp {
        Timestamp::from_millisecond(i64::try_from(ms).unwrap()).unwrap()
    }

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    fn task(id: u8, stream: u8) -> Task {
        Task {
            reminder_lead_s: None,
            id: eref(EntityKind::Task, id),
            created_at: ts(MON),
            updated_at: ts(MON),
            title: "t".into(),
            body: None,
            stream_id: eref(EntityKind::Stream, stream),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn op(n: u8, at_ms: u64, payload: OpPayload) -> OpRecord {
        let target = match &payload {
            OpPayload::TaskCreated(t) | OpPayload::TaskUpdated(t) => t.id,
            _ => eref(EntityKind::Task, 0),
        };
        OpRecord {
            op_id: [n; 16],
            at_ms,
            device: [1; 16],
            target,
            payload,
        }
    }

    /// A 3-week grid whose last week starts on `MON`.
    fn grid3() -> WeekGrid {
        WeekGrid::trailing(MON + 3_600_000, 3, &utc(), Weekday::Mo).unwrap()
    }

    #[test]
    fn trailing_grid_lands_on_week_starts_and_includes_the_current_week() {
        let g = grid3();
        assert_eq!(
            g.starts(),
            [MON - 2 * WEEK_MS, MON - WEEK_MS, MON],
            "three Mondays ending with the one we are in"
        );
        assert_eq!(g.end_ms(), MON + WEEK_MS);
        assert_eq!(g.last_start_ms(), MON);
    }

    #[test]
    fn grid_buckets_instants_and_rejects_everything_outside() {
        let g = grid3();
        assert_eq!(g.index_of(MON - 2 * WEEK_MS), Some(0), "inclusive start");
        assert_eq!(g.index_of(MON - WEEK_MS - 1), Some(0), "last ms of week 0");
        assert_eq!(g.index_of(MON), Some(2));
        assert_eq!(
            g.index_of(MON + WEEK_MS - 1),
            Some(2),
            "last ms of the grid"
        );
        assert_eq!(g.index_of(MON + WEEK_MS), None, "exclusive end");
        assert_eq!(g.index_of(MON - 2 * WEEK_MS - 1), None, "before the grid");
    }

    #[test]
    fn a_week_that_crosses_a_dst_transition_is_25_hours_long() {
        // US DST ends 2025-11-02; the week starting Monday 2025-10-27 in
        // America/New_York therefore spans 7*24+1 hours. A fixed-604800s grid
        // would put the following Monday an hour early.
        let tz = TimeZone::get("America/New_York").expect("tzdb bundled");
        let now = u64::try_from(
            "2025-11-03T12:00:00-05:00"
                .parse::<Timestamp>()
                .unwrap()
                .as_millisecond(),
        )
        .unwrap();
        let g = WeekGrid::trailing(now, 2, &tz, Weekday::Mo).unwrap();
        let [a, b] = [g.starts()[0], g.starts()[1]];
        assert_eq!(
            b - a,
            WEEK_MS + 3_600_000,
            "the DST week really is an hour longer"
        );
        // And every start is local midnight, not 23:00 the day before.
        for s in g.starts() {
            let z = ts(*s).to_zoned(tz.clone());
            assert_eq!(z.hour(), 0, "week starts at local midnight: {z}");
            assert_eq!(z.weekday(), JiffWeekday::Monday);
        }
    }

    /// A Sunday-start grid, so the `week_start` parameter is something other
    /// than the `Weekday::Mo` every caller passes.
    ///
    /// **Unobserved in v1.** No product surface is known to set a non-Monday
    /// week start: every call site in the workspace passes `Weekday::Mo`, and
    /// there is no setting that would produce anything else. This pins the
    /// parameter's arithmetic so a later reader can trust it, and says plainly
    /// that trusting it is not the same as the feature having shipped.
    #[test]
    fn a_sunday_start_grid_begins_its_weeks_on_the_sunday_before() {
        // `MON` is a Monday, so a Sunday-start week containing it began the
        // day before — the one boundary a Monday-only test can never catch.
        let g = WeekGrid::trailing(MON + 3_600_000, 3, &utc(), Weekday::Su).unwrap();
        let sun = MON - 86_400_000;
        assert_eq!(
            g.starts(),
            [sun - 2 * WEEK_MS, sun - WEEK_MS, sun],
            "three Sundays ending with the one we are in"
        );
        assert_eq!(g.end_ms(), sun + WEEK_MS);
        for start in g.starts() {
            assert_eq!(
                ts(*start).to_zoned(utc()).weekday(),
                JiffWeekday::Sunday,
                "every bucket starts on the requested weekday"
            );
        }
        // And `MON` itself falls in the *last* bucket, not the first day of a
        // new one: this is the assertion a Monday-start grid gets wrong.
        assert_eq!(g.index_of(MON), Some(2));
        assert_eq!(
            g.index_of(sun - 2 * WEEK_MS - 1),
            None,
            "the instant before the grid"
        );
        assert_eq!(g.index_of(sun - 1), Some(1), "the last ms of week 1");
    }

    /// Asking for zero weeks is clamped to one rather than yielding a grid
    /// that buckets nothing. `index_of` on an empty grid would answer `None`
    /// for every instant, which reads as "no activity" instead of "bad call".
    #[test]
    fn a_grid_of_zero_weeks_is_clamped_to_one() {
        let g = WeekGrid::trailing(MON + 3_600_000, 0, &utc(), Weekday::Mo).unwrap();
        assert_eq!(g.len(), 1, "clamped, not empty");
        assert!(!g.is_empty());
        assert_eq!(g.starts(), [MON]);
        assert_eq!(g.end_ms(), MON + WEEK_MS);
        assert_eq!(g.last_start_ms(), MON);
        assert_eq!(g.index_of(MON + 3_600_000), Some(0));
    }

    /// The two lengths the spec fixes. They are read by callers that size a
    /// review window, so a change to either is a change to what a user sees.
    #[test]
    fn the_trend_and_drift_windows_are_the_lengths_the_spec_states() {
        assert_eq!(TREND_WEEKS, 12, "the spec asks for the last 12 weeks");
        assert_eq!(DRIFT_WINDOW_WEEKS, 4, "drift is measured over 4 weeks");
    }

    #[test]
    fn completion_is_credited_to_the_week_of_done_at_not_of_the_op() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        // Completed in week 1, but the op that carries it is stamped in week 2
        // (a device that was offline and synced late).
        done.completed_at = Some(ts(MON - WEEK_MS + 3_600_000).into());

        let trends = fold_trends(
            &[
                op(1, MON - WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(2, MON + 1_000, OpPayload::TaskUpdated(Box::new(done))),
            ],
            &g,
        );
        let completed: Vec<i64> = trends.overall.iter().map(|b| b.completed).collect();
        assert_eq!(completed, vec![0, 1, 0], "credited to done_at's week");
    }

    #[test]
    fn a_reopen_decrements_the_week_of_the_original_completion() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON - WEEK_MS).into());
        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;
        reopened.completed_at = None;

        let trends = fold_trends(
            &[
                op(1, MON - 2 * WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(2, MON - WEEK_MS, OpPayload::TaskUpdated(Box::new(done))),
                // Re-opened two weeks later, in the current week.
                op(3, MON + 1_000, OpPayload::TaskUpdated(Box::new(reopened))),
            ],
            &g,
        );
        let completed: Vec<i64> = trends.overall.iter().map(|b| b.completed).collect();
        assert_eq!(
            completed,
            vec![0, 0, 0],
            "week 1's +1 is cancelled where it was counted, not where the re-open landed"
        );
    }

    #[test]
    fn completing_reopening_and_recompleting_nets_out_to_the_new_week() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON - 2 * WEEK_MS).into());
        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;
        reopened.completed_at = None;
        let mut redone = reopened.clone();
        redone.state = TaskState::Done;
        redone.completed_at = Some(ts(MON).into());

        let trends = fold_trends(
            &[
                op(1, MON - 2 * WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(2, MON - 2 * WEEK_MS, OpPayload::TaskUpdated(Box::new(done))),
                op(3, MON - WEEK_MS, OpPayload::TaskUpdated(Box::new(reopened))),
                op(4, MON, OpPayload::TaskUpdated(Box::new(redone))),
            ],
            &g,
        );
        let completed: Vec<i64> = trends.overall.iter().map(|b| b.completed).collect();
        assert_eq!(completed, vec![0, 0, 1], "one task, counted exactly once");
    }

    #[test]
    fn a_reopen_of_a_completion_older_than_the_grid_has_nowhere_to_land() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON - 40 * WEEK_MS).into());
        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;

        let trends = fold_trends(
            &[
                op(1, MON - 40 * WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(
                    2,
                    MON - 40 * WEEK_MS,
                    OpPayload::TaskUpdated(Box::new(done)),
                ),
                op(3, MON, OpPayload::TaskUpdated(Box::new(reopened))),
            ],
            &g,
        );
        assert!(
            trends.overall.iter().all(|b| b.completed == 0),
            "an out-of-window decrement is dropped, not folded into week 0: {:?}",
            trends.overall
        );
    }

    #[test]
    fn defers_are_counted_per_increment_and_split_per_stream() {
        let g = grid3();
        let a = task(1, 7);
        let b = task(2, 8);
        let mut a1 = a.clone();
        a1.deferred_count = 1;
        let mut a2 = a1.clone();
        a2.deferred_count = 3; // two defers batched into one op
        let mut b1 = b.clone();
        b1.deferred_count = 1;

        let trends = fold_trends(
            &[
                op(1, MON - WEEK_MS, OpPayload::TaskCreated(Box::new(a))),
                op(2, MON - WEEK_MS, OpPayload::TaskUpdated(Box::new(a1))),
                op(3, MON, OpPayload::TaskUpdated(Box::new(a2))),
                op(4, MON, OpPayload::TaskCreated(Box::new(b))),
                op(5, MON, OpPayload::TaskUpdated(Box::new(b1))),
            ],
            &g,
        );
        let deferred: Vec<i64> = trends.overall.iter().map(|b| b.deferred).collect();
        assert_eq!(deferred, vec![0, 1, 3], "2 from `a2`, 1 from `b1`");

        let line_a = trends.for_stream(eref(EntityKind::Stream, 7)).unwrap();
        let line_b = trends.for_stream(eref(EntityKind::Stream, 8)).unwrap();
        assert!(
            trends.for_stream(eref(EntityKind::Stream, 99)).is_none(),
            "a Stream with no activity in the window has no line at all — not \
             a zero-filled one, which a chart would draw"
        );
        assert_eq!(
            line_a.iter().map(|b| b.deferred).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            line_b.iter().map(|b| b.deferred).collect::<Vec<_>>(),
            vec![0, 0, 1]
        );
    }

    #[test]
    fn creates_are_bucketed_and_a_task_created_done_counts_once() {
        let g = grid3();
        let mut t = task(1, 9);
        t.state = TaskState::Done;
        t.completed_at = Some(ts(MON).into());
        let trends = fold_trends(&[op(1, MON, OpPayload::TaskCreated(Box::new(t)))], &g);
        assert_eq!(trends.overall[2].created, 1);
        assert_eq!(trends.overall[2].completed, 1);
    }

    #[test]
    fn an_update_with_no_predecessor_is_not_a_completion() {
        let g = grid3();
        let mut done = task(1, 9);
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON).into());
        // Only the update is in the input — the create was compacted away.
        let trends = fold_trends(&[op(1, MON, OpPayload::TaskUpdated(Box::new(done)))], &g);
        assert!(
            trends.overall.iter().all(|b| b.completed == 0),
            "no baseline means no transition: {:?}",
            trends.overall
        );
    }

    #[test]
    fn per_stream_lines_all_have_the_grid_width() {
        let g = grid3();
        let t = task(1, 9);
        let trends = fold_trends(&[op(1, MON, OpPayload::TaskCreated(Box::new(t)))], &g);
        assert_eq!(trends.week_starts.len(), 3);
        for line in &trends.per_stream {
            assert_eq!(line.weeks.len(), 3);
            assert_eq!(
                line.weeks
                    .iter()
                    .map(|b| b.week_start_ms)
                    .collect::<Vec<_>>(),
                trends.week_starts
            );
        }
    }

    // ---- routine drift ----

    fn daily_routine() -> Routine {
        Routine {
            id: eref(EntityKind::Routine, 5),
            created_at: ts(MON),
            updated_at: ts(MON),
            template: TaskTemplate {
                title: "Stretch".into(),
                stream_id: eref(EntityKind::Stream, 9),
                contexts: Vec::new(),
                energy: Some(Energy::Low),
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: ts(MON),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: 0,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    /// The 7 occurrence keys a daily routine anchored at `MON` produces in the
    /// week that starts there.
    fn week_keys() -> Vec<String> {
        (0..7)
            .map(|d| {
                let z = ts(MON + d * 86_400_000).to_zoned(utc());
                format!("{:04}-{:02}-{:02}T00:00", z.year(), z.month(), z.day())
            })
            .collect()
    }

    #[test]
    fn a_routine_completed_every_day_has_zero_drift() {
        let mut r = daily_routine();
        r.streak_keys = week_keys();
        r.streak_keys.sort();
        r.streak_counter = 7;
        let d = routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD).unwrap();
        assert_eq!(d.expected, 7);
        assert_eq!(d.completed, 7);
        assert_eq!(d.missed, 0);
        assert!((d.drift - 0.0).abs() < f64::EPSILON);
        assert!(!d.over_threshold);
        assert_eq!(d.streak, 7);
    }

    #[test]
    fn missing_more_than_thirty_percent_trips_the_tuning_threshold() {
        let mut r = daily_routine();
        // 4 of 7 done → 3 missed → drift 3/7 ≈ 0.4286 > 0.30.
        let mut keys = week_keys();
        keys.truncate(4);
        keys.sort();
        r.streak_keys = keys;

        let d = routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD).unwrap();
        assert_eq!((d.expected, d.completed, d.missed), (7, 4, 3));
        assert!((d.drift - 3.0 / 7.0).abs() < 1e-12);
        assert!(d.over_threshold);
    }

    #[test]
    fn exactly_at_the_threshold_does_not_trip_it() {
        let mut r = daily_routine();
        // 7 of 10 done over 10 days → drift exactly 0.30, which is not "> 30%".
        let keys: Vec<String> = (0..10)
            .map(|d| {
                let z = ts(MON + d * 86_400_000).to_zoned(utc());
                format!("{:04}-{:02}-{:02}T00:00", z.year(), z.month(), z.day())
            })
            .take(7)
            .collect();
        r.streak_keys = keys;
        r.streak_keys.sort();
        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + 10 * 86_400_000)),
            DEFAULT_DRIFT_THRESHOLD,
        )
        .unwrap();
        assert_eq!((d.expected, d.completed, d.missed), (10, 7, 3));
        assert!(!d.over_threshold, "drift was {}", d.drift);
    }

    #[test]
    fn explicit_skips_count_as_missed_but_are_reported_separately() {
        let mut r = daily_routine();
        let keys = week_keys();
        r.streak_keys = keys[..5].to_vec();
        r.streak_keys.sort();
        r.skipped_keys = keys[5..].to_vec();

        let d = routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD).unwrap();
        assert_eq!((d.completed, d.skipped, d.missed), (5, 2, 2));
    }

    #[test]
    fn an_empty_window_yields_zero_drift_and_no_suggestion() {
        let r = daily_routine();
        // A window entirely before the routine's anchor produces nothing.
        let d = routine_drift(
            &r,
            (ts(MON - 10 * WEEK_MS), ts(MON - 9 * WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
        )
        .unwrap();
        assert_eq!(d.expected, 0);
        assert!(!d.over_threshold, "no occurrences is not drift");
    }

    /// `title`, `last_completed_at_ms` and `paused` are carried straight
    /// through from the Routine, and nothing asserted any of the three: a
    /// drift row could have reported an empty title, no last completion, and
    /// the wrong paused flag with the suite green. The fixture sets all three
    /// away from their defaults so the plumbing is what is being read.
    #[test]
    fn drift_carries_the_routines_title_last_completion_and_paused_flag() {
        let mut r = daily_routine();
        r.template.title = "Water the ferns".into();
        r.last_completed_at = Some(ts(MON + 3 * 86_400_000));
        r.paused = true;
        r.streak_counter = 4;

        let d = routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD).unwrap();
        assert_eq!(d.routine, r.id);
        assert_eq!(
            d.title, "Water the ferns",
            "the row is what the History view labels the routine with"
        );
        assert_eq!(
            d.last_completed_at_ms,
            Some(MON + 3 * 86_400_000),
            "epoch milliseconds, straight off the Routine"
        );
        assert!(
            d.paused,
            "a paused routine needs no pause suggestion, so the caller must be told"
        );
        assert_eq!(d.streak, 4);
        // And pausing does **not** suppress the occurrence set: `routine_drift`
        // calls `expand` rather than `Routine::occurrences_in` on purpose, so a
        // paused routine still reports the drift that led to the pause.
        assert_eq!(d.expected, 7, "a paused routine is still measured");
    }

    /// A completion recorded before the epoch cannot be expressed in the
    /// unsigned field, and is reported as "no completion" rather than as a
    /// wrapped instant. Only reachable from a corrupt or hostile row.
    #[test]
    fn a_pre_epoch_last_completion_is_reported_as_none_not_wrapped() {
        let mut r = daily_routine();
        r.last_completed_at = Some(Timestamp::from_millisecond(-1).unwrap());
        let d = routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD).unwrap();
        assert_eq!(d.last_completed_at_ms, None);
    }

    /// `ends_at` truncates the occurrence set, so a routine that finished
    /// mid-window is not scored against the days it was never meant to run.
    /// Without this, a finished routine reads as 100% drift forever.
    #[test]
    fn an_ends_at_inside_the_window_truncates_the_expected_occurrences() {
        let mut r = daily_routine();
        // Four days of a seven-day window: MON, +1, +2, +3. The bound is
        // inclusive (`o.at <= end`), so the occurrence landing exactly on
        // `ends_at` counts.
        r.ends_at = Some(ts(MON + 3 * 86_400_000));
        r.streak_keys = week_keys()[..4].to_vec();
        r.streak_keys.sort();

        let d = routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD).unwrap();
        assert_eq!(
            d.expected, 4,
            "the three days after `ends_at` are not occurrences"
        );
        assert_eq!(d.completed, 4);
        assert_eq!(d.missed, 0, "a routine that ended has not drifted");
        assert!(!d.over_threshold);

        // The same routine without the bound is scored over the whole window,
        // which is what makes the truncation load-bearing rather than a no-op.
        let mut unbounded = r.clone();
        unbounded.ends_at = None;
        let d2 = routine_drift(
            &unbounded,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
        )
        .unwrap();
        assert_eq!((d2.expected, d2.missed), (7, 3));
    }

    /// `skip_dates` holds *instants*, and they are matched by rendering each
    /// one to a civil-minute key in the routine's own zone — the same way
    /// `Routine::is_skipped` does it. The comment in `routine_drift` says this
    /// is so a stored instant a tzdb update moved still matches the occurrence
    /// it was meant to skip; nothing asserted that the path runs at all.
    #[test]
    fn a_skip_date_instant_is_matched_by_civil_key_not_by_equality() {
        let mut r = daily_routine();
        // Wednesday of the anchored week, as an instant rather than a key.
        r.skip_dates = vec![ts(MON + 2 * 86_400_000)];

        let d = routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD).unwrap();
        assert_eq!(d.expected, 7);
        assert_eq!(
            d.skipped, 1,
            "the instant resolved to an occurrence key and matched it"
        );
        assert_eq!(
            d.missed, 7,
            "an explicit skip is still a missed occurrence — `skipped` only says why"
        );
        // The key it must have produced, spelled out: the same civil minute
        // the expansion names that occurrence by.
        assert_eq!(
            occurrence_key_at("UTC", ts(MON + 2 * 86_400_000)).unwrap(),
            week_keys()[2]
        );
    }

    /// The key is a civil **minute**, which decides how close a stored instant
    /// has to be. A second's drift still names the same occurrence; a minute's
    /// names none, and is ignored rather than skipping something adjacent.
    #[test]
    fn a_skip_dates_precision_is_the_civil_minute() {
        let mut within = daily_routine();
        within.skip_dates = vec![ts(MON + 2 * 86_400_000 + 1_000)];
        let d = routine_drift(
            &within,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
        )
        .unwrap();
        assert_eq!(d.skipped, 1, "a second's drift is the same civil minute");

        let mut outside = daily_routine();
        outside.skip_dates = vec![ts(MON + 2 * 86_400_000 + 60_000)];
        let d = routine_drift(
            &outside,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
        )
        .unwrap();
        assert_eq!(
            d.skipped, 0,
            "a minute's drift names no occurrence, and skips nothing rather than \
             skipping the nearest one"
        );
    }

    #[test]
    fn an_unresolvable_timezone_is_an_error_not_a_silent_utc_fallback() {
        let mut r = daily_routine();
        r.timezone = "Mars/Olympus".into();
        assert_eq!(
            routine_drift(&r, (ts(MON), ts(MON + WEEK_MS)), DEFAULT_DRIFT_THRESHOLD),
            Err(ExpandError::InvalidTimeZone("Mars/Olympus".into()))
        );
    }

    #[test]
    fn days_between_counts_whole_days() {
        assert_eq!(days_between(MON, MON + WEEK_MS), 7);
        assert_eq!(days_between(MON, MON), 0);
        assert_eq!(days_between(MON + 1, MON), 0, "saturating, never negative");
    }
}
