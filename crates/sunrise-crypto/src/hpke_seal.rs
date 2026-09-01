//! HPKE single-shot seal/open — the primitive `key_envelope` ops are built on.
//!
//! Per `docs/03-crypto/data-encryption-format.md` §HPKE single-shot and
//! [ADR-0024](../../../docs/11-adr/0024-key-hierarchy.md):
//!
//! - Suite: RFC 9180 **Base** mode, `DHKEM(X25519, HKDF-SHA-256)` /
//!   `HKDF-SHA-256` / `ChaCha20-Poly1305` (suite ids `0x0020 / 0x0001 /
//!   0x0003`). Frozen for v1 by ADR-0004.
//! - Output bytes: `enc (32 B) || ciphertext || tag (16 B)`, stored as one
//!   `bstr`. The encapsulated key is carried inline rather than in its own
//!   field so a sealed blob is a single opaque value everywhere it appears.
//!
//! Base mode means the *sender* is not authenticated by HPKE at all. That is
//! deliberate and it is safe here only because of where these blobs live: a
//! `key_envelope` payload rides inside a signed [`crate::OpEnvelope`], so the
//! sender's `D_S` signature is checked before this module is ever reached. A
//! caller that seals outside a signed envelope has no sender authentication and
//! must add its own.
//!
//! The `info` string is what stops a sealed key being replayed into another
//! `(stream, epoch)`: it is bound into the KDF, so a blob sealed for
//! `(stream A, epoch 3)` fails to open under any other pair rather than
//! decrypting to a key that would then silently fail an AEAD check somewhere
//! far away.

use crate::keys::{DeviceDhKeyPair, IdentityDhKeyPair};
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, Deserializable, OpModeR,
    OpModeS, Serializable,
};
use rand_core::CryptoRngCore;
use thiserror::Error;
use x25519_dalek::StaticSecret;

/// The KEM this suite pins: `DHKEM(X25519, HKDF-SHA-256)`.
type Kem = X25519HkdfSha256;
/// The KDF this suite pins: `HKDF-SHA-256`.
type Kdf = HkdfSha256;
/// The AEAD this suite pins: `ChaCha20-Poly1305`.
type Aead = ChaCha20Poly1305;

/// Length of the encapsulated key prefix (`Nenc` for DHKEM(X25519,…)).
pub const HPKE_ENC_LEN: usize = 32;

/// Length of the ChaCha20-Poly1305 tag the sealed blob ends with.
pub const HPKE_TAG_LEN: usize = 16;

/// Domain-separation prefix for a `key_envelope` info string.
const KEY_ENVELOPE_INFO_PREFIX: &[u8] = b"sunrise.hpke.key_envelope.v1";

/// Errors produced by HPKE seal/open.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HpkeError {
    /// The recipient public key is not a well-formed X25519 point encoding.
    #[error("recipient public key is not a valid X25519 encoding")]
    BadRecipientKey,
    /// The `enc` prefix is not a well-formed encapsulated key.
    #[error("encapsulated key is not a valid X25519 encoding")]
    BadEncappedKey,
    /// The blob is shorter than `enc || tag`, so it cannot be an HPKE output.
    #[error("sealed blob is shorter than enc(32) + tag(16)")]
    TooShort,
    /// Encapsulation or encryption failed.
    #[error("HPKE seal failed")]
    Seal,
    /// Decapsulation or authentication failed — a wrong recipient key, a wrong
    /// `info`, or a tampered blob. All three are one error on purpose: telling
    /// them apart tells an attacker which guess was closer.
    #[error("HPKE open failed")]
    Open,
}

/// The `info` string binding a `key_envelope` to its `(stream_id, epoch)`.
///
/// `"sunrise.hpke.key_envelope.v1" || stream_id || u32_be(epoch)`.
#[must_use]
pub fn key_envelope_info(stream_id: &[u8; 16], epoch: u32) -> Vec<u8> {
    let mut info = Vec::with_capacity(KEY_ENVELOPE_INFO_PREFIX.len() + 16 + 4);
    info.extend_from_slice(KEY_ENVELOPE_INFO_PREFIX);
    info.extend_from_slice(stream_id);
    info.extend_from_slice(&epoch.to_be_bytes());
    info
}

