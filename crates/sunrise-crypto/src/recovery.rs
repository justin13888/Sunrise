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
//!         6: created_at (uint, ms since epoch),
//!         7: account_id_s_pub (32 B, optional),
//!         8: [* meta_genesis_key (32 B)] (optional)
//!     }),
//!     aad       = "sunrise.recovery_blob.v1" || account_identity_id
//! )
//! ```
//!
//! The blob is prefixed with the 5-byte magic from `sunrise-cbor::magic`
//! (`SR\x03\x00\x01`) and stored as `magic || salt(16) || nonce(24) || ct`.
//!
//! # A blob re-issued after an identity rotation (ADR-0070)
//!
//! Fields 7 and 8 are present only in a blob sealed after an identity
//! rotation that did not carry the code, which puts the **successor**
//! identity in fields 1 to 5. Field 7 is the account's genesis `ID_S_pub`,
//! the identity the account record names and therefore the one the AAD binds:
//! `account_identity_id` is `identity_id_from_pub(field 7)` when field 7 is
//! present and field 5 otherwise, so a reader that knows only the account
//! record opens either kind. Field 8 is the vault-meta stream's genesis key,
//! which a vault holding only the successor cannot derive.
//!
//! A reader that predates fields 7 and 8 skips them, opens the AEAD, and
//! refuses the blob because field 5 is not the AAD's identity: a clean
//! failure rather than a vault restored under the wrong anchor.
//!
//! # A retired code
//!
//! When a blob is replaced the relay keeps the first [`RETIRED_CHECK_LEN`]
//! bytes of the old one: the magic, the salt, the nonce and the first four
//! bytes of ciphertext. The plaintext under those four bytes is the fixed CBOR
//! prefix every payload starts with, so [`retired_check_matches`] can tell the
//! code that opened the old blob from any other code, and the kept bytes say
//! nothing about the keys it carried.

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

/// The most vault-meta genesis keys field 8 may carry. One, except where two
/// devices minted the genesis epoch concurrently.
const MAX_META_GENESIS_KEYS: usize = 16;

/// How many ciphertext bytes a retired check keeps. The plaintext under them
/// is fixed by the payload's encoding (see [`known_plaintext_prefixes`]), so
/// they reveal keystream and nothing else.
const RETIRED_CT_PREFIX_LEN: usize = 4;

/// How many bytes of a replaced blob the relay keeps: the magic, the salt,
/// the nonce, and four bytes of ciphertext.
pub const RETIRED_CHECK_LEN: usize =
    MAGIC_LEN + RECOVERY_SALT_LEN + RECOVERY_NONCE_LEN + RETIRED_CT_PREFIX_LEN;

/// Argon2id parameters from the spec.
const ARGON2_M_KIB: u32 = 65536;
const ARGON2_T: u32 = 3;
const ARGON2_P: u32 = 1;

/// Plaintext payload of a recovery blob.
///
/// Four of the six fields are private keys, so this type has a hand-written
/// [`Debug`] that names the identity and redacts the rest — the same rule
/// `sunrise_pairing::PairingPayload` follows, and for the same reason: a
/// derived one would put the whole account in whatever log a caller writes.
#[derive(Clone, PartialEq, Eq)]
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
    /// The account's genesis `ID_S_pub` (field 7), when fields 1 to 5 carry a
    /// successor identity. `None` in a blob of the genesis identity itself.
    pub account_id_s_pub: Option<[u8; 32]>,
    /// The vault-meta stream's genesis keys (field 8), carried by a blob of a
    /// successor identity, which cannot derive them. Empty otherwise.
    pub meta_genesis_keys: Vec<[u8; 32]>,
}

impl RecoveryPayload {
    /// The identity the account record names, and so the one the AAD binds:
    /// the genesis when the payload carries a successor, else its own.
    #[must_use]
    pub fn account_identity_id(&self) -> [u8; 16] {
        self.account_id_s_pub
            .map_or(self.identity_id, |pk| crate::identity_id_from_pub(&pk))
    }
}

impl core::fmt::Debug for RecoveryPayload {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RecoveryPayload { identity_id: ")?;
        for b in &self.identity_id {
            write!(f, "{b:02x}")?;
        }
        write!(f, ", created_at_ms: {}, .. }}", self.created_at_ms)
    }
}

