//! The `PairingPayload` — everything a device that has just been admitted needs
//! to open its vault, and nothing it could use to admit anybody else.
//!
//! This is not a wire message. It is what a completed three-message exchange
//! ([`crate::protocol`]) *assembles* on the joining device: the account's public
//! identity from the offer, the device keys the joiner minted for the request,
//! and the cert, vault root and Stream keys the grant carried. It has an
//! encoding because two seams need to carry it across a process or language
//! boundary — UniFFI's `paired_bundle`, and the CLI's pending-pairing file —
//! not because a peer ever sends it.
//!
//! ```cddl
//! PairingPayload = {
//!     ; 1 was ID_S_priv. Burned, never reused -- see below.
//!     ; 2 was ID_D_priv. Burned, never reused -- see below.
//!     3: bstr .size 32,   ; ID_S_pub
//!     4: bstr .size 32,   ; ID_D_pub
//!     5: bstr .size 16,   ; identity_id
//!     6: { * bstr .size 16 => { * uint => bstr .size 32 } },  ; stream_keys
//!     ; 7 was the sender's nickname, 8 its platform. Burned: the labels that
//!     ;   matter are the ones inside the cert, and two sources disagree.
//!     9: bstr .size 32,   ; vault_root
//!    10: bstr .size 16,   ; genesis_identity_id
//!    11: bstr .size 32,   ; genesis_id_s_pub
//!    12: bstr .size 32,   ; D_S_priv, minted by this device
//!    13: bstr .size 32,   ; D_D_priv, minted by this device
//!    14: bstr,            ; DeviceCert issued for this device under ID_S
//! }
//! ```
//!
//! # Why `ID_S_priv` does not travel (field 1, burned)
//!
//! It used to, and while it did every paired device held the account's signing
//! key. A `DeviceCert` carries no issuer field — `DeviceCertInner` names the
//! *subject* (`device_id`, `d_s_pub`, `d_d_pub`, `identity_id`) and its single
//! signature is the identity's — so any device holding `ID_S_priv` could mint a
//! genuinely valid cert for any device id it liked, including a fresh one it had
//! just invented. That is
//! [#105](https://github.com/justin13888/Sunrise/issues/105): a revoked device
//! minted a new `D_S`, derived a device id the revocation register had never
//! heard of, certified it, and rejoined.
//!
//! ADR-0037's identity rotation bounded that without closing it. The cert the
//! revoked device signed was valid under an identity the account had *retired*,
//! so the fresh id was admitted, current under nothing, and sealed no key. But
//! the device paired after the rotation held the new `ID_S_priv`, so revoking
//! *it* replayed the whole trick one identity along. Rotation made the bypass
//! cost one revocation each time rather than making it impossible.
//!
//! Withholding the key makes it impossible. The joiner mints `D_S`/`D_D`, asks
//! for a cert, and the sponsor — which does hold `ID_S_priv` — issues it. No
//! device admitted this way can produce a cert for anything, itself included,
//! ever, revoked or not. `sunrise_core`'s
//! `a_paired_device_cannot_issue_a_cert_for_a_fresh_device_id` is the test, and
//! it asserts the absence structurally: the paired keychain's `issue_cert_for`
//! returns `KeychainError::IdentitySigningKeyAbsent`, because the key is not
//! there to sign with. Not an intra-doc link: `sunrise-core` depends on this
//! crate, so naming its types here would invert the dependency.
//!
//! ## The two one-shot shapes that do not work
//!
//! Both were priced and both are worse than a second round trip:
//!
//! 1. **The sponsor mints the joiner's keypair.** One message again, and
//!    `ID_S_priv` still never travels. It also hands the sponsor permanent
//!    impersonation of every device it ever paired — and unlike `ID_S_priv`, no
//!    rotation touches `D_S`, so revoking the sponsor does not take the
//!    capability away. It is #105 in a new shape.
//! 2. **The joiner self-issues and is re-certified afterwards.** The joiner
//!    holds no valid cert during the window, so it cannot publish anything —
//!    including the request for the cert that would end the window.
//!
//! ## What this costs, stated plainly
//!
//! A device admitted by pairing cannot sponsor another one, and cannot rotate
//! the account identity, because both need `ID_S_priv`. The device that created
//! the account is the only one that can. `Command::RevokeDevice` from a paired
//! device therefore still cuts the revoked device off from every future epoch —
//! that is Stream-key rotation and needs no identity key — but cannot rotate the
//! *identity*, and says so in the log. That matters in exactly one case: revoking
//! the account's creator, which is the only remaining device that holds
//! `ID_S_priv` and so the only one that could still certify itself back in. See
//! `docs/03-crypto/key-rotation.md` §Revocation.
//!
//! # Why `ID_D_priv` does not travel either (field 2, burned)
//!
//! `ID_D_priv` is the X25519 scalar that opens the identity-sealed copy of every
//! `key_envelope`. Field 2 used to carry it, and that is what made revocation
//! unenforceable ([#76](https://github.com/justin13888/Sunrise/issues/76)): a
//! revoked device dropped from a new epoch's recipient list simply opened the
//! identity copy instead. Every device holding it meant no device could be
//! excluded from anything.
//!
//! A device gets the keys it needs two other ways: field 6 hands it every Stream
//! key the sponsor holds, which covers every epoch minted before it paired, and
//! it is a `Recipient::Device` on every epoch minted after — sealed to its own
//! `D_D_pub`, which a revocation can stop addressing. `ID_D_priv` stays on the
//! device that *created* the account and travels in the recovery blob behind the
//! BIP-39 code, which is what lets a recovery with no surviving device restore
//! readable content.
//!
//! Which device that is stopped being guesswork in `STORAGE_V` 19:
//! `identity.minted_by_device_id` records it at mint time, and `Keychain::load`
//! loads `ID_D_priv` only when the row names the vault reading it.
//!
//! Sealing needs only the public half, so field 4 (`ID_D_pub`) still travels and
//! a paired device can still mint epochs for the identity. Asymmetric
//! cryptography is doing real work here: the device can address the recovery
//! path without being able to walk it.
//!
//! # Why the genesis travels (fields 10 and 11)
//!
//! Since ADR-0032 an account's identity is a *chain*, not a value: a rotation
//! mints a successor and the old identity is retired. Every replica folds that
//! chain from a fixed point — `identity.genesis_identity_id`, with
//! `genesis_id_s_pub` beside it so the first link's signature can be checked —
//! because the identity *in force* moves and a chain read from a moving anchor
//! is not a chain.
//!
//! Fields 5 and 3 carry the identity in force, which is what the joining device
//! seals under and verifies its own cert against. They are not the genesis once
//! the account has rotated even once, and a device paired after a rotation used
//! to record the identity it happened to join at as its own anchor. Two replicas
//! of one account then folded from different starting points and disagreed about
//! who the account was — silently, because each was internally consistent.

