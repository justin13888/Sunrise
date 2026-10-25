//! The triage queue (ADR-0047 §3, issue #335): which open tasks need a
//! decision, in what order, and what each of the four actions may write.
//!
//! Everything here is a pure function of the task, its lateness for the
//! reader and what the engine knows about its stream. The queue is derived
//! on every read and never stored: Keep is the only thing it remembers, as
//! one register on the task (`late_acknowledged_at`), and a task returns by
//! itself at its next lateness threshold because a new state always begins
//! later than the acknowledgement it replaced.

use std::cmp::Ordering;

use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::day_schedule::{planner_date, DaySchedule};
use crate::lateness::LatenessState;
use crate::task::Task;
use crate::time::SunriseTime;
use crate::validation::ValidationError;

/// The longest drop reason, in characters after trimming (`text<512>`).
pub const MAX_CANCEL_REASON_LEN: usize = 512;

/// Whether `task` is in the triage queue for a reader whose lateness for it
/// is `lateness` (ADR-0047 §3):
///
/// ```text
/// in_triage(task) = open(task)
///               AND lateness(task).state ∈ {LateSoft, LateHard, Stale}
///               AND NOT (task.late_acknowledged_at ≥ lateness(task).since)
///               AND NOT stream_paused
/// ```
///
/// `stream_paused` is whether the task's effective stream is paused now,
/// which only the engine can read. A lapsed routine occurrence, the record's
/// fifth clause, is not modelled in the tree yet (#331's catch-up).
#[must_use]
pub fn in_triage(task: &Task, lateness: &LatenessState, stream_paused: bool) -> bool {
    if !task.is_open() || !lateness.state.needs_decision() || stream_paused {
        return false;
    }
    !matches!(
        (task.late_acknowledged_at, lateness.since),
        (Some(ack), Some(since)) if ack >= since
    )
}

/// The queue's order: `LateHard`, then `LateSoft`, then `Stale`; within a
/// state the oldest `since` first; then id. Total, and the same on every
/// replica holding the same data in the same zone.
#[must_use]
pub fn triage_order(a: (&Task, &LatenessState), b: (&Task, &LatenessState)) -> Ordering {
    a.1.state
        .cmp(&b.1.state)
        .then_with(|| a.1.since.cmp(&b.1.since))
        .then_with(|| a.0.id.cmp(&b.0.id))
}

/// Why an "already done" completion time is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CompletionError {
    /// Neither an instant nor an all-day date: a completion is a recorded
    /// fact, a moment or a day (`time.md` §1).
    #[error("a completion time is an instant or a date")]
    Kind,
    /// After now, for the reader.
    #[error("a task cannot be done in the future")]
    Future,
    /// Before the task existed.
    #[error("a task cannot be done before it was created")]
    BeforeCreated,
}

/// Check a backdated completion for `task` at `now`, for a reader in `tz`
/// on `schedule`'s planner days (ADR-0047 §4): an `instant` not after `now`
/// and not before `created_at`; an `all_day` date not after the reader's
/// planner today and not before the planner day the task was created on.
///
/// # Errors
/// [`CompletionError`] naming the rule broken.
pub fn check_completion(
    task: &Task,
    completed_at: &SunriseTime,
    now: Timestamp,
    tz: &TimeZone,
    schedule: &DaySchedule,
) -> Result<(), CompletionError> {
    match completed_at {
        SunriseTime::Instant { at } => {
            if *at > now {
                return Err(CompletionError::Future);
            }
            if *at < task.created_at {
                return Err(CompletionError::BeforeCreated);
            }
            Ok(())
        }
        SunriseTime::AllDay { date } => {
            let today = planner_date(schedule, now, tz).map_err(|_| CompletionError::Future)?;
            if *date > today {
                return Err(CompletionError::Future);
            }
            let created = planner_date(schedule, task.created_at, tz)
                .map_err(|_| CompletionError::BeforeCreated)?;
            if *date < created {
                return Err(CompletionError::BeforeCreated);
            }
            Ok(())
        }
        _ => Err(CompletionError::Kind),
    }
}

/// A drop reason as stored: trimmed, `None` when blank.
///
/// # Errors
/// [`ValidationError::Field`] for one longer than
/// [`MAX_CANCEL_REASON_LEN`] characters.
pub fn cancel_reason(reason: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(r) = reason.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if r.chars().count() > MAX_CANCEL_REASON_LEN {
        return Err(ValidationError::Field {
            field: "task.cancel_reason",
            constraint: "max_512",
        });
    }
    Ok(Some(r.to_owned()))
}

/// The four triage actions, each applied to every task of a selection
/// (ADR-0047 §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TriageAction {
    /// Move `planned_at` and count the deferral. Never a deadline.
    Defer {
        /// Where to.
        to: DeferTarget,
    },
    /// `done`, at a given past moment or day rather than now.
    AlreadyDone {
        /// When the work was done: an instant or an all-day date.
        completed_at: SunriseTime,
    },
    /// `cancelled`, with an optional reason.
    Drop {
        /// Why, if the user said; `text<512>`.
        reason: Option<String>,
    },
    /// Acknowledge: leave the queue until the next lateness transition.
    Keep,
}

/// Where a triage defer moves the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferTarget {
    /// This time, of the kind given.
    At(SunriseTime),
    /// The next free slot, resolved by the core before any op is written.
    /// Until the planner (#343) lands it is the next planner day, as an
    /// all-day value of its date, which starts at that day's start.
    NextFreeSlot,
}

#[cfg(test)]
mod tests;
