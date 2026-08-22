//! Command type submitted via [`crate::Core::submit`].

use serde::{Deserialize, Serialize};
use sunrise_domain::{
    ContextDraft, ContextPatch, Energy, InterruptionReason, RoutineDraft, RoutinePatch,
    ScheduleConstraint, SessionLength, StreamDraft, StreamPatch, TaskDraft, TaskPatch, TaskState,
};
use sunrise_id::EntityRef;

/// Mutating commands. v1 covers Tasks and Streams; deeper entity types
/// expand the surface in follow-up phases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Create a Task in a Stream (Inbox if `stream_id` is `None`).
    CreateTask(TaskDraft),
    /// Mutate an existing Task.
    UpdateTask {
        /// Target task.
        id: EntityRef,
        /// Patch.
        patch: TaskPatch,
    },
    /// Set Task state to `Done`.
    CompleteTask(EntityRef),
    /// Defer a Task by setting a new `scheduled_at` and bumping
    /// `deferred_count`.
    DeferTask {
        /// Target task.
        id: EntityRef,
        /// New `scheduled_at`.
        to_ms: u64,
    },
    /// Soft-delete a Task.
    DeleteTask(EntityRef),
    /// Move a Task between Streams.
    PromoteToStream {
        /// Task to move.
        id: EntityRef,
        /// New owning stream.
        stream: EntityRef,
    },
    /// Create a Stream.
    CreateStream(StreamDraft),
    /// Mutate a Stream.
    UpdateStream {
        /// Target stream.
        id: EntityRef,
        /// Patch.
        patch: StreamPatch,
    },
    /// Soft-delete a Stream.
    DeleteStream(EntityRef),
    /// Create a Context (cross-cutting tag). Rejected if a live Context
    /// already carries the same name, compared case-insensitively.
    CreateContext(ContextDraft),
    /// Mutate a Context (rename, re-describe, archive/unarchive).
    ///
    /// Archiving is *not* deletion: per
    /// `docs/02-domain/contexts-and-tags.md` an archived Context stays on the
    /// Tasks that carry it and only drops out of pickers and `@name` capture
    /// resolution.
    UpdateContext {
        /// Target context.
        id: EntityRef,
        /// Patch.
        patch: ContextPatch,
    },
    /// Soft-delete a Context, **removing it from every Task that carries it**
    /// in the same transaction (spec: "Deleting a Context removes it from all
    /// Tasks").
    DeleteContext(EntityRef),
    /// Create a Routine and materialize its near-horizon occurrences.
    CreateRoutine(RoutineDraft),
    /// Mutate a Routine. An rrule/timezone/anchor change regenerates future
    /// not-yet-started routine tasks.
    UpdateRoutine {
        /// Target routine.
        id: EntityRef,
        /// Patch.
        patch: RoutinePatch,
    },
    /// Soft-delete a Routine (tombstone). Stops generation; existing tasks
    /// remain.
    DeleteRoutine(EntityRef),
    /// Skip a single occurrence of a Routine by its occurrence key
    /// (`YYYY-MM-DDTHH:MM` in the routine's tz).
    SkipRoutineOccurrence {
        /// Target routine.
        id: EntityRef,
        /// Occurrence key to skip.
        occurrence_key: String,
    },
    /// Run materialization for every live Routine using `now_ms` as the clock.
    /// Emitted by `Core::open` and the periodic core timer.
    MaterializeRoutines {
        /// Wall clock (ms since epoch) from the injected clock.
        now_ms: u64,
    },
    /// Trust a peer device by its self-issued [`DeviceCert`] (canonical CBOR).
    ///
    /// Verifies the cert (self-signed, per v1 issuance), then upserts the
    /// device's id + signing pubkey + cert blob into the local `devices` table
    /// so that [`crate::Engine::apply_remote`] can verify envelopes signed by
    /// that device. Emits **no op**: device trust is local state in v1 (device
    /// pairing/attestation is a later slice).
    TrustDevice {
        /// Canonical-CBOR `DeviceCert` bytes for the peer device.
        cert_cbor: Vec<u8>,
    },
    /// Open a focus session on a Task (ADR-0013's `start` op).
    ///
    /// Mints a fresh `fcs_` id and writes an **append-only** record; it does
    /// not touch the Task. Two devices starting a session concurrently mint
    /// different ids, so both survive and both count — that is the whole
    /// multi-device story, and it needs no OR-Set.
    ///
    /// Nothing about the running timer is persisted: the planned length is
    /// stored, the elapsed time is derived on read from the injected clock.
    StartFocus(FocusStartDraft),
    /// Close a focus session (ADR-0013's `end` op) — a *separate* record
    /// addressed to the same session id, never an edit of the start.
    ///
    /// Rejected if the session is unknown or has already ended: a session is
    /// immutable once closed.
    EndFocus {
        /// Session to close.
        session: EntityRef,
        /// Focused time to freeze. `None` freezes the derived elapsed time
        /// (`now - started_at`), which is what a session with no pauses ran
        /// for; a client that tracked pauses passes the smaller real figure.
        actual_focused_ms: Option<u64>,
        /// Whether the Task was completed in this session. Recording it is all
        /// this does — completing the Task itself is a separate
        /// [`Command::CompleteTask`], so the session log never becomes a
        /// second, competing writer of task state.
        completed_task: bool,
    },
    /// Log one interruption against a running session
    /// (`docs/08-features/focus-mode.md` §Interruption capture).
    ///
    /// Grow-only: the `(session, at_ms, reason)` triple is the key, so
    /// re-delivery is idempotent and two devices' interruptions both survive.
    LogInterruption {
        /// Session interrupted.
        session: EntityRef,
        /// One-tap reason.
        reason: InterruptionReason,
    },
}

