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
//! Focus ops are the one family that is **append-only rather than
//! last-writer-wins**: `FocusStart` / `FocusEnd` / `FocusInterrupt` each write
//! a record exactly once, keyed by the session's own `fcs_` id, so they never
//! contend with one another and re-delivery is a no-op. See ADR-0013 and
//! [`crate::engine`]'s `materialize_focus_remote`.
//!
//! Three families are **control** ops rather than entities: `KeyEnvelope`,
//! `DeviceRevoke` and `DeviceCertPublish` carry key material and trust, have no
//! row and no last-writer-wins stamp, and are classed
//! [`OpEffect::Control`] so the compiler keeps them out of the entity
//! materializer. See [`crate::control_op`] and ADR-0024.
//!
//! v1 uses *full-state* ops: `TaskCreate`/`TaskUpdate` carry the entire `Task`,
//! not a field-level delta. This is the accepted v1 approximation of the CRDT
//! model in `docs/05-sync/conflict-resolution.md`: entity-level last-writer-wins
//! rather than per-field merge. `*Delete` ops are full-state too: each carries
//! its entity with `deleted` set, never a bare id — see [`InnerOp::TaskDelete`]
//! and ADR-0014 for why a tombstone marker does not converge.

use crate::control_op::{DeviceRevokePayload, KeyEnvelopePayload};
use serde::{Deserialize, Serialize};
use sunrise_domain::{
    Attachment, Block, Context, FocusEnd, FocusStart, Interruption, ReviewSnapshot, Routine,
    Stream, Task,
};
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
    /// Tombstone a task, carrying the task's **full state** with `deleted`
    /// set — not just its id.
    ///
    /// A delete is an op like any other in a full-state op model, and
    /// entity-level LWW (ADR-0014) is defined as "the winning op's state
    /// replaces the entity". An id-only delete has no state to contribute, so
    /// when it won it set the tombstone and left every other column at
    /// whatever the local replica happened to hold — permanently divergent
    /// across replicas that had applied different updates, and stable, because
    /// both then carried the same winning stamp.
    TaskDelete(Task),
    /// Create a stream (full state).
    StreamCreate(Stream),
    /// Replace a stream's full state.
    StreamUpdate(Stream),
    /// Tombstone a stream, carrying its **full state** with `deleted` set.
    /// See [`Self::TaskDelete`] for why an id alone does not converge.
    StreamDelete(Stream),
    /// Create a context (full state).
    ContextCreate(Context),
    /// Replace a context's full state.
    ContextUpdate(Context),
    /// Tombstone a context, carrying its **full state** with `deleted` set.
    /// See [`Self::TaskDelete`] for why an id alone does not converge.
    ///
    /// Every replica that applies this op also drops the context from every
    /// Task carrying it, per `docs/02-domain/contexts-and-tags.md`.
    ContextDelete(Context),
    /// Create a routine (full state). Boxed to keep the enum small.
    RoutineCreate(Box<Routine>),
    /// Replace a routine's full state.
    RoutineUpdate(Box<Routine>),
    /// Tombstone a routine, carrying its **full state** with `deleted` set.
    /// Boxed to keep the enum small, as create and update are. See
    /// [`Self::TaskDelete`] for why an id alone does not converge.
    RoutineDelete(Box<Routine>),
    /// Create a time block (full state). Boxed to keep the enum small.
    BlockCreate(Box<Block>),
    /// Replace a time block's full state, bindings included.
    BlockUpdate(Box<Block>),
    /// Tombstone a time block, carrying its **full state** with `deleted` set,
    /// bindings included. Boxed to keep the enum small, as create and update
    /// are. See [`Self::TaskDelete`] for why an id alone does not converge.
    BlockDelete(Box<Block>),
    /// Record attachment metadata. Write-once: every field but the tombstone
    /// describes one specific run of ciphertext, so there is no update op.
    AttachmentCreate(Box<Attachment>),
    /// Tombstone attachment metadata, carrying its **full state** with
    /// `deleted` set. Boxed to keep the enum small, as create is. See
    /// [`Self::TaskDelete`] for why an id alone does not converge.
    ///
    /// The blob itself is reclaimed by the relay's GC after the device-cursor
    /// quorum, not here.
    AttachmentDelete(Box<Attachment>),
    /// Open a focus session (ADR-0013's `start` op). Append-only: the record
    /// is written once and never edited.
    FocusStart(Box<FocusStart>),
    /// Close a focus session (ADR-0013's `end` op). A *separate* record
    /// addressed to the same session id — never an update of the start.
    FocusEnd(Box<FocusEnd>),
    /// Log one interruption against a session. Grow-only set semantics.
    FocusInterrupt(Interruption),
    /// Record a completed weekly review (`docs/08-features/reviews-and-stats.md`
    /// §Weekly review step 5). Append-only, exactly like the focus family: the
    /// snapshot is written once under its own `rvw_` id and never edited, so
    /// two devices reviewing the same week produce two records rather than a
    /// lost write.
    ReviewSnapshotCreate(Box<ReviewSnapshot>),
    /// Distribute one `(stream_id, epoch)` Stream key to one recipient
    /// (ADR-0024 decision 4). A **control** op: it has no entity and never
    /// reaches the LWW materializer.
    KeyEnvelope(KeyEnvelopePayload),
    /// Record that a device is no longer a member of this account (ADR-0024
    /// decision 5). Control op.
    DeviceRevoke(DeviceRevokePayload),
    /// Publish a device's identity-signed [`sunrise_crypto::DeviceCert`] as
    /// canonical CBOR, so every replica can verify that device's envelopes.
    ///
    /// Replaces `Command::TrustDevice`, which took a cert straight from a
    /// caller and accepted it if it was self-signed — which every cert is.
    /// This op is **self-authenticating**: the receiver checks the envelope
    /// signature against the cert's own `d_s_pub` and then checks the cert
    /// against the account identity, so a stranger's cert cannot enter the
    /// device list however it is delivered. Control op.
    DeviceCertPublish(#[serde(with = "serde_bytes")] Vec<u8>),
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
    /// Not an entity at all: a control op carrying key material or trust.
    ///
    /// This variant exists so the compiler stops a control op reaching
    /// `materialize_remote`, whose kind table ends in a `_ =>` arm that files
    /// anything it does not recognise under `tasks`. A missing arm there is a
    /// silent wrong-table write; a missing arm here is a build failure.
    Control,
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
            Self::ContextCreate(_) => "context.create",
            Self::ContextUpdate(_) => "context.update",
            Self::ContextDelete(_) => "context.delete",
            Self::RoutineCreate(_) => "routine.create",
            Self::RoutineUpdate(_) => "routine.update",
            Self::RoutineDelete(_) => "routine.delete",
            Self::BlockCreate(_) => "block.create",
            Self::BlockUpdate(_) => "block.update",
            Self::BlockDelete(_) => "block.delete",
            Self::AttachmentCreate(_) => "attachment.create",
            Self::AttachmentDelete(_) => "attachment.delete",
            Self::FocusStart(_) => "focus.start",
            Self::FocusEnd(_) => "focus.end",
            Self::FocusInterrupt(_) => "focus.interrupt",
            Self::ReviewSnapshotCreate(_) => "review.snapshot",
            Self::KeyEnvelope(_) => "key.envelope",
            Self::DeviceRevoke(_) => "device.revoke",
            Self::DeviceCertPublish(_) => "device.cert",
        }
    }

    /// The op-log `target_kind` tag (`"task"`, `"stream"`, `"context"`,
    /// `"routine"`).
    pub(crate) fn target_kind(&self) -> &'static str {
        match self {
            Self::TaskCreate(_) | Self::TaskUpdate(_) | Self::TaskDelete(_) => "task",
            Self::StreamCreate(_) | Self::StreamUpdate(_) | Self::StreamDelete(_) => "stream",
            Self::ContextCreate(_) | Self::ContextUpdate(_) | Self::ContextDelete(_) => "context",
            Self::RoutineCreate(_) | Self::RoutineUpdate(_) | Self::RoutineDelete(_) => "routine",
            Self::BlockCreate(_) | Self::BlockUpdate(_) | Self::BlockDelete(_) => "block",
            Self::AttachmentCreate(_) | Self::AttachmentDelete(_) => "attachment",
            Self::FocusStart(_) | Self::FocusEnd(_) | Self::FocusInterrupt(_) => "focus_session",
            Self::ReviewSnapshotCreate(_) => "review_snapshot",
            Self::KeyEnvelope(_) => "stream_key",
            Self::DeviceRevoke(_) | Self::DeviceCertPublish(_) => "device",
        }
    }

    /// The entity this op targets.
    pub(crate) fn target_ref(&self) -> EntityRef {
        match self {
            Self::TaskCreate(t) | Self::TaskUpdate(t) | Self::TaskDelete(t) => t.id,
            Self::StreamCreate(s) | Self::StreamUpdate(s) | Self::StreamDelete(s) => s.id,
            Self::ContextCreate(c) | Self::ContextUpdate(c) | Self::ContextDelete(c) => c.id,
            Self::RoutineCreate(rt) | Self::RoutineUpdate(rt) | Self::RoutineDelete(rt) => rt.id,
            Self::BlockCreate(b) | Self::BlockUpdate(b) | Self::BlockDelete(b) => b.id,
            Self::AttachmentCreate(a) | Self::AttachmentDelete(a) => a.id,
            Self::FocusStart(f) => f.id,
            Self::FocusEnd(f) => f.session_id,
            Self::FocusInterrupt(i) => i.session_id,
            Self::ReviewSnapshotCreate(r) => r.id,
            // Control ops target no entity. The op log's `target_id` column is
            // nullable and the engine passes `None` for these, so this arm is
            // only reachable through a caller that asked for a ref it will not
            // use; a stream-shaped ref over the stream id is the least
            // misleading answer.
            Self::KeyEnvelope(p) => EntityRef::new(EntityKind::Stream, p.stream_id),
            Self::DeviceRevoke(p) => EntityRef::new(EntityKind::Device, p.revoked_device_id),
            Self::DeviceCertPublish(_) => EntityRef::new(EntityKind::Device, [0u8; 16]),
        }
    }

    /// This op's effect class.
    pub(crate) fn effect(&self) -> OpEffect {
        match self {
            Self::TaskCreate(_)
            | Self::StreamCreate(_)
            | Self::ContextCreate(_)
            | Self::RoutineCreate(_)
            | Self::BlockCreate(_)
            | Self::AttachmentCreate(_)
            | Self::FocusStart(_)
            | Self::ReviewSnapshotCreate(_) => OpEffect::Create,
            Self::TaskUpdate(_)
            | Self::StreamUpdate(_)
            | Self::ContextUpdate(_)
            | Self::RoutineUpdate(_)
            | Self::BlockUpdate(_)
            | Self::FocusEnd(_)
            | Self::FocusInterrupt(_) => OpEffect::Update,
            Self::TaskDelete(_)
            | Self::StreamDelete(_)
            | Self::ContextDelete(_)
            | Self::RoutineDelete(_)
            | Self::BlockDelete(_)
            | Self::AttachmentDelete(_) => OpEffect::Delete,
            Self::KeyEnvelope(_) | Self::DeviceRevoke(_) | Self::DeviceCertPublish(_) => {
                OpEffect::Control
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
            Self::ContextCreate(_) | Self::ContextUpdate(_) | Self::ContextDelete(_) => {
                EntityKind::Context
            }
            Self::RoutineCreate(_) | Self::RoutineUpdate(_) | Self::RoutineDelete(_) => {
                EntityKind::Routine
            }
            Self::BlockCreate(_) | Self::BlockUpdate(_) | Self::BlockDelete(_) => EntityKind::Block,
            Self::AttachmentCreate(_) | Self::AttachmentDelete(_) => EntityKind::Attachment,
            Self::FocusStart(_) | Self::FocusEnd(_) | Self::FocusInterrupt(_) => {
                EntityKind::FocusSession
            }
            Self::ReviewSnapshotCreate(_) => EntityKind::ReviewSnapshot,
            Self::KeyEnvelope(_) => EntityKind::Stream,
            Self::DeviceRevoke(_) | Self::DeviceCertPublish(_) => EntityKind::Device,
        }
    }

    /// Whether this op carries key material or trust rather than user data.
    ///
    /// Checked before materialization rather than inferred from the kind
    /// string, so adding a family cannot forget it.
    pub(crate) const fn is_control(&self) -> bool {
        matches!(
            self,
            Self::KeyEnvelope(_) | Self::DeviceRevoke(_) | Self::DeviceCertPublish(_)
        )
    }

    /// Whether this op is one of the two families that decide **membership**,
    /// and so cannot be gated on membership without becoming self-confirming.
    ///
    /// `DeviceCertPublish` is how a device enters the list and is
    /// self-authenticating. `DeviceRevoke` is how one leaves it, and gating it
    /// on the sender not already being revoked lets two crossed revocations
    /// diverge permanently: the replica that applies one first refuses the
    /// other and never learns of it, while a replica that saw them the other
    /// way round holds both.
    ///
    /// `KeyEnvelope` is deliberately **not** here. It carries key material
    /// rather than membership, and a revoked device has no business
    /// distributing keys after its cut.
    pub(crate) const fn is_revocation_control(&self) -> bool {
        matches!(self, Self::DeviceRevoke(_) | Self::DeviceCertPublish(_))
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
