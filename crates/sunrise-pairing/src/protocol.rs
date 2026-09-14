//! The three-message pairing protocol: offer, request, grant.
//!
//! ```text
//!   sponsor (holds the vault)                joiner (has nothing)
//!   ------------------------------           --------------------------------
//!   1. PairingOffer            --------->    mints D_S / D_D
//!      account identity, public halves
//!      only; no secret of any kind
//!
//!                              <---------    2. PairingRequest
//!                                            device_id, D_S_pub, D_D_pub
//!
//!   3. PairingGrant            --------->    adopts the cert and the keys
//!      DeviceCert issued under ID_S,
//!      vault root, every Stream key
//! ```
//!
//! # Why three messages and not one
//!
//! Pairing used to be one message, and that message carried `ID_S_priv` — the
//! account's Ed25519 signing secret. It had to: a device that cannot sign under
//! the account identity cannot issue itself a `DeviceCert`, and without a cert
//! it is not a member of anything.
//!
//! The cost was [#105](https://github.com/justin13888/Sunrise/issues/105).
//! A `DeviceCert` names only its *subject* — `device_id`, `d_s_pub`, `d_d_pub`,
//! `identity_id` — and its single signature is the identity's, which on the old
//! protocol every paired device could produce. So a revoked device minted a
//! fresh `D_S`, derived a device id the revocation register had never heard of,
//! signed a cert for it that genuinely verified, and rejoined. Identity rotation
//! (ADR-0037) bounded that — the cert was valid under an identity the account
//! had retired — but only until the next revocation, when the device revoked
//! *that* time played the same trick under the new identity.
//!
//! Withholding the key closes it outright, and withholding it is what forces
//! three messages: **the sponsor cannot sign a cert for keys the joiner has not
//! minted yet.** The joiner must mint first and ask second. There is no one-shot
//! shape that works — see the module docs on [`crate::payload`] for the two that
//! were priced and rejected.
//!
//! # This is not ADR-0032's alternative 2
//!
//! ADR-0032 rejected *sponsor-countersigned* certs: a cert carrying a second
//! signature from the sponsoring device, which every verifier would have to
//! check. That binds a device's membership to its sponsor forever, so revoking
//! one device silently invalidates every device it ever paired — including, on a
//! chain of pairings, devices the user never associated with it.
//!
//! What this builds is sponsor-**issued**. The sponsor produces the signature
//! and then the signature is the *identity's*, exactly as it always was: byte
//! for byte the same `DeviceCert` shape, one signature, no issuer field, no
//! sponsor binding. A verifier cannot tell which device held `ID_S_priv` when
//! the cert was signed and does not need to. Revoking a sponsor therefore locks
//! nobody out. The sponsor is a gate at issue time and leaves no trace on the
//! artifact.
//!
//! # Transport-agnostic on purpose
//!
//! Nothing here knows whether a message crosses a Noise transport frame, a file
//! on disk, or a QR code. `sunrise-core-bindings` maps the three onto the
//! `PairedChannel` the Apple clients already drive; `sunrise-cli` maps them onto
//! three files. Both get the same bytes and the same checks.
//!
//! # What each side must check, and where
//!
//! Every field in every message is a value the *other* side chose, so each is
//! recomputed rather than believed:
//!
//! - [`decode_pairing_offer`] recomputes `identity_id` from `id_s_pub` and
//!   `genesis_identity_id` from `genesis_id_s_pub`, the same two checks the
//!   one-shot payload made.
//! - [`decode_pairing_request`] recomputes `device_id` from `d_s_pub`. A joiner
//!   that could name its own id would choose one the account's revocation
//!   register already excludes, or one that collides with a sibling's row.
//! - [`PairingGrant::accept`] is the joiner's check on the way back, and it is
//!   the one that matters: the cert must verify under the `ID_S_pub` the offer
//!   named, and must name *this* joiner's keys. A sponsor that issued a cert for
//!   somebody else's `D_S_pub` would hand the joiner a credential it cannot
//!   sign under, and the failure would not surface until the first op it tried
//!   to publish was refused by every peer.

