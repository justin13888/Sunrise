//! The viewer share grant, anchored to a frozen literal
//! (`docs/03-crypto/sharing-with-others.md`).
//!
//! The rule `frozen_domains.rs` states applies here: the expectation is a
//! literal in `sunrise-crypto-test-vectors`, produced once by this
//! implementation, so a rename of `sunrise.hpke.share_grant.v1` or
//! `sunrise.share_grant.v1`, a change to the AAD that binds the granter, or a
//! change to the payload's field numbering fails here rather than quietly
//! writing grants no other build can open. A failure is a format version
//! bump, not a test fix.

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair, StreamKey};
use sunrise_crypto::{
    decode_share_grant, identity_id_from_pub, seal_share_grant, share_grant_info, ShareGrantError,
    ShareGrantTerms, ShareRole, StreamMetadata,
};
use sunrise_crypto_test_vectors::share_grant as sg;

fn terms() -> ShareGrantTerms {
    ShareGrantTerms {
        stream_id: sg::STREAM_ID,
        epoch: sg::EPOCH,
        recipient_identity_id: sg::RECIPIENT_IDENTITY_ID,
        role: ShareRole::Viewer,
        expires_at_ms: Some(sg::EXPIRES_AT_MS),
    }
}

fn metadata() -> StreamMetadata {
    StreamMetadata {
        name: sg::NAME.into(),
        color: sg::COLOR.into(),
    }
}

#[test]
fn the_share_grant_info_is_frozen() {
    assert_eq!(
        hex::encode(share_grant_info(
            &sg::STREAM_ID,
            sg::EPOCH,
            &sg::RECIPIENT_IDENTITY_ID
        )),
        hex::encode(sg::INFO),
        "sunrise.hpke.share_grant.v1 drifted"
    );
}

/// The frozen bytes decode, verify under the frozen granter and open under the
/// frozen recipient, with no reference to how this build would produce them.
#[test]
fn the_frozen_share_grant_opens_for_its_recipient() {
    let grant = decode_share_grant(&sg::ENCODED).expect("the frozen grant decodes");
    assert_eq!(grant.terms(), &terms());
    grant
        .verify(&sg::GRANTER_SIGNING_PUBLIC)
        .expect("the frozen grant verifies under its granter");
    let recipient = IdentityDhKeyPair::from_secret_bytes(sg::RECIPIENT_DH_SECRET);
    assert_eq!(recipient.public_bytes(), sg::RECIPIENT_DH_PUBLIC);
    let opened = grant
        .open(
            &sg::GRANTER_SIGNING_PUBLIC,
            &sg::RECIPIENT_IDENTITY_ID,
            &recipient,
            sg::EXPIRES_AT_MS - 1,
        )
        .expect("the frozen grant opens");
    assert_eq!(opened.key, StreamKey::from_bytes(sg::STREAM_KEY));
    assert_eq!(opened.metadata, metadata());
    assert_eq!(
        grant.open(
            &sg::GRANTER_SIGNING_PUBLIC,
            &sg::RECIPIENT_IDENTITY_ID,
            &recipient,
            sg::EXPIRES_AT_MS,
        ),
        Err(ShareGrantError::Expired)
    );
    assert_eq!(
        grant.encode().expect("re-encode"),
        sg::ENCODED,
        "the frozen grant re-encodes byte for byte"
    );
}

/// Sealing the same inputs under the same RNG is byte exact.
#[test]
fn the_frozen_share_grant_is_byte_exact() {
    let granter = IdentitySigningKeyPair::from_secret_bytes(&sg::GRANTER_SIGNING_SECRET);
    assert_eq!(granter.public_bytes(), sg::GRANTER_SIGNING_PUBLIC);
    assert_ne!(
        identity_id_from_pub(&granter.public_bytes()),
        sg::RECIPIENT_IDENTITY_ID,
        "the granter and the recipient are two identities"
    );
    let mut rng = ChaCha20Rng::seed_from_u64(sg::RNG_SEED);
    let grant = seal_share_grant(
        &granter,
        terms(),
        &sg::RECIPIENT_DH_PUBLIC,
        &StreamKey::from_bytes(sg::STREAM_KEY),
        &metadata(),
        &mut rng,
    )
    .expect("seal");
    assert_eq!(
        hex::encode(grant.encode().expect("encode")),
        hex::encode(sg::ENCODED),
        "the share grant encoding, its HPKE info or AAD, or its signature domain drifted"
    );
}
