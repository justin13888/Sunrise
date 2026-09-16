//! Every production domain-separation constant this crate owns, anchored to a
//! frozen literal.
//!
//! `frozen_vectors.rs` beside this file anchors the primitives — the two id
//! derivations, `derive_key`, the stream roots, the two envelopes, the HPKE
//! key envelope and the blob chunk. This file anchors the rest: the device
//! cert's signature domain, the two `identity_transition` signature domains,
//! the roster and shares digest domains, the two HPKE share `info` prefixes,
//! the stream-key wrap AAD and the recovery blob's AAD.
//!
//! # The rule these tests exist to enforce
//!
//! A vector that is computed rather than written down is not a vector. Nothing
//! here builds an expected value by calling the code it then checks: every
//! expectation is a literal in `sunrise-crypto-test-vectors`, a crate with no
//! dependencies at all, produced once by this implementation and pinned. A
//! coordinated rename of a domain string *and* its test is exactly what that
//! separation makes impossible — the literal does not move when the constant
//! does.
//!
//! A failure here is a crypto-suite version bump per
//! `docs/03-crypto/key-rotation.md`, not a test fix.

use sunrise_crypto::identity_transition::{body_hash, IdentityTransitionBody};
use sunrise_crypto::keys::{
    DeviceDhKeyPair, DeviceSigningKeyPair, IdentityDhKeyPair, IdentitySigningKeyPair, StreamKey,
    VaultRootKey,
};
use sunrise_crypto::recovery::RecoveryPayload;
use sunrise_crypto::{
    device_id_from_pub, identity_carry_info, identity_id_from_pub, identity_share_info,
    roster_digest, shares_digest, sign_identity_transition, unseal_recovery_blob,
    unwrap_stream_key, verify_identity_transition, DeviceCert, DeviceCertInner,
    IdentityTransitionSigs, StreamKeyWrapError, DEVICE_SHARE_LEN, IDENTITY_SHARE_LEN,
};
use sunrise_crypto_test_vectors as vectors;
use vectors::identity_transition as it;

// ---------------------------------------------------------------------------
// sunrise.device_id.v1
// ---------------------------------------------------------------------------

#[test]
fn device_id_vectors_hold() {
    for v in vectors::DEVICE_ID_VECTORS {
        assert_eq!(
            device_id_from_pub(&v.d_s_pub),
            v.device_id,
            "device_id drifted for D_S_pub {}",
            hex::encode(v.d_s_pub)
        );
    }
}

// ---------------------------------------------------------------------------
// sunrise.device_cert.v1
// ---------------------------------------------------------------------------

/// The frozen cert's body, rebuilt from the frozen inputs.
fn frozen_cert_body() -> DeviceCertInner {
    DeviceCertInner {
        v: it::device_cert::V,
        device_id: it::device_cert::DEVICE_ID,
        d_s_pub: vectors::DEVICE_SIGNING_PUBLIC,
        d_d_pub: it::device_cert::D_D_PUB,
        identity_id: it::IDENTITY_ID,
        created_at_ms: it::device_cert::CREATED_AT_MS,
        nickname: it::device_cert::NICKNAME.to_string(),
        platform: it::device_cert::PLATFORM.to_string(),
    }
}

/// The identity keypair the frozen cert and the frozen transition are signed
/// with, checked against its frozen public half first.
fn frozen_identity() -> IdentitySigningKeyPair {
    let kp = IdentitySigningKeyPair::from_secret_bytes(&it::IDENTITY_SIGNING_SECRET);
    assert_eq!(kp.public_bytes(), it::IDENTITY_SIGNING_PUBLIC);
    assert_eq!(identity_id_from_pub(&kp.public_bytes()), it::IDENTITY_ID);
    kp
}

#[test]
fn the_frozen_device_cert_is_byte_exact() {
    let cert = DeviceCert::issue(frozen_cert_body(), &frozen_identity()).expect("issue");
    assert_eq!(
        hex::encode(cert.body_bytes()),
        hex::encode(it::device_cert::BODY_BYTES),
        "the DeviceCert body encoding drifted"
    );
    assert_eq!(
        hex::encode(cert.sig),
        hex::encode(it::device_cert::SIG),
        "the DeviceCert signature drifted — the sunrise.device_cert.v1 domain, \
         the body hash, or Ed25519 itself moved"
    );
    assert_eq!(
        hex::encode(cert.to_cbor().expect("to_cbor")),
        hex::encode(it::device_cert::ENCODED),
        "the DeviceCert outer encoding drifted"
    );
}

