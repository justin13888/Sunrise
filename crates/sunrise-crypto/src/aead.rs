//! XChaCha20-Poly1305 AEAD wrapper.
//!
//! Per `docs/03-crypto/primitives.md`: every op-envelope and blob-chunk
//! AEAD operation uses XChaCha20-Poly1305 with a 32-byte key and a 24-byte
//! random nonce (or, for blob chunks, a deterministically-derived nonce —
//! see `docs/03-crypto/data-encryption-format.md`).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use thiserror::Error;

/// Length of an AEAD key (XChaCha20-Poly1305).
pub const AEAD_KEY_LEN: usize = 32;
/// Length of an AEAD nonce.
pub const AEAD_NONCE_LEN: usize = 24;
/// Length of an AEAD authentication tag.
pub const AEAD_TAG_LEN: usize = 16;

/// AEAD errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AeadError {
    /// Plaintext too large to seal (well above any in-spec value; never
    /// expected to fire in practice).
    #[error("plaintext too large for AEAD")]
    PlaintextTooLarge,
    /// AEAD verify failed (tag mismatch, AAD mismatch, key mismatch).
    #[error("AEAD authentication failed")]
    AuthFailed,
}

/// Seal `plaintext` with `key` and `nonce`, binding `aad` for authentication.
/// Returns `ciphertext || tag`.
///
/// # Errors
/// Returns [`AeadError::PlaintextTooLarge`] if the input cannot be sealed
/// (extremely unlikely in v1 since envelope max is 1 MiB).
pub fn aead_seal_xchacha(
    key: &[u8; AEAD_KEY_LEN],
    nonce: &[u8; AEAD_NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, AeadError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let n = XNonce::from_slice(nonce);
    cipher
        .encrypt(
            n,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| AeadError::PlaintextTooLarge)
}

/// Open `ciphertext_and_tag` with `key`, `nonce`, and `aad`. Returns the
/// plaintext on successful tag verification.
///
/// # Errors
/// Returns [`AeadError::AuthFailed`] on any tag/AAD mismatch.
pub fn aead_open_xchacha(
    key: &[u8; AEAD_KEY_LEN],
    nonce: &[u8; AEAD_NONCE_LEN],
    ciphertext_and_tag: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, AeadError> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let n = XNonce::from_slice(nonce);
    cipher
        .decrypt(
            n,
            Payload {
                msg: ciphertext_and_tag,
                aad,
            },
        )
        .map_err(|_| AeadError::AuthFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> [u8; AEAD_KEY_LEN] {
        let mut k = [0u8; AEAD_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    fn fixed_nonce() -> [u8; AEAD_NONCE_LEN] {
        let mut n = [0u8; AEAD_NONCE_LEN];
        for (i, b) in n.iter_mut().enumerate() {
            *b = (i + 100) as u8;
        }
        n
    }

    #[test]
    fn round_trip() {
        let key = fixed_key();
        let nonce = fixed_nonce();
        let plain = b"hello sunrise";
        let aad = b"context";
        let ct = aead_seal_xchacha(&key, &nonce, plain, aad).unwrap();
        // Tag is appended; ciphertext is plaintext-length + AEAD_TAG_LEN.
        assert_eq!(ct.len(), plain.len() + AEAD_TAG_LEN);
        let pt = aead_open_xchacha(&key, &nonce, &ct, aad).unwrap();
        assert_eq!(pt, plain);
    }

    #[test]
    fn tampered_ciphertext_rejects() {
        let key = fixed_key();
        let nonce = fixed_nonce();
        let mut ct = aead_seal_xchacha(&key, &nonce, b"hello", b"x").unwrap();
        ct[0] ^= 0x01;
        assert_eq!(
            aead_open_xchacha(&key, &nonce, &ct, b"x"),
            Err(AeadError::AuthFailed)
        );
    }

    #[test]
    fn tampered_aad_rejects() {
        let key = fixed_key();
        let nonce = fixed_nonce();
        let ct = aead_seal_xchacha(&key, &nonce, b"hello", b"x").unwrap();
        assert_eq!(
            aead_open_xchacha(&key, &nonce, &ct, b"y"),
            Err(AeadError::AuthFailed)
        );
    }
}