use std::collections::BTreeMap;

use subtle::ConstantTimeEq;
use sunrise_crypto::{device_id_from_pub, identity_id_from_pub, DeviceCert};
use zeroize::Zeroize;

use crate::payload::{arr16, arr32, int, PairingPayload, PairingPayloadError, MAX_PAIRING_PAYLOAD};

/// Message 1: what the sponsor offers, before it has been asked for anything.
///
/// **Carries no secret.** Every field is a public half or a derivation of one,
/// which is the whole point: a joiner that abandons the pairing here, or an
/// attacker that intercepts this message, has learned nothing it could not have
/// learned from any op the account ever published. The vault root and the Stream
/// keys do not travel until the sponsor has seen and accepted a request.
///
/// It is still sent through the confirmed channel. Not because the contents are
/// secret but because the *binding* is: a joiner that accepted an offer from
/// somebody else would mint keys for a stranger's account and wait forever for a
/// grant that never comes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOffer {
    /// `ID_S_pub` of the identity **in force**, which is what the joiner's cert
    /// will be signed under and what it verifies that cert against.
    pub id_s_pub: [u8; 32],
    /// `ID_D_pub`. Sealing a `key_envelope` to the identity needs only this
    /// half; `ID_D_priv` has not travelled since `#76` and still does not.
    pub id_d_pub: [u8; 32],
    /// The identity id in force — `identity_id_from_pub(id_s_pub)`, recomputed
    /// by the decoder.
    pub identity_id: [u8; 16],
    /// The identity the account **started** as, which no rotation changes.
    ///
    /// The fold anchor (`identity.genesis_identity_id`). Equal to `identity_id`
    /// on an account that has never rotated and carried anyway: "equal unless
    /// rotated" is a rule the receiver has no way to evaluate.
    pub genesis_identity_id: [u8; 16],
    /// `ID_S_pub` of the genesis identity, so the first link of the chain — which
    /// is nobody's successor and so appears in no transition row — has a key its
    /// signature can be checked under.
    pub genesis_id_s_pub: [u8; 32],
    /// Nickname of the sponsoring device, for a screen that wants to say which
    /// device is admitting this one.
    pub nickname: String,
    /// Platform of the sponsoring device.
    pub platform: String,
}

/// Message 2: the joiner's cert request.
///
/// Public halves only, again — the joiner keeps `D_S_priv` and `D_D_priv` and
/// never sends them anywhere. That asymmetry is what distinguishes this from the
/// rejected alternative where the *sponsor* mints the joiner's keypair: a sponsor
/// that minted the joiner's `D_S` would hold permanent impersonation of it, and
/// unlike `ID_S_priv` no rotation touches `D_S`, so revoking the sponsor would
/// not take the capability away. That is #105 wearing a different hat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    /// `device_id_from_pub(d_s_pub)`, recomputed by the decoder.
    pub device_id: [u8; 16],
    /// `D_S_pub` — the Ed25519 key every op this device writes is signed under.
    pub d_s_pub: [u8; 32],
    /// `D_D_pub` — the X25519 key every `key_envelope` addressed to this device
    /// is sealed to, and the one a revocation stops addressing.
    pub d_d_pub: [u8; 32],
    /// The identity the offer named, echoed back.
    ///
    /// The sponsor refuses a request that answers a different account's offer.
    /// Without it a request captured from one pairing could be replayed at a
    /// second sponsor, which would issue a cert for a device it never spoke to.
    pub identity_id: [u8; 16],
    /// The nickname the joiner wants, which the sponsor writes into the cert.
    pub nickname: String,
    /// The joiner's platform, likewise.
    pub platform: String,
}