/// The unconditional half: the frozen bytes verify under the frozen key, with
/// no reference to how this build would have produced them.
///
/// This is the assertion a second implementation has to satisfy, and the one
/// that fails if `sunrise.device_cert.v1` is renamed — the signature covers a
/// domain-prefixed hash, so a different prefix cannot verify.
#[test]
fn the_frozen_device_cert_verifies_under_the_frozen_identity() {
    let cert = DeviceCert::from_cbor(&it::device_cert::ENCODED).expect("frozen cert decodes");
    cert.verify(&it::IDENTITY_SIGNING_PUBLIC)
        .expect("frozen cert verifies");
    assert_eq!(cert.body.device_id, it::device_cert::DEVICE_ID);
    assert_eq!(
        cert.body_bytes(),
        it::device_cert::BODY_BYTES.as_slice(),
        "the bytes the verifier saw are not the frozen body"
    );
}

// ---------------------------------------------------------------------------
// sunrise.identity_share.v1 / sunrise.identity_carry.v1
// ---------------------------------------------------------------------------

/// The two share `info` strings are never transmitted, so a build that spells
/// either differently opens its own shares and nobody else's.
#[test]
fn the_identity_share_info_strings_are_byte_exact() {
    assert_eq!(
        hex::encode(identity_share_info(
            &it::share_info::TO_IDENTITY_ID,
            &it::share_info::DEVICE_ID
        )),
        hex::encode(it::share_info::SHARE),
        "the sunrise.identity_share.v1 info string drifted"
    );
    assert_eq!(
        hex::encode(identity_carry_info(&it::share_info::TO_IDENTITY_ID)),
        hex::encode(it::share_info::CARRY),
        "the sunrise.identity_carry.v1 info string drifted"
    );
}

// ---------------------------------------------------------------------------
// sunrise.identity_roster.v1 / sunrise.identity_shares.v1
// ---------------------------------------------------------------------------

#[test]
fn the_roster_digest_is_byte_exact() {
    let roster = [it::device_cert::ENCODED.as_slice()];
    assert_eq!(
        hex::encode(roster_digest(&roster).expect("roster digest")),
        hex::encode(it::transition::ROSTER_DIGEST),
        "the sunrise.identity_roster.v1 digest drifted"
    );
}

#[test]
fn the_shares_digest_is_byte_exact() {
    assert_eq!(it::transition::DEVICE_SHARE.len(), DEVICE_SHARE_LEN);
    assert_eq!(it::transition::IDENTITY_SHARE.len(), IDENTITY_SHARE_LEN);
    let shares = [(
        it::device_cert::DEVICE_ID,
        it::transition::DEVICE_SHARE.as_slice(),
    )];
    assert_eq!(
        hex::encode(
            shares_digest(&shares, Some(it::transition::IDENTITY_SHARE.as_slice()))
                .expect("shares digest")
        ),
        hex::encode(it::transition::SHARES_DIGEST),
        "the sunrise.identity_shares.v1 digest drifted"
    );
}

// ---------------------------------------------------------------------------
// sunrise.identity_transition.v1 / .succ.v1
// ---------------------------------------------------------------------------

/// The frozen body, rebuilt from the frozen public halves and the two frozen
/// digests — never from a live `roster_digest` call, so this vector still
/// fails when a digest domain moves rather than following it.
const fn frozen_transition_body() -> IdentityTransitionBody {
    IdentityTransitionBody {
        from_identity_id: it::IDENTITY_ID,
        to_identity_id: it::SUCCESSOR_IDENTITY_ID,
        to_id_s_pub: it::SUCCESSOR_SIGNING_PUBLIC,
        to_id_d_pub: it::SUCCESSOR_DH_PUBLIC,
        roster_digest: it::transition::ROSTER_DIGEST,
        shares_digest: it::transition::SHARES_DIGEST,
    }
}

#[test]
fn the_transition_body_encoding_and_hash_are_byte_exact() {
    let body = frozen_transition_body();
    assert_eq!(
        hex::encode(sunrise_cbor::encode_canonical(&body).expect("encode body")),
        hex::encode(it::transition::BODY_CBOR),
        "the IdentityTransitionBody canonical encoding drifted"
    );
    assert_eq!(
        hex::encode(body_hash(&body).expect("body hash")),
        hex::encode(it::transition::BODY_HASH),
        "the IdentityTransitionBody hash drifted"
    );
}