impl Zeroize for RecoveryPayload {
    fn zeroize(&mut self) {
        self.id_s_priv.zeroize();
        self.id_d_priv.zeroize();
        self.id_s_pub.zeroize();
        self.id_d_pub.zeroize();
        self.identity_id.zeroize();
        self.created_at_ms.zeroize();
        for k in &mut self.meta_genesis_keys {
            k.zeroize();
        }
        self.meta_genesis_keys.clear();
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
    /// The inner payload nests deeper than [`sunrise_cbor::MAX_VALUE_DEPTH`]
    /// arrays, maps and tags. The payload is one map of scalars; an unknown
    /// field a newer build adds is held to the same bound, the map included.
    #[error("recovery payload CBOR nests too deep")]
    TooDeep,
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
    let mut map = map;
    if let Some(pk) = p.account_id_s_pub {
        map.push((Value::Integer(Integer::from(7)), Value::Bytes(pk.to_vec())));
    }
    if !p.meta_genesis_keys.is_empty() {
        map.push((
            Value::Integer(Integer::from(8)),
            Value::Array(
                p.meta_genesis_keys
                    .iter()
                    .map(|k| Value::Bytes(k.to_vec()))
                    .collect(),
            ),
        ));
    }
    let mut out = Vec::with_capacity(192);
    ciborium::ser::into_writer(&Value::Map(map), &mut out)
        .map_err(|e| RecoveryError::Cbor(e.to_string()))?;
    Ok(out)
}

fn payload_from_cbor(bytes: &[u8]) -> Result<RecoveryPayload, RecoveryError> {
    let value = sunrise_cbor::decode_value(&mut &bytes[..]).map_err(|e| match e {
        sunrise_cbor::ValueDecodeError::TooDeep => RecoveryError::TooDeep,
        sunrise_cbor::ValueDecodeError::Malformed(m) => RecoveryError::Cbor(m),
    })?;
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
        account_id_s_pub: None,
        meta_genesis_keys: Vec::new(),
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
            (7, Value::Bytes(b)) if b.len() == 32 => {
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&b);
                p.account_id_s_pub = Some(pk);
            }
            (8, Value::Array(keys)) if keys.len() <= MAX_META_GENESIS_KEYS => {
                for k in keys {
                    let Value::Bytes(b) = k else {
                        return Err(RecoveryError::Cbor("field shape error".into()));
                    };
                    let key: [u8; 32] = b
                        .as_slice()
                        .try_into()
                        .map_err(|_| RecoveryError::Cbor("field shape error".into()))?;
                    p.meta_genesis_keys.push(key);
                }
            }
            (id, _) if (1..=8).contains(&id) => {
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
    aad.extend_from_slice(&payload.account_identity_id());
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
    if &payload.account_identity_id() != expected_identity_id {
        // We bind the identity_id via AAD, but a paranoid extra check
        // catches a mismatched-but-AEAD-authentic blob.
        payload.zeroize();
        return Err(RecoveryError::AuthFailed);
    }
    Ok(payload)
}

/// The plaintext every payload's ciphertext starts with: a CBOR map header
/// (six entries, or seven or eight in a re-issued blob), key `1`, and the
/// header of a 32-byte byte string. Nothing secret starts before byte 4.
const fn known_plaintext_prefixes() -> [[u8; RETIRED_CT_PREFIX_LEN]; 3] {
    [
        [0xa6, 0x01, 0x58, 0x20],
        [0xa7, 0x01, 0x58, 0x20],
        [0xa8, 0x01, 0x58, 0x20],
    ]
}

/// Whether `seed` is the code that opened the blob `check` was cut from.
///
/// `check` is what the relay kept of a replaced blob ([`RETIRED_CHECK_LEN`]
/// bytes). The recovery key is re-derived from the kept salt, and the
/// payload's fixed first four plaintext bytes are sealed under the kept nonce:
/// the cipher's output there equals the kept ciphertext bytes if and only if
/// the key is the same. Thirty-two bits, so a wrong code that passed the
/// BIP-39 checksum matches about once in 2^30 tries (three candidate map
/// headers).
///
/// Answers yes or no and nothing more. The kept bytes carry no plaintext but
/// the fixed prefix, so a code that matches learns that it was once the
/// account's code, and no key.
///
/// # Errors
/// [`RecoveryError::Truncated`] or [`RecoveryError::BadMagic`] for a check
/// that is not one, and Argon2id failures.
pub fn retired_check_matches(check: &[u8], seed: &[u8; 32]) -> Result<bool, RecoveryError> {
    if check.len() < RETIRED_CHECK_LEN {
        return Err(RecoveryError::Truncated);
    }
    let prefix = decode_prefix(&check[..MAGIC_LEN]).map_err(|_| RecoveryError::BadMagic)?;
    if prefix.kind != MagicKind::RecoveryBlob || prefix.version != RECOVERY_MAGIC_VERSION {
        return Err(RecoveryError::BadMagic);
    }
    let salt: [u8; RECOVERY_SALT_LEN] = check[MAGIC_LEN..MAGIC_LEN + RECOVERY_SALT_LEN]
        .try_into()
        .map_err(|_| RecoveryError::Truncated)?;
    let nonce_start = MAGIC_LEN + RECOVERY_SALT_LEN;
    let nonce: [u8; RECOVERY_NONCE_LEN] = check[nonce_start..nonce_start + RECOVERY_NONCE_LEN]
        .try_into()
        .map_err(|_| RecoveryError::Truncated)?;
    let kept = &check[nonce_start + RECOVERY_NONCE_LEN..RETIRED_CHECK_LEN];
    let key = derive_recovery_key(seed, &salt)?;
    let mut matched = subtle::Choice::from(0u8);
    for candidate in known_plaintext_prefixes() {
        // XChaCha20-Poly1305 encrypts with the keystream from block 1 on,
        // whatever the AAD, so sealing the known prefix again reproduces the
        // first ciphertext bytes exactly when the key is the same. The output
        // never leaves this function, so the reused nonce exposes nothing.
        let ct = aead_seal_xchacha(key.as_bytes(), &nonce, &candidate, b"")
            .map_err(|_| RecoveryError::AuthFailed)?;
        matched |= subtle::ConstantTimeEq::ct_eq(&ct[..RETIRED_CT_PREFIX_LEN], kept);
    }
    Ok(bool::from(matched))
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
            account_id_s_pub: None,
            meta_genesis_keys: Vec::new(),
        }
    }

    /// A blob of a successor identity: sealed under the genesis's id, which is
    /// the one the account record names, and carrying the vault-meta genesis
    /// key a vault of the successor cannot derive.
    #[test]
    fn a_reissued_blob_opens_under_the_account_identity() {
        let mut rng = ChaCha20Rng::seed_from_u64(8);
        let seed = [9u8; 32];
        let genesis_pub = [7u8; 32];
        let payload = RecoveryPayload {
            account_id_s_pub: Some(genesis_pub),
            meta_genesis_keys: vec![[0x42; 32]],
            ..fixture()
        };
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
        let account = crate::identity_id_from_pub(&genesis_pub);
        assert_eq!(
            unseal_recovery_blob(&blob, &seed, &account).unwrap(),
            payload
        );
        assert!(matches!(
            unseal_recovery_blob(&blob, &seed, &payload.identity_id),
            Err(RecoveryError::AuthFailed)
        ));
    }

    /// What the relay keeps of a replaced blob says which code opened it and
    /// nothing else: that code matches, another does not, and a blob of either
    /// shape is recognised.
    #[test]
    fn a_retired_check_recognises_its_own_code_and_no_other() {
        let mut rng = ChaCha20Rng::seed_from_u64(9);
        let seed = [9u8; 32];
        for payload in [
            fixture(),
            RecoveryPayload {
                account_id_s_pub: Some([7u8; 32]),
                meta_genesis_keys: vec![[0x42; 32]],
                ..fixture()
            },
        ] {
            let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
            let check = &blob[..RETIRED_CHECK_LEN];
            assert!(retired_check_matches(check, &seed).unwrap());
            assert!(!retired_check_matches(check, &[8u8; 32]).unwrap());
        }
        assert!(matches!(
            retired_check_matches(&[0u8; 10], &seed),
            Err(RecoveryError::Truncated)
        ));
    }

    /// The payload with one more field this build does not know: `levels`
    /// one-element arrays around an integer.
    fn payload_with_unknown(levels: usize) -> Vec<u8> {
        let Value::Map(mut map) =
            sunrise_cbor::decode_value(&mut payload_to_cbor(&fixture()).unwrap().as_slice())
                .unwrap()
        else {
            panic!("the payload is a map");
        };
        let mut nest = Value::Integer(1.into());
        for _ in 0..levels {
            nest = Value::Array(vec![nest]);
        }
        map.push((Value::Integer(99.into()), nest));
        let mut out = Vec::new();
        ciborium::ser::into_writer(&Value::Map(map), &mut out).unwrap();
        out
    }

    /// An unknown field is skipped up to the framing bound, the payload map
    /// counted, and one level past it refuses the payload as too deep.
    #[test]
    fn the_payload_is_bounded_at_the_framing_depth() {
        let bound = sunrise_cbor::MAX_VALUE_DEPTH;
        let fits = payload_with_unknown(bound - 1);
        assert_eq!(payload_from_cbor(&fits).unwrap(), fixture());
        assert!(matches!(
            payload_from_cbor(&payload_with_unknown(bound)),
            Err(RecoveryError::TooDeep)
        ));
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
