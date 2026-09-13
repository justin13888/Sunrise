//! Weekly and daily review per `docs/08-features/reviews-and-stats.md`.
//!
//! > *Periodic review is the most important habit Sunrise wants to enable.
//! > Stats support reviews; they are not a leaderboard.*
//!
//! The weekly review is a five-step guided flow. Steps 2 and 4 are
//! *interactions* — the UI walks the user through triage and slipped-commitment
//! decisions one item at a time — but every step is driven by a **list this
//! module computes**, and the shape of those lists is what makes the flow
//! reviewable rather than a wall of text.
//!
//! # Everything here is a pure fold
//!
//! [`build_weekly_review`] takes values the caller already read — streams,
//! tasks, the activity feed for the window, routines, the focus fold, the
//! trend fold — and assembles the review. It never queries, never reads a
//! clock, and never re-derives a number another fold already owns:
//!
//! * "completed / deferred / dropped this week" comes from the **activity
//!   feed** ([`crate::activity::fold_activity`]), so the review and the
//!   timeline can never disagree about what happened;
//! * time-in-focus and the estimate calibration come from
//!   [`crate::focus::fold_focus_stats`] — the review is where the spec says a
//!   user finally *sees* "your 30-minute estimates run ~1.7× long";
//! * the trends come from [`crate::stats::fold_trends`];
//! * streaks and drift come from [`crate::streak`] / [`crate::stats`].
//!
//! # The snapshot
//!
//! Step 5 says the review is *"saved … as an opaque entity (queryable in
//! History)"*. [`ReviewSnapshot`] is that entity. It is the one part of this
//! feature that is **not** derivable: everything else can be recomputed from
//! the op log for any past week, but "the user actually sat down and reviewed
//! week W on device D" is a new fact. It is therefore an append-only record
//! keyed by its own `rvw_` id — the same representation ADR-0013 chose for
//! focus sessions, and for the same reason: two devices can each record a
//! review without contending over a register, and re-delivery is a no-op.

use crate::activity::{ActivityEvent, ActivityKind};
use crate::focus::{FocusStats, StreamFocus};
use crate::inbox::inbox_stream_ref;
use crate::routine::Routine;
use crate::stats::{RoutineDrift, Trends, WeekBucket};
use crate::stream::Stream;
use crate::task::{Task, TaskState};
use crate::time::SunriseTime;
use crate::unknown::Unknowns;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

/// A Routine's streak, for step 3 and the snapshot.
///
/// Defined in [`crate::streak`], which owns the streak mechanics; re-exported
/// here because the review is where a streak is read.
pub use crate::streak::StreakRow;

/// A half-open review window `[start_ms, end_ms)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWindow {
    /// Inclusive start (ms since epoch).
    pub start_ms: u64,
    /// Exclusive end (ms since epoch).
    pub end_ms: u64,
}

impl ReviewWindow {
    /// Construct, normalizing an inverted range to an empty one.
    #[must_use]
    pub fn new(start_ms: u64, end_ms: u64) -> Self {
        Self {
            start_ms,
            end_ms: end_ms.max(start_ms),
        }
    }

    /// Whether `at_ms` falls in the window.
    #[must_use]
    pub const fn contains(&self, at_ms: u64) -> bool {
        at_ms >= self.start_ms && at_ms < self.end_ms
    }
}

/// Identity of a Stream as the review needs it. A thin row rather than the
/// whole [`Stream`], so the caller can pass the synthetic Inbox too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewStream {
    /// Stream id.
    pub id: EntityRef,
    /// Display name.
    pub name: String,
    /// Archived streams drop out of the review.
    pub archived: bool,
    /// Paused streams drop out of the review.
    pub paused: bool,
}

impl From<&Stream> for ReviewStream {
    fn from(s: &Stream) -> Self {
        Self {
            id: s.id,
            name: s.name.clone(),
            archived: s.archived,
            paused: s.paused,
        }
    }
}

