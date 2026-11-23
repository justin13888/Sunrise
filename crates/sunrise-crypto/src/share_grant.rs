//! `share_grant` — one Stream's key, sealed by its owner to another identity
//! (`docs/03-crypto/sharing-with-others.md`).
//!
//! This module is the cryptographic half of a share and nothing else: the
//! payload codec, the HPKE seal to the recipient identity, the owner's
//! signature, the expiry check, and the re-seal a Stream-key rotation emits.
//! The owner's account records a grant as a `ShareGrant` control op
//! (`sunrise-core`'s `engine::share_grant`); no client offers it yet.
//!
//! # Wire shape
//!
//! ```text
//! ShareGrantPayload = {
//!     1: bstr .size 16,  ; stream_id
//!     2: uint,           ; epoch
//!     3: bstr .size 16,  ; recipient_identity_id
//!     4: tstr,           ; role: "viewer" (the only role this build grants)
//!     5: uint?,          ; expires_at_ms
//!     6: bstr,           ; hpke_ct
//!     7: bstr .size 64,  ; identity_sig
//! }
//! ```
//!
//! Canonical CBOR; field 5 is absent, not null, when the grant never expires.
//!
//! - `hpke_ct` is HPKE Base to the recipient's `ID_D_pub` with
//!   `info = "sunrise.hpke.share_grant.v1" || stream_id || u32_be(epoch) ||
//!   recipient_identity_id` and `aad = granter_identity_id ||
//!   canonical_cbor({1..5})`. The plaintext is `canonical_cbor({1: stream_key,
//!   2: {1: name, 2: color}})`.
//! - `identity_sig` is `Ed25519(ID_S_priv, "sunrise.share_grant.v1" ||
//!   canonical_cbor({1..6}) || recipient_id_d_pub)`, by the granting identity.
//!   `recipient_id_d_pub` is the 32-byte `ID_D_pub` the ciphertext is sealed
//!   to. It is not on the wire: the recipient holds it as its own, and the
//!   owner's account carries it beside the grant in the op that records it.
//!
//! # What binds what
//!
//! - **The recipient and the key's coordinates.** `stream_id`, `epoch` and
//!   `recipient_identity_id` are in the HPKE `info`, so a ciphertext moved to
//!   another stream, epoch or recipient does not open.
//! - **The granter.** HPKE Base authenticates no sender, and the signature
//!   alone does not stop a third identity re-signing someone else's
//!   ciphertext as its own grant: the recipient would then hold the owner's
//!   key under the impostor's name. The granter's identity id is therefore in
//!   the AAD, so a ciphertext opens only under the identity whose signature
//!   the recipient checked.
//! - **The role and the expiry.** Fields 4 and 5 are in the AAD and under the
//!   signature, so neither can be changed without the grant failing to verify
//!   and to open.
//! - **The key it is sealed to.** HPKE Base cannot show anyone but the
//!   recipient which `ID_D_pub` a ciphertext was sealed to, so the signature
//!   covers it. The owner's account re-seals every later epoch to the key its
//!   grants name; were that key not signed, any device able to write the
//!   account's log could name its own key beside a grant the account signed,
//!   and receive every epoch re-sealed after it.
//!
//! # Expiry and revocation
//!
//! Expiry is the recipient's check, against its own clock: an expired grant
//! does not open ([`ShareGrantError::Expired`]). Revocation is a Stream-key
//! rotation: [`reseal_share_grants`] seals the new epoch to every recipient
//! still entitled to it and to nobody revoked or expired, so a revoked
//! recipient keeps the epochs it already held and receives no later one.

use crate::hpke_seal::{hpke_open_identity, hpke_seal, share_grant_info, HpkeError};
use crate::identity::identity_id_from_pub;
use crate::keys::{verify_ed25519, IdentityDhKeyPair, IdentitySigningKeyPair, StreamKey};
use ciborium::value::{Integer, Value};
use rand_core::CryptoRngCore;
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroize;

/// Domain prefix of the granting identity's signature input.
const SIG_DOMAIN: &[u8] = b"sunrise.share_grant.v1";

/// The longest name or color a grant's metadata may carry, in bytes. Far
/// above a Stream name's own bound; the point is only that a hostile grant
/// cannot make a reader allocate without bound.
pub const MAX_SHARE_METADATA_TEXT_BYTES: usize = 1024;

