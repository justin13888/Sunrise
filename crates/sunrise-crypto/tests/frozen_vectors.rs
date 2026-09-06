//! Asserts the frozen vectors in `sunrise-crypto-test-vectors` against the
//! live v1 implementation.
//!
//! A failure here is **not** a test bug. It means a byte-visible change landed
//! in the frozen v1 crypto suite — a BLAKE3, canonical-CBOR, Ed25519, or
//! XChaCha20-Poly1305 output moved — which per ADR-0004 and
//! `docs/03-crypto/key-rotation.md` requires a suite-id bump and a rotation
//! plan, not a re-freeze of the expected bytes.
//!
//! The **one** exception, and it is narrow: the two whole-envelope vectors
//! include field 12, the document schema. A deliberate `DOC_SCHEMA_V` bump
//! moves their bytes with no crypto change at all, and re-freezing those two is
//! then correct. It moves three regions and no others — the signature (field 12
//! is in the signature input), the AEAD tag on the sealed vector (field 12 is
//! in the AAD, which is built by exclusion), and the field-12 byte itself; the
//! ciphertext must not move, and [`sealed_envelope_ciphertext_did_not_move`]
//! asserts exactly that. Nothing else here carries a version field, so nothing
//! else has this excuse — if a KDF, identity-id, stream-root or AEAD vector
//! fails, the paragraph above applies in full.

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_crypto::{
    blake3_kdf::derive_key_32,
    chunk_aad, chunk_nonce, derive_key, encode_envelope, hpke_open, hpke_seal,
    identity_id_from_pub, key_envelope_info,
    keys::{DeviceDhKeyPair, DeviceSigningKeyPair, StreamKey},
    open_chunk, seal_chunk, stream_key_id, stream_root_init, stream_root_step, AeadAlgId,
    HPKE_ENC_LEN,
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
    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
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

    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
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

    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
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

/// The chunk nonce is derived, never transmitted. Two implementations that
/// derive it differently each round-trip their own bytes perfectly and cannot
/// read each other's — which is why this is a frozen vector and not a
/// round-trip test.
#[test]
fn blob_chunk_nonce_vectors_hold() {
    for v in vectors::BLOB_CHUNK_NONCE_VECTORS {
        assert_eq!(
            chunk_nonce(&v.blob_key, v.chunk_idx),
            v.nonce,
            "blob chunk nonce drifted at index {}",
            v.chunk_idx
        );
    }
}

#[test]
fn a_sealed_blob_chunk_is_byte_exact_and_opens() {
    use vectors::blob_chunk as v;

    assert_eq!(chunk_aad(&v::BLOB_ID, v::CHUNK_IDX, v::CHUNK_COUNT), v::AAD);
    let sealed = seal_chunk(
        &v::BLOB_KEY,
        &v::BLOB_ID,
        v::CHUNK_IDX,
        v::CHUNK_COUNT,
        v::PLAINTEXT,
    )
    .expect("seal the frozen chunk");
    assert_eq!(sealed, v::SEALED);
    assert_eq!(
        open_chunk(
            &v::BLOB_KEY,
            &v::BLOB_ID,
            v::CHUNK_IDX,
            v::CHUNK_COUNT,
            &v::SEALED
        )
        .expect("open the frozen chunk"),
        v::PLAINTEXT
    );
}

/// The re-freeze at `DOC_SCHEMA_V = 5` is allowed to move the signature, the
/// AEAD tag, and the field-12 byte. It is **not** allowed to move the
/// ciphertext: that would mean the stream key, the nonce or the cipher itself
/// changed, which is the crypto-suite change this whole file exists to catch.
#[test]
fn sealed_envelope_ciphertext_did_not_move() {
    // Payload field 10, `58 27` (39 bytes) at offset 92: 23 ciphertext bytes
    // then the 16-byte tag.
    const CT_START: usize = 94;
    const CT_END: usize = 117;
    assert_eq!(
        hex::encode(&vectors::sealed_envelope::ENCODED[CT_START..CT_END]),
        "6416c4bb3e46b71d10c45af51e2462649e7331f6d5bbb8",
        "the sealed vector's ciphertext moved — that is a key-schedule change, \
         not a doc-schema bump"
    );
}

#[test]
fn stream_key_id_is_the_8_byte_prefix_of_its_kdf() {
    let key = StreamKey::from_bytes(vectors::KEY_ID_STREAM_KEY);
    let want = derive_key_32("sunrise.stream_key_id.v1", &vectors::KEY_ID_STREAM_KEY);
    assert_eq!(stream_key_id(&key).as_slice(), &want[..8]);
}

/// The HPKE `key_envelope` seal, both halves.
///
/// The byte-exact half needs the frozen RNG seed, because HPKE draws an
/// ephemeral KEM key per seal. The round-trip half needs nothing and is the
/// assertion another implementation would have to satisfy.
#[test]
fn key_envelope_vector_holds() {
    use vectors::key_envelope as v;

    let recipient = DeviceDhKeyPair::from_secret_bytes(v::RECIPIENT_SECRET);
    assert_eq!(recipient.public_bytes(), v::RECIPIENT_PUBLIC);

    let info = key_envelope_info(&v::STREAM_ID, v::EPOCH);
    assert_eq!(info, v::INFO, "the key_envelope info string drifted");

    // Unconditional: the frozen blob opens, and its shape is the documented one.
    assert_eq!(v::SEALED.len(), HPKE_ENC_LEN + 32 + 16);
    let opened = hpke_open(&recipient, &info, &v::SEALED, b"").expect("frozen envelope opens");
    assert_eq!(opened, v::STREAM_KEY);

    // Byte-exact, against the stated CSPRNG state.
    let mut rng = ChaCha20Rng::seed_from_u64(v::RNG_SEED);
    let sealed = hpke_seal(&v::RECIPIENT_PUBLIC, &info, &v::STREAM_KEY, b"", &mut rng)
        .expect("seal the frozen envelope");
    assert_eq!(
        hex::encode(&sealed),
        hex::encode(v::SEALED),
        "HPKE key-envelope encoding drifted"
    );
}