/// Step 1 — one Stream's summary.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamReview {
    /// Stream id.
    pub stream: EntityRef,
    /// Display name.
    pub name: String,
    /// Tasks completed in the window (spec: "count + list (collapsed)").
    pub completed: Vec<Task>,
    /// Tasks deferred at least once in the window (spec: "list (full)").
    pub deferred: Vec<Task>,
    /// Tasks created in the window that nothing has happened to since
    /// (spec: "created this week and not yet acted on").
    pub created_untouched: Vec<Task>,
    /// Time-in-focus + calibration for this Stream, when it has any.
    pub focus: Option<StreamFocus>,
    /// This Stream's completed / deferred trend line.
    pub trend: Vec<WeekBucket>,
}

/// Step 5 — the counts on the summary screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewTotals {
    /// Tasks completed in the window.
    pub completed: u32,
    /// Defer events in the window.
    pub deferred: u32,
    /// Tasks cancelled or deleted in the window — the spec's "dropped".
    pub dropped: u32,
    /// Tasks created in the window.
    pub created: u32,
    /// Tasks re-opened in the window. Not in the spec's list, but a review
    /// that reports 8 completions while hiding 3 resurrections is misleading.
    pub reopened: u32,
}

/// The assembled weekly review — one value per spec step.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WeeklyReview {
    /// The window under review.
    pub window: ReviewWindow,
    /// Step 1 — per-Stream summaries, ordered by stream id.
    pub streams: Vec<StreamReview>,
    /// Step 2 — untriaged inbox items, oldest first.
    pub inbox: Vec<Task>,
    /// Step 3 — routines with non-trivial drift, worst first.
    pub drifting_routines: Vec<RoutineDrift>,
    /// Step 3 — every live Routine's streak, longest first.
    pub streaks: Vec<StreakRow>,
    /// Step 4 — commitments that came due on or before the window's end and
    /// are still open.
    ///
    /// Deliberately **not** bounded below by the window. Step 4 asks what the
    /// user has broken their word about, and a commitment that slipped three
    /// weeks ago is still slipped; bounding it to the window would hide
    /// exactly the items most in need of a decision. One consequence worth
    /// knowing: this list grows over a vault's lifetime, so the first weekly
    /// review of an old vault can be long.
    pub slipped: Vec<Task>,
    /// Step 5 — the counts.
    pub totals: ReviewTotals,
    /// Time-in-focus and the estimate calibration for the window.
    pub focus: FocusStats,
    /// 12-week trends.
    pub trends: Trends,
}

/// Everything [`build_weekly_review`] needs, fetched by the caller.
#[derive(Debug, Clone)]
pub struct WeeklyReviewInput {
    /// The window under review.
    pub window: ReviewWindow,
    /// Candidate Streams (including the synthetic Inbox row, if wanted).
    pub streams: Vec<ReviewStream>,
    /// Every live Task. Filtering to the window is this module's job.
    pub tasks: Vec<Task>,
    /// The activity feed covering the window, ascending.
    pub activity: Vec<ActivityEvent>,
    /// Every live Routine.
    pub routines: Vec<Routine>,
    /// Per-Routine drift over the tuning window.
    pub drift: Vec<RoutineDrift>,
    /// Focus fold for the window.
    pub focus: FocusStats,
    /// Trend fold.
    pub trends: Trends,
}

