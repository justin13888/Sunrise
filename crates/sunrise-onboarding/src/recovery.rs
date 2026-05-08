//! Recovery flow: BIP-39 → seed → unseal recovery blob → restore identity.
//!
//! Per `spec/03-crypto/recovery.md`. The flow:
//!
//! 1. User logs in via OIDC on a fresh device.
//! 2. Client fetches the encrypted recovery blob from the server.
//! 3. User enters the 24-word BIP-39 recovery phrase.
//! 4. Phrase → 32-byte seed (BIP-39 entropy).
//! 5. Argon2id over seed + salt → recovery key.
//! 6. AEAD-open the recovery blob → identity private keys.
//!
//! v1 entry point: [`recover_identity`]. The BIP-39 derivation step is left
//! to a higher-level UI/CLI surface that integrates with a chosen wordlist
//! library; this crate accepts the already-derived 32-byte seed.

use sunrise_crypto::{unseal_recovery_blob, RecoveryError};
use thiserror::Error;

/// Recovery flow errors.
#[derive(Debug, Error)]
pub enum RecoveryFlowError {
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
}
