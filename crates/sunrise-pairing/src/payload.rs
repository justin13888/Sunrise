//! The `PairingPayload` — everything a second device needs to become a real
//! member of an account.
//!
//! Before ADR-0024 pairing transferred **only** the 32-byte vault root, and
//! that was enough because every Stream key was *derived* from it: the new
//! device recomputed the whole key schedule from one number. Deleting that
//! derivation deletes the shortcut. A device now needs the account identity —
//! so it can hold an identity-signed cert instead of a self-signed one — and
//! the Stream keys themselves, which are random and cannot be recomputed from
//! anything.
//!
//! ```cddl
//! PairingPayload = {
//!     1: bstr .size 32,   ; ID_S_priv
//!     ; 2 was ID_D_priv. Burned, never reused -- see below.
//!     3: bstr .size 32,   ; ID_S_pub
//!     4: bstr .size 32,   ; ID_D_pub
//!     5: bstr .size 16,   ; identity_id
//!     6: { * bstr .size 16 => { * uint => bstr .size 32 } },  ; stream_keys
//!     7: tstr,            ; nickname of the sending device
//!     8: tstr,            ; platform of the sending device
//!     9: bstr .size 32,   ; vault_root
//! }
//! ```
//!
//! # Why `ID_S_priv` travels and `ID_D_priv` does not
//!
//! The two identity private keys look symmetric and are not, and the asymmetry
//! is the whole of what makes revocation bound a device's reads.
//!
//! `ID_D_priv` is the X25519 scalar that opens the identity-sealed copy of
//! every `key_envelope`. Field 2 used to carry it, and that is what made
//! revocation unenforceable (`#76`): a revoked device dropped from a new
//! epoch's recipient list simply opened the identity copy instead, because
//! pairing had handed it the identity's own unwrapping key. Every device
//! holding it meant no device could be excluded from anything.
//!
//! It does not travel now. A device gets the keys it needs two other ways:
//! field 6 hands it every Stream key the sender holds, which covers every
//! epoch minted before it paired, and it is a `Recipient::Device` on every
//! epoch minted after — sealed to its own `D_D_pub`, which a revocation can
//! stop addressing. `ID_D_priv` stays on the device that *created* the account
//! and is destined for the recovery blob behind the BIP-39 code, which is what
//! will let a recovery with no surviving device restore readable content. That
//! blob is specified and not built — `seal_recovery_blob` has no production
//! caller — so today the creator's vault is the only place the key exists, and
//! revoking that one device does not bound its reads.
//!
//! Sealing needs only the public half, so field 4 (`ID_D_pub`) still travels
//! and a paired device can still mint epochs for the identity. Asymmetric
//! cryptography is doing real work here: the device can address the recovery
//! path without being able to walk it.
//!
//! `ID_S_priv` does still travel, because it is what lets a device issue a
//! `device_cert` for the *next* device it pairs, and a pairing that withheld it
//! would produce a device that can never admit another one. That it is on every
//! device — a revoked one included — is why a cert names its issuer and is
//! refused when that issuer was revoked at the cert's own HLC; see
//! `sunrise_crypto::device_cert`.
//!
//! The channel it travels over is the Noise XX transport confirmed by a SAS
//! both users read aloud. That is the same channel the vault root already used,
//! and the vault root was never the smaller secret.

use std::collections::BTreeMap;
use subtle::ConstantTimeEq;
use sunrise_crypto::identity_id_from_pub;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::handshake::MAX_NOISE_MESSAGE;

/// Largest encoded payload the Noise transport can carry in one message.
///
/// A Noise transport message is capped at 65535 bytes and costs a 16-byte
/// ChaCha20-Poly1305 tag, so this is the real ceiling on the plaintext.
pub const MAX_PAIRING_PAYLOAD: usize = MAX_NOISE_MESSAGE - 16;

/// Errors from the pairing payload codec.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PairingPayloadError {
    /// CBOR encode/decode failure.
    #[error("pairing payload cbor: {0}")]
    Cbor(String),
    /// A field was missing, or had the wrong CBOR type or byte length.
    #[error("invalid pairing payload field: {0}")]
    BadField(&'static str),
    /// `identity_id` is not the id derived from `ID_S_pub`.
    ///
    /// Checked by the receiver before anything is written: the sender chose
    /// every byte here, and a payload whose id names one identity while its key
    /// belongs to another would install a device that signs under one identity
    /// and files itself under a different one.
    #[error("pairing payload identity_id does not match its ID_S_pub")]
    IdentityMismatch,
    /// The encoded payload does not fit in one Noise transport message.
    ///
    /// Surfaced rather than silently truncated. Chunking it across several
    /// transport messages is the eventual answer — the channel is
    /// bidirectional and already carries multiple frames — but it needs a
    /// framing contract that reaches the Swift seam, where a sealed payload is
    /// currently one base64 string. A vault this large is far past anything v1
    /// produces, and a wrong answer here is a device that silently cannot read
    /// half its own content.
    #[error("pairing payload is {len} bytes, over the {MAX_PAIRING_PAYLOAD}-byte transport limit")]
    TooLarge {
        /// Encoded length that did not fit.
        len: usize,
    },
}

