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
//! Two shapes write entity state (ADR-0044):
//!
//! - **Full-state** ops, `TaskCreate`/`TaskUpdate`/`TaskDelete` and their
//!   siblings, carry the entire entity. Every one already in a log or on a
//!   relay stays readable forever. The merge reads one as a write to every
//!   field it carries, at its own stamp (ADR-0044 §7), so a history made only
//!   of them projects exactly as entity-level last-writer-wins did. A delete
//!   carries its entity with `deleted` set, never a bare id: see
//!   `InnerOp::TaskDelete` and ADR-0014.
//! - `InnerOp::Patch` carries only the fields its command wrote, each as a
//!   self-describing field op, and every field merges by its own CRDT type.
//!   See `PatchPayload` and [`crate::engine`]'s `merge` module.
//!
//! This build applies both and emits only the first. ADR-0044 §9 forbids a
//! build from emitting its first `Patch` into a vault until the vault's
//! `vault_requires` lists `core.field_ops`, and `vault_requires` does not exist
//! yet (#324).

use crate::control_op::{DeviceRevokePayload, IdentityTransitionPayload, KeyEnvelopePayload};
use serde::{Deserialize, Serialize};
use sunrise_domain::{
    Attachment, Block, Context, FocusEnd, FocusStart, Interruption, ReviewSnapshot, Routine,
    Stream, Task, Unknowns,
};
use sunrise_id::{EntityKind, EntityRef};
use thiserror::Error;

