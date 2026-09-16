//! The two pairing domain constants, anchored to frozen literals.
//!
//! `sunrise.pair_sas.v1` and `sunrise.account_email_hash.v1` are the crate's
//! whole domain-separation surface. Neither value's derivation is transmitted:
//! two devices independently compute a SAS and show it to a human, and two
//! clients independently compute a rate-limit bucket key for one account. A
//! build that spelled either context differently is self-consistent and wrong,
//! which is what a literal in a crate with no dependencies catches and a
//! round-trip test cannot.
//!
//! A failure here is a crypto-suite version bump per
//! `docs/03-crypto/key-rotation.md`, not a test fix.

use sunrise_crypto_test_vectors::protocol;

#[test]
fn sas_vectors_hold() {
    for v in protocol::SAS_VECTORS {
        assert_eq!(
            sunrise_pairing::compute_sas(&v.handshake_hash),
            v.sas,
            "the sunrise.pair_sas.v1 SAS drifted — two devices would now show \
             the user two different codes for one handshake"
        );
    }
}

#[test]
fn account_email_hash_vectors_hold() {
    for v in protocol::EMAIL_HASH_VECTORS {
        assert_eq!(
            sunrise_pairing::account_email_hash(v.email),
            v.hash,
            "the sunrise.account_email_hash.v1 bucket key drifted for {:?}",
            v.email
        );
    }
}