/// Everything the existing device hands the new one.
///
/// Zeroized on drop: it holds the vault root, the identity signing seed and
/// every Stream key in the account — the single most sensitive value this
/// codebase ever materializes. It does **not** hold `ID_D_priv`; see the
/// module docs for why that one stays in the recovery blob.
#[derive(Clone)]
pub struct PairingPayload {
    /// `ID_S_priv`, the identity Ed25519 seed.
    pub id_s_priv: [u8; 32],
    /// `ID_S_pub`.
    pub id_s_pub: [u8; 32],
    /// `ID_D_pub`.
    pub id_d_pub: [u8; 32],
    /// The account's identity id.
    pub identity_id: [u8; 16],
    /// The vault root, which still keys the local database and wraps
    /// everything at rest.
    pub vault_root: [u8; 32],
    /// Every Stream key the sending device holds: `stream_id -> epoch -> key`.
    ///
    /// `BTreeMap` all the way down so the encoding is canonical without a sort
    /// step, and so two devices that assembled the same set produce the same
    /// bytes.
    pub stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>>,
    /// Nickname of the sending device, for the new device's device list.
    pub nickname: String,
    /// Platform of the sending device.
    pub platform: String,
}

impl Zeroize for PairingPayload {
    /// Hand-written because `BTreeMap` is not `Zeroize`: clearing the map frees
    /// its nodes without scrubbing them, so each key is zeroized in place
    /// first and only then dropped.
    fn zeroize(&mut self) {
        self.id_s_priv.zeroize();
        self.id_s_pub.zeroize();
        self.id_d_pub.zeroize();
        self.identity_id.zeroize();
        self.vault_root.zeroize();
        for epochs in self.stream_keys.values_mut() {
            for key in epochs.values_mut() {
                key.zeroize();
            }
        }
        self.stream_keys.clear();
    }
}

impl ZeroizeOnDrop for PairingPayload {}

impl Drop for PairingPayload {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for PairingPayload {
    /// Names the shape and nothing else. A `Debug` that printed this struct
    /// would put the whole account in whatever log the app writes.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingPayload")
            .field("identity_id", &hex::encode(self.identity_id))
            .field("streams", &self.stream_keys.len())
            .finish_non_exhaustive()
    }
}

impl PairingPayload {
    /// Total `(stream, epoch)` pairs carried.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.stream_keys.values().map(BTreeMap::len).sum()
    }
}

/// Encode to canonical, integer-keyed CBOR.
///
/// # Errors
/// [`PairingPayloadError::Cbor`] on an encode failure, or
/// [`PairingPayloadError::TooLarge`] if the result will not fit in one Noise
/// transport message.
pub fn encode_pairing_payload(p: &PairingPayload) -> Result<Vec<u8>, PairingPayloadError> {
    use ciborium::value::{Integer, Value};

    let mut streams: Vec<(Value, Value)> = Vec::with_capacity(p.stream_keys.len());
    for (stream_id, epochs) in &p.stream_keys {
        let mut per_epoch: Vec<(Value, Value)> = Vec::with_capacity(epochs.len());
        for (epoch, key) in epochs {
            per_epoch.push((
                Value::Integer(Integer::from(*epoch)),
                Value::Bytes(key.to_vec()),
            ));
        }
        streams.push((Value::Bytes(stream_id.to_vec()), Value::Map(per_epoch)));
    }

    let map = vec![
        (int(1), Value::Bytes(p.id_s_priv.to_vec())),
        (int(3), Value::Bytes(p.id_s_pub.to_vec())),
        (int(4), Value::Bytes(p.id_d_pub.to_vec())),
        (int(5), Value::Bytes(p.identity_id.to_vec())),
        (int(6), Value::Map(streams)),
        (int(7), Value::Text(p.nickname.clone())),
        (int(8), Value::Text(p.platform.clone())),
        (int(9), Value::Bytes(p.vault_root.to_vec())),
    ];
    let mut out = Vec::with_capacity(512);
    ciborium::ser::into_writer(&Value::Map(map), &mut out)
        .map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
    if out.len() > MAX_PAIRING_PAYLOAD {
        let len = out.len();
        out.zeroize();
        return Err(PairingPayloadError::TooLarge { len });
    }
    Ok(out)
}