/// Expands the entity registry ([`sunrise_id::for_each_entity!`]) into
/// [`InnerOp`] and its routing.
///
/// Every entity op variant, its payload, its `inner_kind`, its effect, its
/// target field and its entity are read from the registry; only the four
/// control families, which carry key material and trust rather than an
/// entity, are written here. An entity op therefore cannot be added without
/// every routing table below learning it.
///
/// The derived `inner_kind` and `target_kind` reach the op log only on the
/// paths that read them from the op: a remote op
/// ([`crate::engine::Engine::apply_remote`]) and a control op
/// (`emit_control_op`). A local entity write passes its two op-log strings to
/// `ops_insert` by hand, and nothing checks them against the op it seals. One
/// differs on purpose: a local defer is logged as `task.defer`, while the
/// same `TaskUpdate` is logged as `task.update` when a peer receives it.
macro_rules! define_inner_op {
    (
        $(
            $(#[$kind_meta:meta])*
            $kind:ident {
                prefix: $prefix:literal,
                tag: $tag:literal,
                merge: $merge:ident,
                owner: $owner:ident $(($owner_field:literal))?,
                features: [$($feature:literal),* $(,)?],
                ops: [
                    $(
                        $(#[$op_meta:meta])*
                        $op:ident($payload:ty) = $inner_kind:literal, $class:ident, $target:ident;
                    )*
                ],
                records: [
                    $(
                        $record:ident @ $storage:tt {
                            $(
                                $field:ident $(as $wire:literal)?: $field_ty:ty => $crdt:ident;
                            )*
                            $(..$unknown:ident)?
                        }
                    )*
                ],
            }
        )*
    ) => {
        /// The canonical op record. See the module docs: variant names + payload shapes
        /// are wire-stable.
        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub(crate) enum InnerOp {
            $($(
                $(#[$op_meta])*
                $op($payload),
            )*)*
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
            /// Write some fields of one entity, each merged by its own CRDT
            /// type (ADR-0044 §1). It carries only the fields its command
            /// wrote. See [`PatchPayload`].
            ///
            /// Boxed for the reason `RoutineCreate` is: the field map is
            /// unbounded, and the task ops are the ones that occur in bulk.
            Patch(Box<PatchPayload>),
        }

        impl InnerOp {
            /// The op-log `inner_kind` tag (e.g. `"task.create"`).
            pub(crate) fn inner_kind(&self) -> &'static str {
                match self {
                    $($( Self::$op(_) => $inner_kind, )*)*
                    Self::KeyEnvelope(_) => "key.envelope",
                    Self::DeviceRevoke(_) => "device.revoke",
                    Self::DeviceCertPublish(_) => "device.cert",
                    Self::IdentityTransition(_) => "identity.transition",
                    // One kind per entity, so the op log's `inner_kind` still
                    // names the family that wrote a row: `task.patch`.
                    Self::Patch(p) => match p.target.kind() {
                        $( EntityKind::$kind => concat!($tag, ".patch"), )*
                        // `EntityKind` is `#[non_exhaustive]`; every kind it
                        // has is listed above.
                        _ => "patch",
                    },
                }
            }

            /// The op-log `target_kind` tag: the registry's `tag` for an entity
            /// op (`"task"`, `"focus_session"`, …).
            pub(crate) fn target_kind(&self) -> &'static str {
                match self {
                    $($( Self::$op(_) => $tag, )*)*
                    Self::KeyEnvelope(_) => "stream_key",
                    Self::DeviceRevoke(_) | Self::DeviceCertPublish(_) => "device",
                    Self::IdentityTransition(_) => "identity",
                    Self::Patch(p) => p.target.kind().tag(),
                }
            }

            /// The entity this op targets.
            pub(crate) fn target_ref(&self) -> EntityRef {
                match self {
                    $($( Self::$op(p) => p.$target, )*)*
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
                    Self::IdentityTransition(p) => {
                        EntityRef::new(EntityKind::Identity, p.to_identity_id)
                    }
                    Self::Patch(p) => p.target,
                }
            }

            /// This op's effect class.
            pub(crate) fn effect(&self) -> OpEffect {
                match self {
                    $($( Self::$op(_) => OpEffect::$class, )*)*
                    Self::KeyEnvelope(_)
                    | Self::DeviceRevoke(_)
                    | Self::DeviceCertPublish(_)
                    | Self::IdentityTransition(_) => OpEffect::Control,
                    Self::Patch(p) => p.effect(),
                }
            }

            /// The [`EntityKind`] this op targets.
            pub(crate) fn entity_kind(&self) -> EntityKind {
                match self {
                    $($( Self::$op(_) => EntityKind::$kind, )*)*
                    Self::KeyEnvelope(_) => EntityKind::Stream,
                    Self::DeviceRevoke(_) | Self::DeviceCertPublish(_) => EntityKind::Device,
                    Self::IdentityTransition(_) => EntityKind::Identity,
                    Self::Patch(p) => p.target.kind(),
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
    };
}

sunrise_id::for_each_entity!(define_inner_op);

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
    /// This variant exists so a control op is told apart from an entity op by
    /// type rather than by its kind string: `materialize_remote` routes every
    /// entity op by its registry entry, and a control op's `entity_kind` names
    /// the kind it is *about* (a Stream, a Device), not a row it writes.
    Control,
}

/// The payload of [`InnerOp::Patch`]: some fields of one entity (ADR-0044 §1).
///
/// ```cddl
/// patch = {
///   "ref":     entity-ref,          ; the entity; its kind comes from the id prefix
///   ? "create": true,               ; this op creates the entity
///   ? "origin": "generated",        ; written by generation; absent = user
///   "fields":  { + field-name => field-op },
///   unknown-fields
/// }
/// ```
///
/// The field ops are kept as CBOR here and read by the merge, which knows
/// each field's CRDT type. A field op is one of `{"set": v}`,
/// `{"add": [..], "remove": [[v, [op-ref, ..]], ..]}`, `{"inc": n}` and
/// `{"map": {k: {"set": v}, ..}}`, so a build can merge a field it has never
/// heard of (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PatchPayload {
    /// The entity written.
    #[serde(rename = "ref")]
    pub(crate) target: EntityRef,
    /// This op creates the entity (§4). Absent on the wire when false.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub(crate) create: bool,
    /// `"generated"` for a write the system made on the user's behalf;
    /// absent for a user's own command (§3). Any other spelling reads as a
    /// user write and is kept as it came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<String>,
    /// Field name to field op, as CBOR.
    pub(crate) fields: Unknowns,
    /// Top-level keys a newer build added, kept (ADR-0045 §Unknown maps).
    #[serde(flatten)]
    pub(crate) unknown: Unknowns,
}

impl PatchPayload {
    /// The spelling of a generated write's `origin`.
    pub(crate) const GENERATED: &'static str = "generated";

    /// Whether this op was made by the system rather than by a user command.
    pub(crate) fn is_generated(&self) -> bool {
        self.origin.as_deref() == Some(Self::GENERATED)
    }

    /// The op's effect class: a create, a tombstone (`deleted` set to true),
    /// or an update.
    pub(crate) fn effect(&self) -> OpEffect {
        if self.create {
            return OpEffect::Create;
        }
        let deletes = self
            .fields
            .get("deleted")
            .and_then(|op| op.get().as_map())
            .is_some_and(|entries| {
                entries.iter().any(|(k, v)| {
                    k.as_text() == Some("set") && *v == ciborium::value::Value::Bool(true)
                })
            });
        if deletes {
            OpEffect::Delete
        } else {
            OpEffect::Update
        }
    }
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
///
/// The canonical document schema (`crate::doc_schema`) reads it for the same
/// reason.
pub(crate) fn known_kinds() -> &'static [&'static str] {
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
        assert_eq!(kinds.last(), Some(&"Patch"));
        for name in [
            "BlockCreate",
            "AttachmentDelete",
            "FocusInterrupt",
            "KeyEnvelope",
            "IdentityTransition",
        ] {
            assert!(kinds.contains(&name), "{name} is declared");
        }
        let bytes = encode_inner_op(&transition()).unwrap();
        assert!(unknown_kind(&bytes).is_none(), "a known op is not unknown");
    }

    /// `InnerOp` is the registry's ops, in registry order, then the four
    /// control families, then `Patch` — read out of the derive, so it is the
    /// list the decoder actually accepts.
    #[test]
    fn the_derive_declares_the_registry_ops_then_the_control_families() {
        let mut expected: Vec<&str> = sunrise_id::registry::ENTITIES
            .iter()
            .flat_map(|e| e.ops.iter().map(|o| o.variant))
            .collect();
        expected.extend([
            "KeyEnvelope",
            "DeviceRevoke",
            "DeviceCertPublish",
            "IdentityTransition",
            "Patch",
        ]);
        assert_eq!(known_kinds(), expected.as_slice());
    }

    /// The op-log strings every stored op already carries. Generating the
    /// routing from the registry must not move one, and a registry edit that
    /// would is caught here rather than in a vault.
    #[test]
    fn entity_op_log_tags_are_pinned() {
        let tags: Vec<(&str, &str, &str)> = sunrise_id::registry::ENTITIES
            .iter()
            .flat_map(|e| e.ops.iter().map(move |o| (o.variant, o.inner_kind, e.tag)))
            .collect();
        assert_eq!(
            tags,
            [
                ("TaskCreate", "task.create", "task"),
                ("TaskUpdate", "task.update", "task"),
                ("TaskDelete", "task.delete", "task"),
                ("StreamCreate", "stream.create", "stream"),
                ("StreamUpdate", "stream.update", "stream"),
                ("StreamDelete", "stream.delete", "stream"),
                ("ContextCreate", "context.create", "context"),
                ("ContextUpdate", "context.update", "context"),
                ("ContextDelete", "context.delete", "context"),
                ("RoutineCreate", "routine.create", "routine"),
                ("RoutineUpdate", "routine.update", "routine"),
                ("RoutineDelete", "routine.delete", "routine"),
                ("BlockCreate", "block.create", "block"),
                ("BlockUpdate", "block.update", "block"),
                ("BlockDelete", "block.delete", "block"),
                ("AttachmentCreate", "attachment.create", "attachment"),
                ("AttachmentDelete", "attachment.delete", "attachment"),
                ("FocusStart", "focus.start", "focus_session"),
                ("FocusEnd", "focus.end", "focus_session"),
                ("FocusInterrupt", "focus.interrupt", "focus_session"),
                ("ReviewSnapshotCreate", "review.snapshot", "review_snapshot"),
            ]
        );
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
