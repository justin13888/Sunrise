//! The versioned, wire-stable op vocabulary.
//!
//! An [`InnerOp`] is the plaintext CBOR payload carried inside every
//! [`sunrise_crypto::OpEnvelope`]: the engine encodes one per command, seals it
//! under the Stream key, and appends the envelope to the op log. On the receive
//! side ([`crate::engine::Engine::apply_remote`]) the envelope is opened and the
//! inner CBOR is decoded back into an `InnerOp`.
//!
//! Because its CBOR encoding is what envelopes carry on the wire and at rest,
//! **the variant names and field shapes here are a wire contract**. They are
//! serialized with serde's default external tagging (a single-entry map keyed by
//! the variant name, e.g. `{"TaskCreate": <Task>}`). Renaming a variant, adding
//! or reordering a variant's payload fields, or changing a payload type is a
//! breaking format change and must go through the protocol-versioning process —
//! never edit casually.
//!
//! v1 uses *full-state* ops: `TaskCreate`/`TaskUpdate` carry the entire `Task`,
//! not a field-level delta. This is the accepted v1 approximation of the CRDT
//! model in `docs/05-sync/conflict-resolution.md`: entity-level last-writer-wins
//! rather than per-field merge. `*Delete` ops carry only the target
//! [`EntityRef`] (a tombstone marker).

use serde::{Deserialize, Serialize};
use sunrise_domain::{Routine, Stream, Task};
use sunrise_id::{EntityKind, EntityRef};
use thiserror::Error;

/// The canonical op record. See the module docs: variant names + payload shapes
/// are wire-stable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum InnerOp {
    /// Create a task (full state).
    TaskCreate(Task),
    /// Replace a task's full state.
    TaskUpdate(Task),
    /// Tombstone a task.
    TaskDelete(EntityRef),
    /// Create a stream (full state).
    StreamCreate(Stream),
    /// Replace a stream's full state.
    StreamUpdate(Stream),
    /// Tombstone a stream.
    StreamDelete(EntityRef),
    /// Create a routine (full state). Boxed to keep the enum small.
    RoutineCreate(Box<Routine>),
    /// Replace a routine's full state.
    RoutineUpdate(Box<Routine>),
    /// Tombstone a routine.
    RoutineDelete(EntityRef),
}

/// The op's effect class, used to pick the materialization path and the emitted
/// [`crate::events::DomainEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpEffect {
    /// Entity created.
    Create,
    /// Entity mutated.
    Update,
    /// Entity tombstoned.
    Delete,
}

/// Inner-op CBOR codec errors.
#[derive(Debug, Error)]
pub enum InnerOpError {
    /// CBOR encode/decode failure for an inner-op blob.
    #[error("inner-op cbor: {0}")]
    Cbor(String),
}

impl InnerOp {
    /// The op-log `inner_kind` tag (e.g. `"task.create"`).
    pub(crate) fn inner_kind(&self) -> &'static str {
        match self {
            Self::TaskCreate(_) => "task.create",
            Self::TaskUpdate(_) => "task.update",
            Self::TaskDelete(_) => "task.delete",
            Self::StreamCreate(_) => "stream.create",
            Self::StreamUpdate(_) => "stream.update",
            Self::StreamDelete(_) => "stream.delete",
            Self::RoutineCreate(_) => "routine.create",
            Self::RoutineUpdate(_) => "routine.update",
            Self::RoutineDelete(_) => "routine.delete",
        }
    }

    /// The op-log `target_kind` tag (`"task"`, `"stream"`, `"routine"`).
    pub(crate) fn target_kind(&self) -> &'static str {
        match self {
            Self::TaskCreate(_) | Self::TaskUpdate(_) | Self::TaskDelete(_) => "task",
            Self::StreamCreate(_) | Self::StreamUpdate(_) | Self::StreamDelete(_) => "stream",
            Self::RoutineCreate(_) | Self::RoutineUpdate(_) | Self::RoutineDelete(_) => "routine",
        }
    }

    /// The entity this op targets.
    pub(crate) fn target_ref(&self) -> EntityRef {
        match self {
            Self::TaskCreate(t) | Self::TaskUpdate(t) => t.id,
            Self::StreamCreate(s) | Self::StreamUpdate(s) => s.id,
            Self::RoutineCreate(rt) | Self::RoutineUpdate(rt) => rt.id,
            Self::TaskDelete(r) | Self::StreamDelete(r) | Self::RoutineDelete(r) => *r,
        }
    }

    /// This op's effect class.
    pub(crate) fn effect(&self) -> OpEffect {
        match self {
            Self::TaskCreate(_) | Self::StreamCreate(_) | Self::RoutineCreate(_) => {
                OpEffect::Create
            }
            Self::TaskUpdate(_) | Self::StreamUpdate(_) | Self::RoutineUpdate(_) => {
                OpEffect::Update
            }
            Self::TaskDelete(_) | Self::StreamDelete(_) | Self::RoutineDelete(_) => {
                OpEffect::Delete
            }
        }
    }

    /// The [`EntityKind`] this op targets.
    pub(crate) fn entity_kind(&self) -> EntityKind {
        match self {
            Self::TaskCreate(_) | Self::TaskUpdate(_) | Self::TaskDelete(_) => EntityKind::Task,
            Self::StreamCreate(_) | Self::StreamUpdate(_) | Self::StreamDelete(_) => {
                EntityKind::Stream
            }
            Self::RoutineCreate(_) | Self::RoutineUpdate(_) | Self::RoutineDelete(_) => {
                EntityKind::Routine
            }
        }
    }
}

/// Encode an [`InnerOp`] to its canonical CBOR blob (the envelope payload).
pub(crate) fn encode_inner_op(op: &InnerOp) -> Result<Vec<u8>, InnerOpError> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(op, &mut buf).map_err(|e| InnerOpError::Cbor(e.to_string()))?;
    Ok(buf)
}

/// Decode an inner-op CBOR blob back into an [`InnerOp`].
///
/// # Errors
/// [`InnerOpError::Cbor`] if the bytes are not a valid inner-op CBOR map (an
/// unknown variant, a malformed payload, or trailing garbage).
pub(crate) fn decode_inner_op(bytes: &[u8]) -> Result<InnerOp, InnerOpError> {
    ciborium::de::from_reader(bytes).map_err(|e| InnerOpError::Cbor(e.to_string()))
}
