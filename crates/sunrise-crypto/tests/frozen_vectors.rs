//! Asserts the frozen vectors in `sunrise-crypto-test-vectors` against the
//! live v1 implementation.
//!
//! A failure here is **not** a test bug. It means a byte-visible change landed
//! in the frozen v1 crypto suite — a BLAKE3, canonical-CBOR, Ed25519, or
//! XChaCha20-Poly1305 output moved — which per ADR-0004 and
//! `docs/03-crypto/key-rotation.md` requires a suite-id bump and a rotation
//! plan, not a re-freeze of the expected bytes.

use sunrise_crypto::{
    blake3_kdf::derive_key_32,
    derive_key, encode_envelope, identity_id_from_pub,
    keys::{IdentitySigningKeyPair, StreamKey},
    stream_root_init, stream_root_step, AeadAlgId,
};
use sunrise_crypto_test_vectors as vectors;

#[test]
fn identity_id_vectors_hold() {
    for v in vectors::IDENTITY_ID_VECTORS {
        assert_eq!(
            identity_id_from_pub(&v.id_s_pub),
            v.identity_id,
            "identity_id drifted for ID_S_pub {}",
            hex::encode(v.id_s_pub)
        );
    }
}

#[test]
fn identity_id_is_the_16_byte_prefix_of_its_kdf() {
    // Cross-check the frozen id against the spec formula written out longhand,
    // so a change to `identity_id_from_pub`'s *context string* is caught even
    // if someone regenerates the vector.
    let v = vectors::IDENTITY_ID_VECTORS[1];
    let raw = derive_key("sunrise.identity_id.v1", &v.id_s_pub, 16);
    assert_eq!(raw.as_slice(), v.identity_id.as_slice());
}

#[test]
fn frozen_public_key_matches_frozen_secret() {
    let kp = IdentitySigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    assert_eq!(kp.public_bytes(), vectors::DEVICE_SIGNING_PUBLIC);
}

#[test]
fn blake3_kdf_vectors_hold() {
    for v in vectors::KDF_VECTORS {
        assert_eq!(
            derive_key_32(v.context, v.key_material),
            v.out_32,
            "derive_key drifted for context {}",
            v.context
        );
        // The XOF is prefix-stable: a 64-byte draw must start with the same
        // 32 bytes.
        let long = derive_key(v.context, v.key_material, 64);
        assert_eq!(&long[..32], v.out_32.as_slice());
    }
}

#[test]
fn stream_merkle_root_vectors_hold() {
    assert_eq!(
        stream_root_init(&[0x00; 16]),
        vectors::STREAM_ROOT_INIT_ZERO
    );

    let r0 = stream_root_init(&vectors::STREAM_ID);
    assert_eq!(r0, vectors::STREAM_ROOT_0);
    let r1 = stream_root_step(&r0, b"a");
    assert_eq!(r1, vectors::STREAM_ROOT_1);
    let r2 = stream_root_step(&r1, b"b");
    assert_eq!(r2, vectors::STREAM_ROOT_2);
}

#[test]
fn signed_only_envelope_is_byte_exact() {
    use vectors::signed_only_envelope as v;

    let kp = IdentitySigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    let encoded = encode_envelope(
        vectors::ENVELOPE_INNER,
        vectors::STREAM_ID,
        vectors::DEVICE_ID,
        v::SEQ,
        sunrise_cbor::hlc::Hlc::at(v::HLC_MS),
        AeadAlgId::None,
        v::EPOCH,
        v::NONCE,
        None,
        &kp,
    )
    .expect("encode signed-only envelope");

    assert_eq!(
        hex::encode(&encoded),
        hex::encode(v::ENCODED),
        "aead_alg=0 envelope encoding drifted"
    );
}

#[test]
fn sealed_envelope_is_byte_exact() {
    use vectors::sealed_envelope as v;

    let kp = IdentitySigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    let key = StreamKey::from_bytes(v::STREAM_KEY);
    let encoded = encode_envelope(
        vectors::ENVELOPE_INNER,
        vectors::STREAM_ID,
        vectors::DEVICE_ID,
        v::SEQ,
        sunrise_cbor::hlc::Hlc::at(v::HLC_MS),
        AeadAlgId::XChaCha20Poly1305,
        v::EPOCH,
        v::NONCE,
        Some(&key),
        &kp,
    )
    .expect("encode sealed envelope");

    assert_eq!(
        hex::encode(&encoded),
        hex::encode(v::ENCODED),
        "aead_alg=1 envelope encoding drifted"
    );
}

#[test]
fn frozen_envelopes_round_trip_and_verify() {
    use sunrise_crypto::{decode_envelope, verify_envelope};

    for bytes in [
        vectors::signed_only_envelope::ENCODED.as_slice(),
        vectors::sealed_envelope::ENCODED.as_slice(),
    ] {
        let env = decode_envelope(bytes).expect("frozen envelope decodes");
        assert_eq!(env.stream_id, vectors::STREAM_ID);
        assert_eq!(env.device_id, vectors::DEVICE_ID);
        verify_envelope(&env, &vectors::DEVICE_SIGNING_PUBLIC)
            .expect("frozen envelope signature verifies under the frozen key");
    }
}
