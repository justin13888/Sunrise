//! Control-op payloads: the op families that carry key material and trust
//! rather than user data.
//!
//! Per [ADR-0024](../../../docs/11-adr/0024-key-hierarchy.md) and
//! `docs/03-crypto/data-encryption-format.md` §Control ops. Four families; the
//! first three are new at `DOC_SCHEMA_V = 5` and the fourth at `6`:
//!
//! - [`KeyEnvelopePayload`] distributes one `(stream_id, epoch)` Stream key to
//!   one recipient by HPKE. It is what makes a Stream key reachable by a device
//!   that did not mint it, and — sealed a second time to the account identity —
//!   what makes recovery restore readable content.
//! - [`DeviceRevokePayload`] records that a device is no longer a member. The
//!   op itself does not rotate anything; the emitter mints new epochs alongside
//!   it. What the op does is put the same *record* on every replica — and
//!   nothing more: no replica refuses a revoked device's ops or withholds a key
//!   from it. Membership is a fact that has to converge whether or not anything
//!   yet acts on it; acting on it is `#76` for reads and `#82`, behind `#80`,
//!   for writes.
//! - `DeviceCertPublish` carries a canonical-CBOR [`DeviceCert`] so a device
//!   admitted by pairing becomes known to every replica without a manual
//!   trust step. It replaces `Command::TrustDevice`, which accepted any
//!   self-signed cert and so could not be revoked from.
//! - [`IdentityTransitionPayload`] replaces the account identity itself with a
//!   successor, carrying in one op the successor's public halves, a re-issued
//!   cert for every surviving device, and the successor's secrets sealed to
//!   each of them. It is what finally closes the gap
//!   [`crate::keychain::Keychain::issue_cert_for`] names: every member can mint
//!   a valid cert, so a revoked device can certify itself under a fresh id, and
//!   no revocation can undo that — only a new identity can.
//!
//! These are **not** entities. They have no row, no LWW stamp and no
//! materialization: `inner_op`'s `OpEffect::Control` variant exists so the
//! compiler cannot route one into the entity materializer, where the `_ =>`
//! catch-all in the kind table would have filed it as a task.
//!
//! [`DeviceCert`]: sunrise_crypto::DeviceCert

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

// Every fixed-width id and every opaque blob below carries
// `#[serde(with = "serde_bytes")]`. Without it `[u8; N]` and `Vec<u8>` encode
// as CBOR *arrays of integers* — two bytes per byte, and a shape no other id in
// `data-encryption-format.md` has, where every id is `bstr .size 16`. These
// families are new at `DOC_SCHEMA_V = 5`, so this is the one moment the choice
// is free.

/// Who a [`KeyEnvelopePayload`] is sealed to.
///
/// The two classes are what make revocation and recovery both work, and they
/// are deliberately not collapsed into "a public key": a replica has to be able
/// to tell *whose* copy an envelope is without trial-decrypting it, and a
/// device must not try its own key against the identity's copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recipient {
    /// Sealed to a device's `D_D_pub`, so the device learns the epochs it is
    /// entitled to. Every device is a recipient, revoked or not: the epoch is
    /// sealed to the account identity as well, and every paired device holds
    /// `ID_D_priv`, so leaving a revoked device out of this list would withhold
    /// nothing from it. See `#76`.
    Device(#[serde(with = "serde_bytes")] [u8; 16]),
    /// Sealed to the account identity's `ID_D_pub`. Opened by the recovery
    /// blob's `ID_D_priv`, so a recovery with no surviving device still reaches
    /// the content.
    Identity(#[serde(with = "serde_bytes")] [u8; 16]),
}

/// One Stream key, sealed to one recipient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEnvelopePayload {
    /// The stream whose key this is.
    #[serde(with = "serde_bytes")]
    pub stream_id: [u8; 16],
    /// The epoch this key belongs to.
    pub epoch: u32,
    /// Who can open it.
    pub recipient: Recipient,
    /// `BLAKE3.derive_key("sunrise.stream_key_id.v1", stream_key, 8)`.
    ///
    /// A disambiguator, not an authenticator. Two devices can mint the same
    /// epoch concurrently and both keys are kept; this says which row a
    /// recipient should file the opened key under without having to open it
    /// first. Nothing trusts it — the value is re-derived from the opened key.
    #[serde(with = "serde_bytes")]
    pub key_id: [u8; 8],
    /// `enc(32) || ciphertext(32) || tag(16)`, RFC 9180 Base.
    #[serde(with = "serde_bytes")]
    pub hpke_ciphertext: Vec<u8>,
}