/// Message 3: the sponsor's grant.
///
/// This is the message that carries everything worth carrying: the issued cert,
/// the vault root, and every Stream key the sponsor holds. It zeroizes on drop
/// for the same reason the assembled [`PairingPayload`] does.
#[derive(Clone)]
pub struct PairingGrant {
    /// The `DeviceCert` the sponsor issued for the joiner, canonical CBOR.
    ///
    /// Signed by `ID_S_priv`, which the joiner does not have and will never
    /// have. One signature; no countersignature; no issuer field.
    pub device_cert: Vec<u8>,
    /// The vault root, which keys the local database and wraps everything at
    /// rest.
    pub vault_root: [u8; 32],
    /// Every Stream key the sponsoring device holds: `stream_id -> epoch -> key`.
    pub stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>>,
}

impl Zeroize for PairingGrant {
    /// Hand-written because neither `Vec<u8>`'s capacity nor `BTreeMap`'s nodes
    /// are scrubbed by dropping the container.
    fn zeroize(&mut self) {
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

impl Drop for PairingGrant {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl std::fmt::Debug for PairingGrant {
    /// Names the shape and nothing else, like [`PairingPayload`]'s.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingGrant")
            .field("streams", &self.stream_keys.len())
            .finish_non_exhaustive()
    }
}

impl PairingOffer {
    /// Encode to canonical, integer-keyed CBOR.
    ///
    /// ```cddl
    /// PairingOffer = {
    ///     1: bstr .size 32,   ; ID_S_pub
    ///     2: bstr .size 32,   ; ID_D_pub
    ///     3: bstr .size 16,   ; identity_id
    ///     4: bstr .size 16,   ; genesis_identity_id
    ///     5: bstr .size 32,   ; genesis ID_S_pub
    ///     6: tstr,            ; sponsor nickname
    ///     7: tstr,            ; sponsor platform
    /// }
    /// ```
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] on an encode failure.
    pub fn encode(&self) -> Result<Vec<u8>, PairingPayloadError> {
        use ciborium::value::Value;
        let map = vec![
            (int(1), Value::Bytes(self.id_s_pub.to_vec())),
            (int(2), Value::Bytes(self.id_d_pub.to_vec())),
            (int(3), Value::Bytes(self.identity_id.to_vec())),
            (int(4), Value::Bytes(self.genesis_identity_id.to_vec())),
            (int(5), Value::Bytes(self.genesis_id_s_pub.to_vec())),
            (int(6), Value::Text(self.nickname.clone())),
            (int(7), Value::Text(self.platform.clone())),
        ];
        let mut out = Vec::with_capacity(256);
        ciborium::ser::into_writer(&Value::Map(map), &mut out)
            .map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
        Ok(out)
    }
}

/// Decode and validate a [`PairingOffer`].
///
/// Both id/key pairs are recomputed before the value is handed back, so a caller
/// cannot forget to. Both comparisons are constant-time: the attacker is on the
/// far side of a SAS-confirmed channel and the signal is small, but a documented
/// property the code does not have is worse than not claiming it.
///
/// # Errors
/// [`PairingPayloadError::Cbor`] for malformed CBOR,
/// [`PairingPayloadError::BadField`] for a missing or wrongly-shaped field,
/// [`PairingPayloadError::IdentityMismatch`] and
/// [`PairingPayloadError::GenesisMismatch`] when an id and its key disagree.
pub fn decode_pairing_offer(bytes: &[u8]) -> Result<PairingOffer, PairingPayloadError> {
    use ciborium::value::Value;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let Value::Map(map) =
        ciborium::de::from_reader(bytes).map_err(|e| PairingPayloadError::Cbor(e.to_string()))?
    else {
        return Err(PairingPayloadError::Cbor("offer must be a map".into()));
    };

    let mut id_s_pub = None;
    let mut id_d_pub = None;
    let mut identity_id = None;
    let mut genesis_identity_id = None;
    let mut genesis_id_s_pub = None;
    let mut nickname = None;
    let mut platform = None;

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            (1, Value::Bytes(b)) => id_s_pub = Some(arr32(&b, "offer id_s_pub")?),
            (2, Value::Bytes(b)) => id_d_pub = Some(arr32(&b, "offer id_d_pub")?),
            (3, Value::Bytes(b)) => identity_id = Some(arr16(&b, "offer identity_id")?),
            (4, Value::Bytes(b)) => {
                genesis_identity_id = Some(arr16(&b, "offer genesis_identity_id")?);
            }
            (5, Value::Bytes(b)) => genesis_id_s_pub = Some(arr32(&b, "offer genesis_id_s_pub")?),
            (6, Value::Text(s)) => nickname = Some(s),
            (7, Value::Text(s)) => platform = Some(s),
            // The reserved range. A value in it that matched no arm above is a
            // field we know wearing the wrong CBOR type, which is an error
            // rather than something to skip. The `_` arm below is forward
            // compatibility and nothing else.
            (id, _) if (1..=7).contains(&id) => {
                return Err(PairingPayloadError::BadField("offer field shape"));
            }
            _ => {}
        }
    }

    let offer = PairingOffer {
        id_s_pub: id_s_pub.ok_or(PairingPayloadError::BadField("offer id_s_pub"))?,
        id_d_pub: id_d_pub.ok_or(PairingPayloadError::BadField("offer id_d_pub"))?,
        identity_id: identity_id.ok_or(PairingPayloadError::BadField("offer identity_id"))?,
        genesis_identity_id: genesis_identity_id
            .ok_or(PairingPayloadError::BadField("offer genesis_identity_id"))?,
        genesis_id_s_pub: genesis_id_s_pub
            .ok_or(PairingPayloadError::BadField("offer genesis_id_s_pub"))?,
        nickname: nickname.ok_or(PairingPayloadError::BadField("offer nickname"))?,
        platform: platform.ok_or(PairingPayloadError::BadField("offer platform"))?,
    };

    if !bool::from(identity_id_from_pub(&offer.genesis_id_s_pub).ct_eq(&offer.genesis_identity_id))
    {
        return Err(PairingPayloadError::GenesisMismatch);
    }
    if !bool::from(identity_id_from_pub(&offer.id_s_pub).ct_eq(&offer.identity_id)) {
        return Err(PairingPayloadError::IdentityMismatch);
    }
    Ok(offer)
}

