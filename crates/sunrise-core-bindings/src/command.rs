//! The write surface: every [`sunrise_core::Command`], in a shape UniFFI can
//! carry.
//!
//! One variant per command, no aggregation and no convenience wrappers. The
//! core's command list *is* the contract; a seam that offered a smaller one
//! would quietly decide what clients are allowed to do.

use sunrise_core::commands::FocusStartDraft;
use sunrise_core::Command;
use sunrise_domain::{Energy, InterruptionReason, SessionLength};
use sunrise_id::EntityRef;

use crate::dto::{
    ContextDraftIn, ContextEdit, RoutineDraftIn, RoutineEdit, SnapshotDraftIn, StreamDraftIn,
    StreamEdit, TaskDraftIn, TaskEdit,
};

/// A mutating command.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CoreCommand {
    /// Create a task (in the Inbox when `draft.stream_id` is absent).
    CreateTask {
        /// The draft.
        draft: TaskDraftIn,
    },
    /// Edit a task.
    UpdateTask {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: TaskEdit,
    },
    /// Set a task to `done`.
    CompleteTask {
        /// Target.
        id: EntityRef,
    },
    /// Push a task's scheduled time out and bump its deferral count.
    DeferTask {
        /// Target.
        id: EntityRef,
        /// New scheduled time (epoch ms).
        to_ms: u64,
    },
    /// Tombstone a task.
    DeleteTask {
        /// Target.
        id: EntityRef,
    },
    /// Move a task to another stream. Not a `TaskEdit` field: the move re-keys
    /// the task's storage.
    PromoteToStream {
        /// Target.
        id: EntityRef,
        /// Destination stream.
        stream: EntityRef,
    },
    /// Create a stream.
    CreateStream {
        /// The draft.
        draft: StreamDraftIn,
    },
    /// Edit a stream.
    UpdateStream {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: StreamEdit,
    },
    /// Tombstone a stream.
    DeleteStream {
        /// Target.
        id: EntityRef,
    },
    /// Create a context. Rejected if a live context already carries the name,
    /// compared case-insensitively.
    CreateContext {
        /// The draft.
        draft: ContextDraftIn,
    },
    /// Edit a context. Archiving is not deletion: an archived context stays on
    /// the tasks that carry it.
    UpdateContext {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: ContextEdit,
    },
    /// Tombstone a context, removing it from every task that carries it in the
    /// same transaction.
    DeleteContext {
        /// Target.
        id: EntityRef,
    },
    /// Create a routine and materialize its near-horizon occurrences.
    CreateRoutine {
        /// The draft.
        draft: RoutineDraftIn,
    },
    /// Edit a routine. A recurrence, timezone or anchor change regenerates
    /// future not-yet-started routine tasks.
    UpdateRoutine {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: RoutineEdit,
    },
    /// Tombstone a routine. Generation stops; existing tasks remain.
    DeleteRoutine {
        /// Target.
        id: EntityRef,
    },
    /// Skip one occurrence by its key (`YYYY-MM-DDTHH:MM` in the routine's
    /// zone).
    SkipRoutineOccurrence {
        /// Target.
        id: EntityRef,
        /// Occurrence key.
        occurrence_key: String,
    },
    /// Run materialization for every live routine.
    MaterializeRoutines {
        /// Wall clock (epoch ms) to materialize against.
        now_ms: u64,
    },
    /// Trust a peer device by its self-issued certificate (canonical CBOR).
    TrustDevice {
        /// The certificate.
        cert_cbor: Vec<u8>,
    },
    /// Open a focus session on a task.
    StartFocus {
        /// Task to focus on.
        task_id: EntityRef,
        /// Work or break.
        kind: sunrise_domain::FocusKind,
        /// How the session should be sized.
        length: SessionLength,
        /// Declared energy budget; the task's own facet when absent.
        energy: Option<Energy>,
    },
    /// Close a focus session. A separate record addressed to the same session
    /// id, never an edit of the start.
    EndFocus {
        /// Session to close.
        session: EntityRef,
        /// Focused time to freeze; the derived elapsed time when absent.
        actual_focused_ms: Option<u64>,
        /// Whether the task was completed in this session. Recording it is all
        /// this does — completing the task is a separate `CompleteTask`, so the
        /// session log never becomes a second writer of task state.
        completed_task: bool,
    },
    /// Log one interruption against a running session.
    LogInterruption {
        /// The session.
        session: EntityRef,
        /// One-tap reason.
        reason: InterruptionReason,
    },
    /// Record that a weekly review was completed.
    SaveReviewSnapshot {
        /// The snapshot.
        draft: SnapshotDraftIn,
    },
}

impl CoreCommand {
    /// Lower into the core's own command type.
    ///
    /// Infallible: every id arrived as an already-lifted `EntityRef`, so the
    /// parsing that can fail happened at the boundary, in
    /// [`crate::types`]'s `custom_type!`.
    pub(crate) fn into_core(self) -> Command {
        match self {
            Self::CreateTask { draft } => Command::CreateTask(draft.into()),
            Self::UpdateTask { id, edit } => Command::UpdateTask {
                id,
                patch: edit.into(),
            },
            Self::CompleteTask { id } => Command::CompleteTask(id),
            Self::DeferTask { id, to_ms } => Command::DeferTask { id, to_ms },
            Self::DeleteTask { id } => Command::DeleteTask(id),
            Self::PromoteToStream { id, stream } => Command::PromoteToStream { id, stream },
            Self::CreateStream { draft } => Command::CreateStream(draft.into()),
            Self::UpdateStream { id, edit } => Command::UpdateStream {
                id,
                patch: edit.into(),
            },
            Self::DeleteStream { id } => Command::DeleteStream(id),
            Self::CreateContext { draft } => Command::CreateContext(draft.into()),
            Self::UpdateContext { id, edit } => Command::UpdateContext {
                id,
                patch: edit.into(),
            },
            Self::DeleteContext { id } => Command::DeleteContext(id),
            Self::CreateRoutine { draft } => Command::CreateRoutine(draft.into()),
            Self::UpdateRoutine { id, edit } => Command::UpdateRoutine {
                id,
                patch: edit.into(),
            },
            Self::DeleteRoutine { id } => Command::DeleteRoutine(id),
            Self::SkipRoutineOccurrence { id, occurrence_key } => {
                Command::SkipRoutineOccurrence { id, occurrence_key }
            }
            Self::MaterializeRoutines { now_ms } => Command::MaterializeRoutines { now_ms },
            Self::TrustDevice { cert_cbor } => Command::TrustDevice { cert_cbor },
            Self::StartFocus {
                task_id,
                kind,
                length,
                energy,
            } => Command::StartFocus(FocusStartDraft {
                task_id,
                kind,
                length,
                energy,
            }),
            Self::EndFocus {
                session,
                actual_focused_ms,
                completed_task,
            } => Command::EndFocus {
                session,
                actual_focused_ms,
                completed_task,
            },
            Self::LogInterruption { session, reason } => {
                Command::LogInterruption { session, reason }
            }
            Self::SaveReviewSnapshot { draft } => Command::SaveReviewSnapshot(draft.into()),
        }
    }
}