/// Why a device was revoked.
///
/// Recorded rather than inferred. Nothing branches on it — the rotation is
/// identical in every case, and so is the revocation, which is to say inert —
/// but the reason is what a device list has to show a user months later, and it
/// is not recoverable from anything else in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevokeReason {
    /// Misplaced; may still turn up.
    Lost,
    /// Known to be in someone else's hands.
    Stolen,
    /// Deliberately retired by its owner.
    Retired,
    /// Believed to have had its keys extracted.
    Compromised,
}

impl RevokeReason {
    /// The stored form, matching the CBOR encoding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lost => "Lost",
            Self::Stolen => "Stolen",
            Self::Retired => "Retired",
            Self::Compromised => "Compromised",
        }
    }

    /// Parse the stored form.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Lost" => Some(Self::Lost),
            "Stolen" => Some(Self::Stolen),
            "Retired" => Some(Self::Retired),
            "Compromised" => Some(Self::Compromised),
            _ => None,
        }
    }
}

/// A device is no longer a member of this account.
///
/// # There is no `effective_at` field, and that is the design
///
/// The cut is the **HLC of the op that declares it**, read off the envelope by
/// every replica rather than chosen by the emitter. Nothing compares anything
/// against it today — the register is inert, see [`Recipient::Device`] — but it
/// is recorded as a *time* so that whatever eventually enforces it can let the
/// device's earlier work stand. A revocation that retroactively erased that
/// history would be worse than none: a laptop being retired did not un-write
/// the six months of work it did.
///
/// An `effective_at_ms` field was tried and removed. It was a plain field of
/// the signed envelope, so it was the whole of a revocation's meaning and
/// entirely emitter-controlled, and no bound on it survived contact:
///
/// * bounding it *ahead* of the op's HLC left a window in which a cut could be
///   parked far enough forward to refuse nothing;
/// * bounding it *behind* had nothing to anchor to. `Hlc::observe` refuses
///   readings from the future only; the past is unbounded by design, and
///   nothing re-bounds a device's own HLC when its wall clock moves. A device
///   more than a few minutes slow emits, through the ordinary command with no
///   crafted input at all, a cut far enough in the past to refuse its target's
///   entire history.
///
/// The op's own HLC has neither problem: it is monotone, it is merged from
/// every peer this device has heard from rather than read off the wall clock,
/// and the clock gate already refuses one too far ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokePayload {
    /// The device being revoked.
    #[serde(with = "serde_bytes")]
    pub revoked_device_id: [u8; 16],
    /// Why.
    pub reason_code: RevokeReason,
}

/// One surviving device's re-issued [`DeviceCert`], signed by the **new**
/// `ID_S_priv`.
///
/// There is deliberately **no `device_id` field**. The id is inside the cert,
/// in a body the identity signed; a copy beside it would be a second source of
/// truth that nothing keeps in step, and every reader that trusted the outer
/// copy would be trusting an unsigned field. `roster_digest` reads the id back
/// out of the cert for exactly this reason.
///
/// A one-field struct rather than a bare `Vec<u8>` because the roster is the
/// place a later revision most obviously grows a field (a per-device status,
/// say), and an array of bare byte strings could not take one without
/// re-shaping the op.
///
/// [`DeviceCert`]: sunrise_crypto::DeviceCert
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    /// Canonical-CBOR [`DeviceCert`], as [`sunrise_crypto::DeviceCert::to_cbor`]
    /// writes it.
    ///
    /// [`DeviceCert`]: sunrise_crypto::DeviceCert
    #[serde(with = "serde_bytes")]
    pub cert: Vec<u8>,
}

