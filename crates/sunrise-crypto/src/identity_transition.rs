//! `identity_transition` — the signed hand-over from one account identity to
//! its successor.
//!
//! Per `docs/03-crypto/key-rotation.md` §Identity rotation and ADR-0032. The
//! account's `ID_S`/`ID_D` pair is replaced; every device's own `D_S`/`D_D` is
//! **not**, because those bind to `device_id` rather than to the identity.
//! What has to travel with the new identity is therefore three things, and the
//! signed body below is a commitment to all three at once:
//!
//! 1. the successor's public halves (`to_id_s_pub`, `to_id_d_pub`);
//! 2. the **roster** — one fresh [`DeviceCert`] per surviving device, re-issued
//!    under the new `ID_S_priv`, so a peer that trusts the successor can still
//!    verify every device's envelopes;
//! 3. the **shares** — the successor's secret halves, sealed by HPKE to each
//!    surviving device's `D_D_pub`, plus an optional carry-forward copy sealed
//!    to the **outgoing** `ID_D_pub` so a holder of the old recovery blob is
//!    not stranded.
//!
//! The roster and the shares are large and are carried beside the body rather
//! than inside it; the body commits to them by BLAKE3 digest, which is what
//! makes "the signature covers the whole transition" true without putting
//! kilobytes through the signer. See [`roster_digest`] and [`shares_digest`]
//! for why both are order-normalized and length-checked.
//!
//! # `identity_id` changes, and that is the design
//!
//! `docs/03-crypto/key-rotation.md` draws the op with the identity id
//! *unchanged* across a rotation. That cannot hold: `identity_id` is
//! [`identity_id_from_pub`] of `ID_S_pub`, a derivation and not a field, so a
//! new `ID_S` is a new id by construction — and a body that asserted otherwise
//! would be a claim no verifier could check. The body therefore carries both
//! ids, `from` and `to`, and [`verify_identity_transition`] recomputes each one
//! from the key it belongs to rather than believing either.
//!
//! # Two signatures, in one order
//!
//! ```text
//! body_hash = BLAKE3(canonical_cbor(BODY))
//! prev_sig  = Ed25519(OLD ID_S_priv, "sunrise.identity_transition.v1"      || body_hash)
//! next_sig  = Ed25519(NEW ID_S_priv, "sunrise.identity_transition.succ.v1" || body_hash || prev_sig)
//! ```
//!
//! `prev_sig` is the outgoing identity *authorizing* the hand-over; `next_sig`
//! is the successor *accepting* it. `next_sig` covers `prev_sig` as well as the
//! body, so the pair cannot be split: a successor signature lifted from one
//! transition does not fit another transition of the same body signed by a
//! different predecessor. The domain strings differ for the same reason a cert
//! and an envelope have different ones — one signature must never be readable
//! as the other.

use crate::device_cert::DeviceCert;
use crate::hpke_seal::{HPKE_ENC_LEN, HPKE_TAG_LEN};
use crate::identity::identity_id_from_pub;
use crate::keys::{verify_ed25519, IdentitySigningKeyPair};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use thiserror::Error;

/// Domain separator for the outgoing identity's signature.
const PREV_DOMAIN: &[u8] = b"sunrise.identity_transition.v1";

/// Domain separator for the successor identity's signature.
const SUCC_DOMAIN: &[u8] = b"sunrise.identity_transition.succ.v1";

/// Domain tag for [`roster_digest`].
///
/// Distinct from [`SHARES_DOMAIN`], and present at all because both digests
/// used to begin `blake3::Hasher::new()` — an unkeyed space they shared with
/// `body_hash`, the envelope hash and every other bare BLAKE3 in the
/// workspace. `merkle.rs` already does this; these two were the exception.
const ROSTER_DOMAIN: &[u8] = b"sunrise.identity_roster.v1";

/// Domain tag for [`shares_digest`].
const SHARES_DOMAIN: &[u8] = b"sunrise.identity_shares.v1";

/// Length of a per-device share: `enc(32) || ct(32) || tag(16)`.
///
/// The plaintext is one 32-byte secret, so the ciphertext length is fixed. That
/// is not a convenience — see [`shares_digest`], where it is what makes the
/// concatenation unambiguous.
pub const DEVICE_SHARE_LEN: usize = HPKE_ENC_LEN + 32 + HPKE_TAG_LEN;

/// Length of the carry-forward identity share: `enc(32) || ct(64) || tag(16)`.
///
/// The plaintext is `ID_S_priv || ID_D_priv`, 64 bytes, so this is fixed too.
pub const IDENTITY_SHARE_LEN: usize = HPKE_ENC_LEN + 64 + HPKE_TAG_LEN;