/// The longest encoded grant a reader decodes. A grant is a few hundred bytes
/// at the metadata bound.
pub const MAX_SHARE_GRANT_BYTES: usize = 8 * 1024;

/// The role a grant confers. This build grants only read access.
///
/// The role is a text field on the wire, so an `editor` grant is a value a
/// later build can add without a new field; this one refuses it rather than
/// reading it as something it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShareRole {
    /// Reads the Stream; every op the holder emits into it is dropped.
    Viewer,
}

impl ShareRole {
    /// The role's wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
        }
    }
}

/// The Stream's name and color, sealed with its key so a recipient sees a
/// meaningful entry before it has synced anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMetadata {
    /// The Stream's name.
    pub name: String,
    /// The Stream's color, as its wire spelling.
    pub color: String,
}

/// The cleartext fields of a grant: what the granter decides and anyone
/// holding the grant can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareGrantTerms {
    /// The Stream shared.
    pub stream_id: [u8; 16],
    /// The epoch of the key sealed.
    pub epoch: u32,
    /// The identity the grant is to.
    pub recipient_identity_id: [u8; 16],
    /// What the recipient may do.
    pub role: ShareRole,
    /// The recipient's clock time, in ms since the epoch, from which the grant
    /// no longer applies. `None` never expires.
    pub expires_at_ms: Option<u64>,
}

impl ShareGrantTerms {
    /// Whether the grant has expired at `now_ms`: `now_ms >= expires_at_ms`.
    #[must_use]
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| now_ms >= at)
    }

    fn fields(&self) -> Vec<(u64, Value)> {
        let mut fields = vec![
            (1, Value::Bytes(self.stream_id.to_vec())),
            (2, int(u64::from(self.epoch))),
            (3, Value::Bytes(self.recipient_identity_id.to_vec())),
            (4, Value::Text(self.role.as_str().to_owned())),
        ];
        if let Some(at) = self.expires_at_ms {
            fields.push((5, int(at)));
        }
        fields
    }
}

/// A signed share grant, as carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareGrant {
    terms: ShareGrantTerms,
    hpke_ct: Vec<u8>,
    identity_sig: [u8; 64],
}

/// What a recipient holds once a grant has opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenedShareGrant {
    /// The Stream key at the grant's epoch.
    pub key: StreamKey,
    /// The Stream's name and color as the granter sealed them.
    pub metadata: StreamMetadata,
}

/// One identity a Stream is shared with, as the owner's replica records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRecipient {
    /// The recipient identity.
    pub identity_id: [u8; 16],
    /// Its `ID_D_pub`, which the grant is sealed to.
    pub id_d_pub: [u8; 32],
    /// The expiry its grant carries, unchanged across rotations.
    pub expires_at_ms: Option<u64>,
}

/// Share grant errors. One variant per thing a caller does differently.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ShareGrantError {
    /// Not a share grant: the CBOR, a field's shape, or a bound.
    #[error("not a share grant: {0}")]
    Malformed(&'static str),
    /// A well-formed grant conferring a role this build does not grant.
    #[error("the share grant confers a role this build does not know")]
    UnsupportedRole,
    /// A well-formed grant to another identity.
    #[error("the share grant is to another identity")]
    OtherRecipient,
    /// The signature does not verify under the granter's `ID_S_pub`.
    #[error("the share grant's signature does not verify")]
    BadSignature,
    /// The grant expired at or before the recipient's clock.
    #[error("the share grant has expired")]
    Expired,
    /// The key did not seal or did not open: a recipient key that is not a
    /// key, a wrong `ID_D_priv`, a moved or altered ciphertext, or a plaintext
    /// that is not a key and its metadata.
    #[error("the share grant's key does not open")]
    Open,
    /// One identity is named twice among the recipients of a re-seal.
    #[error("a recipient is named twice")]
    DuplicateRecipient,
}

impl From<HpkeError> for ShareGrantError {
    fn from(_: HpkeError) -> Self {
        Self::Open
    }
}

fn int(n: u64) -> Value {
    Value::Integer(Integer::from(n))
}

fn encode(v: &Value) -> Result<Vec<u8>, ShareGrantError> {
    sunrise_cbor::encode_canonical(v).map_err(|_| ShareGrantError::Malformed("encode"))
}

fn map(fields: &[(u64, Value)]) -> Value {
    Value::Map(fields.iter().map(|(k, v)| (int(*k), v.clone())).collect())
}

