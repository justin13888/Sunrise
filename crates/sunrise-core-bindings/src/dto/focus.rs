//! Focus: sessions, their interruptions and the plan they run.

use sunrise_core::queries::{ActionableTask, FocusPlanRow, FocusSessionRow};
use sunrise_domain::{
    Calibration, Chunk, EffectiveTaskState, Energy, EnergyFit, EnergyFocus, FocusEnd, FocusKind,
    FocusSession, FocusStart, FocusStats, Interruption, InterruptionReason, InterruptionTally,
    SessionPlan, StreamFocus, UnblockCascade,
};
use sunrise_id::EntityRef;

use super::TaskItem;

/// See [`sunrise_domain::Chunk`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct ChunkMarker {
    /// 1-based index of this sitting.
    pub index: u32,
    /// Total sittings the estimate implies.
    pub total: u32,
}

impl From<&Chunk> for ChunkMarker {
    fn from(c: &Chunk) -> Self {
        let Chunk {
            index,
            total,
            unknown: _,
        } = c;
        Self {
            index: *index,
            total: *total,
        }
    }
}

/// See [`sunrise_domain::SessionPlan`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct PlannedSession {
    /// Planned length; absent means "until done".
    pub planned_ms: Option<u64>,
    /// Chunk marker when the estimate exceeds one sitting.
    pub chunk: Option<ChunkMarker>,
}

impl From<&SessionPlan> for PlannedSession {
    fn from(p: &SessionPlan) -> Self {
        let SessionPlan { planned_ms, chunk } = p;
        Self {
            planned_ms: *planned_ms,
            chunk: chunk.as_ref().map(ChunkMarker::from),
        }
    }
}

/// See [`sunrise_domain::Interruption`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct InterruptionRow {
    /// The session interrupted.
    pub session_id: EntityRef,
    /// When.
    pub at: jiff::Timestamp,
    /// One-tap reason.
    pub reason: InterruptionReason,
}

impl From<&Interruption> for InterruptionRow {
    fn from(i: &Interruption) -> Self {
        let Interruption {
            session_id,
            at,
            reason,
        } = i;
        Self {
            session_id: *session_id,
            at: *at,
            reason: reason.clone(),
        }
    }
}

/// See [`sunrise_domain::FocusStart`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionStart {
    /// Session id.
    pub id: EntityRef,
    /// Task focused on.
    pub task_id: EntityRef,
    /// Owning stream.
    pub stream_id: EntityRef,
    /// When it opened.
    pub started_at: jiff::Timestamp,
    /// Planned length; absent means "until done".
    pub planned_ms: Option<u64>,
    /// Declared energy budget.
    pub energy: Option<Energy>,
    /// Work or break.
    pub kind: FocusKind,
    /// Chunk marker.
    pub chunk: Option<ChunkMarker>,
}

impl From<&FocusStart> for SessionStart {
    fn from(s: &FocusStart) -> Self {
        let FocusStart {
            id,
            task_id,
            stream_id,
            started_at,
            planned_ms,
            energy,
            kind,
            chunk,
            unknown: _,
        } = s;
        Self {
            id: *id,
            task_id: *task_id,
            stream_id: *stream_id,
            started_at: *started_at,
            planned_ms: *planned_ms,
            energy: energy.clone(),
            kind: kind.clone(),
            chunk: chunk.as_ref().map(ChunkMarker::from),
        }
    }
}