/// Errors produced by the `identity_transition` primitives.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentityTransitionError {
    /// Canonical-CBOR encoding of the body failed.
    #[error("identity transition CBOR error: {0}")]
    Cbor(String),
    /// A roster entry is not a decodable [`DeviceCert`].
    #[error("roster entry {index} is not a decodable device cert")]
    RosterCert {
        /// Position of the offending entry in the caller's slice.
        index: usize,
    },
    /// Two roster entries name the same device.
    ///
    /// A roster is the *membership* the successor identity certifies, so a
    /// duplicated device id has no defined meaning: the two certs disagree, and
    /// nothing downstream could say which one is the device's cert.
    #[error("roster names device {0} twice")]
    RosterDuplicate(String),
    /// The roster, or one cert in it, does not fit its `u32` length prefix.
    ///
    /// Unreachable from anything this workspace produces — a roster is bounded
    /// by the device count and a cert by its field widths — and refused rather
    /// than truncated because a length prefix that wrapped would reintroduce
    /// exactly the ambiguity it is there to remove.
    #[error("roster or roster entry is too large to length-prefix")]
    RosterTooLarge,
    /// A per-device share is not [`DEVICE_SHARE_LEN`] bytes.
    #[error("device share for {device} is {len} bytes, expected {DEVICE_SHARE_LEN}")]
    ShareLen {
        /// Hex of the device id the share is addressed to.
        device: String,
        /// The length actually supplied.
        len: usize,
    },
    /// Two shares are addressed to the same device.
    #[error("shares name device {0} twice")]
    ShareDuplicate(String),
    /// The carry-forward identity share is not [`IDENTITY_SHARE_LEN`] bytes.
    #[error("identity share is {0} bytes, expected {IDENTITY_SHARE_LEN}")]
    IdentityShareLen(usize),
    /// `from_identity_id` is not the id derived from the outgoing `ID_S_pub`.
    #[error("from_identity_id does not derive from the outgoing ID_S_pub")]
    FromIdentityMismatch,
    /// `to_identity_id` is not the id derived from `to_id_s_pub`.
    #[error("to_identity_id does not derive from to_id_s_pub")]
    ToIdentityMismatch,
    /// The transition does not move to a different identity.
    ///
    /// A "rotation" onto the same key is either a replay of a transition
    /// already applied or a mistake; either way there is nothing to hand over.
    #[error("the successor identity is the outgoing identity")]
    NotATransition,
    /// The outgoing identity's signature does not verify.
    #[error("prev_sig does not verify under the outgoing ID_S_pub")]
    PrevSigVerify,
    /// The successor identity's signature does not verify.
    #[error("next_sig does not verify under to_id_s_pub")]
    NextSigVerify,
}

/// The signed body of an `identity_transition`.
///
/// Encoded with [`sunrise_cbor::encode_canonical`], so the field order in this
/// declaration is **not** the byte order: keys sort by their encoded bytes. The
/// hand-rolled integer-keyed map used by [`crate::device_cert`] predates that
/// helper and is not the pattern to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityTransitionBody {
    /// The identity being retired, derived from the outgoing `ID_S_pub`.
    #[serde(with = "serde_bytes")]
    pub from_identity_id: [u8; 16],
    /// The successor identity, derived from `to_id_s_pub`.
    #[serde(with = "serde_bytes")]
    pub to_identity_id: [u8; 16],
    /// The successor's Ed25519 public key.
    #[serde(with = "serde_bytes")]
    pub to_id_s_pub: [u8; 32],
    /// The successor's X25519 public key.
    #[serde(with = "serde_bytes")]
    pub to_id_d_pub: [u8; 32],
    /// [`roster_digest`] of the re-issued device certs.
    #[serde(with = "serde_bytes")]
    pub roster_digest: [u8; 32],
    /// [`shares_digest`] of the sealed successor secrets.
    #[serde(with = "serde_bytes")]
    pub shares_digest: [u8; 32],
}

/// The signature pair over an [`IdentityTransitionBody`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityTransitionSigs {
    /// Ed25519 by the **outgoing** `ID_S_priv` — the hand-over is authorized.
    pub prev_sig: [u8; 64],
    /// Ed25519 by the **successor** `ID_S_priv`, over the body *and*
    /// `prev_sig` — the hand-over is accepted, from this predecessor.
    pub next_sig: [u8; 64],
}

/// `BLAKE3(canonical_cbor(body))` — what both signatures are taken over.
///
/// # Errors
/// [`IdentityTransitionError::Cbor`] if the body cannot be encoded, which is
/// structural rather than input-dependent.
pub fn body_hash(body: &IdentityTransitionBody) -> Result<[u8; 32], IdentityTransitionError> {
    let cbor = sunrise_cbor::encode_canonical(body)
        .map_err(|e| IdentityTransitionError::Cbor(e.to_string()))?;
    Ok(*blake3::hash(&cbor).as_bytes())
}