/// The successor identity's secrets, sealed to one surviving device.
///
/// Sealed under [`sunrise_crypto::identity_share_info`], which binds the blob
/// to the successor identity *and* to this device — HPKE Base authenticates no
/// sender, so without the device in the `info` a blob could be moved between
/// entries and the roster would name one device beside another's share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyShare {
    /// The device this share is for.
    #[serde(with = "serde_bytes")]
    pub device_id: [u8; 16],
    /// `enc(32) || ct(32) || tag(16)` — 80 bytes, RFC 9180 Base.
    #[serde(with = "serde_bytes")]
    pub hpke_ciphertext: Vec<u8>,
}

/// The account identity is replaced by a successor.
///
/// ADR-0032 and `docs/03-crypto/key-rotation.md` §Identity rotation. One op
/// carries the whole hand-over, because the parts are not independently
/// applicable: a replica that learned the new `ID_S_pub` without the re-issued
/// certs would reject every device in the account, and one that learned the
/// certs without the transition would have no key to verify them under.
///
/// `identity_id` is `identity_id_from_pub(ID_S_pub)`, a derivation rather than
/// a field, so a new `ID_S` *is* a new id: hence `from_identity_id` and
/// `to_identity_id` rather than the single unchanged id
/// `docs/03-crypto/key-rotation.md` draws. Neither is believed — both are
/// recomputed by [`sunrise_crypto::verify_identity_transition`] from the key
/// they name.
///
/// The devices' own `D_S`/`D_D` are untouched. Those wrap under an AAD binding
/// to `device_id`, not to the identity, and nothing about rotating the account
/// key invalidates a device key; only the *cert* — the identity's statement
/// about the device — has to be re-issued.
///
/// # There is no `effective_at` field, for [`DeviceRevokePayload`]'s reasons
///
/// The same argument applies, and applies harder. An emitter-chosen cut on a
/// revocation decided which of one device's ops to refuse; an emitter-chosen
/// cut on a transition decides which *identity* every op in the account
/// verifies under. Bounded ahead of the op's HLC it parks the hand-over far
/// enough forward to take effect nowhere; bounded behind it has nothing to
/// anchor to, and a device a few minutes slow would retroactively re-attribute
/// the account's history to a key that did not sign it. The op's own HLC is
/// monotone, merged from every peer, and already gated for drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityTransitionPayload {
    /// The identity being retired.
    #[serde(with = "serde_bytes")]
    pub from_identity_id: [u8; 16],
    /// The successor identity.
    #[serde(with = "serde_bytes")]
    pub to_identity_id: [u8; 16],
    /// The successor's `ID_S_pub`.
    #[serde(with = "serde_bytes")]
    pub to_id_s_pub: [u8; 32],
    /// The successor's `ID_D_pub`.
    #[serde(with = "serde_bytes")]
    pub to_id_d_pub: [u8; 32],
    /// One re-issued cert per surviving device.
    pub roster: Vec<RosterEntry>,
    /// The successor's secrets, sealed to each surviving device.
    pub device_shares: Vec<KeyShare>,
    /// The successor's `ID_S_priv || ID_D_priv` (64 B) sealed to the
    /// **outgoing** `ID_D_pub` — 112 B — under
    /// [`sunrise_crypto::identity_carry_info`].
    ///
    /// Optional because the outgoing `ID_D_priv` is held by at most one device
    /// and by the recovery blob, and a rotation triggered by *suspected key
    /// extraction* is precisely the case where carrying the successor forward
    /// under the compromised key would hand the attacker the new identity too.
    /// Present, a holder of the old recovery code follows the account forward
    /// without re-enrolling; absent, they do not, and that is the point.
    ///
    /// A [`ByteBuf`] rather than `Option<Vec<u8>>` with a `serde_bytes`
    /// attribute: the attribute form is easy to drop in a later edit and the
    /// field would silently start encoding as an array of integers, which is
    /// the failure the note at the top of this module exists to prevent. The
    /// type carries the rule instead.
    ///
    /// [`ByteBuf`]: serde_bytes::ByteBuf
    pub identity_share: Option<ByteBuf>,
    /// Ed25519 by the **outgoing** `ID_S_priv` over
    /// `"sunrise.identity_transition.v1" || body_hash`.
    #[serde(with = "serde_bytes")]
    pub prev_sig: [u8; 64],
    /// Ed25519 by the **successor** `ID_S_priv` over
    /// `"sunrise.identity_transition.succ.v1" || body_hash || prev_sig`.
    #[serde(with = "serde_bytes")]
    pub next_sig: [u8; 64],
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoke_reason_round_trips_through_its_stored_form() {
        for r in [
            RevokeReason::Lost,
            RevokeReason::Stolen,
            RevokeReason::Retired,
            RevokeReason::Compromised,
        ] {
            assert_eq!(RevokeReason::parse(r.as_str()), Some(r));
        }
        assert_eq!(RevokeReason::parse("Borrowed"), None);
    }

    /// The wire encoding is externally tagged, exactly as the CDDL in
    /// `data-encryption-format.md` says. Pinning it here means a serde
    /// attribute added upstream cannot silently re-shape a signed op.
    #[test]
    fn recipient_encodes_as_a_single_entry_map() {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&Recipient::Device([1u8; 16]), &mut buf).unwrap();
        let value: ciborium::value::Value = ciborium::de::from_reader(buf.as_slice()).unwrap();
        let ciborium::value::Value::Map(map) = value else {
            panic!("Recipient must encode as a map");
        };
        assert_eq!(map.len(), 1);
        assert_eq!(map[0].0, ciborium::value::Value::Text("Device".into()));
    }

    #[test]
    fn key_envelope_payload_round_trips() {
        let p = KeyEnvelopePayload {
            stream_id: [2u8; 16],
            epoch: 7,
            recipient: Recipient::Identity([3u8; 16]),
            key_id: [4u8; 8],
            hpke_ciphertext: vec![5u8; 80],
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: KeyEnvelopePayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);
    }

    fn a_transition() -> IdentityTransitionPayload {
        IdentityTransitionPayload {
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
        }
    }

    #[test]
    fn identity_transition_payload_round_trips() {
        let p = a_transition();
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: IdentityTransitionPayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);

        // The carry-forward share is genuinely optional, and `None` must not
        // decode back as an empty blob: absent and empty mean different things
        // to `shares_digest`'s present flag.
        let mut without = a_transition();
        without.identity_share = None;
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&without, &mut buf).unwrap();
        let back: IdentityTransitionPayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back.identity_share, None);
    }

    /// Every fixed-width field is a `bstr`, never an array of integers. The
    /// note at the top of this module is the rule; this is the assertion, and
    /// it reaches the nested `KeyShare` and `RosterEntry` too, where a dropped
    /// `serde_bytes` attribute would be easiest to miss.
    #[test]
    fn identity_transition_byte_fields_encode_as_byte_strings() {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&a_transition(), &mut buf).unwrap();
        let value: ciborium::value::Value = ciborium::de::from_reader(buf.as_slice()).unwrap();
        let ciborium::value::Value::Map(map) = value else {
            panic!("the payload must encode as a map");
        };
        for (key, entry) in &map {
            let ciborium::value::Value::Text(name) = key else {
                panic!("field names are text keys");
            };
            match name.as_str() {
                "roster" | "device_shares" => {
                    let ciborium::value::Value::Array(items) = entry else {
                        panic!("{name} must be an array");
                    };
                    let ciborium::value::Value::Map(inner) = &items[0] else {
                        panic!("{name} entries must be maps");
                    };
                    for (_, v) in inner {
                        assert!(
                            matches!(v, ciborium::value::Value::Bytes(_)),
                            "{name} carries only byte strings"
                        );
                    }
                }
                _ => assert!(
                    matches!(entry, ciborium::value::Value::Bytes(_)),
                    "`{name}` must be a bstr, not an array of integers"
                ),
            }
        }
        assert_eq!(map.len(), 9);
    }

    #[test]
    fn device_revoke_payload_round_trips() {
        let p = DeviceRevokePayload {
            revoked_device_id: [6u8; 16],
            reason_code: RevokeReason::Stolen,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: DeviceRevokePayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);
    }
}
