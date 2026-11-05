//! Lateness: whether an open task has missed a deadline or gone untouched,
//! derived at read time (ADR-0047 §2, `docs/02-domain/tasks.md` §Lateness and
//! triage, issue #334).
//!
//! Lateness is **never stored and never synced**, and no client computes it:
//! the core reads it from here for every view that shows it (Today's Late
//! section, search's `is:late` and `is:stale`, saved views, the triage queue).
//! Two devices in different zones may legitimately disagree about whether an
//! all-day deadline has passed, because they are in different days, and that
//! is the right answer for each reader. It is also why the value cannot be a
//! field.
//!
//! Everything is a pure function of the task, `now`, the reader's zone and
//! the day schedule. Day boundaries are planner days in the reader's zone
//! (`docs/10-cross-cutting/time.md` §4), and staleness counts **civil days**
//! between two planner dates, never multiples of 86,400 seconds, so a DST day
//! is one day whether it lasts 23 hours or 25.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use serde::{Deserialize, Serialize};

use crate::day_schedule::{day_bounds, deadline_at, planner_date, DaySchedule};
use crate::task::Task;

/// The default of the `stale_after_days` preference (`preferences.md`).
pub const DEFAULT_STALE_AFTER_DAYS: u16 = 14;

/// Where a task stands against its deadlines, for one reader.
///
/// The declared order is the triage queue's (`LateHard` first), and is what
/// [`Ord`] compares, so a client that sorts by it gets the core's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lateness {
    /// The hard deadline (`hard_due_at`) has been missed.
    LateHard,
    /// The soft deadline (`target_at`) has been missed, and the hard one, if
    /// any, has not.
    LateSoft,
    /// No deadline at all, and untouched for `stale_after_days` planner days.
    Stale,
    /// None of the above, or not an open task.
    OnTrack,
}

impl Lateness {
    /// Every state, in the declared order.
    pub const ALL: [Self; 4] = [Self::LateHard, Self::LateSoft, Self::Stale, Self::OnTrack];

    /// The stable spelling: `late_hard`, `late_soft`, `stale`, `on_track`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LateHard => "late_hard",
            Self::LateSoft => "late_soft",
            Self::Stale => "stale",
            Self::OnTrack => "on_track",
        }
    }

    /// The state a spelling names, as [`Self::as_str`] writes it.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.as_str() == s)
    }

    /// Late, soft or hard.
    #[must_use]
    pub const fn is_late(self) -> bool {
        matches!(self, Self::LateHard | Self::LateSoft)
    }

    /// One of the states the triage queue collects (ADR-0047 §3).
    #[must_use]
    pub const fn needs_decision(self) -> bool {
        !matches!(self, Self::OnTrack)
    }
}

/// A task's [`Lateness`] and the instant it began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LatenessState {
    /// The state.
    pub state: Lateness,
    /// When the state began: the missed deadline's instant for `LateHard`
    /// and `LateSoft`, the start of the planner day the task turned stale on
    /// for `Stale`, and `None` for `OnTrack`.
    ///
    /// Keep in the triage queue compares an acknowledgement with it, so a new
    /// state, which always has a later `since`, brings the task back by
    /// itself (ADR-0047 §3).
    pub since: Option<Timestamp>,
}

impl LatenessState {
    /// On track, since nothing.
    pub const ON_TRACK: Self = Self {
        state: Lateness::OnTrack,
        since: None,
    };
}

/// The lateness of `task` for a reader in `tz` at `now`, whose days are
/// `schedule`'s planner days (ADR-0047 §2).
///
/// Evaluated for open tasks only ([`Task::is_open`]: `todo` or
/// `in_progress`, not archived, not deleted); every other task is
/// [`Lateness::OnTrack`]. First match wins:
///
/// | State | Condition | `since` |
/// |---|---|---|
/// | `LateHard` | `hard_due_at` is set and `now ≥ due(hard_due_at)` | `due(hard_due_at)` |
/// | `LateSoft` | `target_at` is set and `now ≥ due(target_at)` | `due(target_at)` |
/// | `Stale` | neither deadline is set, `stale_after_days > 0`, and the planner date of `now` is at least `stale_after_days` after the planner date of `updated_at` | start of that planner day |
/// | `OnTrack` | otherwise | — |
///
/// `due()` is [`deadline_at`]: an all-day deadline is missed at the END of
/// its planner day. A deadline whose kind this build cannot place is set (so
/// the task is never stale) and is never missed (it is compared with
/// nothing, `docs/11-adr/0066-times-survive-travel.md` §3). A slipped
/// `planned_at` is not lateness: a plan is intent, not a promise.
///
/// `updated_at` is the task's touch: the latest stamp of any field op
/// applied to it, so any edit, a Keep included, resets the stale clock.
#[must_use]
pub fn lateness(
    task: &Task,
    now: Timestamp,
    tz: &TimeZone,
    schedule: &DaySchedule,
    stale_after_days: u16,
) -> LatenessState {
    if !task.is_open() {
        return LatenessState::ON_TRACK;
    }
    let missed = |value: Option<&crate::SunriseTime>| {
        value
            .and_then(|v| deadline_at(schedule, v, tz))
            .filter(|due| now >= *due)
    };
    if let Some(since) = missed(task.hard_due_at.as_ref()) {
        return LatenessState {
            state: Lateness::LateHard,
            since: Some(since),
        };
    }
    if let Some(since) = missed(task.target_at.as_ref()) {
        return LatenessState {
            state: Lateness::LateSoft,
            since: Some(since),
        };
    }
    if task.has_deadline() || stale_after_days == 0 {
        return LatenessState::ON_TRACK;
    }
    match stale_since(task.updated_at, now, tz, schedule, stale_after_days) {
        Some(since) => LatenessState {
            state: Lateness::Stale,
            since: Some(since),
        },
        None => LatenessState::ON_TRACK,
    }
}

/// The start of the planner day a task touched at `touched` turns stale on,
/// when that day has begun by `now`; `None` otherwise, or when either instant
/// or that day is at the edge of the calendar (which is no evidence of
/// staleness).
fn stale_since(
    touched: Timestamp,
    now: Timestamp,
    tz: &TimeZone,
    schedule: &DaySchedule,
    stale_after_days: u16,
) -> Option<Timestamp> {
    let touched_day = planner_date(schedule, touched, tz).ok()?;
    let today = planner_date(schedule, now, tz).ok()?;
    let stale_day = add_days(touched_day, stale_after_days)?;
    if today < stale_day {
        return None;
    }
    day_bounds(schedule, stale_day, tz)
        .ok()
        .map(|(start, _)| start)
}

/// `date` plus `days` civil days, or `None` off the calendar.
fn add_days(date: Date, days: u16) -> Option<Date> {
    date.checked_add(Span::new().days(i64::from(days))).ok()
}

#[cfg(test)]
mod tests;