/// `granter_identity_id || canonical({1..5})`: the HPKE AAD.
fn aad(
    granter_identity_id: &[u8; 16],
    terms: &ShareGrantTerms,
) -> Result<Vec<u8>, ShareGrantError> {
    let mut out = granter_identity_id.to_vec();
    out.extend_from_slice(&encode(&map(&terms.fields()))?);
    Ok(out)
}

/// `"sunrise.share_grant.v1" || canonical({1..6}) || recipient_id_d_pub`:
/// what the granter signs. The canonical map is self-delimiting, so the key
/// after it is unambiguous.
fn signed_message(
    terms: &ShareGrantTerms,
    hpke_ct: &[u8],
    recipient_id_d_pub: &[u8; 32],
) -> Result<Vec<u8>, ShareGrantError> {
    let mut fields = terms.fields();
    fields.push((6, Value::Bytes(hpke_ct.to_vec())));
    let mut out = SIG_DOMAIN.to_vec();
    out.extend_from_slice(&encode(&map(&fields))?);
    out.extend_from_slice(recipient_id_d_pub);
    Ok(out)
}

fn text_within(v: &Value) -> Option<String> {
    v.as_text()
        .filter(|t| t.len() <= MAX_SHARE_METADATA_TEXT_BYTES)
        .map(str::to_owned)
}

fn check_metadata(metadata: &StreamMetadata) -> Result<(), ShareGrantError> {
    if metadata.name.len() > MAX_SHARE_METADATA_TEXT_BYTES
        || metadata.color.len() > MAX_SHARE_METADATA_TEXT_BYTES
    {
        return Err(ShareGrantError::Malformed("metadata too long"));
    }
    Ok(())
}

/// Seal `key` and `metadata` to the recipient `terms` names, whose `ID_D_pub`
/// is `recipient_id_d_pub`, and sign the grant as `granter`.
///
/// # Errors
/// [`ShareGrantError::Malformed`] for metadata past its bound, and
/// [`ShareGrantError::Open`] for a recipient key that is not a key.
pub fn seal_share_grant<R: CryptoRngCore>(
    granter: &IdentitySigningKeyPair,
    terms: ShareGrantTerms,
    recipient_id_d_pub: &[u8; 32],
    key: &StreamKey,
    metadata: &StreamMetadata,
    rng: &mut R,
) -> Result<ShareGrant, ShareGrantError> {
    check_metadata(metadata)?;
    let granter_identity_id = identity_id_from_pub(&granter.public_bytes());
    let mut plain = encode(&map(&[
        (1, Value::Bytes(key.as_bytes().to_vec())),
        (
            2,
            map(&[
                (1, Value::Text(metadata.name.clone())),
                (2, Value::Text(metadata.color.clone())),
            ]),
        ),
    ]))?;
    let info = share_grant_info(&terms.stream_id, terms.epoch, &terms.recipient_identity_id);
    let sealed = hpke_seal(
        recipient_id_d_pub,
        &info,
        &plain,
        &aad(&granter_identity_id, &terms)?,
        rng,
    );
    plain.zeroize();
    let hpke_ct = sealed?;
    let identity_sig = granter.sign(&signed_message(&terms, &hpke_ct, recipient_id_d_pub)?);
    Ok(ShareGrant {
        terms,
        hpke_ct,
        identity_sig,
    })
}

/// The epoch a Stream-key rotation just minted, and what a grant of it
/// carries.
#[derive(Debug, Clone, Copy)]
pub struct SharedEpoch<'a> {
    /// The Stream rotated.
    pub stream_id: [u8; 16],
    /// The new epoch.
    pub epoch: u32,
    /// The new epoch's key.
    pub key: &'a StreamKey,
    /// The Stream's name and color.
    pub metadata: &'a StreamMetadata,
}

