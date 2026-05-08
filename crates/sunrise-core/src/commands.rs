//! Command type submitted via [`crate::Core::submit`].

use serde::{Deserialize, Serialize};
use sunrise_domain::{StreamDraft, StreamPatch, TaskDraft, TaskPatch, TaskState};
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
}