/// Decode and validate a pairing payload.
///
/// The `identity_id` is recomputed from `ID_S_pub` and compared before the
/// value is handed back, so a caller cannot forget to.
///
/// # Errors
/// [`PairingPayloadError::Cbor`] for malformed CBOR,
/// [`PairingPayloadError::BadField`] for a missing or wrongly-shaped field, and
/// [`PairingPayloadError::IdentityMismatch`] when the id and the key disagree.
pub fn decode_pairing_payload(bytes: &[u8]) -> Result<PairingPayload, PairingPayloadError> {
    use ciborium::value::Value;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let value: Value =
        ciborium::de::from_reader(bytes).map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
    let Value::Map(map) = value else {
        return Err(PairingPayloadError::Cbor("payload must be a map".into()));
    };

    let mut id_s_priv: Option<[u8; 32]> = None;
    let mut id_s_pub: Option<[u8; 32]> = None;
    let mut id_d_pub: Option<[u8; 32]> = None;
    let mut identity_id: Option<[u8; 16]> = None;
    let mut vault_root: Option<[u8; 32]> = None;
    let mut stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> = BTreeMap::new();
    let mut nickname: Option<String> = None;
    let mut platform: Option<String> = None;

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            (1, Value::Bytes(b)) => id_s_priv = Some(arr32(&b, "id_s_priv")?),
            // Key 2 was `ID_D_priv` and is burned. A sender old enough to
            // still emit it is refused rather than tolerated: accepting the
            // field and dropping it would leave the operator believing a
            // revocation binds when the peer that paired still holds the key
            // that unbinds it. Falling through to the unknown-key arm below is
            // deliberate.
            (3, Value::Bytes(b)) => id_s_pub = Some(arr32(&b, "id_s_pub")?),
            (4, Value::Bytes(b)) => id_d_pub = Some(arr32(&b, "id_d_pub")?),
            (5, Value::Bytes(b)) => identity_id = Some(arr16(&b, "identity_id")?),
            (6, Value::Map(streams)) => {
                for (sid, per_epoch) in streams {
                    let Value::Bytes(sid) = sid else {
                        return Err(PairingPayloadError::BadField("stream_keys key"));
                    };
                    let Value::Map(per_epoch) = per_epoch else {
                        return Err(PairingPayloadError::BadField("stream_keys value"));
                    };
                    let sid = arr16(&sid, "stream_keys key")?;
                    let entry = stream_keys.entry(sid).or_default();
                    for (epoch, key) in per_epoch {
                        let Value::Integer(epoch) = epoch else {
                            return Err(PairingPayloadError::BadField("epoch"));
                        };
                        let Value::Bytes(key) = key else {
                            return Err(PairingPayloadError::BadField("stream key"));
                        };
                        let epoch = u32::try_from(i128::from(epoch))
                            .map_err(|_| PairingPayloadError::BadField("epoch range"))?;
                        entry.insert(epoch, arr32(&key, "stream key")?);
                    }
                }
            }
            (7, Value::Text(s)) => nickname = Some(s),
            (8, Value::Text(s)) => platform = Some(s),
            (9, Value::Bytes(b)) => vault_root = Some(arr32(&b, "vault_root")?),
            (id, _) if (1..=9).contains(&id) => {
                return Err(PairingPayloadError::BadField("field shape"));
            }
            // Forward-compat: a newer sender's extra fields are ignored, not
            // fatal. Nothing here is signed, so there is nothing to preserve.
            _ => {}
        }
    }

    let payload = PairingPayload {
        id_s_priv: id_s_priv.ok_or(PairingPayloadError::BadField("id_s_priv"))?,
        id_s_pub: id_s_pub.ok_or(PairingPayloadError::BadField("id_s_pub"))?,
        id_d_pub: id_d_pub.ok_or(PairingPayloadError::BadField("id_d_pub"))?,
        identity_id: identity_id.ok_or(PairingPayloadError::BadField("identity_id"))?,
        vault_root: vault_root.ok_or(PairingPayloadError::BadField("vault_root"))?,
        stream_keys,
        nickname: nickname.ok_or(PairingPayloadError::BadField("nickname"))?,
        platform: platform.ok_or(PairingPayloadError::BadField("platform"))?,
    };

    // The checks that cannot be skipped: every public half in the payload is a
    // derivation of a private half that is also in the payload, so each one is
    // recomputed rather than trusted.
    //
    // Both comparisons are constant-time, which is what
    // `docs/03-crypto/pairing-and-onboarding.md` §7 has always said they were
    // and what a plain `!=` on `[u8; 16]` is not. The timing signal is small —
    // the attacker here is on the far side of a SAS-confirmed Noise channel —
    // but a documented property that the code does not have is worse than
    // either having it or not claiming it, and the fix is one call.
    let id_ok = identity_id_from_pub(&payload.id_s_pub).ct_eq(&payload.identity_id);
    // `ID_D_pub` used to be checked against the `ID_D_priv` beside it. With the
    // private half gone there is nothing to recompute it from, and no check
    // here can establish that the public key is the account's: a receiver
    // holding only public material cannot tell a real `ID_D_pub` from any other
    // valid X25519 point. What stands behind it is the channel — Noise XX,
    // SAS-confirmed — which is the same thing that stands behind `ID_S_priv`
    // and the vault root in the same message. A wrong `ID_D_pub` here does not
    // expose anything; it produces epochs the recovery blob cannot open, which
    // is a recovery failure and is the reason `identity_id` is still checked
    // below.
    if !bool::from(id_ok) {
        return Err(PairingPayloadError::IdentityMismatch);
    }
    Ok(payload)
}