/// Seal `shared`, the epoch a Stream-key rotation just minted, to every
/// recipient still entitled to it, as `granter`.
///
/// A recipient is left out when its identity is in `revoked` or its grant has
/// expired at `now_ms`; every other one receives a grant at the new epoch
/// carrying its unchanged expiry. The grants come back ordered by recipient
/// identity, so one rotation's output does not depend on the order its
/// recipients were listed in.
///
/// # Errors
/// [`ShareGrantError::DuplicateRecipient`] where one identity is listed twice,
/// whichever keys the two entries carry; otherwise as [`seal_share_grant`].
pub fn reseal_share_grants<R: CryptoRngCore>(
    granter: &IdentitySigningKeyPair,
    shared: SharedEpoch<'_>,
    recipients: &[ShareRecipient],
    revoked: &[[u8; 16]],
    now_ms: u64,
    rng: &mut R,
) -> Result<Vec<ShareGrant>, ShareGrantError> {
    let mut ordered: Vec<&ShareRecipient> = recipients.iter().collect();
    ordered.sort_by_key(|r| r.identity_id);
    if ordered
        .windows(2)
        .any(|w| w[0].identity_id == w[1].identity_id)
    {
        return Err(ShareGrantError::DuplicateRecipient);
    }
    let mut grants = Vec::with_capacity(ordered.len());
    for recipient in ordered {
        if revoked.contains(&recipient.identity_id) {
            continue;
        }
        let terms = ShareGrantTerms {
            stream_id: shared.stream_id,
            epoch: shared.epoch,
            recipient_identity_id: recipient.identity_id,
            role: ShareRole::Viewer,
            expires_at_ms: recipient.expires_at_ms,
        };
        if terms.is_expired_at(now_ms) {
            continue;
        }
        grants.push(seal_share_grant(
            granter,
            terms,
            &recipient.id_d_pub,
            shared.key,
            shared.metadata,
            rng,
        )?);
    }
    Ok(grants)
}

fn fixed<const N: usize>(v: &Value) -> Option<[u8; N]> {
    v.as_bytes()?.as_slice().try_into().ok()
}

fn as_u64(v: &Value) -> Option<u64> {
    u64::try_from(v.as_integer()?).ok()
}

/// Read an integer-keyed map into `N` slots, refusing unknown and repeated
/// keys.
fn slots<const N: usize>(v: Value) -> Option<[Option<Value>; N]> {
    let Value::Map(entries) = v else {
        return None;
    };
    let mut out: [Option<Value>; N] = std::array::from_fn(|_| None);
    for (k, v) in entries {
        let slot = k
            .as_integer()
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| (1..=N).contains(i))?;
        if out[slot - 1].replace(v).is_some() {
            return None;
        }
    }
    Some(out)
}

/// Decode a grant: canonical CBOR and every field's shape and bound. Nothing
/// is verified yet.
///
/// # Errors
/// [`ShareGrantError::Malformed`], or [`ShareGrantError::UnsupportedRole`] for
/// a well-formed grant whose role is not `viewer`.
pub fn decode_share_grant(bytes: &[u8]) -> Result<ShareGrant, ShareGrantError> {
    if bytes.len() > MAX_SHARE_GRANT_BYTES {
        return Err(ShareGrantError::Malformed("too large"));
    }
    let value: Value = sunrise_cbor::decode_canonical(bytes)
        .map_err(|_| ShareGrantError::Malformed("not canonical CBOR"))?;
    let fields = slots::<7>(value).ok_or(ShareGrantError::Malformed("not a grant map"))?;
    let required = |i: usize| {
        fields[i - 1]
            .as_ref()
            .ok_or(ShareGrantError::Malformed("missing field"))
    };
    let stream_id = fixed(required(1)?).ok_or(ShareGrantError::Malformed("stream_id"))?;
    let epoch = as_u64(required(2)?)
        .and_then(|e| u32::try_from(e).ok())
        .ok_or(ShareGrantError::Malformed("epoch"))?;
    let recipient_identity_id =
        fixed(required(3)?).ok_or(ShareGrantError::Malformed("recipient_identity_id"))?;
    let role = match required(4)?.as_text() {
        Some("viewer") => ShareRole::Viewer,
        Some(_) => return Err(ShareGrantError::UnsupportedRole),
        None => return Err(ShareGrantError::Malformed("role")),
    };
    let expires_at_ms = match &fields[4] {
        None => None,
        Some(v) => Some(as_u64(v).ok_or(ShareGrantError::Malformed("expires_at_ms"))?),
    };
    let hpke_ct = required(6)?
        .as_bytes()
        .ok_or(ShareGrantError::Malformed("hpke_ct"))?
        .clone();
    let identity_sig = fixed(required(7)?).ok_or(ShareGrantError::Malformed("identity_sig"))?;
    Ok(ShareGrant {
        terms: ShareGrantTerms {
            stream_id,
            epoch,
            recipient_identity_id,
            role,
            expires_at_ms,
        },
        hpke_ct,
        identity_sig,
    })
}