/// Seal `plaintext` to `recipient_pub` under `info` and `aad`.
///
/// Returns `enc(32) || ciphertext || tag(16)`.
///
/// The `rng` is the caller's injected CSPRNG, never `OsRng` picked up here: the
/// ephemeral KEM key is drawn from it, so a test that seeds it gets a
/// reproducible blob and the frozen vector below is possible at all.
///
/// # Errors
/// [`HpkeError::BadRecipientKey`] for a malformed recipient key,
/// [`HpkeError::Seal`] if encapsulation or encryption fails.
pub fn hpke_seal<R: CryptoRngCore>(
    recipient_pub: &[u8; 32],
    info: &[u8],
    plaintext: &[u8],
    aad: &[u8],
    rng: &mut R,
) -> Result<Vec<u8>, HpkeError> {
    let pk = <Kem as hpke::Kem>::PublicKey::from_bytes(recipient_pub)
        .map_err(|_| HpkeError::BadRecipientKey)?;
    let mut bridged = RngBridge(rng);
    let (enc, ct) = hpke::single_shot_seal::<Aead, Kdf, Kem, RngBridge<'_, R>>(
        &OpModeS::Base,
        &pk,
        info,
        plaintext,
        aad,
        &mut bridged,
    )
    .map_err(|_| HpkeError::Seal)?;
    let enc_bytes = enc.to_bytes();
    debug_assert_eq!(enc_bytes.len(), HPKE_ENC_LEN);
    let mut out = Vec::with_capacity(enc_bytes.len() + ct.len());
    out.extend_from_slice(&enc_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a blob sealed to a **device**'s `D_D`.
///
/// # Errors
/// [`HpkeError::TooShort`] for a truncated blob, [`HpkeError::BadEncappedKey`]
/// for a malformed `enc`, [`HpkeError::Open`] for a wrong recipient, a wrong
/// `info`/`aad`, or tampering.
pub fn hpke_open(
    recipient: &DeviceDhKeyPair,
    info: &[u8],
    sealed: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, HpkeError> {
    open_with_secret(recipient.dalek(), info, sealed, aad)
}

/// Open a blob sealed to the **identity**'s `ID_D`.
///
/// This is the recovery half of ADR-0024 decision 4: the same envelope op that
/// gives a device its Stream keys also carries a copy sealed to the identity,
/// so a recovery code alone — with no surviving device — still reaches the
/// content. It is also the reason `ID_D_priv` is a long-lived unwrapping key
/// whose compromise reaches every epoch ever sealed to it, which
/// `docs/03-crypto/recovery.md` states rather than implies.
///
/// # Errors
/// As [`hpke_open`].
pub fn hpke_open_identity(
    recipient: &IdentityDhKeyPair,
    info: &[u8],
    sealed: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, HpkeError> {
    open_with_secret(recipient.dalek(), info, sealed, aad)
}

/// Bridges the workspace's `rand_core` 0.6 CSPRNG to the `rand_core` 0.9 that
/// `hpke` 0.13 was built against.
///
/// Two major versions of one trait are in the tree because `hpke` moved and
/// `ed25519-dalek` / `x25519-dalek` have not. This forwards `fill_bytes`
/// verbatim, so the byte stream — and therefore the ephemeral KEM key, and
/// therefore the frozen `key_envelope` vector — is exactly what the caller's
/// RNG produced. The 0.9 trait is reached through `hpke`'s own re-export rather
/// than a second manifest entry, so the bridge cannot drift from the version
/// `hpke` actually wants.
struct RngBridge<'a, R: CryptoRngCore>(&'a mut R);

impl<R: CryptoRngCore> hpke::rand_core::RngCore for RngBridge<'_, R> {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }
    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        self.0.fill_bytes(dst);
    }
}

impl<R: CryptoRngCore> hpke::rand_core::CryptoRng for RngBridge<'_, R> {}

fn open_with_secret(
    secret: &StaticSecret,
    info: &[u8],
    sealed: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, HpkeError> {
    if sealed.len() < HPKE_ENC_LEN + HPKE_TAG_LEN {
        return Err(HpkeError::TooShort);
    }
    let (enc_bytes, ct) = sealed.split_at(HPKE_ENC_LEN);
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(enc_bytes)
        .map_err(|_| HpkeError::BadEncappedKey)?;
    // Both key types wrap `x25519_dalek::StaticSecret` over the same 32 bytes,
    // and `hpke`'s `PrivateKey::from_bytes` is `StaticSecret::from(arr)` — so
    // this is a re-encoding, not a re-derivation, and cannot change the key.
    let sk_bytes = secret.to_bytes();
    let sk = <Kem as hpke::Kem>::PrivateKey::from_bytes(&sk_bytes)
        .map_err(|_| HpkeError::BadRecipientKey)?;
    hpke::single_shot_open::<Aead, Kdf, Kem>(&OpModeR::Base, &sk, &enc, info, ct, aad)
        .map_err(|_| HpkeError::Open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    #[test]
    fn info_is_the_documented_concatenation() {
        let info = key_envelope_info(&[0xaa; 16], 3);
        assert_eq!(
            &info[..KEY_ENVELOPE_INFO_PREFIX.len()],
            b"sunrise.hpke.key_envelope.v1"
        );
        assert_eq!(&info[KEY_ENVELOPE_INFO_PREFIX.len()..][..16], &[0xaa; 16]);
        assert_eq!(&info[info.len() - 4..], &3u32.to_be_bytes());
    }

    #[test]
    fn round_trip_to_a_device_key() {
        let mut rng = ChaCha20Rng::seed_from_u64(1);
        let recipient = DeviceDhKeyPair::generate(&mut rng);
        let info = key_envelope_info(&[1u8; 16], 2);
        let sealed = hpke_seal(
            &recipient.public_bytes(),
            &info,
            b"stream key",
            b"aad",
            &mut rng,
        )
        .expect("seal");
        assert_eq!(
            sealed.len(),
            HPKE_ENC_LEN + b"stream key".len() + HPKE_TAG_LEN
        );
        assert_eq!(
            hpke_open(&recipient, &info, &sealed, b"aad").expect("open"),
            b"stream key"
        );
    }

    #[test]
    fn round_trip_to_the_identity_key() {
        let mut rng = ChaCha20Rng::seed_from_u64(2);
        let recipient = IdentityDhKeyPair::generate(&mut rng);
        let info = key_envelope_info(&[4u8; 16], 9);
        let sealed =
            hpke_seal(&recipient.public_bytes(), &info, b"k", b"", &mut rng).expect("seal");
        assert_eq!(
            hpke_open_identity(&recipient, &info, &sealed, b"").expect("open"),
            b"k"
        );
    }
}
