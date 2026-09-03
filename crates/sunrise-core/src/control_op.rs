//! Control-op payloads: the op families that carry key material and trust
//! rather than user data.
//!
//! Per [ADR-0024](../../../docs/11-adr/0024-key-hierarchy.md) and
//! `docs/03-crypto/data-encryption-format.md` §Control ops. Three families,
//! all of them new at `DOC_SCHEMA_V = 5`:
//!
//! - [`KeyEnvelopePayload`] distributes one `(stream_id, epoch)` Stream key to
//!   one recipient by HPKE. It is what makes a Stream key reachable by a device
//!   that did not mint it, and — sealed a second time to the account identity —
//!   what makes recovery restore readable content.
//! - [`DeviceRevokePayload`] records that a device is no longer a member. The
//!   op itself does not rotate anything; the emitter mints new epochs alongside
//!   it. What the op does is make every *other* replica refuse the revoked
//!   device's later ops, which is a fact that has to converge.
//! - `DeviceCertPublish` carries a canonical-CBOR [`DeviceCert`] so a device
//!   admitted by pairing becomes known to every replica without a manual
//!   trust step. It replaces `Command::TrustDevice`, which accepted any
//!   self-signed cert and so could not be revoked from.
//!
//! These are **not** entities. They have no row, no LWW stamp and no
//! materialization: [`crate::inner_op::OpEffect::Control`] exists so the
//! compiler cannot route one into the entity materializer, where the `_ =>`
//! catch-all in the kind table would have filed it as a task.
//!
//! [`DeviceCert`]: sunrise_crypto::DeviceCert

use serde::{Deserialize, Serialize};

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
    /// Sealed to a device's `D_D_pub`. The device learns the epochs it is
    /// entitled to; a revoked device is simply not among the recipients of the
    /// epochs minted after it was cut off.
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
/// Recorded rather than inferred. The rotation is identical in every case — a
/// revoked device keeps what it already had and reads nothing written
/// afterwards — but the reason is what a device list has to show a user
/// months later, and it is not recoverable from anything else in the log.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRevokePayload {
    /// The device being revoked.
    #[serde(with = "serde_bytes")]
    pub revoked_device_id: [u8; 16],
    /// Why.
    pub reason_code: RevokeReason,
    /// From when. Ops the revoked device signed with an HLC at or after this
    /// are refused; earlier ones stand.
    ///
    /// A revocation that retroactively erased the device's history would be
    /// worse than none: a laptop being retired did not un-write the six months
    /// of work it did. The cut is at a time, and the time is recorded so every
    /// replica makes the same cut.
    pub effective_at_ms: u64,
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

    #[test]
    fn device_revoke_payload_round_trips() {
        let p = DeviceRevokePayload {
            revoked_device_id: [6u8; 16],
            reason_code: RevokeReason::Stolen,
            effective_at_ms: 1_700_000_000_000,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&p, &mut buf).unwrap();
        let back: DeviceRevokePayload = ciborium::de::from_reader(buf.as_slice()).unwrap();
        assert_eq!(back, p);
    }
}