impl PairingRequest {
    /// Encode to canonical, integer-keyed CBOR.
    ///
    /// ```cddl
    /// PairingRequest = {
    ///     1: bstr .size 16,   ; device_id
    ///     2: bstr .size 32,   ; D_S_pub
    ///     3: bstr .size 32,   ; D_D_pub
    ///     4: bstr .size 16,   ; identity_id the offer named
    ///     5: tstr,            ; nickname
    ///     6: tstr,            ; platform
    /// }
    /// ```
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] on an encode failure.
    pub fn encode(&self) -> Result<Vec<u8>, PairingPayloadError> {
        use ciborium::value::Value;
        let map = vec![
            (int(1), Value::Bytes(self.device_id.to_vec())),
            (int(2), Value::Bytes(self.d_s_pub.to_vec())),
            (int(3), Value::Bytes(self.d_d_pub.to_vec())),
            (int(4), Value::Bytes(self.identity_id.to_vec())),
            (int(5), Value::Text(self.nickname.clone())),
            (int(6), Value::Text(self.platform.clone())),
        ];
        let mut out = Vec::with_capacity(192);
        ciborium::ser::into_writer(&Value::Map(map), &mut out)
            .map_err(|e| PairingPayloadError::Cbor(e.to_string()))?;
        Ok(out)
    }
}

