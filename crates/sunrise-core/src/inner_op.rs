//! The versioned, wire-stable op vocabulary.
//!
//! An `InnerOp` is the plaintext CBOR payload carried inside every
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
//! Four families are **control** ops rather than entities: `KeyEnvelope`,
//! `DeviceRevoke`, `DeviceCertPublish` and `IdentityTransition` carry key
//! material and trust, have no row and no last-writer-wins stamp, and are
//! classed `OpEffect::Control` so the compiler keeps them out of the entity
//! materializer. See [`crate::control_op`], ADR-0024 and ADR-0032.
//!
//! The core uses *full-state* ops: `TaskCreate`/`TaskUpdate` carry the entire `Task`,
//! not a field-level delta. This is the accepted current approximation of the CRDT
//! model in `docs/05-sync/conflict-resolution.md`: entity-level last-writer-wins
//! rather than per-field merge. `*Delete` ops are full-state too: each carries
//! its entity with `deleted` set, never a bare id — see `InnerOp::TaskDelete`
//! and ADR-0014 for why a tombstone marker does not converge.

use crate::control_op::{DeviceRevokePayload, IdentityTransitionPayload, KeyEnvelopePayload};
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
    /// Replace the account identity with a successor (ADR-0032,
    /// `docs/03-crypto/key-rotation.md` §Identity rotation). Control op.
    ///
    /// Boxed for the reason `RoutineCreate` is: the payload carries a roster
    /// and a share per device, and an unboxed variant would set the size of
    /// *every* `InnerOp` — including the task ops, which are the ones that
    /// actually occur in bulk — to the size of the largest rotation.
    IdentityTransition(Box<IdentityTransitionPayload>),
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
    /// The blob is a well-formed externally tagged op whose variant name this
    /// build does not know: a family added by a newer build, not damage.
    ///
    /// Carries the name. The receive path parks the op rather than refusing
    /// it (ADR-0045 §4).
    #[error("inner-op kind `{0}` is not one this build knows")]
    UnknownKind(String),
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
            Self::IdentityTransition(_) => "identity.transition",
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
            Self::IdentityTransition(_) => "identity",
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
            // The *successor*, not the predecessor: a ref names the thing the
            // op brings about, and after this op the account is `to`.
            Self::IdentityTransition(p) => EntityRef::new(EntityKind::Identity, p.to_identity_id),
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
            Self::KeyEnvelope(_)
            | Self::DeviceRevoke(_)
            | Self::DeviceCertPublish(_)
            | Self::IdentityTransition(_) => OpEffect::Control,
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
            Self::IdentityTransition(_) => EntityKind::Identity,
        }
    }

    /// Whether this op carries key material or trust rather than user data.
    ///
    /// Checked before materialization rather than inferred from the kind
    /// string, so adding a family cannot forget it.
    pub(crate) const fn is_control(&self) -> bool {
        matches!(
            self,
            Self::KeyEnvelope(_)
                | Self::DeviceRevoke(_)
                | Self::DeviceCertPublish(_)
                | Self::IdentityTransition(_)
        )
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
/// [`InnerOpError::UnknownKind`] if the bytes are an externally tagged op —
/// a single-entry map keyed by a text variant name, or a bare text name — and
/// that name is not one [`InnerOp`] declares. The payload under an unknown
/// name is not inspected: this build has no shape to check it against.
///
/// [`InnerOpError::Cbor`] for anything else that does not decode: bytes that
/// are not CBOR, a shape that is not an externally tagged op, or a known
/// variant whose payload is malformed. A known name with a bad payload is
/// never `UnknownKind`, so a damaged op of a family this build reads is still
/// reported as damage.
pub(crate) fn decode_inner_op(bytes: &[u8]) -> Result<InnerOp, InnerOpError> {
    ciborium::de::from_reader(bytes).map_err(|e| {
        unknown_kind(bytes).map_or_else(
            || InnerOpError::Cbor(e.to_string()),
            InnerOpError::UnknownKind,
        )
    })
}

/// The variant name of an externally tagged op whose name [`InnerOp`] does
/// not declare, or `None` when the bytes are not that shape or the name is
/// known.
fn unknown_kind(bytes: &[u8]) -> Option<String> {
    use ciborium::value::Value;
    let name = match ciborium::de::from_reader::<Value, _>(bytes).ok()? {
        // `{"Name": payload}`: every variant this build declares.
        Value::Map(mut entries) if entries.len() == 1 => match entries.pop()?.0 {
            Value::Text(name) => name,
            _ => return None,
        },
        // `"Name"`: serde's external tagging of a unit variant, which a later
        // family is free to be.
        Value::Text(name) => name,
        _ => return None,
    };
    (!known_kinds().contains(&name.as_str())).then_some(name)
}

/// Every variant name [`InnerOp`]'s derived `Deserialize` accepts.
///
/// Read out of the derive itself, not kept as a second list: serde passes the
/// enum's variant names to `Deserializer::deserialize_enum`, and a probe
/// deserializer that records them and stops is the only way to get the list
/// the decoder actually uses. A hand-kept list would drift the first time a
/// family was added without it, and the op of that family would then be
/// parked by the very build that knows how to apply it.
fn known_kinds() -> &'static [&'static str] {
    use serde::de::{self, Deserializer, Visitor};
    use std::sync::OnceLock;

    struct Probe<'a>(&'a mut &'static [&'static str]);

    impl<'de> Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("probe"))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = variants;
            Err(de::Error::custom("probe"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct identifier ignored_any
        }
    }

    static KINDS: OnceLock<&'static [&'static str]> = OnceLock::new();
    KINDS.get_or_init(|| {
        let mut kinds: &'static [&'static str] = &[];
        let _ = InnerOp::deserialize(Probe(&mut kinds));
        kinds
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_op::{KeyShare, RosterEntry};
    use serde_bytes::ByteBuf;

    fn transition() -> InnerOp {
        InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
            from_identity_id: [7u8; 16],
            to_identity_id: [8u8; 16],
            to_id_s_pub: [9u8; 32],
            to_id_d_pub: [10u8; 32],
            roster: vec![RosterEntry {
                cert: vec![11u8; 200],
            }],
            device_shares: vec![KeyShare {
                device_id: [12u8; 16],
                hpke_ciphertext: vec![13u8; 80],
            }],
            identity_share: Some(ByteBuf::from(vec![14u8; 112])),
            prev_sig: [15u8; 64],
            next_sig: [16u8; 64],
        }))
    }

    #[test]
    fn an_identity_transition_round_trips_through_the_op_codec() {
        let op = transition();
        let back = decode_inner_op(&encode_inner_op(&op).expect("encode")).expect("decode");
        let (InnerOp::IdentityTransition(a), InnerOp::IdentityTransition(b)) = (&op, &back) else {
            panic!("the variant must survive the round trip");
        };
        assert_eq!(a, b);
    }

    /// The wire encoding is externally tagged, exactly as the module docs say.
    /// Pinned here for the reason `control_op`'s `Recipient` test pins its
    /// shape: a serde attribute added upstream must not silently re-shape a
    /// signed op, and this one carries the account's whole trust root.
    #[test]
    fn identity_transition_encodes_as_a_single_entry_map() {
        let bytes = encode_inner_op(&transition()).expect("encode");
        let value: ciborium::value::Value = ciborium::de::from_reader(bytes.as_slice()).unwrap();
        let ciborium::value::Value::Map(map) = value else {
            panic!("an InnerOp must encode as a map");
        };
        assert_eq!(map.len(), 1);
        assert_eq!(
            map[0].0,
            ciborium::value::Value::Text("IdentityTransition".into())
        );
        // Boxing must not show on the wire: `Box<T>` is transparent to serde,
        // so the entry is the payload map and not a wrapper around it.
        assert!(matches!(map[0].1, ciborium::value::Value::Map(_)));
    }

    fn cbor(value: &ciborium::value::Value) -> Vec<u8> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(value, &mut buf).unwrap();
        buf
    }

    /// The probe reads the derive's own list. Were it to come back empty,
    /// every op would read as unknown and park, including a damaged op of a
    /// family this build applies, so the list is pinned from both ends.
    #[test]
    fn known_kinds_is_the_derives_variant_list() {
        let kinds = known_kinds();
        assert_eq!(kinds.first(), Some(&"TaskCreate"));
        assert_eq!(kinds.last(), Some(&"IdentityTransition"));
        for name in [
            "BlockCreate",
            "AttachmentDelete",
            "FocusInterrupt",
            "KeyEnvelope",
        ] {
            assert!(kinds.contains(&name), "{name} is declared");
        }
        let bytes = encode_inner_op(&transition()).unwrap();
        assert!(unknown_kind(&bytes).is_none(), "a known op is not unknown");
    }

    #[test]
    fn a_variant_this_build_does_not_declare_is_an_unknown_kind() {
        use ciborium::value::Value;
        let newtype = cbor(&Value::Map(vec![(
            Value::Text("FutureKind".into()),
            Value::Map(vec![(Value::Text("x".into()), Value::Integer(1.into()))]),
        )]));
        assert!(matches!(
            decode_inner_op(&newtype),
            Err(InnerOpError::UnknownKind(k)) if k == "FutureKind"
        ));
        let unit = cbor(&Value::Text("FutureUnit".into()));
        assert!(matches!(
            decode_inner_op(&unit),
            Err(InnerOpError::UnknownKind(k)) if k == "FutureUnit"
        ));
    }

    /// Only the variant name decides. A family this build reads, with a
    /// payload that does not decode, is damage and not a newer build's op,
    /// and so is anything that is not an externally tagged op at all.
    #[test]
    fn damage_is_never_an_unknown_kind() {
        use ciborium::value::Value;
        let cases = [
            cbor(&Value::Map(vec![(
                Value::Text("TaskCreate".into()),
                Value::Integer(7.into()),
            )])),
            cbor(&Value::Map(vec![
                (Value::Text("FutureA".into()), Value::Null),
                (Value::Text("FutureB".into()), Value::Null),
            ])),
            cbor(&Value::Map(vec![(Value::Integer(1.into()), Value::Null)])),
            cbor(&Value::Integer(3.into())),
            vec![0xff, 0x00, 0x13],
        ];
        for bytes in cases {
            assert!(
                matches!(decode_inner_op(&bytes), Err(InnerOpError::Cbor(_))),
                "{bytes:02x?} must stay damage"
            );
        }
    }

    /// The six tables every new variant has to be added to, asserted together
    /// so that a family added with five of them cannot pass on the strength of
    /// the one that happened to be exercised elsewhere.
    #[test]
    fn an_identity_transition_is_classed_as_a_control_op_everywhere() {
        let op = transition();
        assert_eq!(op.inner_kind(), "identity.transition");
        assert_eq!(op.target_kind(), "identity");
        assert_eq!(
            op.target_ref(),
            EntityRef::new(EntityKind::Identity, [8u8; 16]),
            "the ref names the successor, which is what the op brings about"
        );
        assert_eq!(op.effect(), OpEffect::Control);
        assert_eq!(op.entity_kind(), EntityKind::Identity);
        assert!(op.is_control());
    }
}
