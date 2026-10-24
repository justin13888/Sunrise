//! The Upcoming view: the next 7, 14 or 30 days, day by day, and a no-date
//! lane per Stream (`docs/08-features/planning-views.md` §Upcoming, #351).
//!
//! Every decision the view makes is here, so no client re-derives one:
//!
//! - **Days are planner days in the reader's zone**, never multiples of
//!   86,400,000 ms (`docs/10-cross-cutting/time.md` §4). Each day carries its
//!   own `[start, end)`, which is 23 or 25 hours across a DST change. With no
//!   day schedule a day starts at civil midnight; with one, at the boundary
//!   `docs/02-domain/day-schedule.md` §Planner day defines.
//! - **A task belongs to the planner day its time falls on**
//!   ([`crate::planner_day_of`]), and keeps its kind: a floating 09:00 is on the
//!   same wall-clock day wherever the reader is, a zoned one is on whichever
//!   day its instant falls in the reader's zone, and an all-day value is on
//!   the date it names (`docs/11-adr/0066-times-survive-travel.md`).
//! - **Ordering is total**, so the same vault gives the same view: every list
//!   ends its comparison on the entity id.
//!
//! The fold is pure. [`build_upcoming`] takes the rows the engine read and the
//! bounds it resolved, and is generic over the block row so the engine can
//! hand in its titled rows while a test hands in bare [`Block`]s.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::Serialize;
use sunrise_id::EntityRef;

use crate::block::Block;
use crate::day_schedule::{day_bounds, planner_day_of, DaySchedule};
use crate::task::{Task, TaskState};
use crate::time::SunriseTime;

/// The spans the view offers, as the chip group lists them. The device-local
/// preference `views.upcoming.span_days` takes the same three values.
pub const UPCOMING_SPANS: [u32; 3] = [7, 14, 30];

/// The span shown when the reader has not chosen one.
pub const DEFAULT_UPCOMING_SPAN_DAYS: u32 = 7;

/// At most this many tasks are listed per Stream in the no-date lane; the
/// rest are counted in [`NoDateGroup::total`].
pub const NO_DATE_LANE_CAP: usize = 5;

/// Why the view cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpcomingError {
    /// The span is not one of [`UPCOMING_SPANS`].
    #[error("an Upcoming span is 7, 14 or 30 days, not {0}")]
    Span(u32),
    /// A day's start has no instant in the reader's zone, or the span runs
    /// off the calendar.
    #[error("the day {0} has no start in the reader's zone")]
    Calendar(Date),
}

/// One planner day of the view.
#[derive(Debug, Clone, Serialize)]
pub struct UpcomingDay<B> {
    /// The planner day's date, in the reader's zone.
    pub date: Date,
    /// The instant the day starts: civil midnight in the reader's zone with
    /// no day schedule, its planner-day boundary with one.
    pub start: Timestamp,
    /// The instant the next day starts. `end - start` is 23 or 25 hours on a
    /// DST day.
    pub end: Timestamp,
    /// Blocks overlapping `[start, end)`, by resolved start, then id. A block
    /// that runs past midnight is on both days, as on the calendar grid.
    pub blocks: Vec<B>,
    /// Tasks whose `scheduled_at` falls on this day: timed ones in time
    /// order, then all-day ones by Stream, each list then by id.
    pub planned: Vec<Task>,
    /// Tasks whose `due_at` falls on this day and that are not planned for
    /// it, by deadline, then priority, then id. #334 splits this list by
    /// which deadline it is (`target_at`, `hard_due_at`); today a task has
    /// one deadline.
    pub due: Vec<Task>,
}

/// One Stream's group in the no-date lane.
#[derive(Debug, Clone, Serialize)]
pub struct NoDateGroup {
    /// The Stream. The Inbox when `no_stream` is set.
    pub stream: EntityRef,
    /// Whether this is the "No stream" group
    /// (`docs/11-adr/0046-optional-stream.md`): the tasks still in the Inbox.
    pub no_stream: bool,
    /// The first [`NO_DATE_LANE_CAP`] tasks: by deadline (none last), then
    /// priority (none last), then id.
    pub tasks: Vec<Task>,
    /// How many tasks the group holds in all; `total - tasks.len()` is the
    /// "N more".
    pub total: u32,
}

/// The whole view.
#[derive(Debug, Clone, Serialize)]
pub struct Upcoming<B> {
    /// The reader's planner today: the first day.
    pub today: Date,
    /// How many days the view spans: one of [`UPCOMING_SPANS`].
    pub span_days: u32,
    /// One entry per planner day from today, `span_days` of them.
    pub days: Vec<UpcomingDay<B>>,
    /// Open tasks with no `scheduled_at`, grouped by Stream in the stream
    /// list's order, the "No stream" group last.
    pub no_date: Vec<NoDateGroup>,
}

