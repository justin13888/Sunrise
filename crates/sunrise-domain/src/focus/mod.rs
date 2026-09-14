//! Focus sessions per `docs/08-features/focus-mode.md`, represented as
//! [ADR-0013](../../../docs/11-adr/0013-focus-session-op-representation.md)
//! decides: an **append-only record keyed by its own `EntityRef`** (`fcs_`),
//! never a mutable field on a Task.
//!
//! A session is two ops and no mutable register:
//!
//! * a [`FocusStart`] — `(task_id, stream_id, started_at_ms, planned_ms,
//!   energy, kind)`, and
//! * later, a separate [`FocusEnd`] — `(ended_at_ms, actual_focused_ms,
//!   interruptions, completed_task)`.
//!
//! Both address the same session id; the session is otherwise immutable. A
//! `start` with no `end` is a **valid** state — it reads as *still running*
//! ([`FocusSession::is_running`]) — and never a tombstone case.
//!
//! **Nothing ticking is ever persisted.** A live session's elapsed time is
//! derived on read from the caller's injected clock
//! ([`FocusSession::elapsed_ms`]); `actual_focused_ms` is frozen only at the
//! `end` op. That is the determinism rule from
//! `docs/01-architecture/shared-core.md` §determinism applied to a timer.
//!
//! Everything in this module is a **pure function over values the caller has
//! already read** — no storage, no clock, no ambient state — so the planner
//! ranking ([`rank_focus_plan`]), the estimate calibration
//! ([`fold_focus_stats`]), the session sizing ([`plan_session`]) and the
//! unblock cascade ([`unblock_cascade`]) all unit-test in isolation.

mod planner;
mod session;
mod stats_fold;
mod timer;

pub use planner::{
    energy_fit, rank_focus_plan, unblock_cascade, EnergyFit, PlanCandidate, PlanRanked,
    UnblockCascade,
};
pub use session::{
    Chunk, FocusEnd, FocusKind, FocusSession, FocusStart, Interruption, InterruptionReason,
};
pub use stats_fold::{
    fold_focus_stats, Calibration, EnergyFocus, FocusStats, InterruptionTally, SessionRecord,
    StreamFocus,
};
pub use timer::{
    break_after, chunk_count, plan_session, Segment, SessionLength, SessionPlan,
    CYCLES_BEFORE_LONG_BREAK, LONG_BREAK_MS, POMODORO_MS, SHORT_BREAK_MS,
};