/// Assemble the weekly review.
///
/// The window filter is applied to **events**, not to task timestamps: "tasks
/// completed this week" means "tasks whose completion event landed this week",
/// which stays right for a task completed on another device and merged in
/// late, and for a task completed twice with a re-open in between.
#[must_use]
pub fn build_weekly_review(input: WeeklyReviewInput) -> WeeklyReview {
    let WeeklyReviewInput {
        window,
        streams,
        tasks,
        activity,
        routines,
        drift,
        focus,
        trends,
    } = input;

    let mut totals = ReviewTotals::default();
    let mut completed_ids: BTreeSet<EntityRef> = BTreeSet::new();
    let mut deferred_ids: BTreeSet<EntityRef> = BTreeSet::new();
    let mut created_ids: BTreeSet<EntityRef> = BTreeSet::new();
    // "Acted on" = any event other than the creation itself, including focus
    // sessions: opening a timer on a task is unmistakably acting on it.
    let mut touched_ids: BTreeSet<EntityRef> = BTreeSet::new();

    for e in activity.iter().filter(|e| window.contains(e.at_ms)) {
        match &e.kind {
            ActivityKind::TaskCreated => {
                totals.created = totals.created.saturating_add(1);
                created_ids.insert(e.entity);
            }
            ActivityKind::TaskCompleted => {
                totals.completed = totals.completed.saturating_add(1);
                completed_ids.insert(e.entity);
                touched_ids.insert(e.entity);
            }
            ActivityKind::TaskReopened => {
                totals.reopened = totals.reopened.saturating_add(1);
                // A re-open un-does the completion for the purposes of this
                // review's list: the task is open again and belongs in
                // "slipped", not in "completed".
                completed_ids.remove(&e.entity);
                touched_ids.insert(e.entity);
            }
            ActivityKind::TaskDeferred { .. } => {
                totals.deferred = totals.deferred.saturating_add(1);
                deferred_ids.insert(e.entity);
                touched_ids.insert(e.entity);
            }
            ActivityKind::TaskCancelled | ActivityKind::TaskDeleted => {
                totals.dropped = totals.dropped.saturating_add(1);
                completed_ids.remove(&e.entity);
                touched_ids.insert(e.entity);
            }
            _ => {
                touched_ids.insert(e.entity);
            }
        }
    }

    let by_id: BTreeMap<EntityRef, &Task> = tasks.iter().map(|t| (t.id, t)).collect();
    let focus_by_stream: BTreeMap<EntityRef, &StreamFocus> =
        focus.per_stream.iter().map(|f| (f.stream, f)).collect();

    let mut stream_reviews: Vec<StreamReview> = streams
        .iter()
        .filter(|s| !s.archived && !s.paused)
        .map(|s| {
            let pick = |ids: &BTreeSet<EntityRef>| -> Vec<Task> {
                let mut v: Vec<Task> = ids
                    .iter()
                    .filter_map(|id| by_id.get(id).copied())
                    .filter(|t| t.stream_id == s.id && !t.deleted)
                    .cloned()
                    .collect();
                v.sort_by_key(|t| (t.created_at, t.id));
                v
            };
            let created_untouched: BTreeSet<EntityRef> =
                created_ids.difference(&touched_ids).copied().collect();
            StreamReview {
                stream: s.id,
                name: s.name.clone(),
                completed: pick(&completed_ids),
                // "Deferred ≥ 1 time" — the event says it happened this week,
                // the counter confirms the task still carries the deferral.
                deferred: pick(&deferred_ids)
                    .into_iter()
                    .filter(|t| t.deferred_count >= 1)
                    .collect(),
                created_untouched: pick(&created_untouched),
                focus: focus_by_stream.get(&s.id).map(|f| (*f).clone()),
                trend: trends.for_stream(s.id).unwrap_or_default().to_vec(),
            }
        })
        .collect();
    stream_reviews.sort_by_key(|s| s.stream);

    let inbox_id = inbox_stream_ref();
    let mut inbox: Vec<Task> = tasks
        .iter()
        .filter(|t| {
            t.stream_id == inbox_id && !t.deleted && !t.archived && t.state == TaskState::Todo
        })
        .cloned()
        .collect();
    inbox.sort_by_key(|t| (t.created_at, t.id));

    let mut slipped: Vec<Task> = tasks
        .iter()
        .filter(|t| !t.deleted && !t.archived)
        .filter(|t| !matches!(t.state, TaskState::Done | TaskState::Cancelled))
        .filter(|t| commitment_at(t).is_some_and(|m| m < window.end_ms))
        .cloned()
        .collect();
    // Earliest commitment first: the thing that slipped furthest is the first
    // decision the user is asked to make.
    slipped.sort_by_key(|t| (commitment_at(t), t.id));

    let mut drifting_routines: Vec<RoutineDrift> =
        drift.into_iter().filter(|d| d.over_threshold).collect();
    drifting_routines.sort_by(|a, b| {
        b.drift
            .partial_cmp(&a.drift)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.routine.cmp(&b.routine))
    });

    let mut streaks: Vec<StreakRow> = routines
        .iter()
        .filter(|r| !r.deleted && !r.archived)
        .map(|r| StreakRow {
            routine: r.id,
            title: r.template.title.clone(),
            streak: r.streak_counter,
            last_completed_at_ms: r.last_completed_at.and_then(ts_to_ms),
        })
        .collect();
    streaks.sort_by(|a, b| {
        b.streak
            .cmp(&a.streak)
            .then_with(|| a.routine.cmp(&b.routine))
    });

    WeeklyReview {
        window,
        streams: stream_reviews,
        inbox,
        drifting_routines,
        streaks,
        slipped,
        totals,
        focus,
        trends,
    }
}