use std::collections::BTreeMap;
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::handshake::MAX_NOISE_MESSAGE;

/// Largest encoded payload the Noise transport can carry in one message.
///
/// A Noise transport message is capped at 65535 bytes and costs a 16-byte
/// ChaCha20-Poly1305 tag, so this is the real ceiling on the plaintext.
pub const MAX_PAIRING_PAYLOAD: usize = MAX_NOISE_MESSAGE - 16;

/// Errors from the pairing codecs — the payload and all three wire messages.
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
    /// `genesis_identity_id` is not the id derived from `genesis_id_s_pub`.
    ///
    /// Checked for the same reason [`Self::IdentityMismatch`] is: both are
    /// values the sender chose, and a mismatched pair would anchor the
    /// receiver's identity-chain fold at a point no other replica of the
    /// account can reach.
    #[error("pairing payload genesis_identity_id does not match its genesis ID_S_pub")]
    GenesisMismatch,
    /// A pairing request's `device_id` is not the id derived from its
    /// `D_S_pub`.
    ///
    /// The joiner does not get to name its own id. Choosing it is how a device
    /// would claim an id the account's revocation register has no row for, or
    /// one that collides with a sibling's.
    #[error("pairing request device_id does not match its D_S_pub")]
    DeviceIdMismatch,
    /// The granted `DeviceCert` does not decode, does not verify under the
    /// identity the offer named, or names a different device.
    #[error("pairing grant certificate: {0}")]
    BadCert(&'static str),
    /// The encoded payload does not fit in one Noise transport message.
    ///
    /// Surfaced rather than silently truncated. Chunking it across several
    /// transport messages is the eventual answer — the channel is
    /// bidirectional and already carries multiple frames — but it needs a
    /// framing contract that reaches the Swift seam, where a sealed message is
    /// currently one base64 string. A vault this large is far past anything today
    /// produces, and a wrong answer here is a device that silently cannot read
    /// half its own content.
    #[error("pairing payload is {len} bytes, over the {MAX_PAIRING_PAYLOAD}-byte transport limit")]
    TooLarge {
        /// Encoded length that did not fit.
        len: usize,
    },
}