impl ShareGrant {
    /// The cleartext fields.
    #[must_use]
    pub fn terms(&self) -> &ShareGrantTerms {
        &self.terms
    }

    /// The canonical encoding, fields 1 to 7.
    ///
    /// # Errors
    /// [`ShareGrantError::Malformed`] if encoding fails, which is structural
    /// rather than input-dependent.
    pub fn encode(&self) -> Result<Vec<u8>, ShareGrantError> {
        let mut fields = self.terms.fields();
        fields.push((6, Value::Bytes(self.hpke_ct.clone())));
        fields.push((7, Value::Bytes(self.identity_sig.to_vec())));
        encode(&map(&fields))
    }

    /// Check the granter's signature under `granter_id_s_pub`, for a grant
    /// sealed to `recipient_id_d_pub`: what a recipient does before it asks
    /// its user to accept, and what the owner's account does before it
    /// re-seals a later epoch to that key.
    ///
    /// # Errors
    /// [`ShareGrantError::BadSignature`], for a grant the granter did not
    /// sign or did not seal to `recipient_id_d_pub`.
    pub fn verify(
        &self,
        granter_id_s_pub: &[u8; 32],
        recipient_id_d_pub: &[u8; 32],
    ) -> Result<(), ShareGrantError> {
        let msg = signed_message(&self.terms, &self.hpke_ct, recipient_id_d_pub)?;
        if verify_ed25519(granter_id_s_pub, &msg, &self.identity_sig) {
            Ok(())
        } else {
            Err(ShareGrantError::BadSignature)
        }
    }

    /// Open the grant as `recipient_identity_id`, holding `recipient_id_d`, at
    /// the recipient's clock `now_ms`.
    ///
    /// The recipient is checked first and by itself, so a grant delivered to
    /// the wrong account is told apart from a forgery; then the signature,
    /// then the expiry, and only then is anything decrypted.
    ///
    /// # Errors
    /// [`ShareGrantError::OtherRecipient`], [`ShareGrantError::BadSignature`],
    /// [`ShareGrantError::Expired`], [`ShareGrantError::Open`].
    pub fn open(
        &self,
        granter_id_s_pub: &[u8; 32],
        recipient_identity_id: &[u8; 16],
        recipient_id_d: &IdentityDhKeyPair,
        now_ms: u64,
    ) -> Result<OpenedShareGrant, ShareGrantError> {
        let ours: bool = self
            .terms
            .recipient_identity_id
            .ct_eq(recipient_identity_id)
            .into();
        if !ours {
            return Err(ShareGrantError::OtherRecipient);
        }
        self.verify(granter_id_s_pub, &recipient_id_d.public_bytes())?;
        if self.terms.is_expired_at(now_ms) {
            return Err(ShareGrantError::Expired);
        }
        let terms = &self.terms;
        let info = share_grant_info(&terms.stream_id, terms.epoch, &terms.recipient_identity_id);
        let granter_identity_id = identity_id_from_pub(granter_id_s_pub);
        let mut plain = hpke_open_identity(
            recipient_id_d,
            &info,
            &self.hpke_ct,
            &aad(&granter_identity_id, terms)?,
        )?;
        let decoded: Result<Value, _> = sunrise_cbor::decode_canonical(&plain);
        plain.zeroize();
        let [key, metadata] = decoded
            .ok()
            .and_then(slots::<2>)
            .ok_or(ShareGrantError::Open)?;
        let (Some(key), Some(metadata)) = (key, metadata) else {
            return Err(ShareGrantError::Open);
        };
        let Some(mut key_bytes) = fixed::<32>(&key) else {
            return Err(ShareGrantError::Open);
        };
        let key = StreamKey::from_bytes(key_bytes);
        key_bytes.zeroize();
        let [name, color] = slots::<2>(metadata).ok_or(ShareGrantError::Open)?;
        let (Some(name), Some(color)) = (
            name.as_ref().and_then(text_within),
            color.as_ref().and_then(text_within),
        ) else {
            return Err(ShareGrantError::Open);
        };
        Ok(OpenedShareGrant {
            key,
            metadata: StreamMetadata { name, color },
        })
    }
}

#[cfg(test)]
mod tests;
