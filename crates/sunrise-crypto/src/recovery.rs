//! Recovery blob.
//!
//! Per `docs/03-crypto/recovery.md`. The recovery blob is opaque to the
//! server and only decryptable with the user's BIP-39 recovery code.
//!
//! ```text
//! recovery_key = Argon2id(
//!     password = recovery_seed (32 B from BIP-39 mnemonic),
//!     salt     = recovery_salt (16 random bytes; stored in blob header),
//!     version = 0x13, m = 65536 KiB, t = 3, p = 1, out_len = 32
//! )
//!
//! recovery_blob_ct = XChaCha20-Poly1305_seal(
//!     key       = recovery_key,
//!     nonce     = recovery_nonce (24 random bytes; stored in blob header),
//!     plaintext = canonical_cbor({
//!         1: ID_S_priv (32 B),
//!         2: ID_D_priv (32 B),
//!         3: ID_S_pub  (32 B),
//!         4: ID_D_pub  (32 B),
//!         5: identity_id_bytes (16 B),
//!         6: created_at (uint, ms since epoch)
//!     }),
//!     aad       = "sunrise.recovery_blob.v1" || identity_id_bytes
//! )
//! ```
//!
//! The blob is prefixed with the 5-byte magic from `sunrise-cbor::magic`
//! (`SR\x03\x00\x01`) and stored as `magic || salt(16) || nonce(24) || ct`.

use crate::aead::{aead_open_xchacha, aead_seal_xchacha, AEAD_NONCE_LEN};
use crate::keys::RecoveryKey;
use argon2::{Argon2, Version};
use ciborium::value::{Integer, Value};
use rand_core::CryptoRngCore;
use sunrise_cbor::magic::{decode_prefix, write_prefix, MagicKind, MAGIC_LEN};
use thiserror::Error;
use zeroize::Zeroize;

/// Per spec, the magic prefix version for the recovery blob is 1.
const RECOVERY_MAGIC_VERSION: u16 = 1;
/// AAD prefix for the AEAD seal.
const RECOVERY_AAD_PREFIX: &[u8] = b"sunrise.recovery_blob.v1";

/// Length of the recovery salt (Argon2id).
pub const RECOVERY_SALT_LEN: usize = 16;
/// Length of the AEAD nonce.
pub const RECOVERY_NONCE_LEN: usize = AEAD_NONCE_LEN;

/// Argon2id parameters from the spec.
const ARGON2_M_KIB: u32 = 65536;
const ARGON2_T: u32 = 3;
const ARGON2_P: u32 = 1;

/// Plaintext payload of a recovery blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPayload {
    /// `ID_S_priv` — Ed25519 private seed.
    pub id_s_priv: [u8; 32],
    /// `ID_D_priv` — X25519 secret.
    pub id_d_priv: [u8; 32],
    /// `ID_S_pub` — Ed25519 public.
    pub id_s_pub: [u8; 32],
    /// `ID_D_pub` — X25519 public.
    pub id_d_pub: [u8; 32],
    /// 16-byte identity id.
    pub identity_id: [u8; 16],
    /// Account creation time (ms since epoch).
    pub created_at_ms: u64,
}

impl Zeroize for RecoveryPayload {
    fn zeroize(&mut self) {
        self.id_s_priv.zeroize();
        self.id_d_priv.zeroize();
        self.id_s_pub.zeroize();
        self.id_d_pub.zeroize();
        self.identity_id.zeroize();
        self.created_at_ms.zeroize();
    }
}

/// Recovery errors.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// Magic prefix mismatch.
    #[error("recovery blob magic mismatch")]
    BadMagic,
    /// Blob shorter than `MAGIC_LEN + SALT_LEN + NONCE_LEN + 16 (tag)`.
    #[error("recovery blob too short")]
    Truncated,
    /// Argon2id failed (parameter values out of supported range).
    #[error("Argon2id error: {0}")]
    Kdf(String),
    /// AEAD auth failed (wrong recovery seed, corrupted blob, etc.).
    #[error("recovery blob authentication failed")]
    AuthFailed,
    /// Inner CBOR decode failed.
    #[error("recovery payload CBOR error: {0}")]
    Cbor(String),
}