/// Both signature domains at once, in the order the format defines them.
///
/// `next_sig` covers `prev_sig`, so freezing the pair also pins the chaining
/// rule: a build that signed the body alone with the successor key would
/// produce the frozen `prev_sig` and a different `next_sig`.
#[test]
fn the_transition_signature_pair_is_byte_exact() {
    let succ = IdentitySigningKeyPair::from_secret_bytes(&it::SUCCESSOR_SIGNING_SECRET);
    assert_eq!(succ.public_bytes(), it::SUCCESSOR_SIGNING_PUBLIC);
    assert_eq!(
        IdentityDhKeyPair::from_secret_bytes(it::SUCCESSOR_DH_SECRET).public_bytes(),
        it::SUCCESSOR_DH_PUBLIC
    );

    let sigs = sign_identity_transition(&frozen_transition_body(), &frozen_identity(), &succ)
        .expect("sign the frozen transition");
    assert_eq!(
        hex::encode(sigs.prev_sig),
        hex::encode(it::transition::PREV_SIG),
        "the sunrise.identity_transition.v1 signature drifted"
    );
    assert_eq!(
        hex::encode(sigs.next_sig),
        hex::encode(it::transition::NEXT_SIG),
        "the sunrise.identity_transition.succ.v1 signature drifted"
    );
}

/// The unconditional half: the frozen signature pair verifies, with no signing
/// key in the test at all.
#[test]
fn the_frozen_transition_verifies() {
    verify_identity_transition(
        &frozen_transition_body(),
        &it::IDENTITY_SIGNING_PUBLIC,
        &IdentityTransitionSigs {
            prev_sig: it::transition::PREV_SIG,
            next_sig: it::transition::NEXT_SIG,
        },
    )
    .expect("the frozen transition verifies under the frozen predecessor");
}

// ---------------------------------------------------------------------------
// sunrise.wrap.stream_key.v1
// ---------------------------------------------------------------------------

/// The wrap AAD is not stored beside the blob, so the only thing that can
/// catch a build that spells it differently is a blob that build did not
/// write.
#[test]
fn the_frozen_wrapped_stream_key_opens_only_at_its_own_epoch() {
    use vectors::at_rest::wrapped_stream_key as w;

    assert_eq!(
        w::WRAPPED.len(),
        w::WRAPPED_STREAM_KEY_LEN,
        "the frozen wrap is not WRAPPED_STREAM_KEY_LEN bytes"
    );
    assert_eq!(
        sunrise_crypto::stream_key::WRAPPED_STREAM_KEY_LEN,
        w::WRAPPED_STREAM_KEY_LEN,
        "WRAPPED_STREAM_KEY_LEN moved away from the length every stored wrap has"
    );

    let root = VaultRootKey::from_bytes(w::VAULT_ROOT);
    let opened = unwrap_stream_key(&root, &w::WRAPPED, &w::STREAM_ID, w::EPOCH)
        .expect("the frozen wrapped stream key opens");
    assert_eq!(opened, StreamKey::from_bytes(w::STREAM_KEY));

    // The epoch and the stream id are in the AAD and nowhere else, which is
    // the binding this vector is really pinning.
    assert_eq!(
        unwrap_stream_key(&root, &w::WRAPPED, &w::STREAM_ID, w::EPOCH + 1),
        Err(StreamKeyWrapError::AuthFailed)
    );
    assert_eq!(
        unwrap_stream_key(&root, &w::WRAPPED, &[0x23; 16], w::EPOCH),
        Err(StreamKeyWrapError::AuthFailed)
    );
}

// ---------------------------------------------------------------------------
// sunrise.recovery_blob.v1
// ---------------------------------------------------------------------------

/// A recovery blob is the last copy of an account that exists. This opens one
/// this build did not write in this process, which is the only way to check
/// the AAD prefix, the Argon2id parameters and the header layout at once.
#[test]
fn the_frozen_recovery_blob_opens() {
    use vectors::at_rest::recovery_blob as r;

    let payload = unseal_recovery_blob(&r::BLOB, &r::SEED, &r::IDENTITY_ID)
        .expect("the frozen recovery blob opens");
    assert_eq!(
        payload,
        RecoveryPayload {
            id_s_priv: r::ID_S_PRIV,
            id_d_priv: r::ID_D_PRIV,
            id_s_pub: r::ID_S_PUB,
            id_d_pub: r::ID_D_PUB,
            identity_id: r::IDENTITY_ID,
            created_at_ms: r::CREATED_AT_MS,
        }
    );

    // The identity id is the AAD's second half, so a blob cannot be replayed
    // onto another account even with the right seed.
    assert!(unseal_recovery_blob(&r::BLOB, &r::SEED, &[0x00; 16]).is_err());
}

// ---------------------------------------------------------------------------
// The device-DH keypair the cert names
// ---------------------------------------------------------------------------

/// `D_D_pub` in the frozen cert has to be the public half of the frozen DH
/// secret, or the cert's binding assertion is checking nothing.
#[test]
fn the_frozen_cert_names_the_frozen_device_dh_key() {
    let dh = DeviceDhKeyPair::from_secret_bytes(vectors::key_envelope::RECIPIENT_SECRET);
    assert_eq!(dh.public_bytes(), it::device_cert::D_D_PUB);
    let signing = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    assert_eq!(
        device_id_from_pub(&signing.public_bytes()),
        it::device_cert::DEVICE_ID
    );
}