impl From<&SessionStart> for FocusStart {
    fn from(s: &SessionStart) -> Self {
        Self {
            id: s.id,
            task_id: s.task_id,
            stream_id: s.stream_id,
            started_at: s.started_at,
            planned_ms: s.planned_ms,
            energy: s.energy.clone(),
            kind: s.kind.clone(),
            chunk: s.chunk.map(|c| Chunk::new(c.index, c.total)),
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

/// See [`sunrise_domain::FocusEnd`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionEnd {
    /// Session closed.
    pub session_id: EntityRef,
    /// When.
    pub ended_at: jiff::Timestamp,
    /// Frozen focused time.
    pub actual_focused_ms: u64,
    /// Interruptions recorded against the session.
    pub interruptions: Vec<InterruptionRow>,
    /// Whether the task was completed in this session.
    pub completed_task: bool,
}

impl From<&FocusEnd> for SessionEnd {
    fn from(e: &FocusEnd) -> Self {
        let FocusEnd {
            session_id,
            ended_at,
            actual_focused_ms,
            interruptions,
            completed_task,
            unknown: _,
        } = e;
        Self {
            session_id: *session_id,
            ended_at: *ended_at,
            actual_focused_ms: *actual_focused_ms,
            interruptions: interruptions.iter().map(InterruptionRow::from).collect(),
            completed_task: *completed_task,
        }
    }
}

impl From<&SessionEnd> for FocusEnd {
    fn from(e: &SessionEnd) -> Self {
        Self {
            session_id: e.session_id,
            ended_at: e.ended_at,
            actual_focused_ms: e.actual_focused_ms,
            interruptions: e.interruptions.iter().map(Interruption::from).collect(),
            completed_task: e.completed_task,
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

impl From<&InterruptionRow> for Interruption {
    fn from(i: &InterruptionRow) -> Self {
        Self {
            session_id: i.session_id,
            at: i.at,
            reason: i.reason.clone(),
        }
    }
}

impl From<&SessionRow> for FocusSession {
    fn from(r: &SessionRow) -> Self {
        Self {
            start: FocusStart::from(&r.start),
            end: r.end.as_ref().map(FocusEnd::from),
            interruptions: r.interruptions.iter().map(Interruption::from).collect(),
        }
    }
}

/// One focus session, with the two numbers derived from the clock rather than
/// stored. See [`sunrise_core::queries::FocusSessionRow`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionRow {
    /// How it opened.
    pub start: SessionStart,
    /// How it closed, if it has.
    pub end: Option<SessionEnd>,
    /// Interruptions, from both ops.
    pub interruptions: Vec<InterruptionRow>,
    /// Still running (a `start` with no `end` — a valid state).
    pub running: bool,
    /// Focused time: frozen once ended, derived while running.
    pub focused_ms: u64,
}

impl From<&FocusSessionRow> for SessionRow {
    fn from(r: &FocusSessionRow) -> Self {
        let FocusSessionRow {
            session,
            running,
            focused_ms,
        } = r;
        let FocusSession {
            start,
            end,
            interruptions,
        } = session;
        Self {
            start: SessionStart::from(start),
            end: end.as_ref().map(SessionEnd::from),
            interruptions: interruptions.iter().map(InterruptionRow::from).collect(),
            running: *running,
            focused_ms: *focused_ms,
        }
    }
}

/// See [`sunrise_domain::Calibration`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct CalibrationRow {
    /// Actual over estimated.
    pub factor: f64,
    /// How many sessions the factor folds.
    pub samples: u32,
    /// Total estimated time, in ms.
    pub estimated_ms: u64,
    /// Total actual focused time, in ms.
    pub actual_ms: u64,
}

impl From<&Calibration> for CalibrationRow {
    fn from(c: &Calibration) -> Self {
        let Calibration {
            factor,
            samples,
            estimated_ms,
            actual_ms,
        } = c;
        Self {
            factor: *factor,
            samples: *samples,
            estimated_ms: *estimated_ms,
            actual_ms: *actual_ms,
        }
    }
}

/// See [`sunrise_domain::StreamFocus`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamFocusRow {
    /// The stream.
    pub stream: EntityRef,
    /// Sessions folded.
    pub sessions: u32,
    /// Focused time, in ms.
    pub focused_ms: u64,
    /// Estimate calibration, when there is enough data.
    pub calibration: Option<CalibrationRow>,
}

impl From<&StreamFocus> for StreamFocusRow {
    fn from(s: &StreamFocus) -> Self {
        let StreamFocus {
            stream,
            sessions,
            focused_ms,
            calibration,
        } = s;
        Self {
            stream: *stream,
            sessions: *sessions,
            focused_ms: *focused_ms,
            calibration: calibration.as_ref().map(CalibrationRow::from),
        }
    }
}

/// See [`sunrise_domain::EnergyFocus`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct EnergyFocusRow {
    /// The energy facet; absent is the "no signal" bucket.
    pub energy: Option<Energy>,
    /// Sessions folded.
    pub sessions: u32,
    /// Focused time, in ms.
    pub focused_ms: u64,
    /// Estimate calibration.
    pub calibration: Option<CalibrationRow>,
}

impl From<&EnergyFocus> for EnergyFocusRow {
    fn from(e: &EnergyFocus) -> Self {
        let EnergyFocus {
            energy,
            sessions,
            focused_ms,
            calibration,
        } = e;
        Self {
            energy: energy.clone(),
            sessions: *sessions,
            focused_ms: *focused_ms,
            calibration: calibration.as_ref().map(CalibrationRow::from),
        }
    }
}

/// See [`sunrise_domain::InterruptionTally`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct InterruptionTallyRow {
    /// The reason.
    pub reason: InterruptionReason,
    /// How many times.
    pub count: u32,
}

