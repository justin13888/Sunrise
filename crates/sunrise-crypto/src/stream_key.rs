//! Stream-key wrap/unwrap under the vault root key.
//!
//! Per `docs/03-crypto/key-rotation.md`:
//!
//! ```text
//! wrapped_stream_key = XChaCha20-Poly1305_seal(
//!     key       = vault_root,
//!     nonce     = random 24 B,
//!     plaintext = stream_key (32 B),
//!     aad       = "sunrise.wrap.stream_key.v1" || stream_id || u32_be(epoch)
//! )
//! ```
//!
//! Stored as `nonce || ct_and_tag`.

use crate::aead::{aead_open_xchacha, aead_seal_xchacha, AeadError, AEAD_NONCE_LEN};
use crate::blake3_kdf::derive_key;
use crate::keys::{StreamKey, VaultRootKey};
use rand_core::CryptoRngCore;
use thiserror::Error;

const WRAP_AAD_PREFIX: &[u8] = b"sunrise.wrap.stream_key.v1";
/// Length of a wrapped stream-key blob: nonce (24) + ciphertext (32) + tag (16).
pub const WRAPPED_STREAM_KEY_LEN: usize = AEAD_NONCE_LEN + 32 + 16;

/// Length of a stream-key id.
///
/// Eight bytes is a *disambiguator*, not a security boundary: it names which of
/// the (at most a handful of) keys stored at one `(stream_id, epoch)` a row
/// holds. Nothing authenticates on it — the AEAD tag decides which key actually
/// opens an op — so a collision costs one wasted trial decrypt, and it is
/// derived rather than random so two devices that receive the same key by
/// different routes agree on the row it belongs in.
pub const STREAM_KEY_ID_LEN: usize = 8;

/// `key_id = BLAKE3.derive_key("sunrise.stream_key_id.v1", stream_key, 8)`.
#[must_use]
pub fn stream_key_id(key: &StreamKey) -> [u8; STREAM_KEY_ID_LEN] {
    let bytes = derive_key(
        "sunrise.stream_key_id.v1",
        key.as_bytes(),
        STREAM_KEY_ID_LEN,
    );
    let mut out = [0u8; STREAM_KEY_ID_LEN];
    out.copy_from_slice(&bytes);
    out
}

/// Errors produced by stream-key wrap/unwrap.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StreamKeyWrapError {
    /// Wrapped blob length wrong (must be exactly [`WRAPPED_STREAM_KEY_LEN`]).
    #[error("wrapped stream-key blob has wrong length")]
    BadLength,
    /// AEAD auth failed (wrong vault root, wrong stream_id/epoch, corrupted).
    #[error("AEAD auth failed")]
    AuthFailed,
}

impl From<AeadError> for StreamKeyWrapError {
    fn from(_: AeadError) -> Self {
        Self::AuthFailed
    }
}

fn build_aad(stream_id: &[u8; 16], epoch: u32) -> Vec<u8> {
    let mut aad = Vec::with_capacity(WRAP_AAD_PREFIX.len() + 16 + 4);
    aad.extend_from_slice(WRAP_AAD_PREFIX);
    aad.extend_from_slice(stream_id);
    aad.extend_from_slice(&epoch.to_be_bytes());
    aad
}

/// Seal a stream-key under the vault root.
///
/// Returns `nonce || ciphertext || tag` of length [`WRAPPED_STREAM_KEY_LEN`].
///
/// # Errors
/// Only an `AuthFailed` mapping if the AEAD primitive errors (essentially
/// impossible at v1 sizes).
pub fn wrap_stream_key<R: CryptoRngCore>(
    vault_root: &VaultRootKey,
    stream_key: &StreamKey,
    stream_id: &[u8; 16],
    epoch: u32,
    rng: &mut R,
) -> Result<Vec<u8>, StreamKeyWrapError> {
    let mut nonce = [0u8; AEAD_NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let aad = build_aad(stream_id, epoch);
    let ct = aead_seal_xchacha(vault_root.as_bytes(), &nonce, stream_key.as_bytes(), &aad)?;
    let mut out = Vec::with_capacity(AEAD_NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a wrapped stream-key.
///
/// # Errors
/// Length / AEAD auth failures.
pub fn unwrap_stream_key(
    vault_root: &VaultRootKey,
    wrapped: &[u8],
    stream_id: &[u8; 16],
    epoch: u32,
) -> Result<StreamKey, StreamKeyWrapError> {
    if wrapped.len() != WRAPPED_STREAM_KEY_LEN {
        return Err(StreamKeyWrapError::BadLength);
    }
    let nonce = &wrapped[..AEAD_NONCE_LEN];
    let ct = &wrapped[AEAD_NONCE_LEN..];
    let aad = build_aad(stream_id, epoch);
    let mut nonce_arr = [0u8; AEAD_NONCE_LEN];
    nonce_arr.copy_from_slice(nonce);
    let pt = aead_open_xchacha(vault_root.as_bytes(), &nonce_arr, ct, &aad)?;
    if pt.len() != 32 {
        return Err(StreamKeyWrapError::BadLength);
    }
    let mut sk = [0u8; 32];
    sk.copy_from_slice(&pt);
    Ok(StreamKey::from_bytes(sk))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    #[test]
    fn round_trip() {
        let vault = VaultRootKey::from_bytes([1u8; 32]);
        let sk = StreamKey::from_bytes([2u8; 32]);
        let stream_id = [3u8; 16];
        let epoch = 4;
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let wrapped = wrap_stream_key(&vault, &sk, &stream_id, epoch, &mut rng).unwrap();
        assert_eq!(wrapped.len(), WRAPPED_STREAM_KEY_LEN);
        let back = unwrap_stream_key(&vault, &wrapped, &stream_id, epoch).unwrap();
        assert_eq!(back, sk);
    }

    #[test]
    fn aad_binding_works() {
        let vault = VaultRootKey::from_bytes([1u8; 32]);
        let sk = StreamKey::from_bytes([2u8; 32]);
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let wrapped = wrap_stream_key(&vault, &sk, &[3u8; 16], 4, &mut rng).unwrap();
        // Wrong stream_id rejects.
        assert_eq!(
            unwrap_stream_key(&vault, &wrapped, &[9u8; 16], 4),
            Err(StreamKeyWrapError::AuthFailed)
        );
        // Wrong epoch rejects.
        assert_eq!(
            unwrap_stream_key(&vault, &wrapped, &[3u8; 16], 9),
            Err(StreamKeyWrapError::AuthFailed)
        );
    }

    #[test]
    fn key_id_is_deterministic_and_key_dependent() {
        let a = StreamKey::from_bytes([0x11; 32]);
        let b = StreamKey::from_bytes([0x12; 32]);
        assert_eq!(stream_key_id(&a), stream_key_id(&a));
        assert_ne!(stream_key_id(&a), stream_key_id(&b));
    }

    #[test]
    fn wrong_root_rejects() {
        let v1 = VaultRootKey::from_bytes([1u8; 32]);
        let v2 = VaultRootKey::from_bytes([2u8; 32]);
        let sk = StreamKey::from_bytes([2u8; 32]);
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let wrapped = wrap_stream_key(&v1, &sk, &[3u8; 16], 4, &mut rng).unwrap();
        assert_eq!(
            unwrap_stream_key(&v2, &wrapped, &[3u8; 16], 4),
            Err(StreamKeyWrapError::AuthFailed)
        );
    }
}