/// Everything a device admitted by pairing installs at its first open.
///
/// Zeroized on drop: it holds the vault root, this device's signing and DH
/// secrets, and every Stream key in the account. It does **not** hold
/// `ID_S_priv` or `ID_D_priv`; see the module docs for why neither travels and
/// what the first of those costs.
#[derive(Clone)]
pub struct PairingPayload {
    /// `ID_S_pub`. The account's signing key, public half only.
    pub id_s_pub: [u8; 32],
    /// `ID_D_pub`.
    pub id_d_pub: [u8; 32],
    /// The account's identity id — the one **in force**, which is what this
    /// device seals under and verifies its own cert against.
    pub identity_id: [u8; 16],
    /// The identity the account **started** as, which no rotation changes.
    ///
    /// The fold anchor (`identity.genesis_identity_id`, migration 0022). Equal
    /// to `identity_id` on an account that has never rotated, and deliberately
    /// carried anyway rather than inferred: "equal unless rotated" is a rule
    /// the receiver would have to know, and it has no way to tell.
    pub genesis_identity_id: [u8; 16],
    /// `ID_S_pub` of the genesis identity (migration 0023).
    ///
    /// The fold checks each link's `prev_sig` under the previous link's key,
    /// and the genesis key is in no transition row — it is nobody's successor.
    /// Without it a device paired into a rotated account could never verify a
    /// cert issued before the rotation it joined after.
    pub genesis_id_s_pub: [u8; 32],
    /// `D_S_priv`, minted **by this device** and never sent anywhere.
    ///
    /// Present here rather than minted inside `Keychain::create` because the
    /// two-message protocol forces it: the cert the sponsor issued names a
    /// specific `D_S_pub`, so the keychain has to adopt the key that cert was
    /// written for rather than mint a fresh one it would not match.
    pub d_s_priv: [u8; 32],
    /// `D_D_priv`, likewise minted by this device.
    pub d_d_priv: [u8; 32],
    /// The `DeviceCert` the sponsor issued for these keys, canonical CBOR.
    ///
    /// Signed by `ID_S_priv`, which this device does not hold. The device's
    /// nickname and platform are read back out of here rather than carried
    /// beside it: the cert is what the account signed, and a second copy of a
    /// label is a second thing that can disagree.
    pub device_cert: Vec<u8>,
    /// The vault root, which still keys the local database and wraps
    /// everything at rest.
    pub vault_root: [u8; 32],
    /// Every Stream key the sponsoring device held: `stream_id -> epoch -> key`.
    ///
    /// `BTreeMap` all the way down so the encoding is canonical without a sort
    /// step, and so two devices that assembled the same set produce the same
    /// bytes.
    pub stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>>,
}