/// Decode and validate a [`PairingRequest`].
///
/// `device_id` is recomputed from `d_s_pub`. It is the one field a joiner could
/// otherwise choose freely, and choosing it is how a device would claim an id
/// the account has already revoked or one that collides with a sibling's row.
///
/// # Errors
/// [`PairingPayloadError::Cbor`], [`PairingPayloadError::BadField`], or
/// [`PairingPayloadError::DeviceIdMismatch`].
pub fn decode_pairing_request(bytes: &[u8]) -> Result<PairingRequest, PairingPayloadError> {
    use ciborium::value::Value;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let Value::Map(map) =
        ciborium::de::from_reader(bytes).map_err(|e| PairingPayloadError::Cbor(e.to_string()))?
    else {
        return Err(PairingPayloadError::Cbor("request must be a map".into()));
    };

    let mut device_id = None;
    let mut d_s_pub = None;
    let mut d_d_pub = None;
    let mut identity_id = None;
    let mut nickname = None;
    let mut platform = None;

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            (1, Value::Bytes(b)) => device_id = Some(arr16(&b, "request device_id")?),
            (2, Value::Bytes(b)) => d_s_pub = Some(arr32(&b, "request d_s_pub")?),
            (3, Value::Bytes(b)) => d_d_pub = Some(arr32(&b, "request d_d_pub")?),
            (4, Value::Bytes(b)) => identity_id = Some(arr16(&b, "request identity_id")?),
            (5, Value::Text(s)) => nickname = Some(s),
            (6, Value::Text(s)) => platform = Some(s),
            (id, _) if (1..=6).contains(&id) => {
                return Err(PairingPayloadError::BadField("request field shape"));
            }
            _ => {}
        }
    }

    let request = PairingRequest {
        device_id: device_id.ok_or(PairingPayloadError::BadField("request device_id"))?,
        d_s_pub: d_s_pub.ok_or(PairingPayloadError::BadField("request d_s_pub"))?,
        d_d_pub: d_d_pub.ok_or(PairingPayloadError::BadField("request d_d_pub"))?,
        identity_id: identity_id.ok_or(PairingPayloadError::BadField("request identity_id"))?,
        nickname: nickname.ok_or(PairingPayloadError::BadField("request nickname"))?,
        platform: platform.ok_or(PairingPayloadError::BadField("request platform"))?,
    };
    if !bool::from(device_id_from_pub(&request.d_s_pub).ct_eq(&request.device_id)) {
        return Err(PairingPayloadError::DeviceIdMismatch);
    }
    Ok(request)
}

