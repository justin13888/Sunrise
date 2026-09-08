//! Recovery flow: BIP-39 → seed → unseal recovery blob → restore identity.
//!
//! Per `docs/03-crypto/recovery.md`. The flow:
//!
//! 1. User logs in via OIDC on a fresh device.
//! 2. Client fetches the encrypted recovery blob from the server.
//! 3. User enters the 24-word BIP-39 recovery phrase.
//! 4. Phrase → 32-byte seed (BIP-39 entropy).
//! 5. Argon2id over seed + salt → recovery key.
//! 6. AEAD-open the recovery blob → identity private keys.
//!
//! Steps 3 and 4 are [`recover_identity_from_code`], which is the whole flow
//! from what a user types. This crate used to say the BIP-39 step was "left to
//! a higher-level UI/CLI surface that integrates with a chosen wordlist
//! library" and no such surface existed anywhere in the workspace, so a typed
//! recovery code had nothing to be handed to; `sunrise_crypto::bip39` is that
//! codec now. [`recover_identity`] remains for a caller that already holds the
//! 32-byte seed.

use sunrise_crypto::bip39::{decode_recovery_code, Bip39Error};
use sunrise_crypto::{unseal_recovery_blob, RecoveryError};
use thiserror::Error;

/// Recovery flow errors.
#[derive(Debug, Error)]
pub enum RecoveryFlowError {
    /// The typed recovery code is not a well-formed 24-word BIP-39 mnemonic.
    ///
    /// Kept apart from [`RecoveryFlowError::Crypto`] because it is the failure
    /// a user can *fix by retyping*, and it is decided before Argon2id runs —
    /// which on the slowest supported device is several seconds of work that a
    /// mistyped word should not have to wait for.
    #[error(transparent)]
    Code(#[from] Bip39Error),
    /// Wrapped crypto-layer error from the recovery blob unseal.
    #[error(transparent)]
    Crypto(#[from] RecoveryError),
}

/// Restore identity keys from a recovery blob using the BIP-39 derived seed.
///
/// `expected_identity_id` MUST come from the OIDC account record; binding
/// it as AAD prevents replay across accounts.
pub fn recover_identity(
    blob: &[u8],
    seed: &[u8; 32],
    expected_identity_id: &[u8; 16],
) -> Result<sunrise_crypto::recovery::RecoveryPayload, RecoveryFlowError> {
    let payload = unseal_recovery_blob(blob, seed, expected_identity_id)?;
    Ok(payload)
}

/// Restore identity keys from a recovery blob and the 24 words a user typed.
///
/// The whole of steps 2 to 5 of `docs/03-crypto/recovery.md` §Recovery flow:
/// the code's checksum is verified first, so a typo surfaces immediately
/// rather than after an Argon2id run, and the blob is then opened under the
/// seed those words carry.
///
/// `expected_identity_id` MUST come from the OIDC account record, for the same
/// reason [`recover_identity`] says so: it is the AAD, and it is what stops a
/// blob from one account being replayed against another.
///
/// # Errors
/// [`RecoveryFlowError::Code`] for a mistyped or wrong-length code, and
/// [`RecoveryFlowError::Crypto`] for a code that is well formed and simply not
/// this account's — the two are distinguishable because the advice differs.
pub fn recover_identity_from_code(
    blob: &[u8],
    recovery_code: &str,
    expected_identity_id: &[u8; 16],
) -> Result<sunrise_crypto::recovery::RecoveryPayload, RecoveryFlowError> {
    let seed = decode_recovery_code(recovery_code)?;
    recover_identity(blob, &seed, expected_identity_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::SeedableRng;
    use sunrise_crypto::recovery::RecoveryPayload;
    use sunrise_crypto::seal_recovery_blob;

    #[test]
    fn round_trip() {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let seed = [9u8; 32];
        let payload = RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [5u8; 16],
            created_at_ms: 1,
        };
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();
        let back = recover_identity(&blob, &seed, &payload.identity_id).unwrap();
        assert_eq!(back, payload);
    }

    /// The flow a user actually runs: twenty-four words in, identity out.
    #[test]
    fn a_typed_recovery_code_restores_the_identity() {
        let mut rng = ChaCha20Rng::seed_from_u64(11);
        let seed = [0x2bu8; 32];
        let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
        let payload = RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [5u8; 16],
            created_at_ms: 1,
        };
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();

        let back = recover_identity_from_code(&blob, code.reveal(), &payload.identity_id).unwrap();
        assert_eq!(back, payload);
    }

    /// A mistyped word is refused before Argon2id runs, and is reported as a
    /// code failure rather than as a decryption failure — the user's next
    /// action is "check what you typed", not "find another code".
    #[test]
    fn a_mistyped_code_is_refused_as_a_code() {
        let mut rng = ChaCha20Rng::seed_from_u64(12);
        let seed = [0x2cu8; 32];
        let code = sunrise_crypto::bip39::encode_recovery_code(&seed);
        let payload = RecoveryPayload {
            id_s_priv: [1u8; 32],
            id_d_priv: [2u8; 32],
            id_s_pub: [3u8; 32],
            id_d_pub: [4u8; 32],
            identity_id: [6u8; 16],
            created_at_ms: 1,
        };
        let blob = seal_recovery_blob(&seed, &payload, &mut rng).unwrap();

        let mut words: Vec<&str> = code.reveal().split(' ').collect();
        words.pop();
        assert!(matches!(
            recover_identity_from_code(&blob, &words.join(" "), &payload.identity_id),
            Err(RecoveryFlowError::Code(_))
        ));

        // And a well-formed code for another account fails the AEAD, which is
        // the other half: the two errors must not collapse.
        let other = sunrise_crypto::bip39::encode_recovery_code(&[0x2du8; 32]);
        assert!(matches!(
            recover_identity_from_code(&blob, other.reveal(), &payload.identity_id),
            Err(RecoveryFlowError::Crypto(_))
        ));
    }
}
