//! Activity: the events an activity timeline lists, one row each.

use sunrise_domain::{ActivityEvent, ActivityKind};
use sunrise_id::EntityRef;

use super::hex16;

/// What happened, in one event. See [`sunrise_domain::ActivityKind`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ActivityDetail {
    /// Task created.
    TaskCreated,
    /// Task completed.
    TaskCompleted,
    /// Task reopened.
    TaskReopened,
    /// Task cancelled.
    TaskCancelled,
    /// Task deferred.
    TaskDeferred {
        /// Deferral count after the write.
        count: i64,
    },
    /// Task moved between streams.
    TaskMoved {
        /// Stream it left; the legacy Inbox id for no stream.
        from: EntityRef,
        /// Stream it joined; the legacy Inbox id for no stream.
        to: EntityRef,
    },
    /// Task edited.
    TaskUpdated {
        /// How many tracked fields changed.
        fields: u32,
    },
    /// Task tombstoned.
    TaskDeleted,
    /// Stream created.
    StreamCreated,
    /// Stream tombstoned.
    StreamDeleted,
    /// Focus session opened.
    FocusStarted {
        /// The session.
        session: EntityRef,
        /// Planned length.
        planned_ms: Option<u64>,
    },
    /// Focus session closed.
    FocusEnded {
        /// The session.
        session: EntityRef,
        /// Frozen focused time.
        focused_ms: u64,
        /// Whether the task was completed in it.
        completed_task: bool,
    },
}

impl From<&ActivityKind> for ActivityDetail {
    fn from(k: &ActivityKind) -> Self {
        match k {
            ActivityKind::TaskCreated => Self::TaskCreated,
            ActivityKind::TaskCompleted => Self::TaskCompleted,
            ActivityKind::TaskReopened => Self::TaskReopened,
            ActivityKind::TaskCancelled => Self::TaskCancelled,
            ActivityKind::TaskDeferred { count } => Self::TaskDeferred { count: *count },
            // No stream is the legacy Inbox id, as on `TaskItem`.
            ActivityKind::TaskMoved { from, to } => Self::TaskMoved {
                from: sunrise_domain::stream_key(*from),
                to: sunrise_domain::stream_key(*to),
            },
            ActivityKind::TaskUpdated { fields } => Self::TaskUpdated { fields: *fields },
            ActivityKind::TaskDeleted => Self::TaskDeleted,
            ActivityKind::StreamCreated => Self::StreamCreated,
            ActivityKind::StreamDeleted => Self::StreamDeleted,
            ActivityKind::FocusStarted {
                session,
                planned_ms,
            } => Self::FocusStarted {
                session: *session,
                planned_ms: *planned_ms,
            },
            ActivityKind::FocusEnded {
                session,
                focused_ms,
                completed_task,
            } => Self::FocusEnded {
                session: *session,
                focused_ms: *focused_ms,
                completed_task: *completed_task,
            },
        }
    }
}

/// One row of an activity timeline.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ActivityRow {
    /// When (epoch ms).
    pub at_ms: u64,
    /// Op id, lowercase hex — UniFFI has no fixed-size array type.
    pub op_id: String,
    /// Authoring device id, lowercase hex.
    pub device: String,
    /// The entity the event is about.
    pub entity: EntityRef,
    /// The entity's title at the time.
    pub label: String,
    /// What happened.
    pub detail: ActivityDetail,
    /// What happened, in words — the shared phrasing, so every client says it
    /// the same way.
    pub phrase: String,
}

impl From<&ActivityEvent> for ActivityRow {
    fn from(e: &ActivityEvent) -> Self {
        let ActivityEvent {
            at_ms,
            op_id,
            device,
            entity,
            label,
            kind,
        } = e;
        Self {
            at_ms: *at_ms,
            op_id: hex16(op_id),
            device: hex16(device),
            entity: *entity,
            label: label.clone(),
            detail: ActivityDetail::from(kind),
            phrase: sunrise_domain::activity_phrase(kind),
        }
    }
}