/// What [`build_upcoming`] folds.
#[derive(Debug)]
pub struct UpcomingInput<'a, B> {
    /// Live tasks. Closed, archived and deleted ones are skipped here, so the
    /// caller may over-read.
    pub tasks: &'a [Task],
    /// Blocks that may overlap the span. Deleted ones are skipped.
    pub blocks: &'a [B],
    /// The [`Block`] inside a block row.
    pub block_of: fn(&B) -> &Block,
    /// Stream ids in the stream list's order; the no-date lane's groups and a
    /// day's all-day tasks follow it. A Stream not in it sorts after every
    /// one that is, by id.
    pub stream_order: &'a [EntityRef],
    /// The Stream that means "no stream": the Inbox.
    pub no_stream: EntityRef,
    /// Keep only tasks carrying at least one of these; empty keeps all. Blocks
    /// carry no contexts and are never filtered.
    pub contexts: &'a [EntityRef],
    /// The reader's today: the planner day holding "now".
    pub today: Date,
    /// One of [`UPCOMING_SPANS`].
    pub span_days: u32,
    /// The reader's zone.
    pub tz: &'a TimeZone,
    /// The day schedule whose planner days the view is laid out in. Empty
    /// lays it out in civil days.
    pub schedule: &'a DaySchedule,
}

// By hand rather than derived: a derive would require `B: Copy`, and the
// input only borrows its rows.
impl<B> Clone for UpcomingInput<'_, B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<B> Copy for UpcomingInput<'_, B> {}

/// Whether `span_days` is one the view offers.
#[must_use]
pub fn is_upcoming_span(span_days: u32) -> bool {
    UPCOMING_SPANS.contains(&span_days)
}

/// The civil days `today ..= today + span_days - 1` with their bounds in
/// `tz`, each day's end being the next day's start.
///
/// # Errors
///
/// [`UpcomingError::Span`] for a span the view does not offer, and
/// [`UpcomingError::Calendar`] when a day falls off the calendar.
pub fn upcoming_days(
    today: Date,
    span_days: u32,
    tz: &TimeZone,
) -> Result<Vec<(Date, Timestamp, Timestamp)>, UpcomingError> {
    upcoming_planner_days(today, span_days, tz, &DaySchedule::default())
}

/// The planner days `today ..= today + span_days - 1` of `schedule` with
/// their bounds in `tz` (`docs/02-domain/day-schedule.md` §Planner day), each
/// day's end being the next day's start. With an empty schedule these are
/// [`upcoming_days`]' civil days.
///
/// # Errors
///
/// As [`upcoming_days`].
pub fn upcoming_planner_days(
    today: Date,
    span_days: u32,
    tz: &TimeZone,
    schedule: &DaySchedule,
) -> Result<Vec<(Date, Timestamp, Timestamp)>, UpcomingError> {
    if !is_upcoming_span(span_days) {
        return Err(UpcomingError::Span(span_days));
    }
    let mut out = Vec::with_capacity(span_days as usize);
    let mut date = today;
    for _ in 0..span_days {
        let (start, end) =
            day_bounds(schedule, date, tz).map_err(|_| UpcomingError::Calendar(date))?;
        out.push((date, start, end));
        date = date.tomorrow().map_err(|_| UpcomingError::Calendar(date))?;
    }
    Ok(out)
}

