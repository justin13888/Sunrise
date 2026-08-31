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
    fn entity_ref_uses_identity_kind() {
        let r = identity_entity_ref(&[7u8; 32]);
        assert_eq!(r.kind(), EntityKind::Identity);
        assert!(r.to_str().starts_with("idn_"));
    }
}