/// `BLAKE3` over every re-issued cert blob, in **device-id ascending** order.
///
/// ```text
/// "sunrise.identity_roster.v1" || u32_be(count)
/// for each entry ascending by device_id:  device_id(16) || u32_be(len) || cert
/// ```
///
/// The order is normalized here rather than trusted from the caller because the
/// roster travels as a CBOR array and an array has an order: a peer that
/// rebuilt the same membership in a different order would otherwise compute a
/// different digest and reject a valid transition.
///
/// The device id comes out of the cert, never beside it. A roster entry carries
/// only the cert precisely so there is one source of truth for which device it
/// describes; a second copy in the entry could disagree with the signed body
/// inside it. It is hashed as well as the cert so that the hashed input carries
/// the same identity the duplicate check ran against.
///
/// # Why the count and the lengths
///
/// A cert is **variable-length by construction** — `nickname` is 1..=64 bytes
/// and `platform` is a `tstr` with no bound anywhere — and `DeviceCert::from_cbor`
/// reads one CBOR item without checking for trailing bytes. So a digest over
/// bare concatenated blobs was ambiguous in the way [`shares_digest`]'s own
/// documentation had already described and this function did not implement:
/// entries could be **fused**. `certA ‖ certB ‖ certC`, presented as a
/// single entry, decodes as `certA`, passes `verify_binding`, passes the
/// duplicate check, and hashes to exactly the digest the three separate certs
/// hash to.
///
/// That was not theoretical. Because entries sort ascending by device id, the
/// holder of any signed transition could fuse any contiguous run and present a
/// roster that was **any subset of the signed one retaining the lowest device
/// id** — including a roster of one. `apply_roster` moves only the devices it
/// is given onto the successor identity, and everything that seals a key joins
/// on `devices.identity_id = head.identity_id`, so the dropped devices were
/// sealed no epoch ever again: silently revoked, with no revocation record, by
/// whoever relayed the op. The signatures and the digest were all untouched.
///
/// The count is prefixed as well as the lengths because without it a digest is
/// still a prefix of a longer one; with both, the hashed input determines the
/// entry list uniquely.
///
/// # Errors
/// [`IdentityTransitionError::RosterCert`] for an undecodable entry or one
/// carrying bytes past the cert, [`IdentityTransitionError::RosterDuplicate`]
/// if two entries name one device, and
/// [`IdentityTransitionError::RosterTooLarge`] if the roster or a cert is too
/// large for its length prefix.
pub fn roster_digest<C: AsRef<[u8]>>(certs: &[C]) -> Result<[u8; 32], IdentityTransitionError> {
    let mut entries: Vec<([u8; 16], &[u8])> = Vec::with_capacity(certs.len());
    for (index, blob) in certs.iter().enumerate() {
        let bytes = blob.as_ref();
        let cert = DeviceCert::from_cbor(bytes)
            .map_err(|_| IdentityTransitionError::RosterCert { index })?;
        entries.push((cert.body.device_id, bytes));
    }
    entries.sort_unstable_by_key(|(device_id, _)| *device_id);
    for pair in entries.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(IdentityTransitionError::RosterDuplicate(hex16(&pair[0].0)));
        }
    }
    let count =
        u32::try_from(entries.len()).map_err(|_| IdentityTransitionError::RosterTooLarge)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(ROSTER_DOMAIN);
    hasher.update(&count.to_be_bytes());
    for (device_id, bytes) in &entries {
        let len =
            u32::try_from(bytes.len()).map_err(|_| IdentityTransitionError::RosterTooLarge)?;
        hasher.update(device_id);
        hasher.update(&len.to_be_bytes());
        hasher.update(bytes);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// `BLAKE3` over every sealed share, then the carry-forward share.
///
/// ```text
/// for each (device_id, ct) ascending by device_id:  device_id(16) || ct(80)
/// then                                              0x00
///   or                                              0x01 || identity_share(112)
/// ```
///
/// Both lengths are **checked, not assumed**. A digest over a concatenation of
/// variable-length records is ambiguous — `a||bc` and `ab||c` hash the same —
/// and this digest is inside a signature, so the ambiguity would be a way to
/// present one signed transition as a different set of shares. Every record
/// here is fixed-width by construction ([`DEVICE_SHARE_LEN`],
/// [`IDENTITY_SHARE_LEN`]), so rejecting anything else costs nothing and
/// removes the question; the trailing present-flag does the same job for the
/// optional tail.
///
/// # Errors
/// [`IdentityTransitionError::ShareLen`] or
/// [`IdentityTransitionError::IdentityShareLen`] for a wrong-width record,
/// [`IdentityTransitionError::ShareDuplicate`] if two shares name one device.
pub fn shares_digest<C: AsRef<[u8]>>(
    device_shares: &[([u8; 16], C)],
    identity_share: Option<&[u8]>,
) -> Result<[u8; 32], IdentityTransitionError> {
    let mut entries: Vec<([u8; 16], &[u8])> = Vec::with_capacity(device_shares.len());
    for (device_id, ct) in device_shares {
        let bytes = ct.as_ref();
        if bytes.len() != DEVICE_SHARE_LEN {
            return Err(IdentityTransitionError::ShareLen {
                device: hex16(device_id),
                len: bytes.len(),
            });
        }
        entries.push((*device_id, bytes));
    }
    entries.sort_unstable_by_key(|(device_id, _)| *device_id);
    for pair in entries.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(IdentityTransitionError::ShareDuplicate(hex16(&pair[0].0)));
        }
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(SHARES_DOMAIN);
    for (device_id, bytes) in &entries {
        hasher.update(device_id);
        hasher.update(bytes);
    }
    match identity_share {
        None => {
            hasher.update(&[0x00]);
        }
        Some(share) => {
            if share.len() != IDENTITY_SHARE_LEN {
                return Err(IdentityTransitionError::IdentityShareLen(share.len()));
            }
            hasher.update(&[0x01]);
            hasher.update(share);
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Sign a transition body with the outgoing and successor identity keys.
///
/// The successor key is taken as a keypair rather than as bytes so that the
/// caller cannot sign a body naming one successor with another successor's
/// key: the `to_id_s_pub` binding is checked here, before either signature is
/// produced, and the same check runs again in
/// [`verify_identity_transition`].
///
/// # Errors
/// [`IdentityTransitionError::FromIdentityMismatch`] /
/// [`IdentityTransitionError::ToIdentityMismatch`] if either id in the body is
/// not the derivation of the key it names,
/// [`IdentityTransitionError::NotATransition`] if the two identities are one,
/// or [`IdentityTransitionError::Cbor`].
pub fn sign_identity_transition(
    body: &IdentityTransitionBody,
    from_signing: &IdentitySigningKeyPair,
    to_signing: &IdentitySigningKeyPair,
) -> Result<IdentityTransitionSigs, IdentityTransitionError> {
    check_identity_binding(body, &from_signing.public_bytes())?;
    if to_signing.public_bytes() != body.to_id_s_pub {
        return Err(IdentityTransitionError::ToIdentityMismatch);
    }
    let hash = body_hash(body)?;
    let prev_sig = from_signing.sign(&prev_sig_input(&hash));
    let next_sig = to_signing.sign(&next_sig_input(&hash, &prev_sig));
    Ok(IdentityTransitionSigs { prev_sig, next_sig })
}

/// Verify a transition against the identity it claims to succeed.
///
/// `from_id_s_pub` is the `ID_S_pub` the verifier *already trusts* — the
/// identity it is currently a member of — and is passed separately rather than
/// read from the body for the same reason
/// [`DeviceCert::verify_binding`] takes an expected identity: a body that
/// verifies under some other well-formed identity is still a stranger's
/// transition.
///
/// Checked in order: the two id derivations, then that this is a real move,
/// then `prev_sig`, then `next_sig`. The signature order is the one
/// `key-rotation.md` specifies — authorization before acceptance — so a caller
/// reporting the first failure reports the more meaningful one.
///
/// # Errors
/// [`IdentityTransitionError::FromIdentityMismatch`],
/// [`IdentityTransitionError::ToIdentityMismatch`],
/// [`IdentityTransitionError::NotATransition`],
/// [`IdentityTransitionError::PrevSigVerify`],
/// [`IdentityTransitionError::NextSigVerify`], or
/// [`IdentityTransitionError::Cbor`].
pub fn verify_identity_transition(
    body: &IdentityTransitionBody,
    from_id_s_pub: &[u8; 32],
    sigs: &IdentityTransitionSigs,
) -> Result<(), IdentityTransitionError> {
    check_identity_binding(body, from_id_s_pub)?;
    let hash = body_hash(body)?;
    if !verify_ed25519(from_id_s_pub, &prev_sig_input(&hash), &sigs.prev_sig) {
        return Err(IdentityTransitionError::PrevSigVerify);
    }
    if !verify_ed25519(
        &body.to_id_s_pub,
        &next_sig_input(&hash, &sigs.prev_sig),
        &sigs.next_sig,
    ) {
        return Err(IdentityTransitionError::NextSigVerify);
    }
    Ok(())
}

/// Verify the **successor** signature alone, without knowing the predecessor.
///
/// `next_sig` is taken under `to_id_s_priv`, and every input to it —
/// `body_hash` and `prev_sig` — is reconstructible from a transition payload on
/// its own. So a reader that has not yet established the predecessor, and
/// therefore cannot check `prev_sig`, can still establish that **whoever
/// authored this payload held the successor's identity key**.
///
/// That is what an apply path needs. Applying is unconditional by design — a
/// replica records the op and decides later, when the chain is folded, whether
/// it is the account's — and the row is keyed on `to_identity_id`. Without this
/// check, anyone could take an honest transition off the wire, substitute the
/// roster or the shares, and publish it first: the substituted copy occupies
/// the key, the honest one is `INSERT OR IGNORE`d away, and the rotation can
/// never be recorded on that replica. The forged row fails
/// [`verify_identity_transition`] at fold time and is never *believed* — but it
/// is never *replaced* either, so the account is simply stuck, and a revoked
/// device could keep itself in the roster of every peer's view by suppressing
/// its own removal.
///
/// This does not replace [`verify_identity_transition`]. It establishes one of
/// its two signatures; the chain is what establishes the other, and only that
/// says the transition is *this account's*.
///
/// # Errors
/// [`IdentityTransitionError::ToIdentityMismatch`] if `to_identity_id` is not
/// the derivation of `to_id_s_pub`, [`IdentityTransitionError::Cbor`] if the
/// body will not encode, and [`IdentityTransitionError::NextSigVerify`] if the
/// successor signature does not verify.
pub fn verify_successor_signature(
    body: &IdentityTransitionBody,
    sigs: &IdentityTransitionSigs,
) -> Result<(), IdentityTransitionError> {
    let to_derived = identity_id_from_pub(&body.to_id_s_pub);
    let to_ok: bool = to_derived.ct_eq(&body.to_identity_id).into();
    if !to_ok {
        return Err(IdentityTransitionError::ToIdentityMismatch);
    }
    let hash = body_hash(body)?;
    if !verify_ed25519(
        &body.to_id_s_pub,
        &next_sig_input(&hash, &sigs.prev_sig),
        &sigs.next_sig,
    ) {
        return Err(IdentityTransitionError::NextSigVerify);
    }
    Ok(())
}

/// Both ids are derivations of the keys they name, and the two differ.
///
/// Constant-time comparison for the same reason
/// [`DeviceCert::verify_binding`] uses it: these run over attacker-supplied
/// bytes, and a byte-at-a-time comparison leaks how much of a forged id was
/// right.
fn check_identity_binding(
    body: &IdentityTransitionBody,
    from_id_s_pub: &[u8; 32],
) -> Result<(), IdentityTransitionError> {
    let from_derived = identity_id_from_pub(from_id_s_pub);
    let from_ok: bool = from_derived.ct_eq(&body.from_identity_id).into();
    if !from_ok {
        return Err(IdentityTransitionError::FromIdentityMismatch);
    }
    let to_derived = identity_id_from_pub(&body.to_id_s_pub);
    let to_ok: bool = to_derived.ct_eq(&body.to_identity_id).into();
    if !to_ok {
        return Err(IdentityTransitionError::ToIdentityMismatch);
    }
    let same: bool = from_derived.ct_eq(&to_derived).into();
    if same {
        return Err(IdentityTransitionError::NotATransition);
    }
    Ok(())
}

fn prev_sig_input(hash: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PREV_DOMAIN.len() + 32);
    out.extend_from_slice(PREV_DOMAIN);
    out.extend_from_slice(hash);
    out
}

fn next_sig_input(hash: &[u8; 32], prev_sig: &[u8; 64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SUCC_DOMAIN.len() + 32 + 64);
    out.extend_from_slice(SUCC_DOMAIN);
    out.extend_from_slice(hash);
    out.extend_from_slice(prev_sig);
    out
}

/// Short hex, for an error message naming a device. Four bytes is what
/// `sunrise_core::keychain`'s own `hex16` prints, and an error string is not
/// an identifier anything parses back.
fn hex16(bytes: &[u8; 16]) -> String {
    use core::fmt::Write;
    let mut s = String::with_capacity(8);
    for b in bytes.iter().take(4) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_cert::DeviceCertInner;
    use crate::keys::{DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair};
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    /// One device's worth of roster + share, built under a given identity.
    struct Member {
        device_id: [u8; 16],
        cert: Vec<u8>,
        share: Vec<u8>,
    }

    fn member(seed: u64, to_signing: &IdentitySigningKeyPair, tag: u8) -> Member {
        let mut rng = ChaCha20Rng::seed_from_u64(seed);
        let d_s = DeviceSigningKeyPair::generate(&mut rng);
        let d_d = DeviceDhKeyPair::generate(&mut rng);
        let device_id = [tag; 16];
        let body = DeviceCertInner {
            v: 1,
            device_id,
            d_s_pub: d_s.public_bytes(),
            d_d_pub: d_d.public_bytes(),
            identity_id: identity_id_from_pub(&to_signing.public_bytes()),
            created_at_ms: 1_700_000_000_000,
            nickname: "laptop".into(),
            platform: "macos".into(),
        };
        let cert = DeviceCert::issue(body, to_signing)
            .expect("issue")
            .to_cbor()
            .expect("cbor");
        Member {
            device_id,
            cert,
            share: vec![tag; DEVICE_SHARE_LEN],
        }
    }

    /// A full, valid transition: two identities, two members, a carry share.
    struct Fixture {
        from: IdentitySigningKeyPair,
        to: IdentitySigningKeyPair,
        body: IdentityTransitionBody,
        sigs: IdentityTransitionSigs,
    }

    fn fixture() -> Fixture {
        let from = IdentitySigningKeyPair::from_secret_bytes(&[0x11; 32]);
        let to = IdentitySigningKeyPair::from_secret_bytes(&[0x22; 32]);
        let to_dh = IdentityDhKeyPair::from_secret_bytes([0x33; 32]);
        let a = member(1, &to, 0xa1);
        let b = member(2, &to, 0xb2);
        let carry = vec![0x44; IDENTITY_SHARE_LEN];
        let body = IdentityTransitionBody {
            from_identity_id: identity_id_from_pub(&from.public_bytes()),
            to_identity_id: identity_id_from_pub(&to.public_bytes()),
            to_id_s_pub: to.public_bytes(),
            to_id_d_pub: to_dh.public_bytes(),
            roster_digest: roster_digest(&[a.cert.clone(), b.cert.clone()]).expect("roster"),
            shares_digest: shares_digest(
                &[
                    (a.device_id, a.share.clone()),
                    (b.device_id, b.share.clone()),
                ],
                Some(&carry),
            )
            .expect("shares"),
        };
        let sigs = sign_identity_transition(&body, &from, &to).expect("sign");
        Fixture {
            from,
            to,
            body,
            sigs,
        }
    }

    #[test]
    fn a_signed_transition_verifies() {
        let f = fixture();
        verify_identity_transition(&f.body, &f.from.public_bytes(), &f.sigs).expect("verify");
    }

    #[test]
    fn the_roster_digest_is_order_independent() {
        let to = IdentitySigningKeyPair::from_secret_bytes(&[0x22; 32]);
        let a = member(1, &to, 0xa1);
        let b = member(2, &to, 0xb2);
        let forwards = roster_digest(&[a.cert.clone(), b.cert.clone()]).expect("forwards");
        let backwards = roster_digest(&[b.cert.clone(), a.cert.clone()]).expect("backwards");
        assert_eq!(forwards, backwards, "the roster is a set, not a sequence");
        // ...and it is not order-*blind*: dropping a member must change it.
        assert_ne!(forwards, roster_digest(&[a.cert]).expect("single"));
    }

    #[test]
    fn a_duplicated_roster_device_is_refused() {
        let to = IdentitySigningKeyPair::from_secret_bytes(&[0x22; 32]);
        let a = member(1, &to, 0xa1);
        let err = roster_digest(&[a.cert.clone(), a.cert]).expect_err("duplicate");
        assert!(matches!(err, IdentityTransitionError::RosterDuplicate(_)));
    }

    /// **A roster of three cannot be presented as a roster of one.**
    ///
    /// The digest used to be a bare `BLAKE3` over concatenated cert blobs, and
    /// a cert is variable-length: `nickname` is 1..=64 bytes and `platform` is
    /// unbounded. `DeviceCert::from_cbor` also read one item and ignored
    /// whatever followed. So `certA ‖ certB ‖ certC` was a single roster entry
    /// that decoded as `certA`, passed every check, and hashed to exactly the
    /// digest the three separate certs hashed to — which meant the holder of a
    /// signed transition could drop any devices from the signed membership and
    /// keep the signature valid. `apply_roster` would then leave those devices
    /// on the retired identity, where nothing ever seals them a key again.
    ///
    /// Two independent things now stop it, and this test asserts both: the
    /// blobs are length-prefixed and counted, and a blob with a tail is not a
    /// cert.
    #[test]
    fn a_fused_roster_entry_neither_decodes_nor_collides() {
        let to = IdentitySigningKeyPair::from_secret_bytes(&[0x22; 32]);
        // Ascending by device id, which is the order the digest imposes, and
        // deliberately of three different lengths.
        let a = member(1, &to, 0xa1);
        let b = member(2, &to, 0xb2);
        let c = member(3, &to, 0xc3);
        let honest = roster_digest(&[a.cert.clone(), b.cert.clone(), c.cert.clone()])
            .expect("the honest roster digests");

        let mut fused = a.cert.clone();
        fused.extend_from_slice(&b.cert);
        fused.extend_from_slice(&c.cert);

        // First line: the fused blob is not a device cert at all.
        let err = roster_digest(&[fused.clone()]).expect_err("a fused entry is not a cert");
        assert_eq!(err, IdentityTransitionError::RosterCert { index: 0 });

        // Second line, asserted independently of the first: even a hypothetical
        // decoder that tolerated the tail could not reach the honest digest,
        // because the count and the per-entry lengths are hashed.
        let mut hasher = blake3::Hasher::new();
        hasher.update(ROSTER_DOMAIN);
        hasher.update(&1u32.to_be_bytes());
        hasher.update(&a.device_id);
        hasher.update(&u32::try_from(fused.len()).expect("fits").to_be_bytes());
        hasher.update(&fused);
        assert_ne!(
            honest,
            *hasher.finalize().as_bytes(),
            "a one-entry roster must not reach a three-entry digest"
        );
    }

    /// A cert with anything after it is refused, rather than read as the cert
    /// it starts with.
    #[test]
    fn a_device_cert_with_a_tail_is_not_a_device_cert() {
        let to = IdentitySigningKeyPair::from_secret_bytes(&[0x22; 32]);
        let a = member(1, &to, 0xa1);
        assert!(
            DeviceCert::from_cbor(&a.cert).is_ok(),
            "the control decodes"
        );

        let mut with_tail = a.cert.clone();
        with_tail.push(0x00);
        assert!(
            DeviceCert::from_cbor(&with_tail).is_err(),
            "one trailing byte is enough to make these bytes not a cert"
        );
    }

    /// The two digests live in different domains, so no input to one can ever
    /// be an input to the other.
    #[test]
    fn the_roster_and_share_digests_do_not_share_a_hash_space() {
        assert_ne!(ROSTER_DOMAIN, SHARES_DOMAIN);
        let to = IdentitySigningKeyPair::from_secret_bytes(&[0x22; 32]);
        let a = member(1, &to, 0xa1);
        let roster = roster_digest(std::slice::from_ref(&a.cert)).expect("roster");
        // The same bytes, hashed without the domain tag, is what this used to
        // be. It must not be what it is now.
        let mut untagged = blake3::Hasher::new();
        untagged.update(&a.cert);
        assert_ne!(roster, *untagged.finalize().as_bytes());
    }

    #[test]
    fn an_undecodable_roster_entry_names_its_index() {
        let to = IdentitySigningKeyPair::from_secret_bytes(&[0x22; 32]);
        let a = member(1, &to, 0xa1);
        let err = roster_digest(&[a.cert, b"not a cert".to_vec()]).expect_err("bad cert");
        assert_eq!(err, IdentityTransitionError::RosterCert { index: 1 });
    }

    #[test]
    fn the_shares_digest_is_order_independent_and_flags_the_carry() {
        let a = ([0xa1u8; 16], vec![0xa1u8; DEVICE_SHARE_LEN]);
        let b = ([0xb2u8; 16], vec![0xb2u8; DEVICE_SHARE_LEN]);
        let carry = vec![0x44u8; IDENTITY_SHARE_LEN];
        let forwards = shares_digest(&[a.clone(), b.clone()], Some(&carry)).expect("forwards");
        let backwards = shares_digest(&[b.clone(), a.clone()], Some(&carry)).expect("backwards");
        assert_eq!(forwards, backwards);
        // The present flag is what keeps "no carry" from colliding with a
        // carry that happens to be absent-shaped.
        let absent = shares_digest(&[a, b], None).expect("absent");
        assert_ne!(forwards, absent);
    }

    #[test]
    fn a_wrong_width_share_is_refused_rather_than_hashed() {
        let short = shares_digest(&[([0xa1u8; 16], vec![0u8; DEVICE_SHARE_LEN - 1])], None)
            .expect_err("short device share");
        assert!(matches!(short, IdentityTransitionError::ShareLen { .. }));
        let carry = vec![0u8; IDENTITY_SHARE_LEN + 1];
        let long = shares_digest::<Vec<u8>>(&[], Some(&carry)).expect_err("long carry");
        assert_eq!(
            long,
            IdentityTransitionError::IdentityShareLen(IDENTITY_SHARE_LEN + 1)
        );
    }

    #[test]
    fn a_duplicated_share_device_is_refused() {
        let a = ([0xa1u8; 16], vec![0xa1u8; DEVICE_SHARE_LEN]);
        let err = shares_digest(&[a.clone(), a], None).expect_err("duplicate");
        assert!(matches!(err, IdentityTransitionError::ShareDuplicate(_)));
    }

    /// Every field the body commits to, flipped one bit at a time. Each one
    /// must break verification — that is the whole claim the digests make.
    #[test]
    fn flipping_any_committed_byte_breaks_verification() {
        let f = fixture();
        let from_pub = f.from.public_bytes();

        let mut roster = f.body;
        roster.roster_digest[0] ^= 0x01;
        assert_eq!(
            verify_identity_transition(&roster, &from_pub, &f.sigs),
            Err(IdentityTransitionError::PrevSigVerify),
            "the roster digest is inside the signed body"
        );

        let mut shares = f.body;
        shares.shares_digest[31] ^= 0x80;
        assert_eq!(
            verify_identity_transition(&shares, &from_pub, &f.sigs),
            Err(IdentityTransitionError::PrevSigVerify)
        );

        let mut dh = f.body;
        dh.to_id_d_pub[0] ^= 0x01;
        assert_eq!(
            verify_identity_transition(&dh, &from_pub, &f.sigs),
            Err(IdentityTransitionError::PrevSigVerify),
            "the successor's DH key is signed, not merely carried"
        );
    }

    #[test]
    fn flipping_either_signature_breaks_verification() {
        let f = fixture();
        let from_pub = f.from.public_bytes();

        let mut prev = f.sigs;
        prev.prev_sig[0] ^= 0x01;
        assert_eq!(
            verify_identity_transition(&f.body, &from_pub, &prev),
            Err(IdentityTransitionError::PrevSigVerify)
        );

        let mut next = f.sigs;
        next.next_sig[63] ^= 0x01;
        assert_eq!(
            verify_identity_transition(&f.body, &from_pub, &next),
            Err(IdentityTransitionError::NextSigVerify)
        );
    }

    /// `next_sig` covers `prev_sig`, so a successor's acceptance cannot be
    /// lifted onto a transition authorized by somebody else.
    #[test]
    fn a_successor_signature_does_not_transplant() {
        let f = fixture();
        // A second predecessor signing the very same body.
        let other_from = IdentitySigningKeyPair::from_secret_bytes(&[0x99; 32]);
        let mut body = f.body;
        body.from_identity_id = identity_id_from_pub(&other_from.public_bytes());
        let hash = body_hash(&body).expect("hash");
        let prev_sig = other_from.sign(&prev_sig_input(&hash));
        // ...reusing the original successor signature.
        let transplanted = IdentityTransitionSigs {
            prev_sig,
            next_sig: f.sigs.next_sig,
        };
        assert_eq!(
            verify_identity_transition(&body, &other_from.public_bytes(), &transplanted),
            Err(IdentityTransitionError::NextSigVerify)
        );
        // The successor really would accept it if it re-signed.
        let honest = sign_identity_transition(&body, &other_from, &f.to).expect("sign");
        verify_identity_transition(&body, &other_from.public_bytes(), &honest).expect("verify");
    }

    #[test]
    fn a_body_naming_the_wrong_predecessor_is_refused() {
        let f = fixture();
        let stranger = IdentitySigningKeyPair::from_secret_bytes(&[0x77; 32]);
        assert_eq!(
            verify_identity_transition(&f.body, &stranger.public_bytes(), &f.sigs),
            Err(IdentityTransitionError::FromIdentityMismatch)
        );
    }

    /// Flipping the successor's signing key breaks the `to_identity_id`
    /// derivation *before* any signature is checked: the id is not a field a
    /// forger can choose independently of the key.
    #[test]
    fn flipping_the_successor_pubkey_breaks_its_derived_id() {
        let f = fixture();
        let mut body = f.body;
        body.to_id_s_pub[0] ^= 0x01;
        assert_eq!(
            verify_identity_transition(&body, &f.from.public_bytes(), &f.sigs),
            Err(IdentityTransitionError::ToIdentityMismatch)
        );
        // ...and flipping the id instead fails the same check from the other
        // side, so neither half can be moved alone.
        let mut body = f.body;
        body.to_identity_id[0] ^= 0x01;
        assert_eq!(
            verify_identity_transition(&body, &f.from.public_bytes(), &f.sigs),
            Err(IdentityTransitionError::ToIdentityMismatch)
        );
    }

    #[test]
    fn a_transition_onto_the_same_identity_is_refused() {
        let key = IdentitySigningKeyPair::from_secret_bytes(&[0x11; 32]);
        let dh = IdentityDhKeyPair::from_secret_bytes([0x33; 32]);
        let id = identity_id_from_pub(&key.public_bytes());
        let body = IdentityTransitionBody {
            from_identity_id: id,
            to_identity_id: id,
            to_id_s_pub: key.public_bytes(),
            to_id_d_pub: dh.public_bytes(),
            roster_digest: [0u8; 32],
            shares_digest: [0u8; 32],
        };
        assert_eq!(
            sign_identity_transition(&body, &key, &key),
            Err(IdentityTransitionError::NotATransition)
        );
    }

    #[test]
    fn signing_with_a_key_the_body_does_not_name_is_refused() {
        let f = fixture();
        let stranger = IdentitySigningKeyPair::from_secret_bytes(&[0x88; 32]);
        assert_eq!(
            sign_identity_transition(&f.body, &f.from, &stranger),
            Err(IdentityTransitionError::ToIdentityMismatch)
        );
    }

    /// The body is canonical CBOR, so its hash is reproducible across peers
    /// that built the same struct — the property every signature here rests on.
    #[test]
    fn the_body_hash_is_the_canonical_encoding() {
        let f = fixture();
        let cbor = sunrise_cbor::encode_canonical(&f.body).expect("encode");
        assert_eq!(
            body_hash(&f.body).expect("hash"),
            *blake3::hash(&cbor).as_bytes()
        );
        let round: IdentityTransitionBody =
            sunrise_cbor::decode_canonical(&cbor).expect("canonical round trip");
        assert_eq!(round, f.body);
    }
}
