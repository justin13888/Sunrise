//! `docs/10-cross-cutting/protocol-versioning.md` §12: "a synthetic v2 op
//! (with extra fields) decoded by a v1 codec; round-trip MUST preserve the v2
//! fields byte-for-byte."
//!
//! This is the test that proves the promise in §7 — "unknown CBOR map keys
//! round-trip unchanged" — is implemented rather than merely written down. It
//! matters more than it looks: the envelope signature covers every field the
//! sender wrote, so a decoder that DROPS an unknown field cannot re-emit an
//! envelope that still verifies. Preservation is not politeness, it is the
//! difference between relaying an op and corrupting it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use sunrise_cbor::hlc::Hlc;
use sunrise_cbor::{decode_envelope_header, CborValue, ENVELOPE_FORMAT_FLOOR, ENVELOPE_FORMAT_V};
use sunrise_crypto::keys::{DeviceSigningKeyPair, StreamKey};
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{decode_envelope, seal_envelope, verify_envelope, AeadAlgId, OpEnvelope};
use sunrise_crypto_test_vectors as vectors;

/// Field ids a FUTURE container format might add. Both sit above every field
/// this build knows (1..=12) and above the ids ADR-0045 assigns or reserves
/// (13 for the schema fingerprint, 14–15 for the commit tree), so neither stops
/// naming an unknown field when those land. 23 is the largest id whose CBOR key
/// is one byte; 40 needs two, which is where a naive "sort by decoded key"
/// would go wrong.
const FUTURE_SMALL_FIELD: u64 = 23;
const FUTURE_LARGE_FIELD: u64 = 40;

/// Field 13 as a writer one document-schema version newer would stamp it: the
/// prefix of a fingerprint this build has no registry entry for (ADR-0045 §3).
const NEWER_SCHEMA_FP: [u8; 8] = [0x5f; 8];

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests")
        .join("fixtures")
        .join("forward-compat")
        .join("v1-reads-v2.cbor")
}

/// An envelope as a hypothetical newer build would emit it: everything this
/// build knows, plus two fields it does not.
fn synthetic_v2_envelope() -> Vec<u8> {
    use ciborium::value::Value;

    let mut unknown = BTreeMap::new();
    unknown.insert(
        FUTURE_SMALL_FIELD,
        CborValue(Value::Text("a field from the future".into())),
    );
    unknown.insert(
        FUTURE_LARGE_FIELD,
        CborValue(Value::Array(vec![
            Value::Integer(1.into()),
            Value::Bytes(vec![0xde, 0xad]),
        ])),
    );

    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    seal_envelope(
        OpEnvelope {
            v: u32::from(sunrise_cbor::ENVELOPE_FORMAT_V),
            doc_schema_v: u32::from(sunrise_cbor::DOC_SCHEMA_V) + 1,
            // A newer writer stamps its own version's fingerprint, which this
            // build has no registry entry for and so cannot check.
            schema_fp: Some(NEWER_SCHEMA_FP),
            stream_id: vectors::STREAM_ID,
            device_id: vectors::DEVICE_ID,
            seq: 11,
            hlc: Hlc {
                physical_ms: 1_700_000_000_000,
                logical: 3,
            },
            aead_alg: AeadAlgId::None,
            sig_alg: sunrise_crypto::SigAlgId::Ed25519,
            epoch: 0,
            nonce: [0u8; 24],
            payload: vectors::ENVELOPE_INNER.to_vec(),
            sig: [0u8; 64],
            unknown,
        },
        None,
        &kp,
    )
    .expect("seal synthetic v2 envelope")
}

#[test]
fn the_v1_reads_v2_fixture_is_stable() {
    let bytes = synthetic_v2_envelope();
    let path = fixture_path();
    if std::env::var("SUNRISE_REGEN_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(&path, &bytes).expect("write fixture");
        return;
    }
    let want = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(bytes, want, "the synthetic v2 envelope drifted");
}

#[test]
fn unknown_envelope_fields_round_trip_byte_for_byte() {
    let original = std::fs::read(fixture_path()).expect("fixture");

    let env = decode_envelope(&original).expect("a newer doc schema is still decodable");
    assert_eq!(
        env.doc_schema_v,
        u32::from(sunrise_cbor::DOC_SCHEMA_V) + 1,
        "the payload schema is newer than this build's"
    );
    assert_eq!(
        env.schema_fp,
        Some(NEWER_SCHEMA_FP),
        "field 13 is read as written, though this build cannot check it"
    );
    assert_eq!(
        env.unknown.len(),
        2,
        "both future fields must be preserved, not discarded"
    );

    // The signature was computed over the future fields too, so it verifies
    // here ONLY because they were kept.
    verify_envelope(&env, &vectors::DEVICE_SIGNING_PUBLIC).expect("signature still verifies");

    // Re-emit. Byte-for-byte, per §12.
    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    let mut round_tripped = env.clone();
    round_tripped.payload = vectors::ENVELOPE_INNER.to_vec();
    let re = seal_envelope(round_tripped, None, &kp).expect("re-seal");
    assert_eq!(re, original, "re-emission must be byte-identical");
}

