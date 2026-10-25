//! The account's key bundle, anchored to a frozen literal (ADR-0069).
//!
//! The rule `frozen_domains.rs` states applies here: the expectation is a
//! literal in `sunrise-crypto-test-vectors`, produced once by this
//! implementation, so a rename of `sunrise.key_bundle.hpke.v1`,
//! `sunrise.key_bundle.sig.v1` or `sunrise.key_bundle.manifest.v1`, or a change
//! to the field layout, fails here rather than quietly writing bundles no
//! earlier build can open. A failure is a format version bump, not a test fix.

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_crypto::keys::{DeviceSigningKeyPair, IdentityDhKeyPair, StreamKey};
use sunrise_crypto::{
    decode_key_bundle, manifest_digest, manifest_of, seal_key_bundle, BundledKey, KeyBundleError,
};
use sunrise_crypto_test_vectors as vectors;
use vectors::at_rest::key_bundle as kb;
use vectors::identity_transition as it;

fn keys() -> Vec<BundledKey> {
    kb::KEYS
        .iter()
        .map(|(stream_id, epoch, key)| BundledKey {
            stream_id: *stream_id,
            epoch: *epoch,
            key: StreamKey::from_bytes(*key),
        })
        .collect()
}

/// The frozen bytes verify under the frozen identity and open under its
/// `ID_D_priv`, with no reference to how this build would produce them.
#[test]
fn the_frozen_key_bundle_opens_under_the_frozen_identity() {
    let bundle = decode_key_bundle(&kb::BUNDLE).expect("the frozen bundle decodes");
    let verified = bundle
        .verify(&it::IDENTITY_ID, &it::IDENTITY_SIGNING_PUBLIC)
        .expect("the frozen bundle verifies");
    assert_eq!(
        verified.signer().body.device_id,
        it::device_cert::DEVICE_ID,
        "published by the frozen device"
    );
    let opened = verified
        .open(&IdentityDhKeyPair::from_secret_bytes(
            it::IDENTITY_DH_SECRET,
        ))
        .expect("the frozen bundle opens");
    assert_eq!(opened, keys());
    assert_eq!(bundle.header().generated_at_ms, kb::GENERATED_AT_MS);
    assert_eq!(
        hex::encode(manifest_digest(&bundle.header().manifest)),
        hex::encode(kb::MANIFEST_DIGEST),
        "sunrise.key_bundle.manifest.v1 drifted"
    );
    // Any other identity's key opens nothing.
    assert_eq!(
        verified
            .open(&IdentityDhKeyPair::from_secret_bytes([0x42; 32]))
            .unwrap_err(),
        KeyBundleError::Open
    );
}

/// Sealing the same inputs under the same RNG is byte exact.
#[test]
fn the_frozen_key_bundle_is_byte_exact() {
    let d_s = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    let mut rng = ChaCha20Rng::seed_from_u64(kb::RNG_SEED);
    let sealed = seal_key_bundle(
        &it::IDENTITY_ID,
        &it::IDENTITY_DH_PUBLIC,
        &it::device_cert::ENCODED,
        kb::GENERATED_AT_MS,
        &keys(),
        &|m| d_s.sign(m),
        &mut rng,
    )
    .unwrap();
    assert_eq!(
        hex::encode(&sealed),
        hex::encode(kb::BUNDLE),
        "the key bundle encoding, its HPKE info or AAD, or its signature domain drifted"
    );
    assert_eq!(manifest_digest(&manifest_of(&keys())), kb::MANIFEST_DIGEST);
}