/// Build the view.
///
/// # Errors
///
/// As [`upcoming_days`].
pub fn build_upcoming<B: Clone>(input: UpcomingInput<'_, B>) -> Result<Upcoming<B>, UpcomingError> {
    let tz = input.tz;
    let bounds = upcoming_planner_days(input.today, input.span_days, tz, input.schedule)?;
    let rank = StreamRank::new(input.stream_order);
    let open: Vec<&Task> = input
        .tasks
        .iter()
        .filter(|t| is_open(t) && carries_any(t, input.contexts))
        .collect();

    let mut days: Vec<UpcomingDay<B>> = bounds
        .iter()
        .map(|&(date, start, end)| UpcomingDay {
            date,
            start,
            end,
            blocks: Vec::new(),
            planned: Vec::new(),
            due: Vec::new(),
        })
        .collect();
    let index: BTreeMap<Date, usize> = bounds
        .iter()
        .enumerate()
        .map(|(i, &(d, _, _))| (d, i))
        .collect();

    let mut no_date: BTreeMap<EntityRef, Vec<Task>> = BTreeMap::new();
    for t in open {
        let day_of = |v: &SunriseTime| planner_day_of(input.schedule, v, tz);
        let planned_day = t.scheduled_at.as_ref().and_then(day_of);
        if let Some(i) = planned_day.and_then(|d| index.get(&d)) {
            days[*i].planned.push(t.clone());
        }
        let due_day = t.due_at.as_ref().and_then(day_of);
        if let Some(d) = due_day.filter(|d| Some(*d) != planned_day) {
            if let Some(i) = index.get(&d) {
                days[*i].due.push(t.clone());
            }
        }
        if t.scheduled_at.is_none() {
            no_date.entry(t.stream_id).or_default().push(t.clone());
        }
    }

    for day in &mut days {
        day.planned.sort_by(|a, b| planned_order(a, b, tz, &rank));
        day.due.sort_by(|a, b| deadline_order(a, b, tz));
        day.blocks = blocks_on(input.blocks, input.block_of, day.start, day.end, tz);
    }

    let mut groups: Vec<NoDateGroup> = no_date
        .into_iter()
        .map(|(stream, mut tasks)| {
            tasks.sort_by(|a, b| deadline_order(a, b, tz));
            let total = u32::try_from(tasks.len()).unwrap_or(u32::MAX);
            tasks.truncate(NO_DATE_LANE_CAP);
            NoDateGroup {
                stream,
                no_stream: stream == input.no_stream,
                tasks,
                total,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        a.no_stream
            .cmp(&b.no_stream)
            .then_with(|| rank.cmp(a.stream, b.stream))
    });

    Ok(Upcoming {
        today: input.today,
        span_days: input.span_days,
        days,
        no_date: groups,
    })
}

/// Live, not archived, and not finished. An unknown state reads as open, as
/// it does on Today.
fn is_open(t: &Task) -> bool {
    !t.deleted
        && !t.archived
        && matches!(t.state.effective(), TaskState::Todo | TaskState::InProgress)
}

/// The OR-set context filter of `docs/02-domain/contexts-and-tags.md`.
fn carries_any(t: &Task, contexts: &[EntityRef]) -> bool {
    contexts.is_empty() || contexts.iter().any(|c| t.contexts.contains(c))
}

/// Timed plans in the reader's time order, then all-day ones by Stream, then
/// id.
fn planned_order(a: &Task, b: &Task, tz: &TimeZone, rank: &StreamRank) -> Ordering {
    let all_day = |t: &Task| t.scheduled_at.as_ref().is_some_and(SunriseTime::is_all_day);
    all_day(a)
        .cmp(&all_day(b))
        .then_with(|| {
            if all_day(a) {
                rank.cmp(a.stream_id, b.stream_id)
            } else {
                cmp_time(a.scheduled_at.as_ref(), b.scheduled_at.as_ref(), tz)
            }
        })
        .then_with(|| a.id.cmp(&b.id))
}

/// By deadline (none last), then priority (none last), then id: the order
/// `docs/08-features/planning-views.md` gives the no-date lane, and the one a
/// day's deadlines use.
fn deadline_order(a: &Task, b: &Task, tz: &TimeZone) -> Ordering {
    cmp_time(a.due_at.as_ref(), b.due_at.as_ref(), tz)
        .then_with(|| {
            a.priority
                .unwrap_or(u8::MAX)
                .cmp(&b.priority.unwrap_or(u8::MAX))
        })
        .then_with(|| a.id.cmp(&b.id))
}

/// [`SunriseTime::cmp_in`], with an absent value last.
fn cmp_time(a: Option<&SunriseTime>, b: Option<&SunriseTime>, tz: &TimeZone) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp_in(y, tz),
        (x, y) => x.is_none().cmp(&y.is_none()),
    }
}

/// The live blocks overlapping `[start, end)` once both bounds are resolved
/// in `tz`, by resolved start, then id. A bound this build cannot place keeps
/// the block off every day.
fn blocks_on<B: Clone>(
    blocks: &[B],
    block_of: fn(&B) -> &Block,
    start: Timestamp,
    end: Timestamp,
    tz: &TimeZone,
) -> Vec<B> {
    let mut out: Vec<B> = blocks
        .iter()
        .filter(|row| {
            let b = block_of(row);
            !b.deleted
                && matches!(
                    (b.starts_at.resolve_in(tz), b.ends_at.resolve_in(tz)),
                    (Some(s), Some(e)) if s < end && e > start
                )
        })
        .cloned()
        .collect();
    out.sort_by(|x, y| {
        let (x, y) = (block_of(x), block_of(y));
        x.starts_at
            .cmp_in(&y.starts_at, tz)
            .then_with(|| x.id.cmp(&y.id))
    });
    out
}

/// A Stream's position in the stream list, for a total order over Streams.
struct StreamRank(BTreeMap<EntityRef, usize>);

impl StreamRank {
    fn new(order: &[EntityRef]) -> Self {
        let mut m = BTreeMap::new();
        for (i, s) in order.iter().enumerate() {
            m.entry(*s).or_insert(i);
        }
        Self(m)
    }

    /// Listed Streams in list order, then unlisted ones by id.
    fn cmp(&self, a: EntityRef, b: EntityRef) -> Ordering {
        let pos = |s: EntityRef| self.0.get(&s).copied().unwrap_or(usize::MAX);
        pos(a).cmp(&pos(b)).then_with(|| a.cmp(&b))
    }
}

#[cfg(test)]
mod tests;