/// The failure mode this guards against: drop the unknown fields and the
/// sender's own signature stops verifying, because it covered them.
#[test]
fn dropping_an_unknown_field_breaks_the_senders_signature() {
    let original = std::fs::read(fixture_path()).expect("fixture");
    let mut env = decode_envelope(&original).expect("decode");
    let sender_sig = env.sig;

    env.unknown.clear();
    env.sig = sender_sig;

    assert!(
        verify_envelope(&env, &vectors::DEVICE_SIGNING_PUBLIC).is_err(),
        "if this passed, the unknown fields were never inside the signature and \
         preserving them would be pointless"
    );
}

/// ADR-0045 §5, issue #329: an envelope from a container one step newer than
/// this build's, carrying a field this build does not know, at a floor this
/// build implements. It is sealed, so the new field is inside the AEAD's
/// associated data as well as the signature input.
fn next_container_envelope() -> (Vec<u8>, StreamKey) {
    use ciborium::value::Value;

    let mut unknown = BTreeMap::new();
    unknown.insert(
        FUTURE_SMALL_FIELD,
        CborValue(Value::Text("a field from the next container".into())),
    );
    let stream_key = StreamKey::from_bytes([0x44; 32]);
    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    let bytes = seal_envelope(
        OpEnvelope {
            v: u32::from(ENVELOPE_FORMAT_V) + 1,
            doc_schema_v: u32::from(sunrise_cbor::DOC_SCHEMA_V),
            schema_fp: sunrise_cbor::doc_schema_fp_prefix(u32::from(sunrise_cbor::DOC_SCHEMA_V)),
            stream_id: vectors::STREAM_ID,
            device_id: vectors::DEVICE_ID,
            seq: 12,
            hlc: Hlc {
                physical_ms: 1_700_000_000_000,
                logical: 4,
            },
            aead_alg: AeadAlgId::XChaCha20Poly1305,
            sig_alg: sunrise_crypto::SigAlgId::Ed25519,
            epoch: 1,
            nonce: [0x55; 24],
            payload: vectors::ENVELOPE_INNER.to_vec(),
            sig: [0u8; 64],
            unknown,
        },
        Some(&stream_key),
        &kp,
    )
    .expect("seal an envelope from the next container");
    (bytes, stream_key)
}

#[test]
fn a_newer_container_at_this_floor_decodes_verifies_opens_and_round_trips() {
    let (original, stream_key) = next_container_envelope();
    // The writer stamps its floor, which this build implements, and its own
    // newer container in field 1.
    assert_eq!(
        u16::from_be_bytes([original[3], original[4]]),
        ENVELOPE_FORMAT_FLOOR
    );

    let env = decode_envelope(&original).expect("a newer container at this floor decodes");
    assert_eq!(env.v, u32::from(ENVELOPE_FORMAT_V) + 1);
    assert_eq!(
        env.unknown.keys().copied().collect::<Vec<_>>(),
        vec![FUTURE_SMALL_FIELD],
        "the new field is kept, not discarded"
    );

    // Verify and open: the new field is inside both the signature input and
    // the AAD, so either succeeding proves it was kept where it arrived.
    let plaintext = open_envelope(&env, &vectors::DEVICE_SIGNING_PUBLIC, Some(&stream_key))
        .expect("verifies and opens");
    assert_eq!(plaintext, vectors::ENVELOPE_INNER);

    // Re-emit byte for byte. Sealing again under the same key and nonce with
    // the new field still in place reproduces the same AAD, ciphertext and
    // deterministic signature, and only if the field was kept where it was.
    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    let mut round_tripped = env;
    round_tripped.payload = plaintext;
    let re = seal_envelope(round_tripped, Some(&stream_key), &kp).expect("re-encode");
    assert_eq!(re, original, "re-emission must be byte-identical");

    // The relay's header decoder applies the same floor rule, so it routes it.
    let head = decode_envelope_header(&original).expect("the relay routes it");
    assert_eq!(head.stream_id, vectors::STREAM_ID);
    assert_eq!(head.device_id, vectors::DEVICE_ID);
    assert_eq!(head.seq, 12);
}

/// The other side of the floor: a writer whose floor is above this build's
/// container has said this build would misread it, and both decoders refuse.
#[test]
fn a_floor_above_this_build_is_refused_by_client_and_relay() {
    let (mut bytes, _) = next_container_envelope();
    bytes[3..5].copy_from_slice(&(ENVELOPE_FORMAT_V + 1).to_be_bytes());
    assert!(matches!(
        decode_envelope(&bytes),
        Err(sunrise_crypto::OpEnvelopeError::BadMagic)
    ));
    assert_eq!(
        decode_envelope_header(&bytes),
        Err(sunrise_cbor::EnvelopeHeaderError::BadMagic)
    );
}