impl Zeroize for PairingPayload {
    /// Hand-written because `BTreeMap` is not `Zeroize`: clearing the map frees
    /// its nodes without scrubbing them, so each key is zeroized in place
    /// first and only then dropped.
    fn zeroize(&mut self) {
        self.id_s_pub.zeroize();
        self.id_d_pub.zeroize();
        self.identity_id.zeroize();
        self.genesis_identity_id.zeroize();
        self.genesis_id_s_pub.zeroize();
        self.d_s_priv.zeroize();
        self.d_d_priv.zeroize();
        self.device_cert.zeroize();
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
        (int(3), Value::Bytes(p.id_s_pub.to_vec())),
        (int(4), Value::Bytes(p.id_d_pub.to_vec())),
        (int(5), Value::Bytes(p.identity_id.to_vec())),
        (int(6), Value::Map(streams)),
        (int(9), Value::Bytes(p.vault_root.to_vec())),
        (int(10), Value::Bytes(p.genesis_identity_id.to_vec())),
        (int(11), Value::Bytes(p.genesis_id_s_pub.to_vec())),
        (int(12), Value::Bytes(p.d_s_priv.to_vec())),
        (int(13), Value::Bytes(p.d_d_priv.to_vec())),
        (int(14), Value::Bytes(p.device_cert.clone())),
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
/// The `identity_id`s are recomputed from their public keys and compared before
/// the value is handed back, so a caller cannot forget to. What is *not*
/// re-checked here is the cert: verifying it needs nothing this function lacks,
/// but refusing it needs somewhere for the failure to go that is not "your vault
/// will not open", and `Keychain::create` is where that check belongs — it is the
/// only caller that can act on the answer.
///
/// # Errors
/// [`PairingPayloadError::Cbor`] for malformed CBOR,
/// [`PairingPayloadError::BadField`] for a missing or wrongly-shaped field, and
/// [`PairingPayloadError::IdentityMismatch`] / [`PairingPayloadError::GenesisMismatch`]
/// when an id and its key disagree.
pub fn decode_pairing_payload(bytes: &[u8]) -> Result<PairingPayload, PairingPayloadError> {
    use ciborium::value::Value;
    use subtle::ConstantTimeEq;
    use sunrise_crypto::identity_id_from_pub;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let value: Value =
        ciborium::de::from_reader(bytes).map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
    let Value::Map(map) = value else {
        return Err(PairingPayloadError::Cbor("payload must be a map".into()));
    };

    let mut id_s_pub: Option<[u8; 32]> = None;
    let mut id_d_pub: Option<[u8; 32]> = None;
    let mut identity_id: Option<[u8; 16]> = None;
    let mut genesis_identity_id: Option<[u8; 16]> = None;
    let mut genesis_id_s_pub: Option<[u8; 32]> = None;
    let mut vault_root: Option<[u8; 32]> = None;
    let mut d_s_priv: Option<[u8; 32]> = None;
    let mut d_d_priv: Option<[u8; 32]> = None;
    let mut device_cert: Option<Vec<u8>> = None;
    let mut stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> = BTreeMap::new();

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            // Keys 1 and 2 were `ID_S_priv` and `ID_D_priv`, and both are
            // burned. A sender old enough to still emit either is refused
            // rather than tolerated: accepting the field and dropping it would
            // leave the operator believing a revocation binds when the device
            // that paired still holds the key that unbinds it (key 2, `#76`),
            // or believing a revoked device cannot certify itself back in when
            // it still can (key 1, `#105`).
            //
            // Having no arm here is what refuses them, and the arm that catches
            // them is the **reserved-range** one -- `(1..=14)` -- not the
            // unknown-key `_` arm after it. That distinction is the whole
            // guard: 1 and 2 are inside the reserved range, so they error; the
            // `_` arm ignores what it catches, so if either reached it the
            // payload would decode with the burned key silently dropped.
            (3, Value::Bytes(b)) => id_s_pub = Some(arr32(&b, "id_s_pub")?),
            (4, Value::Bytes(b)) => id_d_pub = Some(arr32(&b, "id_d_pub")?),
            (5, Value::Bytes(b)) => identity_id = Some(arr16(&b, "identity_id")?),
            (6, Value::Map(streams)) => read_stream_keys(streams, &mut stream_keys)?,
            // 7 and 8 were the sending device's nickname and platform. Burned
            // for a duller reason than 1 and 2: the labels that matter now are
            // the ones the account signed into the cert, and a second copy
            // beside it is a second thing that can disagree with it.
            (9, Value::Bytes(b)) => vault_root = Some(arr32(&b, "vault_root")?),
            (10, Value::Bytes(b)) => {
                genesis_identity_id = Some(arr16(&b, "genesis_identity_id")?);
            }
            (11, Value::Bytes(b)) => {
                genesis_id_s_pub = Some(arr32(&b, "genesis_id_s_pub")?);
            }
            (12, Value::Bytes(b)) => d_s_priv = Some(arr32(&b, "d_s_priv")?),
            (13, Value::Bytes(b)) => d_d_priv = Some(arr32(&b, "d_d_priv")?),
            (14, Value::Bytes(b)) => device_cert = Some(b),
            // The reserved range: every field this version defines, plus the
            // four burned ones. A value in it that matched none of the arms
            // above is either the wrong CBOR shape for a field we know or a
            // burned key itself, and both are errors rather than things to
            // ignore.
            //
            // The upper bound moves when a field is added; the **lower** bound
            // and fields 1 and 2's membership do not. Shrinking the range past
            // them, or special-casing either into the ignore arm, makes a
            // payload carrying an identity private key decode successfully with
            // the key dropped, which is exactly the state `#76` and `#105` were
            // filed about. `a_payload_still_carrying_a_burned_identity_key_is_refused`
            // pins it.
            (id, _) if (1..=14).contains(&id) => {
                return Err(PairingPayloadError::BadField("field shape"));
            }
            // Forward-compat: a newer sender's extra fields are ignored, not
            // fatal. Nothing here is signed, so there is nothing to preserve.
            _ => {}
        }
    }

