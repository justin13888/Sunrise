//! `identity_revocation` — a device revocation signed by the account identity
//! (ADR-0058 §2).
//!
//! An ordinary `device_revoke` is a claim by the device that sent it, and the
//! revocation fold weighs it by that device's standing. An authority
//! revocation is the same claim made by the account identity in force: the
//! holder of `ID_S_priv` signs it, and every replica checks the signature
//! under the `ID_S_pub` its identity chain folds to.
//!
//! ```text
//! claim        = canonical_cbor({identity_id, sender_device_id, revoked_device_id,
//!                                hlc_physical_ms, hlc_logical, reason_code})
//! identity_sig = Ed25519(ID_S_priv, "sunrise.device_revoke.identity.v1" || BLAKE3(claim))
//! ```
//!
//! `sender_device_id` and the HLC are the authenticated envelope's, not the
//! payload's, so the signature cannot be lifted into another device's op or
//! re-dated. A verifier rebuilds the claim from the ledger row, which stores
//! every one of them.
//!
//! `reason_code` is the reason's wire spelling (`"Lost"`, `"Stolen"`, …): the
//! text a unit variant of the payload's reason enum encodes as, so the claim
//! does not depend on that enum's Rust type.

use crate::keys::{verify_ed25519, IdentitySigningKeyPair};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Domain separator for the identity's signature over a revocation claim.
///
/// Distinct from every other Ed25519 domain in the workspace, so a revocation
/// signature can never be read as a transition or a certificate signature.
const DOMAIN: &[u8] = b"sunrise.device_revoke.identity.v1";

/// What the account identity signs when it revokes a device.
///
/// Encoded with [`sunrise_cbor::encode_canonical`], so the field order in this
/// declaration is not the byte order: keys sort by their encoded bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationClaim {
    /// The identity that signs: `identity_id_from_pub(ID_S_pub)`.
    #[serde(with = "serde_bytes")]
    pub identity_id: [u8; 16],
    /// The device whose envelope carries the op.
    #[serde(with = "serde_bytes")]
    pub sender_device_id: [u8; 16],
    /// The device being revoked.
    #[serde(with = "serde_bytes")]
    pub revoked_device_id: [u8; 16],
    /// The envelope HLC's physical half.
    pub hlc_physical_ms: u64,
    /// The envelope HLC's logical half.
    pub hlc_logical: u32,
    /// The reason's wire spelling.
    pub reason_code: String,
}

/// Errors produced by the revocation-claim primitives.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RevocationClaimError {
    /// Canonical-CBOR encoding of the claim failed.
    #[error("revocation claim CBOR error: {0}")]
    Cbor(String),
    /// The signature does not verify under the given `ID_S_pub`.
    #[error("identity_sig does not verify under ID_S_pub")]
    Verify,
}

impl RevocationClaim {
    /// `"sunrise.device_revoke.identity.v1" || BLAKE3(canonical_cbor(claim))`,
    /// the exact bytes the identity signs.
    ///
    /// # Errors
    /// [`RevocationClaimError::Cbor`] if the claim cannot be encoded, which is
    /// structural rather than input-dependent.
    pub fn signed_message(&self) -> Result<Vec<u8>, RevocationClaimError> {
        let cbor = sunrise_cbor::encode_canonical(self)
            .map_err(|e| RevocationClaimError::Cbor(e.to_string()))?;
        let mut msg = Vec::with_capacity(DOMAIN.len() + 32);
        msg.extend_from_slice(DOMAIN);
        msg.extend_from_slice(blake3::hash(&cbor).as_bytes());
        Ok(msg)
    }

    /// Sign the claim with the identity's `ID_S_priv`.
    ///
    /// The caller is responsible for `identity_id` naming the key it signs
    /// with: a claim that names another identity produces a signature no
    /// verifier accepts, because verifiers check it under the `ID_S_pub` of
    /// the identity the row names.
    ///
    /// # Errors
    /// [`RevocationClaimError::Cbor`], as [`Self::signed_message`].
    pub fn sign(&self, key: &IdentitySigningKeyPair) -> Result<[u8; 64], RevocationClaimError> {
        Ok(key.sign(&self.signed_message()?))
    }

