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
use sunrise_cbor::CborValue;
use sunrise_crypto::keys::IdentitySigningKeyPair;
use sunrise_crypto::{decode_envelope, seal_envelope, verify_envelope, AeadAlgId, OpEnvelope};
use sunrise_crypto_test_vectors as vectors;

/// Field ids a FUTURE container format might add. Both sit above every field
/// this build knows (1..=12), and one is above 23 so its CBOR key needs two
/// bytes — which is where a naive "sort by decoded key" would go wrong.
const FUTURE_SMALL_FIELD: u64 = 13;
const FUTURE_LARGE_FIELD: u64 = 40;

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

    let kp = IdentitySigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    seal_envelope(
        OpEnvelope {
            v: u32::from(sunrise_cbor::ENVELOPE_FORMAT_V),
            doc_schema_v: u32::from(sunrise_cbor::DOC_SCHEMA_V) + 1,
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
        env.unknown.len(),
        2,
        "both future fields must be preserved, not discarded"
    );

    // The signature was computed over the future fields too, so it verifies
    // here ONLY because they were kept.
    verify_envelope(&env, &vectors::DEVICE_SIGNING_PUBLIC).expect("signature still verifies");

    // Re-emit. Byte-for-byte, per §12.
    let kp = IdentitySigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
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