fn int(n: u8) -> ciborium::value::Value {
    ciborium::value::Value::Integer(ciborium::value::Integer::from(n))
}

fn arr32(b: &[u8], what: &'static str) -> Result<[u8; 32], PairingPayloadError> {
    b.try_into()
        .map_err(|_| PairingPayloadError::BadField(what))
}

fn arr16(b: &[u8], what: &'static str) -> Result<[u8; 16], PairingPayloadError> {
    b.try_into()
        .map_err(|_| PairingPayloadError::BadField(what))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};

    fn payload(streams: u32, epochs: u32) -> PairingPayload {
        let signing = IdentitySigningKeyPair::from_secret_bytes(&[0x21; 32]);
        let id_s_pub = signing.public_bytes();
        // `id_s_pub` is still recomputed from its private half by the decoder,
        // so that one has to be a real pair. `id_d_pub` no longer has a private
        // half in the payload to be checked against, but it is kept a real
        // X25519 point here rather than a constant: a fixture that is not a
        // valid point would pass this codec and fail the first time anything
        // sealed to it.
        let id_d_pub = IdentityDhKeyPair::from_secret_bytes([0x22; 32]).public_bytes();
        let mut stream_keys = BTreeMap::new();
        for s in 0..streams {
            let mut sid = [0u8; 16];
            sid[..4].copy_from_slice(&s.to_be_bytes());
            let mut per_epoch = BTreeMap::new();
            for e in 1..=epochs {
                per_epoch.insert(e, [u8::try_from(s % 251).unwrap_or(0); 32]);
            }
            stream_keys.insert(sid, per_epoch);
        }
        PairingPayload {
            id_s_priv: [0x21; 32],
            id_s_pub,
            id_d_pub,
            identity_id: identity_id_from_pub(&id_s_pub),
            vault_root: [0x25; 32],
            stream_keys,
            nickname: "a laptop".into(),
            platform: "macos".into(),
        }
    }

    #[test]
    fn round_trip() {
        let p = payload(3, 2);
        let bytes = encode_pairing_payload(&p).unwrap();
        let back = decode_pairing_payload(&bytes).unwrap();
        assert_eq!(back.id_s_priv, p.id_s_priv);
        assert_eq!(back.id_s_pub, p.id_s_pub);
        assert_eq!(back.id_d_pub, p.id_d_pub);
        assert_eq!(back.identity_id, p.identity_id);
        assert_eq!(back.vault_root, p.vault_root);
        assert_eq!(back.stream_keys, p.stream_keys);
        assert_eq!(back.nickname, p.nickname);
        assert_eq!(back.platform, p.platform);
        assert_eq!(back.key_count(), 6);
    }

    #[test]
    fn encoding_is_deterministic() {
        let p = payload(4, 3);
        assert_eq!(
            encode_pairing_payload(&p).unwrap(),
            encode_pairing_payload(&p).unwrap()
        );
    }

    #[test]
    fn a_rewritten_identity_id_is_refused() {
        let mut p = payload(1, 1);
        p.identity_id = [0xff; 16];
        let bytes = encode_pairing_payload(&p).unwrap();
        assert!(matches!(
            decode_pairing_payload(&bytes),
            Err(PairingPayloadError::IdentityMismatch)
        ));
    }

    #[test]
    fn a_missing_field_is_refused() {
        use ciborium::value::Value;
        // Everything but the vault root.
        let p = payload(1, 1);
        let full = encode_pairing_payload(&p).unwrap();
        let Value::Map(mut map) = ciborium::de::from_reader::<Value, _>(full.as_slice()).unwrap()
        else {
            unreachable!()
        };
        map.retain(|(k, _)| !matches!(k, Value::Integer(i) if i128::from(*i) == 9));
        let mut trimmed = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut trimmed).unwrap();
        assert!(matches!(
            decode_pairing_payload(&trimmed),
            Err(PairingPayloadError::BadField("vault_root"))
        ));
    }

    /// The limit is real and is surfaced, not truncated. A vault big enough to
    /// hit it would otherwise hand the new device a silently partial key set.
    #[test]
    fn an_oversized_payload_is_an_error_not_a_truncation() {
        let p = payload(2000, 1);
        match encode_pairing_payload(&p) {
            Err(PairingPayloadError::TooLarge { len }) => {
                assert!(len > MAX_PAIRING_PAYLOAD);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    /// ...and one just under the limit still encodes, so the bound is not so
    /// conservative that a realistic vault trips it.
    #[test]
    fn a_thousand_keys_still_fit() {
        let p = payload(500, 2);
        let bytes = encode_pairing_payload(&p).expect("1000 keys fit in one message");
        assert!(bytes.len() <= MAX_PAIRING_PAYLOAD);
        assert_eq!(decode_pairing_payload(&bytes).unwrap().key_count(), 1000);
    }

    /// `identity_id` is checked against `ID_S_pub`, and `ID_D_pub` is **not**
    /// checked at all — which is a statement about what a receiver can know,
    /// not an omission.
    ///
    /// This test used to assert both halves. It could, because the payload
    /// carried `ID_D_priv` and the decoder recomputed `ID_D_pub` from it. With
    /// the private half gone (`#76`) there is nothing to recompute from: a
    /// receiver holding only public material cannot distinguish the account's
    /// real `ID_D_pub` from any other valid X25519 point, and no check written
    /// here could. What stands behind the field is the Noise XX channel and the
    /// SAS both users read aloud, which is the same thing that stands behind
    /// `ID_S_priv` and the vault root in the same message.
    ///
    /// The consequence of a wrong `ID_D_pub` is bounded and is not disclosure:
    /// it seals epochs to a key the recovery blob cannot open, so the account
    /// becomes unrecoverable rather than readable by a stranger. That is why
    /// `identity_id` — which *is* checkable, and which decides whose account
    /// this device joins — keeps its constant-time check.
    #[test]
    fn a_payload_whose_identity_id_does_not_match_its_signing_key_is_refused() {
        let mut q = payload(2, 1);
        q.identity_id = [0x88; 16];
        let bytes = encode_pairing_payload(&q).unwrap();
        assert!(matches!(
            decode_pairing_payload(&bytes),
            Err(PairingPayloadError::IdentityMismatch)
        ));

        // And the honest payload still round-trips, so the check is not simply
        // refusing everything.
        let good = payload(2, 1);
        let bytes = encode_pairing_payload(&good).unwrap();
        assert_eq!(
            decode_pairing_payload(&bytes).unwrap().id_d_pub,
            good.id_d_pub
        );
    }

    /// A payload carrying the burned key 2 is refused, not tolerated.
    ///
    /// Key 2 was `ID_D_priv`. A sender still emitting it is a build from before
    /// revocation bounded reads, and pairing with it would produce a device
    /// holding the identity's unwrapping key — exactly the state `#76` is
    /// about. Silently ignoring the field would leave the operator believing a
    /// later revocation binds when the device it paired can still open every
    /// rotated epoch.
    #[test]
    fn a_payload_still_carrying_the_burned_id_d_priv_is_refused() {
        let good = payload(2, 1);
        let bytes = encode_pairing_payload(&good).unwrap();
        let mut v: ciborium::value::Value =
            ciborium::de::from_reader(bytes.as_slice()).expect("decode to a cbor map");
        let ciborium::value::Value::Map(entries) = &mut v else {
            panic!("a pairing payload is a map");
        };
        entries.push((int(2), ciborium::value::Value::Bytes(vec![0x22; 32])));
        let mut with_key_2 = Vec::new();
        ciborium::ser::into_writer(&v, &mut with_key_2).expect("re-encode");

        assert!(
            decode_pairing_payload(&with_key_2).is_err(),
            "a payload carrying ID_D_priv must not decode"
        );
    }

    #[test]
    fn debug_does_not_leak() {
        let p = payload(1, 1);
        let rendered = format!("{p:?}");
        assert!(!rendered.contains(&hex::encode(p.id_s_priv)));
        assert!(!rendered.contains(&hex::encode(p.vault_root)));
    }
}