    let payload = PairingPayload {
        id_s_pub: id_s_pub.ok_or(PairingPayloadError::BadField("id_s_pub"))?,
        id_d_pub: id_d_pub.ok_or(PairingPayloadError::BadField("id_d_pub"))?,
        identity_id: identity_id.ok_or(PairingPayloadError::BadField("identity_id"))?,
        genesis_identity_id: genesis_identity_id
            .ok_or(PairingPayloadError::BadField("genesis_identity_id"))?,
        genesis_id_s_pub: genesis_id_s_pub
            .ok_or(PairingPayloadError::BadField("genesis_id_s_pub"))?,
        d_s_priv: d_s_priv.ok_or(PairingPayloadError::BadField("d_s_priv"))?,
        d_d_priv: d_d_priv.ok_or(PairingPayloadError::BadField("d_d_priv"))?,
        device_cert: device_cert.ok_or(PairingPayloadError::BadField("device_cert"))?,
        vault_root: vault_root.ok_or(PairingPayloadError::BadField("vault_root"))?,
        stream_keys,
    };

    // Both comparisons are constant-time, which is what
    // `docs/03-crypto/pairing-and-onboarding.md` §7 has always said they were
    // and what a plain `!=` on `[u8; 16]` is not. The timing signal is small —
    // this payload never crosses a network — but a documented property that the
    // code does not have is worse than either having it or not claiming it.
    let genesis_ok =
        identity_id_from_pub(&payload.genesis_id_s_pub).ct_eq(&payload.genesis_identity_id);
    if !bool::from(genesis_ok) {
        return Err(PairingPayloadError::GenesisMismatch);
    }
    // `ID_D_pub` is not checked against anything, and cannot be: a receiver
    // holding only public material cannot tell a real `ID_D_pub` from any other
    // valid X25519 point. What stands behind it is the channel the offer came
    // over. A wrong one does not expose anything; it produces epochs the
    // recovery blob cannot open, which is a recovery failure.
    if !bool::from(identity_id_from_pub(&payload.id_s_pub).ct_eq(&payload.identity_id)) {
        return Err(PairingPayloadError::IdentityMismatch);
    }
    Ok(payload)
}

pub(crate) fn int(n: u8) -> ciborium::value::Value {
    ciborium::value::Value::Integer(ciborium::value::Integer::from(n))
}

pub(crate) fn arr32(b: &[u8], what: &'static str) -> Result<[u8; 32], PairingPayloadError> {
    b.try_into()
        .map_err(|_| PairingPayloadError::BadField(what))
}

pub(crate) fn arr16(b: &[u8], what: &'static str) -> Result<[u8; 16], PairingPayloadError> {
    b.try_into()
        .map_err(|_| PairingPayloadError::BadField(what))
}