    /// Check `sig` over this claim under `id_s_pub`.
    ///
    /// # Errors
    /// [`RevocationClaimError::Verify`] when it does not verify, or
    /// [`RevocationClaimError::Cbor`] as [`Self::signed_message`].
    pub fn verify(&self, id_s_pub: &[u8; 32], sig: &[u8; 64]) -> Result<(), RevocationClaimError> {
        if verify_ed25519(id_s_pub, &self.signed_message()?, sig) {
            Ok(())
        } else {
            Err(RevocationClaimError::Verify)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::identity_id_from_pub;

    fn claim(key: &IdentitySigningKeyPair) -> RevocationClaim {
        RevocationClaim {
            identity_id: identity_id_from_pub(&key.public_bytes()),
            sender_device_id: [1; 16],
            revoked_device_id: [2; 16],
            hlc_physical_ms: 1_700_000_000_000,
            hlc_logical: 3,
            reason_code: "Stolen".into(),
        }
    }

    #[test]
    fn a_signed_claim_verifies_under_its_identity() {
        let key = IdentitySigningKeyPair::from_secret_bytes(&[7; 32]);
        let c = claim(&key);
        let sig = c.sign(&key).unwrap();
        assert_eq!(c.verify(&key.public_bytes(), &sig), Ok(()));
    }

    /// Every field is covered: changing any one of them, the sender and the
    /// HLC included, breaks the signature, so it cannot be moved into another
    /// device's op or re-dated.
    #[test]
    fn every_field_of_the_claim_is_signed() {
        let key = IdentitySigningKeyPair::from_secret_bytes(&[7; 32]);
        let c = claim(&key);
        let sig = c.sign(&key).unwrap();
        let pk = key.public_bytes();
        let variants = [
            RevocationClaim {
                identity_id: [9; 16],
                ..c.clone()
            },
            RevocationClaim {
                sender_device_id: [9; 16],
                ..c.clone()
            },
            RevocationClaim {
                revoked_device_id: [9; 16],
                ..c.clone()
            },
            RevocationClaim {
                hlc_physical_ms: c.hlc_physical_ms - 1,
                ..c.clone()
            },
            RevocationClaim {
                hlc_logical: c.hlc_logical + 1,
                ..c.clone()
            },
            RevocationClaim {
                reason_code: "Lost".into(),
                ..c.clone()
            },
        ];
        for v in variants {
            assert_eq!(
                v.verify(&pk, &sig),
                Err(RevocationClaimError::Verify),
                "{v:?}"
            );
        }
    }

    #[test]
    fn another_identity_does_not_verify() {
        let key = IdentitySigningKeyPair::from_secret_bytes(&[7; 32]);
        let other = IdentitySigningKeyPair::from_secret_bytes(&[8; 32]);
        let c = claim(&key);
        let sig = c.sign(&other).unwrap();
        assert_eq!(
            c.verify(&key.public_bytes(), &sig),
            Err(RevocationClaimError::Verify)
        );
    }

    /// The domain is part of the message, so a signature over the bare claim
    /// hash, or under another domain, does not verify.
    #[test]
    fn the_domain_separator_is_signed() {
        let key = IdentitySigningKeyPair::from_secret_bytes(&[7; 32]);
        let c = claim(&key);
        let msg = c.signed_message().unwrap();
        assert!(msg.starts_with(b"sunrise.device_revoke.identity.v1"));
        assert_eq!(msg.len(), DOMAIN.len() + 32);
        let bare = key.sign(&msg[DOMAIN.len()..]);
        assert_eq!(
            c.verify(&key.public_bytes(), &bare),
            Err(RevocationClaimError::Verify)
        );
    }
}