/// Draft for [`Command::StartFocus`]. The core fills the session id, the
/// owning Stream, the start time, the planned length and the chunk marker —
/// all of which are derived, not caller-supplied, so two clients starting the
/// same kind of session record the same shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusStartDraft {
    /// Task to focus on.
    pub task_id: EntityRef,
    /// Work or break.
    pub kind: sunrise_domain::FocusKind,
    /// How long this session should run.
    pub length: SessionLength,
    /// The session's declared **energy budget** — what the user has in the
    /// tank. Defaults to the Task's own `energy` facet when `None`.
    pub energy: Option<Energy>,
}

/// Result of a command, returned synchronously to the caller after the
/// in-process apply step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    /// Stable id of the entity touched.
    pub entity: EntityRef,
    /// New state if applicable (e.g., for Task transitions).
    pub state: Option<TaskState>,
    /// Op id assigned by the core.
    pub op_id: [u8; 16],
    /// Sequence number assigned by the core.
    pub seq: u64,
    /// `soft` scheduling constraints the accepted `scheduled_at` violates.
    ///
    /// A `soft` violation never blocks the write (a `hard` one is rejected
    /// outright with [`sunrise_domain::ValidationError::HardScheduleConstraint`]),
    /// but it must not vanish silently either: per
    /// `docs/02-domain/scheduling-constraints.md` it demotes the item in
    /// planning views and the UI surfaces *which* window was missed. Empty for
    /// every command that does not schedule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub soft_violations: Vec<ScheduleConstraint>,
}

impl CommandResult {
    /// Result for a command that schedules nothing (the common case).
    #[must_use]
    pub const fn new(
        entity: EntityRef,
        state: Option<TaskState>,
        op_id: [u8; 16],
        seq: u64,
    ) -> Self {
        Self {
            entity,
            state,
            op_id,
            seq,
            soft_violations: Vec::new(),
        }
    }

    /// Attach the `soft` constraint violations observed while scheduling.
    #[must_use]
    pub fn with_soft_violations(mut self, v: Vec<ScheduleConstraint>) -> Self {
        self.soft_violations = v;
        self
    }
}