/// Read a `stream_id -> epoch -> key` CBOR map into `out`.
///
/// Shared by the payload and the grant, which carry the same map for the same
/// reason and must therefore reject the same malformed shapes.
pub(crate) fn read_stream_keys(
    streams: Vec<(ciborium::value::Value, ciborium::value::Value)>,
    out: &mut BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>>,
) -> Result<(), PairingPayloadError> {
    use ciborium::value::Value;
    for (sid, per_epoch) in streams {
        let Value::Bytes(sid) = sid else {
            return Err(PairingPayloadError::BadField("stream_keys key"));
        };
        let Value::Map(per_epoch) = per_epoch else {
            return Err(PairingPayloadError::BadField("stream_keys value"));
        };
        let sid = arr16(&sid, "stream_keys key")?;
        let entry = out.entry(sid).or_default();
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_crypto::identity_id_from_pub;
    use sunrise_crypto::keys::{
        DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair, IdentitySigningKeyPair,
    };

    fn payload(streams: u32, epochs: u32) -> PairingPayload {
        let signing = IdentitySigningKeyPair::from_secret_bytes(&[0x21; 32]);
        let id_s_pub = signing.public_bytes();
        // `id_s_pub` is still recomputed from `identity_id` by the decoder, so
        // that one has to be a real key. `id_d_pub` has no private half here to
        // be checked against, but it is kept a real X25519 point rather than a
        // constant: a fixture that is not a valid point would pass this codec
        // and fail the first time anything sealed to it.
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
            id_s_pub,
            id_d_pub,
            identity_id: identity_id_from_pub(&id_s_pub),
            // An account that has never rotated is its own genesis, which is
            // every account these unit tests build.
            genesis_identity_id: identity_id_from_pub(&id_s_pub),
            genesis_id_s_pub: id_s_pub,
            d_s_priv: DeviceSigningKeyPair::from_secret_bytes(&[0x31; 32]).secret_bytes(),
            d_d_priv: DeviceDhKeyPair::from_secret_bytes([0x32; 32]).secret_bytes(),
            device_cert: vec![0xa0; 48],
            vault_root: [0x25; 32],
            stream_keys,
        }
    }

    #[test]
    fn round_trip() {
        let p = payload(3, 2);
        let bytes = encode_pairing_payload(&p).unwrap();
        let back = decode_pairing_payload(&bytes).unwrap();
        assert_eq!(back.id_s_pub, p.id_s_pub);
        assert_eq!(back.id_d_pub, p.id_d_pub);
        assert_eq!(back.identity_id, p.identity_id);
        assert_eq!(back.vault_root, p.vault_root);
        assert_eq!(back.d_s_priv, p.d_s_priv);
        assert_eq!(back.d_d_priv, p.d_d_priv);
        assert_eq!(back.device_cert, p.device_cert);
        assert_eq!(back.stream_keys, p.stream_keys);
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
    fn a_rewritten_genesis_anchor_is_refused() {
        let mut p = payload(1, 1);
        p.genesis_identity_id = [0xff; 16];
        let bytes = encode_pairing_payload(&p).unwrap();
        assert!(matches!(
            decode_pairing_payload(&bytes),
            Err(PairingPayloadError::GenesisMismatch)
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

    /// A payload carrying either burned identity key is refused, not tolerated.
    ///
    /// Key 1 was `ID_S_priv` and key 2 was `ID_D_priv`. A sender still emitting
    /// one is a build from before the bound it removes: with key 2 a revoked
    /// device opens every rotated epoch (`#76`), and with key 1 it certifies
    /// itself back in under a fresh device id (`#105`). Silently ignoring the
    /// field would leave the operator believing a revocation binds when it does
    /// not.
    #[test]
    fn a_payload_still_carrying_a_burned_identity_key_is_refused() {
        for burned in [1u8, 2] {
            let good = payload(2, 1);
            let bytes = encode_pairing_payload(&good).unwrap();
            let mut v: ciborium::value::Value =
                ciborium::de::from_reader(bytes.as_slice()).expect("decode to a cbor map");
            let ciborium::value::Value::Map(entries) = &mut v else {
                panic!("a pairing payload is a map");
            };
            entries.push((int(burned), ciborium::value::Value::Bytes(vec![0x22; 32])));
            let mut with_burned = Vec::new();
            ciborium::ser::into_writer(&v, &mut with_burned).expect("re-encode");

            assert!(
                decode_pairing_payload(&with_burned).is_err(),
                "a payload carrying burned field {burned} must not decode"
            );
        }
    }

    /// The property this whole change exists for, asserted on the bytes.
    ///
    /// A struct with no `id_s_priv` field cannot encode one, but "the struct has
    /// no field" is a claim about today's source. This is a claim about the
    /// wire: whatever the encoder does, the account's signing secret is not in
    /// what comes out of it.
    #[test]
    fn no_encoded_payload_contains_the_account_signing_secret() {
        let signing = IdentitySigningKeyPair::from_secret_bytes(&[0x21; 32]);
        let secret = signing.secret_bytes();
        let bytes = encode_pairing_payload(&payload(3, 2)).unwrap();
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret),
            "ID_S_priv must not appear anywhere in an encoded pairing payload"
        );
    }

    #[test]
    fn debug_does_not_leak() {
        let p = payload(1, 1);
        let rendered = format!("{p:?}");
        assert!(!rendered.contains(&hex::encode(p.d_s_priv)));
        assert!(!rendered.contains(&hex::encode(p.vault_root)));
    }
}