fn derive_recovery_key(
    seed: &[u8; 32],
    salt: &[u8; RECOVERY_SALT_LEN],
) -> Result<RecoveryKey, RecoveryError> {
    let params = argon2::Params::new(ARGON2_M_KIB, ARGON2_T, ARGON2_P, Some(32))
        .map_err(|e| RecoveryError::Kdf(e.to_string()))?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon
        .hash_password_into(seed, salt, &mut out)
        .map_err(|e| RecoveryError::Kdf(e.to_string()))?;
    Ok(RecoveryKey::from_bytes(out))
}

fn payload_to_cbor(p: &RecoveryPayload) -> Result<Vec<u8>, RecoveryError> {
    let map = vec![
        (
            Value::Integer(Integer::from(1)),
            Value::Bytes(p.id_s_priv.to_vec()),
        ),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(p.id_d_priv.to_vec()),
        ),
        (
            Value::Integer(Integer::from(3)),
            Value::Bytes(p.id_s_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(4)),
            Value::Bytes(p.id_d_pub.to_vec()),
        ),
        (
            Value::Integer(Integer::from(5)),
            Value::Bytes(p.identity_id.to_vec()),
        ),
        (
            Value::Integer(Integer::from(6)),
            Value::Integer(p.created_at_ms.into()),
        ),
    ];
    let mut out = Vec::with_capacity(192);
    ciborium::ser::into_writer(&Value::Map(map), &mut out)
        .map_err(|e| RecoveryError::Cbor(e.to_string()))?;
    Ok(out)
}

fn payload_from_cbor(bytes: &[u8]) -> Result<RecoveryPayload, RecoveryError> {
    let value: Value =
        ciborium::de::from_reader(bytes).map_err(|e| RecoveryError::Cbor(e.to_string()))?;
    let map = match value {
        Value::Map(m) => m,
        _ => return Err(RecoveryError::Cbor("not a map".into())),
    };
    let mut p = RecoveryPayload {
        id_s_priv: [0u8; 32],
        id_d_priv: [0u8; 32],
        id_s_pub: [0u8; 32],
        id_d_pub: [0u8; 32],
        identity_id: [0u8; 16],
        created_at_ms: 0,
    };
    let mut got = [false; 6];
    for (k, v) in map {
        let id = match k {
            Value::Integer(i) => i128::from(i),
            _ => return Err(RecoveryError::Cbor("non-int key".into())),
        };
        match (id, v) {
            (1, Value::Bytes(b)) if b.len() == 32 => {
                p.id_s_priv.copy_from_slice(&b);
                got[0] = true;
            }
            (2, Value::Bytes(b)) if b.len() == 32 => {
                p.id_d_priv.copy_from_slice(&b);
                got[1] = true;
            }
            (3, Value::Bytes(b)) if b.len() == 32 => {
                p.id_s_pub.copy_from_slice(&b);
                got[2] = true;
            }
            (4, Value::Bytes(b)) if b.len() == 32 => {
                p.id_d_pub.copy_from_slice(&b);
                got[3] = true;
            }
            (5, Value::Bytes(b)) if b.len() == 16 => {
                p.identity_id.copy_from_slice(&b);
                got[4] = true;
            }
            (6, Value::Integer(i)) => {
                let raw: i128 = i128::from(i);
                p.created_at_ms = u64::try_from(raw)
                    .map_err(|_| RecoveryError::Cbor("created_at out of range".into()))?;
                got[5] = true;
            }
            (id, _) if (1..=6).contains(&id) => {
                return Err(RecoveryError::Cbor("field shape error".into()));
            }
            _ => {} // unknown forward-compat field; skip
        }
    }
    if !got.iter().all(|x| *x) {
        return Err(RecoveryError::Cbor("missing required field".into()));
    }
    Ok(p)
}