/// The optional 60-second daily flow (spec §Daily review, off by default).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyReview {
    /// The glance window — typically yesterday evening to now.
    pub window: ReviewWindow,
    /// Captures made in the window that are still sitting untriaged.
    pub inbox: Vec<Task>,
    /// The day's planned tasks.
    pub today: Vec<Task>,
    /// The subset of `today` that cannot be started because a blocker is still
    /// open — the concrete half of the spec's "Any blocker for today?" prompt.
    pub blocked: Vec<Task>,
}

/// Build the daily review from already-fetched lists.
///
/// `inbox` and `today` come from the existing Inbox / Today read paths;
/// `blocked_ids` is the derived-blocked set the dependency index already
/// computes. Nothing is re-derived here — the flow is a *selection*, and
/// keeping it one is what makes it a 60-second screen.
#[must_use]
pub fn build_daily_review(
    window: ReviewWindow,
    inbox: Vec<Task>,
    today: Vec<Task>,
    blocked_ids: &BTreeSet<EntityRef>,
) -> DailyReview {
    let mut recent: Vec<Task> = inbox
        .into_iter()
        .filter(|t| ts_to_ms(t.created_at).is_some_and(|ms| window.contains(ms)))
        .collect();
    recent.sort_by_key(|t| (t.created_at, t.id));

    let blocked: Vec<Task> = today
        .iter()
        .filter(|t| blocked_ids.contains(&t.id))
        .cloned()
        .collect();

    DailyReview {
        window,
        inbox: recent,
        today,
        blocked,
    }
}

/// One Stream's line in a saved snapshot: counts only, never task lists.
///
/// A snapshot is a record that a review happened and what it concluded, not a
/// second copy of the vault. The lists are recomputable from the op log for the
/// same window; the counts are what the History view shows at a glance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSnapshotStream {
    /// Stream id.
    pub stream: EntityRef,
    /// Its name at review time.
    pub name: String,
    /// Completed in the window.
    pub completed: u32,
    /// Deferred in the window.
    pub deferred: u32,
    /// Created in the window.
    pub created: u32,
}

/// Step 5's saved artifact: *"a review snapshot stored as an opaque entity
/// (queryable in History)"*.
///
/// **Append-only.** A snapshot is written once, keyed by its own `rvw_` id, and
/// never updated — so two devices that both record a review of the same week
/// produce two rows rather than a lost write, and a re-delivered op is an
/// ignored duplicate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSnapshot {
    /// Snapshot id (`rvw_`).
    pub id: EntityRef,
    /// When the review was completed.
    pub created_at: Timestamp,
    /// Window the review covered.
    ///
    /// A `Timestamp`, like `created_at` two fields up. This struct used to
    /// hold both representations at once, which is how the mixture became
    /// visible enough to fix. Wire form unchanged (integer ms).
    #[serde(rename = "window_start_ms", with = "crate::epoch_ms")]
    pub window_start: Timestamp,
    /// Exclusive end of that window.
    #[serde(rename = "window_end_ms", with = "crate::epoch_ms")]
    pub window_end: Timestamp,
    /// The step-5 counts.
    pub totals: ReviewTotals,
    /// Per-Stream counts, ordered by stream id.
    #[serde(default)]
    pub streams: Vec<ReviewSnapshotStream>,
    /// Streaks as they stood at review time.
    #[serde(default)]
    pub streaks: Vec<StreakRow>,
    /// Optional free-text reflection the user typed.
    #[serde(default)]
    pub note: Option<String>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl ReviewSnapshot {
    /// Window start as epoch milliseconds.
    #[must_use]
    pub fn window_start_ms(&self) -> u64 {
        crate::epoch_ms::to_u64(self.window_start)
    }

    /// Window end as epoch milliseconds (exclusive).
    #[must_use]
    pub fn window_end_ms(&self) -> u64 {
        crate::epoch_ms::to_u64(self.window_end)
    }
}