impl PairingGrant {
    /// Encode to canonical, integer-keyed CBOR.
    ///
    /// ```cddl
    /// PairingGrant = {
    ///     1: bstr,            ; DeviceCert, canonical CBOR
    ///     2: bstr .size 32,   ; vault_root
    ///     3: { * bstr .size 16 => { * uint => bstr .size 32 } },  ; stream_keys
    /// }
    /// ```
    ///
    /// # Errors
    /// [`PairingPayloadError::Cbor`] on an encode failure, or
    /// [`PairingPayloadError::TooLarge`] if the result will not fit in one Noise
    /// transport message. Surfaced rather than truncated: a truncated grant is a
    /// device that silently cannot read half its own account.
    pub fn encode(&self) -> Result<Vec<u8>, PairingPayloadError> {
        use ciborium::value::{Integer, Value};
        let mut streams: Vec<(Value, Value)> = Vec::with_capacity(self.stream_keys.len());
        for (stream_id, epochs) in &self.stream_keys {
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
            (int(1), Value::Bytes(self.device_cert.clone())),
            (int(2), Value::Bytes(self.vault_root.to_vec())),
            (int(3), Value::Map(streams)),
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

    /// Total `(stream, epoch)` pairs carried.
    #[must_use]
    pub fn key_count(&self) -> usize {
        self.stream_keys.values().map(BTreeMap::len).sum()
    }

    /// Turn an accepted grant into the material [`PairingPayload`] carries,
    /// checking that the cert is one this device can actually use.
    ///
    /// This is the joiner's only opportunity to refuse. Everything it checks is
    /// something a hostile or merely broken sponsor could get wrong, and each
    /// failure would otherwise surface much later as a device whose ops every
    /// peer silently rejects:
    ///
    /// - the cert parses, and **verifies under the `ID_S_pub` the offer named**.
    ///   Not under the cert's own claim of an identity: `verify_binding`
    ///   recomputes `identity_id` from the key and compares it to the one the
    ///   offer committed to, so a cert signed by a well-formed identity that is
    ///   not this account's is refused.
    /// - the cert names *this* joiner's `d_s_pub`, `d_d_pub` and derived
    ///   `device_id`. A sponsor that certified a different device's keys would
    ///   hand over a credential the joiner holds no signing key for.
    ///
    /// `d_s_priv` and `d_d_priv` are the secrets the joiner minted for
    /// [`PairingRequest`] and never sent.
    ///
    /// # Errors
    /// [`PairingPayloadError::BadCert`] when the cert does not parse, does not
    /// verify under the offer's identity, or names keys other than these.
    pub fn accept(
        self,
        offer: &PairingOffer,
        d_s_priv: [u8; 32],
        d_d_priv: [u8; 32],
        request: &PairingRequest,
    ) -> Result<PairingPayload, PairingPayloadError> {
        let cert = DeviceCert::from_cbor(&self.device_cert)
            .map_err(|_| PairingPayloadError::BadCert("granted cert does not decode"))?;
        cert.verify_binding(&offer.id_s_pub, &offer.identity_id)
            .map_err(|_| {
                PairingPayloadError::BadCert("granted cert is not signed by this account identity")
            })?;
        // Constant-time, like every other comparison on this path, and for the
        // same reason: these run on bytes the peer chose.
        let subject_ok = cert.body.d_s_pub.ct_eq(&request.d_s_pub)
            & cert.body.d_d_pub.ct_eq(&request.d_d_pub)
            & cert.body.device_id.ct_eq(&request.device_id);
        if !bool::from(subject_ok) {
            return Err(PairingPayloadError::BadCert(
                "granted cert names a different device",
            ));
        }
        // Moved out field by field so `PairingGrant`'s `Drop` does not scrub
        // values that are now the payload's.
        let mut grant = self;
        let device_cert = std::mem::take(&mut grant.device_cert);
        let vault_root = grant.vault_root;
        let stream_keys = std::mem::take(&mut grant.stream_keys);
        grant.vault_root.zeroize();
        Ok(PairingPayload {
            id_s_pub: offer.id_s_pub,
            id_d_pub: offer.id_d_pub,
            identity_id: offer.identity_id,
            genesis_identity_id: offer.genesis_identity_id,
            genesis_id_s_pub: offer.genesis_id_s_pub,
            d_s_priv,
            d_d_priv,
            device_cert,
            vault_root,
            stream_keys,
        })
    }
}

/// Decode a [`PairingGrant`].
///
/// Nothing here can be validated in isolation — a cert means nothing without the
/// offer's `ID_S_pub` and the request's keys to check it against — so the
/// checking lives in [`PairingGrant::accept`], which has all three.
///
/// # Errors
/// [`PairingPayloadError::Cbor`], [`PairingPayloadError::BadField`], or
/// [`PairingPayloadError::TooLarge`].
pub fn decode_pairing_grant(bytes: &[u8]) -> Result<PairingGrant, PairingPayloadError> {
    use ciborium::value::Value;

    if bytes.len() > MAX_PAIRING_PAYLOAD {
        return Err(PairingPayloadError::TooLarge { len: bytes.len() });
    }
    let Value::Map(map) =
        ciborium::de::from_reader(bytes).map_err(|e| PairingPayloadError::Cbor(e.to_string()))?
    else {
        return Err(PairingPayloadError::Cbor("grant must be a map".into()));
    };

    let mut device_cert: Option<Vec<u8>> = None;
    let mut vault_root: Option<[u8; 32]> = None;
    let mut stream_keys: BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>> = BTreeMap::new();

    for (k, v) in map {
        let Value::Integer(i) = k else {
            return Err(PairingPayloadError::Cbor("non-int key".into()));
        };
        match (i128::from(i), v) {
            (1, Value::Bytes(b)) => device_cert = Some(b),
            (2, Value::Bytes(b)) => vault_root = Some(arr32(&b, "grant vault_root")?),
            (3, Value::Map(streams)) => {
                crate::payload::read_stream_keys(streams, &mut stream_keys)?;
            }
            (id, _) if (1..=3).contains(&id) => {
                return Err(PairingPayloadError::BadField("grant field shape"));
            }
            _ => {}
        }
    }

    Ok(PairingGrant {
        device_cert: device_cert.ok_or(PairingPayloadError::BadField("grant device_cert"))?,
        vault_root: vault_root.ok_or(PairingPayloadError::BadField("grant vault_root"))?,
        stream_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_crypto::keys::{
        DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair, IdentitySigningKeyPair,
    };
    use sunrise_crypto::DeviceCertInner;

    fn sponsor_identity() -> IdentitySigningKeyPair {
        IdentitySigningKeyPair::from_secret_bytes(&[0x21; 32])
    }

    fn offer() -> PairingOffer {
        let signing = sponsor_identity();
        let id_s_pub = signing.public_bytes();
        PairingOffer {
            id_s_pub,
            id_d_pub: IdentityDhKeyPair::from_secret_bytes([0x22; 32]).public_bytes(),
            identity_id: identity_id_from_pub(&id_s_pub),
            genesis_identity_id: identity_id_from_pub(&id_s_pub),
            genesis_id_s_pub: id_s_pub,
            nickname: "a laptop".into(),
            platform: "macos".into(),
        }
    }

    fn joiner() -> (DeviceSigningKeyPair, DeviceDhKeyPair, PairingRequest) {
        let d_s = DeviceSigningKeyPair::from_secret_bytes(&[0x31; 32]);
        let d_d = DeviceDhKeyPair::from_secret_bytes([0x32; 32]);
        let request = PairingRequest {
            device_id: device_id_from_pub(&d_s.public_bytes()),
            d_s_pub: d_s.public_bytes(),
            d_d_pub: d_d.public_bytes(),
            identity_id: offer().identity_id,
            nickname: "a phone".into(),
            platform: "ios".into(),
        };
        (d_s, d_d, request)
    }

    fn grant_for(request: &PairingRequest, signing: &IdentitySigningKeyPair) -> PairingGrant {
        let body = DeviceCertInner {
            v: 1,
            device_id: request.device_id,
            d_s_pub: request.d_s_pub,
            d_d_pub: request.d_d_pub,
            identity_id: identity_id_from_pub(&signing.public_bytes()),
            created_at_ms: 1_700_000_000_000,
            nickname: request.nickname.clone(),
            platform: request.platform.clone(),
        };
        let mut stream_keys = BTreeMap::new();
        stream_keys.insert([7u8; 16], BTreeMap::from([(1u32, [0x44u8; 32])]));
        PairingGrant {
            device_cert: DeviceCert::issue(body, signing).unwrap().to_cbor().unwrap(),
            vault_root: [0x25; 32],
            stream_keys,
        }
    }

    #[test]
    fn the_offer_round_trips() {
        let o = offer();
        let back = decode_pairing_offer(&o.encode().unwrap()).unwrap();
        assert_eq!(back, o);
    }

    /// The offer is the one message that carries nothing worth stealing, and
    /// that is a property rather than an accident. Asserted on the *bytes*,
    /// because a field added later that happened to be a secret would still
    /// encode fine and this is what would catch it.
    #[test]
    fn the_offer_carries_no_secret() {
        let signing = sponsor_identity();
        let bytes = offer().encode().unwrap();
        let windows_contain = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(
            !windows_contain(&signing.secret_bytes()),
            "ID_S_priv must not be anywhere in a pairing offer"
        );
        assert!(
            !windows_contain(&[0x22u8; 32]),
            "ID_D_priv must not be anywhere in a pairing offer"
        );
    }

    #[test]
    fn an_offer_whose_identity_id_is_rewritten_is_refused() {
        let mut o = offer();
        o.identity_id = [0xff; 16];
        assert!(matches!(
            decode_pairing_offer(&o.encode().unwrap()),
            Err(PairingPayloadError::IdentityMismatch)
        ));
    }

    #[test]
    fn an_offer_whose_genesis_anchor_is_rewritten_is_refused() {
        let mut o = offer();
        o.genesis_identity_id = [0xff; 16];
        assert!(matches!(
            decode_pairing_offer(&o.encode().unwrap()),
            Err(PairingPayloadError::GenesisMismatch)
        ));
    }

    #[test]
    fn the_request_round_trips() {
        let (_, _, r) = joiner();
        assert_eq!(decode_pairing_request(&r.encode().unwrap()).unwrap(), r);
    }

    /// A joiner does not get to name its own device id. Choosing it is how a
    /// device would claim an id the revocation register has no row for, or one
    /// that collides with a sibling.
    #[test]
    fn a_request_naming_a_device_id_its_key_does_not_derive_is_refused() {
        let (_, _, mut r) = joiner();
        r.device_id = [0x99; 16];
        assert!(matches!(
            decode_pairing_request(&r.encode().unwrap()),
            Err(PairingPayloadError::DeviceIdMismatch)
        ));
    }

    #[test]
    fn the_grant_round_trips_and_accepts() {
        let (d_s, d_d, request) = joiner();
        let g = grant_for(&request, &sponsor_identity());
        let bytes = g.encode().unwrap();
        let back = decode_pairing_grant(&bytes).unwrap();
        assert_eq!(back.key_count(), 1);
        let payload = back
            .accept(&offer(), d_s.secret_bytes(), d_d.secret_bytes(), &request)
            .expect("an honest grant is accepted");
        assert_eq!(payload.vault_root, [0x25; 32]);
        assert_eq!(payload.d_s_priv, d_s.secret_bytes());
    }

    /// The check that makes a second sponsor useless. A request replayed at a
    /// stranger's vault gets a cert signed by *that* account's identity, and the
    /// joiner refuses it because the offer it holds names a different key.
    #[test]
    fn a_cert_signed_by_another_identity_is_refused() {
        let (d_s, d_d, request) = joiner();
        let stranger = IdentitySigningKeyPair::from_secret_bytes(&[0x99; 32]);
        let g = grant_for(&request, &stranger);
        let err = g
            .accept(&offer(), d_s.secret_bytes(), d_d.secret_bytes(), &request)
            .expect_err("a cert from another identity must be refused");
        assert!(matches!(err, PairingPayloadError::BadCert(_)));
    }

    /// A sponsor that certifies somebody else's keys hands over a credential
    /// this device cannot sign under. Caught here rather than at the first
    /// rejected op.
    #[test]
    fn a_cert_naming_another_device_is_refused() {
        let (d_s, d_d, request) = joiner();
        let other = DeviceSigningKeyPair::from_secret_bytes(&[0x51; 32]);
        let mut wrong = request.clone();
        wrong.d_s_pub = other.public_bytes();
        wrong.device_id = device_id_from_pub(&other.public_bytes());
        let g = grant_for(&wrong, &sponsor_identity());
        let err = g
            .accept(&offer(), d_s.secret_bytes(), d_d.secret_bytes(), &request)
            .expect_err("a cert for another device must be refused");
        assert!(matches!(err, PairingPayloadError::BadCert(_)));
    }

    #[test]
    fn a_grant_missing_its_cert_is_refused() {
        use ciborium::value::Value;
        let (_, _, request) = joiner();
        let bytes = grant_for(&request, &sponsor_identity()).encode().unwrap();
        let Value::Map(mut map) = ciborium::de::from_reader::<Value, _>(bytes.as_slice()).unwrap()
        else {
            unreachable!()
        };
        map.retain(|(k, _)| !matches!(k, Value::Integer(i) if i128::from(*i) == 1));
        let mut trimmed = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut trimmed).unwrap();
        assert!(matches!(
            decode_pairing_grant(&trimmed),
            Err(PairingPayloadError::BadField("grant device_cert"))
        ));
    }

    #[test]
    fn grant_debug_does_not_leak() {
        let (_, _, request) = joiner();
        let g = grant_for(&request, &sponsor_identity());
        let rendered = format!("{g:?}");
        assert!(!rendered.contains(&hex::encode(g.vault_root)));
    }
}