/// Seal a recovery blob.
///
/// `seed` is the 32-byte BIP-39 derived seed (caller produces it; see
/// `docs/03-crypto/recovery.md`).
///
/// # Errors
/// Argon2id parameter or CBOR encode failure.
pub fn seal_recovery_blob<R: CryptoRngCore>(
    seed: &[u8; 32],
    payload: &RecoveryPayload,
    rng: &mut R,
) -> Result<Vec<u8>, RecoveryError> {
    let mut salt = [0u8; RECOVERY_SALT_LEN];
    rng.fill_bytes(&mut salt);
    let mut nonce = [0u8; RECOVERY_NONCE_LEN];
    rng.fill_bytes(&mut nonce);
    let key = derive_recovery_key(seed, &salt)?;
    let pt = payload_to_cbor(payload)?;
    let mut aad = Vec::with_capacity(RECOVERY_AAD_PREFIX.len() + 16);
    aad.extend_from_slice(RECOVERY_AAD_PREFIX);
    aad.extend_from_slice(&payload.identity_id);
    let ct = aead_seal_xchacha(key.as_bytes(), &nonce, &pt, &aad)
        .map_err(|_| RecoveryError::AuthFailed)?;
    let mut out = Vec::with_capacity(MAGIC_LEN + RECOVERY_SALT_LEN + RECOVERY_NONCE_LEN + ct.len());
    let mut prefix = [0u8; MAGIC_LEN];
    write_prefix(&mut prefix, MagicKind::RecoveryBlob, RECOVERY_MAGIC_VERSION);
    out.extend_from_slice(&prefix);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a recovery blob using the BIP-39 derived seed.
///
/// Caller MUST already know the identity id (e.g., from the OIDC account
/// record) — it's used as AAD. This binding prevents replaying a blob from
/// account A onto account B.
///
/// # Errors
/// Magic / Argon2id / AEAD / CBOR errors.
pub fn unseal_recovery_blob(
    blob: &[u8],
    seed: &[u8; 32],
    expected_identity_id: &[u8; 16],
) -> Result<RecoveryPayload, RecoveryError> {
    if blob.len() < MAGIC_LEN + RECOVERY_SALT_LEN + RECOVERY_NONCE_LEN + 16 {
        return Err(RecoveryError::Truncated);
    }
    let prefix = decode_prefix(&blob[..MAGIC_LEN]).map_err(|_| RecoveryError::BadMagic)?;
    if prefix.kind != MagicKind::RecoveryBlob || prefix.version != RECOVERY_MAGIC_VERSION {
        return Err(RecoveryError::BadMagic);
    }
    let salt: [u8; RECOVERY_SALT_LEN] = blob[MAGIC_LEN..MAGIC_LEN + RECOVERY_SALT_LEN]
        .try_into()
        .map_err(|_| RecoveryError::Truncated)?;
    let nonce_start = MAGIC_LEN + RECOVERY_SALT_LEN;
    let nonce: [u8; RECOVERY_NONCE_LEN] = blob[nonce_start..nonce_start + RECOVERY_NONCE_LEN]
        .try_into()
        .map_err(|_| RecoveryError::Truncated)?;
    let ct = &blob[nonce_start + RECOVERY_NONCE_LEN..];

    let key = derive_recovery_key(seed, &salt)?;
    let mut aad = Vec::with_capacity(RECOVERY_AAD_PREFIX.len() + 16);
    aad.extend_from_slice(RECOVERY_AAD_PREFIX);
    aad.extend_from_slice(expected_identity_id);
    let pt = aead_open_xchacha(key.as_bytes(), &nonce, ct, &aad)
        .map_err(|_| RecoveryError::AuthFailed)?;
    let mut payload = payload_from_cbor(&pt)?;
    if &payload.identity_id != expected_identity_id {
        // We bind the identity_id via AAD, but a paranoid extra check
        // catches a mismatched-but-AEAD-authentic blob.
        payload.zeroize();
        return Err(RecoveryError::AuthFailed);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;

    fn fixture() -> RecoveryPayload {
        RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [5u8; 16],
            created_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn round_trip() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let seed = [9u8; 32];
        let payload = fixture();
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
        // Magic prefix.
        assert_eq!(&blob[..MAGIC_LEN], b"SR\x03\x00\x01");
        let back = unseal_recovery_blob(&blob, &seed, &payload.identity_id).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn wrong_seed_rejects() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let seed = [9u8; 32];
        let payload = fixture();
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
        let wrong = [0u8; 32];
        assert!(matches!(
            unseal_recovery_blob(&blob, &wrong, &payload.identity_id),
            Err(RecoveryError::AuthFailed)
        ));
    }

    #[test]
    fn wrong_identity_rejects() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let seed = [9u8; 32];
        let payload = fixture();
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
        let other = [0u8; 16];
        assert!(matches!(
            unseal_recovery_blob(&blob, &seed, &other),
            Err(RecoveryError::AuthFailed)
        ));
    }
}
