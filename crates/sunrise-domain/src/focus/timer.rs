//! Timer policy: how long the next segment runs, when a break is due, and how
//! a task whose estimate exceeds one sitting is chunked.
//!
//! Pure integer arithmetic over the four constants below, and the one part of
//! [`crate::focus`] that is only that: nothing here reads an
//! [`EntityRef`](sunrise_id::EntityRef), an instant, or an
//! [`Unknowns`](crate::unknown::Unknowns) map. Sizing a session and deciding
//! which segment follows it are the same decision made at two moments, so the
//! constants that answer both live beside them rather than at the top of a
//! file that also holds entities.

use super::session::{Chunk, FocusKind};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Timer policy constants (`docs/08-features/focus-mode.md` §Pomodoro)
// ---------------------------------------------------------------------------

/// Default work segment: 25 minutes.
pub const POMODORO_MS: u64 = 25 * 60 * 1000;

/// Default short break: 5 minutes.
pub const SHORT_BREAK_MS: u64 = 5 * 60 * 1000;

/// Default long break, taken after [`CYCLES_BEFORE_LONG_BREAK`] work
/// segments: 15 minutes.
pub const LONG_BREAK_MS: u64 = 15 * 60 * 1000;

/// Work segments between long breaks.
pub const CYCLES_BEFORE_LONG_BREAK: u32 = 4;

/// How long *this* session runs — the per-session choice from
/// `docs/08-features/focus-mode.md` §Adaptive session length.
///
/// `timeboxed to my next Block` is deliberately absent: Block has no command
/// path in v1, so there is no next Block to box against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLength {
    /// One pomodoro (the 25-minute default).
    OnePomodoro,
    /// Sized to the task's `estimated_duration_s`, capped at one pomodoro —
    /// past that the work chunks (see [`plan_session`]).
    SizedToEstimate,
    /// Open-ended: no planned length, the user stops when the task is done.
    UntilDone,
}

// ---------------------------------------------------------------------------
// Adaptive session length + chunking
// ---------------------------------------------------------------------------

/// The next segment the timer should run (`docs/08-features/focus-mode.md`
/// §Pomodoro and timer policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    /// A work segment.
    Work,
    /// The 5-minute break between pomodoros.
    ShortBreak,
    /// The longer break after [`CYCLES_BEFORE_LONG_BREAK`] work segments.
    LongBreak,
}

impl Segment {
    /// Default length of this segment in ms.
    #[must_use]
    pub const fn default_ms(self) -> u64 {
        match self {
            Self::Work => POMODORO_MS,
            Self::ShortBreak => SHORT_BREAK_MS,
            Self::LongBreak => LONG_BREAK_MS,
        }
    }

    /// The [`FocusKind`] a session of this segment records as.
    #[must_use]
    pub const fn kind(self) -> FocusKind {
        match self {
            Self::Work => FocusKind::Work,
            Self::ShortBreak | Self::LongBreak => FocusKind::Break,
        }
    }
}

/// Which segment follows `work_sessions_done` completed work segments.
///
/// `0` → the first work segment. After a work segment the answer is a break,
/// long every [`CYCLES_BEFORE_LONG_BREAK`]. Callers alternate by counting only
/// work segments, so a skipped break never desynchronizes the cycle.
#[must_use]
pub const fn break_after(work_sessions_done: u32) -> Segment {
    if work_sessions_done == 0 {
        return Segment::Work;
    }
    if work_sessions_done.is_multiple_of(CYCLES_BEFORE_LONG_BREAK) {
        Segment::LongBreak
    } else {
        Segment::ShortBreak
    }
}

