//! Identity ID derivation.
//!
//! Per `docs/03-crypto/identity-and-device-keys.md`:
//!
//! ```text
//! identity_id_bytes = BLAKE3.derive_key("sunrise.identity_id.v1", ID_S_pub, 16)
//! identity_id_str   = "idn_" || crockford_base32(identity_id_bytes)
//! ```
//!
//! Stable forever; does not change on rotation.

use crate::blake3_kdf::derive_key;
use sunrise_id::{EntityKind, EntityRef};

/// 16-byte raw identity id.
pub type IdentityId = [u8; 16];

/// Derive the identity id from `ID_S_pub` (32-byte Ed25519 public key).
#[must_use]
pub fn identity_id_from_pub(id_s_pub: &[u8; 32]) -> IdentityId {
    let bytes = derive_key("sunrise.identity_id.v1", id_s_pub, 16);
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

/// 16-byte raw device id.
pub type DeviceId = [u8; 16];

/// Derive the device id from `D_S_pub` (32-byte Ed25519 public key).
///
/// ```text
/// device_id = BLAKE3.derive_key("sunrise.device_id.v1", D_S_pub, 16)
/// ```
///
/// Lives here rather than in `sunrise-core` because two crates now need it and
/// neither can own it. A device's id is a *derivation* of the key it was minted
/// from, which is what lets a sponsor check that a joiner's pairing request
/// names the id its own `D_S_pub` implies rather than one it chose — a claim
/// that would otherwise simply be believed, and that decides which row the
/// account's revocation register will one day address.
#[must_use]
pub fn device_id_from_pub(d_s_pub: &[u8; 32]) -> DeviceId {
    let bytes = derive_key("sunrise.device_id.v1", d_s_pub, 16);
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
}

/// Build a typed `EntityRef` for the identity from `ID_S_pub`.
#[must_use]
pub fn identity_entity_ref(id_s_pub: &[u8; 32]) -> EntityRef {
    let id = identity_id_from_pub(id_s_pub);
    EntityRef::new(EntityKind::Identity, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let pubk = [7u8; 32];
        let a = identity_id_from_pub(&pubk);
        let b = identity_id_from_pub(&pubk);
        assert_eq!(a, b);
    }

    #[test]
    fn different_pubs_diverge() {
        let a = identity_id_from_pub(&[1u8; 32]);
        let b = identity_id_from_pub(&[2u8; 32]);
        assert_ne!(a, b);
    }

    #[test]
    fn device_ids_are_deterministic_and_separate_from_identity_ids() {
        let pubk = [7u8; 32];
        assert_eq!(device_id_from_pub(&pubk), device_id_from_pub(&pubk));
        assert_ne!(
            device_id_from_pub(&[1u8; 32]),
            device_id_from_pub(&[2u8; 32])
        );
        // Different context strings, so one key never derives a device id that
        // is also a valid identity id. A collision would let a device id name
        // an identity row.
        assert_ne!(device_id_from_pub(&pubk), identity_id_from_pub(&pubk));
    }

    #[test]
    fn entity_ref_uses_identity_kind() {
        let r = identity_entity_ref(&[7u8; 32]);
        assert_eq!(r.kind(), EntityKind::Identity);
        assert!(r.to_str().starts_with("idn_"));
    }
}