impl From<&InterruptionTally> for InterruptionTallyRow {
    fn from(t: &InterruptionTally) -> Self {
        let InterruptionTally { reason, count } = t;
        Self {
            reason: reason.clone(),
            count: *count,
        }
    }
}

/// See [`sunrise_domain::FocusStats`] — totals and a calibration factor. No
/// score, no streak, no quota.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FocusTotals {
    /// Sessions folded.
    pub sessions: u32,
    /// Of which work (rather than break) sessions.
    pub work_sessions: u32,
    /// Sessions still running.
    pub running: u32,
    /// Focused time, in ms.
    pub total_focused_ms: u64,
    /// Interruptions logged.
    pub interruptions: u32,
    /// Per-stream breakdown.
    pub per_stream: Vec<StreamFocusRow>,
    /// Per-energy breakdown.
    pub per_energy: Vec<EnergyFocusRow>,
    /// Whole-vault calibration.
    pub overall: Option<CalibrationRow>,
    /// Most common interruption reasons.
    pub top_interruptions: Vec<InterruptionTallyRow>,
}

impl From<&FocusStats> for FocusTotals {
    fn from(s: &FocusStats) -> Self {
        let FocusStats {
            sessions,
            work_sessions,
            running,
            total_focused_ms,
            interruptions,
            per_stream,
            per_energy,
            overall,
            top_interruptions,
        } = s;
        Self {
            sessions: *sessions,
            work_sessions: *work_sessions,
            running: *running,
            total_focused_ms: *total_focused_ms,
            interruptions: *interruptions,
            per_stream: per_stream.iter().map(StreamFocusRow::from).collect(),
            per_energy: per_energy.iter().map(EnergyFocusRow::from).collect(),
            overall: overall.as_ref().map(CalibrationRow::from),
            top_interruptions: top_interruptions
                .iter()
                .map(InterruptionTallyRow::from)
                .collect(),
        }
    }
}

/// See [`sunrise_domain::UnblockCascade`] — what completing a task released.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Cascade {
    /// The task that was completed.
    pub completed: EntityRef,
    /// Tasks it released.
    pub released: Vec<EntityRef>,
    /// Tasks still waiting on something else.
    pub still_blocked: Vec<EntityRef>,
}

impl From<&UnblockCascade> for Cascade {
    fn from(c: &UnblockCascade) -> Self {
        let UnblockCascade {
            completed,
            released,
            still_blocked,
        } = c;
        Self {
            completed: *completed,
            released: released.clone(),
            still_blocked: still_blocked.clone(),
        }
    }
}

/// One row of the focus planner. See [`sunrise_core::queries::FocusPlanRow`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlanRow {
    /// The proposed task.
    pub task: TaskItem,
    /// Open tasks finishing this one would release — the leverage signal.
    pub unblocks: u32,
    /// How its energy facet scored against the session budget.
    pub energy_fit: EnergyFit,
    /// The session the core would open.
    pub suggested: PlannedSession,
    /// Work sessions this task has already had.
    pub prior_sessions: u32,
    /// Why it sits where it does, in one line.
    pub reason: String,
}

impl From<&FocusPlanRow> for PlanRow {
    fn from(r: &FocusPlanRow) -> Self {
        let FocusPlanRow {
            task,
            unblocks,
            energy_fit,
            suggested,
            prior_sessions,
        } = r;
        Self {
            task: TaskItem::from(task),
            unblocks: *unblocks,
            energy_fit: *energy_fit,
            suggested: PlannedSession::from(suggested),
            prior_sessions: *prior_sessions,
            reason: sunrise_domain::plan_reason(*unblocks, *energy_fit, suggested),
        }
    }
}

/// One row of the actionable list: a task plus the two dependency facts that
/// are recomputed rather than stored.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionableTaskRow {
    /// The task.
    pub task: TaskItem,
    /// User state widened with the derived `Blocked` case.
    pub effective_state: EffectiveTaskState,
    /// How many blockers are still open.
    pub open_blockers: u32,
    /// How many open tasks are waiting on this one.
    pub unblocks: u32,
}

impl From<&ActionableTask> for ActionableTaskRow {
    fn from(a: &ActionableTask) -> Self {
        let ActionableTask {
            task,
            effective_state,
            open_blockers,
            unblocks,
        } = a;
        Self {
            task: TaskItem::from(task),
            effective_state: *effective_state,
            open_blockers: *open_blockers,
            unblocks: *unblocks,
        }
    }
}