/// Draft submitted to save a snapshot; the core fills the id and timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewSnapshotDraft {
    /// Window start (ms since epoch).
    pub window_start_ms: u64,
    /// Window end (ms since epoch, exclusive).
    pub window_end_ms: u64,
    /// Counts.
    pub totals: ReviewTotals,
    /// Per-Stream counts.
    pub streams: Vec<ReviewSnapshotStream>,
    /// Streaks.
    pub streaks: Vec<StreakRow>,
    /// Optional note.
    pub note: Option<String>,
}

impl WeeklyReview {
    /// Turn a completed review into the draft the user saves.
    ///
    /// The per-Stream counts are read straight off the assembled review, so a
    /// snapshot can never disagree with the screen it was saved from.
    #[must_use]
    pub fn to_draft(&self, note: Option<String>) -> ReviewSnapshotDraft {
        ReviewSnapshotDraft {
            window_start_ms: self.window.start_ms,
            window_end_ms: self.window.end_ms,
            totals: self.totals,
            streams: self
                .streams
                .iter()
                .map(|s| ReviewSnapshotStream {
                    stream: s.stream,
                    name: s.name.clone(),
                    completed: u32::try_from(s.completed.len()).unwrap_or(u32::MAX),
                    deferred: u32::try_from(s.deferred.len()).unwrap_or(u32::MAX),
                    created: u32::try_from(s.created_untouched.len()).unwrap_or(u32::MAX),
                })
                .collect(),
            streaks: self.streaks.clone(),
            note,
        }
    }
}

/// When a Task was committed to: the earlier of its deadline and its planned
/// slot. `None` means the Task carries no commitment at all.
fn commitment_at(t: &Task) -> Option<u64> {
    let ms = |v: &SunriseTime| u64::try_from(v.index_ms()).ok();
    match (t.due_at.as_ref(), t.scheduled_at.as_ref()) {
        (Some(due), Some(sched)) => ms(due).zip(ms(sched)).map(|(d, s)| d.min(s)),
        (due, sched) => due.and_then(ms).or_else(|| sched.and_then(ms)),
    }
}