/// How many sittings of `session_ms` an estimate of `estimated_ms` implies.
///
/// `None` when the estimate fits inside one session (no chunk marker is shown)
/// or when there is no usable estimate.
#[must_use]
pub fn chunk_count(estimated_ms: u64, session_ms: u64) -> Option<u32> {
    if session_ms == 0 || estimated_ms == 0 || estimated_ms <= session_ms {
        return None;
    }
    let n = estimated_ms.div_ceil(session_ms);
    Some(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The sizing decision for one session: how long to run, and whether to show a
/// "chunk N of M" checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlan {
    /// Planned length, `None` for `until done`.
    pub planned_ms: Option<u64>,
    /// Chunk marker when the estimate exceeds one session.
    pub chunk: Option<Chunk>,
}

/// Size a session from the chosen [`SessionLength`], the task's estimate, and
/// how many work sessions the task has already had.
///
/// * [`SessionLength::OnePomodoro`] — always `pomodoro_ms`.
/// * [`SessionLength::SizedToEstimate`] — the whole estimate when it fits in
///   one pomodoro; otherwise one pomodoro, chunked.
/// * [`SessionLength::UntilDone`] — no planned length at all.
///
/// Chunking is orthogonal to the length choice: whenever the estimate exceeds
/// the session about to run, the caller gets `chunk N of M` with `N` derived
/// from `work_sessions_done` (clamped to `M`, so a task that overruns its
/// estimate reads "chunk 4 of 4" rather than "5 of 4").
#[must_use]
pub fn plan_session(
    length: SessionLength,
    estimated_duration_s: Option<u64>,
    work_sessions_done: u32,
    pomodoro_ms: u64,
) -> SessionPlan {
    let estimated_ms = estimated_duration_s.unwrap_or(0).saturating_mul(1000);
    let planned_ms = match length {
        SessionLength::UntilDone => None,
        SessionLength::OnePomodoro => Some(pomodoro_ms),
        SessionLength::SizedToEstimate => {
            if estimated_ms == 0 {
                // No estimate to size against: fall back to the default.
                Some(pomodoro_ms)
            } else {
                Some(estimated_ms.min(pomodoro_ms))
            }
        }
    };
    // An open-ended session has no sitting to be "N of", so it carries no
    // chunk marker even for a large estimate.
    let chunk = match planned_ms {
        None => None,
        Some(session_ms) => chunk_count(estimated_ms, session_ms).map(|total| Chunk {
            index: work_sessions_done.saturating_add(1).min(total),
            total,
        }),
    };
    SessionPlan { planned_ms, chunk }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- adaptive session length + chunking ----

    #[test]
    fn one_pomodoro_is_always_a_pomodoro() {
        let p = plan_session(SessionLength::OnePomodoro, Some(300), 0, POMODORO_MS);
        assert_eq!(p.planned_ms, Some(POMODORO_MS));
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn sized_to_estimate_shrinks_to_a_short_task() {
        // 10 minutes of work does not get a 25-minute timer.
        let p = plan_session(SessionLength::SizedToEstimate, Some(600), 0, POMODORO_MS);
        assert_eq!(p.planned_ms, Some(600_000));
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn sized_to_estimate_without_an_estimate_falls_back_to_a_pomodoro() {
        let p = plan_session(SessionLength::SizedToEstimate, None, 0, POMODORO_MS);
        assert_eq!(p.planned_ms, Some(POMODORO_MS));
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn an_estimate_over_one_session_chunks_n_of_m() {
        // 90 minutes against a 25-minute session: 4 sittings.
        let p = plan_session(
            SessionLength::SizedToEstimate,
            Some(90 * 60),
            0,
            POMODORO_MS,
        );
        assert_eq!(p.planned_ms, Some(POMODORO_MS));
        assert_eq!(p.chunk, Some(Chunk { index: 1, total: 4 }));
        // Third sitting of the same task.
        let p3 = plan_session(
            SessionLength::SizedToEstimate,
            Some(90 * 60),
            2,
            POMODORO_MS,
        );
        assert_eq!(p3.chunk, Some(Chunk { index: 3, total: 4 }));
        // Overrunning the estimate clamps rather than reading "5 of 4".
        let p9 = plan_session(
            SessionLength::SizedToEstimate,
            Some(90 * 60),
            8,
            POMODORO_MS,
        );
        assert_eq!(p9.chunk, Some(Chunk { index: 4, total: 4 }));
    }

    #[test]
    fn until_done_has_no_plan_and_no_chunks() {
        let p = plan_session(SessionLength::UntilDone, Some(90 * 60), 3, POMODORO_MS);
        assert_eq!(p.planned_ms, None);
        assert_eq!(p.chunk, None);
    }

    #[test]
    fn chunk_count_edges() {
        assert_eq!(chunk_count(0, POMODORO_MS), None);
        assert_eq!(chunk_count(POMODORO_MS, POMODORO_MS), None);
        assert_eq!(chunk_count(POMODORO_MS + 1, POMODORO_MS), Some(2));
        assert_eq!(chunk_count(1_000, 0), None);
    }

    #[test]
    fn long_break_lands_every_fourth_cycle() {
        assert_eq!(break_after(0), Segment::Work);
        assert_eq!(break_after(1), Segment::ShortBreak);
        assert_eq!(break_after(3), Segment::ShortBreak);
        assert_eq!(break_after(4), Segment::LongBreak);
        assert_eq!(break_after(8), Segment::LongBreak);
        assert_eq!(Segment::Work.kind(), FocusKind::Work);
        assert_eq!(Segment::LongBreak.kind(), FocusKind::Break);
        assert_eq!(Segment::ShortBreak.default_ms(), SHORT_BREAK_MS);
        // The other two arms, which nothing read: a long break that lasted
        // five minutes, or a work segment that lasted twenty-five seconds,
        // would have passed. `LONG_BREAK_MS` had no reader at all.
        assert_eq!(Segment::LongBreak.default_ms(), LONG_BREAK_MS);
        assert_eq!(Segment::Work.default_ms(), POMODORO_MS);
        assert_ne!(
            SHORT_BREAK_MS, LONG_BREAK_MS,
            "the two breaks must differ or the arms are indistinguishable"
        );
    }
}