/// jiff [`Timestamp`] → epoch milliseconds, dropping pre-epoch instants.
fn ts_to_ms(t: Timestamp) -> Option<u64> {
    u64::try_from(t.as_millisecond()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{fold_activity, OpPayload, OpRecord};
    use crate::focus::FocusStats;
    use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
    use crate::rrule::RRule;
    use crate::stats::{fold_trends, WeekGrid};
    use crate::stream::{StreamColor, StreamReviewCadence};
    use sunrise_id::EntityKind;

    /// Monday 2026-01-05T00:00:00Z.
    const MON: u64 = 1_767_571_200_000;
    const DAY: u64 = 24 * 60 * 60 * 1000;
    const WEEK: u64 = 7 * DAY;

    fn eref(kind: EntityKind, b: u8) -> EntityRef {
        EntityRef::new(kind, [b; 16])
    }

    fn ts(ms: u64) -> Timestamp {
        Timestamp::from_millisecond(i64::try_from(ms).unwrap()).unwrap()
    }

    fn task(id: u8, stream: EntityRef, title: &str, created_ms: u64) -> Task {
        Task {
            reminder_lead_s: None,
            id: eref(EntityKind::Task, id),
            created_at: ts(created_ms),
            updated_at: ts(created_ms),
            title: title.into(),
            body: None,
            stream_id: stream,
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

    fn stream(id: u8, name: &str) -> Stream {
        Stream {
            reminder_lead_s: None,
            id: eref(EntityKind::Stream, id),
            created_at: ts(MON),
            updated_at: ts(MON),
            name: name.into(),
            description: None,
            color: StreamColor::Slate,
            icon: None,
            parent_id: None,
            sort_order: "a0".into(),
            archived: false,
            paused: false,
            paused_until: None,
            review_cadence: StreamReviewCadence::Weekly,
            default_context: None,
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

    fn empty_focus() -> FocusStats {
        FocusStats {
            sessions: 0,
            work_sessions: 0,
            running: 0,
            total_focused_ms: 0,
            interruptions: 0,
            per_stream: Vec::new(),
            per_energy: Vec::new(),
            overall: None,
            top_interruptions: Vec::new(),
        }
    }

    fn window() -> ReviewWindow {
        ReviewWindow::new(MON, MON + WEEK)
    }

    /// A vault with one Stream, four tasks and a week of history:
    /// `t1` completed, `t2` deferred, `t3` created and untouched,
    /// `t4` created and then edited.
    struct Fixture {
        tasks: Vec<Task>,
        ops: Vec<OpRecord>,
    }

    fn fixture() -> Fixture {
        let s = eref(EntityKind::Stream, 3);
        let t1 = task(1, s, "finish the report", MON + DAY);
        let mut t1_done = t1.clone();
        t1_done.state = TaskState::Done;
        t1_done.completed_at = Some(ts(MON + 2 * DAY).into());

        let t2 = task(2, s, "call the bank", MON + DAY);
        let mut t2_def = t2.clone();
        t2_def.deferred_count = 1;
        t2_def.scheduled_at = Some(ts(MON + 5 * DAY).into());

        let t3 = task(3, s, "read the RFC", MON + 3 * DAY);

        let t4 = task(4, s, "book flights", MON + 3 * DAY);
        let mut t4_edited = t4.clone();
        t4_edited.priority = Some(1);

        let ops = vec![
            op(1, MON + DAY, OpPayload::TaskCreated(Box::new(t1.clone()))),
            op(2, MON + DAY, OpPayload::TaskCreated(Box::new(t2.clone()))),
            op(
                3,
                MON + 2 * DAY,
                OpPayload::TaskUpdated(Box::new(t1_done.clone())),
            ),
            op(
                4,
                MON + 2 * DAY,
                OpPayload::TaskUpdated(Box::new(t2_def.clone())),
            ),
            op(
                5,
                MON + 3 * DAY,
                OpPayload::TaskCreated(Box::new(t3.clone())),
            ),
            op(
                6,
                MON + 3 * DAY,
                OpPayload::TaskCreated(Box::new(t4.clone())),
            ),
            op(
                7,
                MON + 4 * DAY,
                OpPayload::TaskUpdated(Box::new(t4_edited.clone())),
            ),
        ];
        Fixture {
            tasks: vec![t1_done, t2_def, t3, t4_edited],
            ops,
        }
    }

    fn review_from(f: &Fixture, streams: Vec<ReviewStream>) -> WeeklyReview {
        let grid = WeekGrid::trailing(
            MON + 6 * DAY,
            3,
            &jiff::tz::TimeZone::UTC,
            crate::Weekday::Mo,
        )
        .unwrap();
        build_weekly_review(WeeklyReviewInput {
            window: window(),
            streams,
            tasks: f.tasks.clone(),
            activity: fold_activity(&f.ops),
            routines: Vec::new(),
            drift: Vec::new(),
            focus: empty_focus(),
            trends: fold_trends(&f.ops, &grid),
        })
    }

    #[test]
    fn step_one_splits_a_streams_week_into_completed_deferred_and_untouched() {
        let f = fixture();
        let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
        assert_eq!(r.streams.len(), 1);
        let s = &r.streams[0];
        assert_eq!(s.name, "Work");
        assert_eq!(
            s.completed
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["finish the report"]
        );
        assert_eq!(
            s.deferred
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["call the bank"]
        );
        assert_eq!(
            s.created_untouched
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["read the RFC"],
            "`book flights` was edited after creation, so it has been acted on"
        );
    }

    #[test]
    fn step_five_counts_match_the_activity_feed() {
        let f = fixture();
        let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
        assert_eq!(
            r.totals,
            ReviewTotals {
                completed: 1,
                deferred: 1,
                dropped: 0,
                created: 4,
                reopened: 0,
            }
        );
    }

    #[test]
    fn a_reopen_inside_the_window_removes_the_task_from_completed() {
        let mut f = fixture();
        let mut reopened = f.tasks[0].clone();
        reopened.state = TaskState::Todo;
        reopened.completed_at = None;
        f.ops.push(op(
            8,
            MON + 5 * DAY,
            OpPayload::TaskUpdated(Box::new(reopened.clone())),
        ));
        f.tasks[0] = reopened;

        let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
        assert!(
            r.streams[0].completed.is_empty(),
            "a task re-opened before the review is not a completion: {:#?}",
            r.streams[0].completed
        );
        assert_eq!(r.totals.completed, 1, "the event still happened…");
        assert_eq!(r.totals.reopened, 1, "…and so did the resurrection");
    }

    #[test]
    fn a_dropped_task_is_counted_and_never_listed_as_completed() {
        let mut f = fixture();
        let mut cancelled = f.tasks[2].clone();
        cancelled.state = TaskState::Cancelled;
        f.ops.push(op(
            8,
            MON + 5 * DAY,
            OpPayload::TaskUpdated(Box::new(cancelled.clone())),
        ));
        f.tasks[2] = cancelled;
        let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
        assert_eq!(r.totals.dropped, 1);
        assert!(r.streams[0].created_untouched.is_empty(), "it was acted on");
    }

    #[test]
    fn activity_outside_the_window_does_not_reach_the_review() {
        let f = fixture();
        let grid = WeekGrid::trailing(
            MON + 6 * DAY,
            3,
            &jiff::tz::TimeZone::UTC,
            crate::Weekday::Mo,
        )
        .unwrap();
        // Review the *previous* week; nothing in the fixture happened then.
        let r = build_weekly_review(WeeklyReviewInput {
            window: ReviewWindow::new(MON - WEEK, MON),
            streams: vec![ReviewStream::from(&stream(3, "Work"))],
            tasks: f.tasks.clone(),
            activity: fold_activity(&f.ops),
            routines: Vec::new(),
            drift: Vec::new(),
            focus: empty_focus(),
            trends: fold_trends(&f.ops, &grid),
        });
        assert_eq!(r.totals, ReviewTotals::default());
        assert!(r.streams[0].completed.is_empty());
    }

    #[test]
    fn archived_and_paused_streams_are_skipped() {
        let f = fixture();
        let mut archived = stream(3, "Work");
        archived.archived = true;
        assert!(review_from(&f, vec![ReviewStream::from(&archived)])
            .streams
            .is_empty());

        let mut paused = stream(3, "Work");
        paused.paused = true;
        assert!(review_from(&f, vec![ReviewStream::from(&paused)])
            .streams
            .is_empty());
    }

    #[test]
    fn step_two_lists_only_untriaged_inbox_items() {
        let inbox = inbox_stream_ref();
        let mut f = fixture();
        let capture = task(9, inbox, "idea from the shower", MON + DAY);
        let mut promoted = task(10, inbox, "already handled", MON + DAY);
        promoted.state = TaskState::Done;
        let mut archived = task(11, inbox, "filed away", MON + DAY);
        archived.archived = true;
        f.tasks.extend([capture, promoted, archived]);

        let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
        assert_eq!(
            r.inbox.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            vec!["idea from the shower"]
        );
    }

    #[test]
    fn step_four_lists_commitments_that_came_due_and_stayed_open() {
        let s = eref(EntityKind::Stream, 3);
        let mut due_and_open = task(20, s, "renew the domain", MON);
        due_and_open.due_at = Some(ts(MON + 2 * DAY).into());
        let mut due_and_done = task(21, s, "pay the invoice", MON);
        due_and_done.due_at = Some(ts(MON + 2 * DAY).into());
        due_and_done.state = TaskState::Done;
        let mut scheduled_open = task(22, s, "draft the memo", MON);
        scheduled_open.scheduled_at = Some(ts(MON + 3 * DAY).into());
        let mut future = task(23, s, "next month's thing", MON);
        future.due_at = Some(ts(MON + 40 * DAY).into());
        let unscheduled = task(24, s, "someday", MON);

        let f = Fixture {
            tasks: vec![
                due_and_open,
                due_and_done,
                scheduled_open,
                future,
                unscheduled,
            ],
            ops: Vec::new(),
        };
        let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
        let titles: Vec<&str> = r.slipped.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["renew the domain", "draft the memo"]);
    }

    #[test]
    fn step_three_reports_streaks_longest_first_and_only_drifting_routines() {
        fn routine(id: u8, title: &str, streak: i64) -> Routine {
            Routine {
                id: eref(EntityKind::Routine, id),
                created_at: ts(MON),
                updated_at: ts(MON),
                template: TaskTemplate {
                    title: title.into(),
                    stream_id: eref(EntityKind::Stream, 3),
                    contexts: Vec::new(),
                    energy: None,
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
                streak_counter: streak,
                last_completed_at: Some(ts(MON + DAY)),
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
        fn drift(id: u8, drift: f64, over: bool) -> RoutineDrift {
            RoutineDrift {
                routine: eref(EntityKind::Routine, id),
                title: "r".into(),
                expected: 10,
                completed: 5,
                skipped: 0,
                missed: 5,
                drift,
                over_threshold: over,
                streak: 0,
                last_completed_at_ms: None,
                paused: false,
            }
        }

        let f = fixture();
        let grid = WeekGrid::trailing(
            MON + 6 * DAY,
            3,
            &jiff::tz::TimeZone::UTC,
            crate::Weekday::Mo,
        )
        .unwrap();
        let mut deleted = routine(3, "gone", 99);
        deleted.deleted = true;
        let r = build_weekly_review(WeeklyReviewInput {
            window: window(),
            streams: vec![ReviewStream::from(&stream(3, "Work"))],
            tasks: f.tasks.clone(),
            activity: fold_activity(&f.ops),
            routines: vec![routine(1, "stretch", 4), routine(2, "journal", 12), deleted],
            drift: vec![
                drift(1, 0.5, true),
                drift(2, 0.1, false),
                drift(4, 0.9, true),
            ],
            focus: empty_focus(),
            trends: fold_trends(&f.ops, &grid),
        });

        assert_eq!(
            r.streaks
                .iter()
                .map(|s| (s.title.as_str(), s.streak))
                .collect::<Vec<_>>(),
            vec![("journal", 12), ("stretch", 4)],
            "longest first, deleted routines excluded"
        );
        assert_eq!(
            r.drifting_routines
                .iter()
                .map(|d| d.routine)
                .collect::<Vec<_>>(),
            vec![eref(EntityKind::Routine, 4), eref(EntityKind::Routine, 1)],
            "worst drift first, sub-threshold routines omitted"
        );
    }

    #[test]
    fn a_snapshot_draft_agrees_with_the_review_it_was_saved_from() {
        let f = fixture();
        let r = review_from(&f, vec![ReviewStream::from(&stream(3, "Work"))]);
        let draft = r.to_draft(Some("felt scattered".into()));
        assert_eq!(draft.window_start_ms, MON);
        assert_eq!(draft.window_end_ms, MON + WEEK);
        assert_eq!(draft.totals, r.totals);
        assert_eq!(draft.streams.len(), 1);
        assert_eq!(draft.streams[0].completed, 1);
        assert_eq!(draft.streams[0].deferred, 1);
        assert_eq!(draft.note.as_deref(), Some("felt scattered"));
    }

    #[test]
    fn daily_review_shows_recent_captures_and_the_blocked_subset_of_today() {
        let inbox = inbox_stream_ref();
        let s = eref(EntityKind::Stream, 3);
        let last_night = task(30, inbox, "captured at 22:04", MON + 20 * 3_600_000);
        let old = task(31, inbox, "captured last week", MON - 3 * DAY);
        let ready = task(32, s, "write it up", MON);
        let waiting = task(33, s, "send for review", MON);

        let daily = build_daily_review(
            ReviewWindow::new(MON + 18 * 3_600_000, MON + 30 * 3_600_000),
            vec![last_night, old],
            vec![ready, waiting.clone()],
            &BTreeSet::from([waiting.id]),
        );
        assert_eq!(
            daily
                .inbox
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["captured at 22:04"],
            "only captures inside the glance window"
        );
        assert_eq!(daily.today.len(), 2);
        assert_eq!(
            daily
                .blocked
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["send for review"]
        );
    }

    #[test]
    fn review_window_normalizes_an_inverted_range() {
        let w = ReviewWindow::new(MON + WEEK, MON);
        assert_eq!(w.start_ms, MON + WEEK);
        assert_eq!(w.end_ms, MON + WEEK);
        assert!(!w.contains(MON + WEEK), "an empty window contains nothing");
    }
}
