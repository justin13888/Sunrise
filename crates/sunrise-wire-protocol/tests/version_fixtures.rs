//! The byte fixtures `docs/10-cross-cutting/protocol-versioning.md` §12
//! mandates, and the tests that keep them honest.
//!
//! §12 asks for them and says "any change to the fixtures is a version-bump
//! ADR". They are checked in under `tests/fixtures/` at the workspace root
//! because two crates read them: this one for `Hello`/`HelloAck` and the
//! negotiation error paths, and `sunrise-crypto` for the forward-compat
//! envelope.
//!
//! A failure here is not a test bug. It means a byte-visible change landed in
//! the v1 wire protocol.

use std::path::PathBuf;
use sunrise_cbor::{decode_canonical, encode_canonical};
use sunrise_wire_protocol::capability::{REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS};
use sunrise_wire_protocol::negotiation::{Hello, HelloAck, NegotiationError};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests")
        .join("fixtures")
}

fn read(rel: &str) -> Vec<u8> {
    let p = fixture_dir().join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("read fixture {}: {e}", p.display()))
}

/// Write `bytes` to `rel` when `SUNRISE_REGEN_FIXTURES=1`, otherwise compare.
///
/// Regeneration is behind an env var rather than automatic so drift shows up as
/// a failing test — which is what forces the ADR §12 asks for — instead of
/// being silently absorbed by the next `cargo test`.
fn assert_fixture(rel: &str, bytes: &[u8]) {
    let path = fixture_dir().join(rel);
    if std::env::var("SUNRISE_REGEN_FIXTURES").is_ok() {
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("mkdir");
        std::fs::write(&path, bytes).expect("write fixture");
        return;
    }
    let want = read(rel);
    assert_eq!(
        bytes,
        want.as_slice(),
        "{rel} drifted; per protocol-versioning.md §12 a change here needs a version-bump ADR"
    );
}

fn v1_hello() -> Hello {
    Hello {
        client_app_v: "1.0.0".into(),
        client_platform: "linux-x86_64".into(),
        wire_proto_supported: vec![1],
        doc_schema_min: u32::from(sunrise_cbor::DOC_SCHEMA_FLOOR),
        doc_schema_max: u32::from(sunrise_cbor::DOC_SCHEMA_V),
        crypto_suite_supported: vec![1],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: "01HXYZ".into(),
    }
}

#[test]
fn hello_v1_fixture_is_byte_exact() {
    let bytes = encode_canonical(&v1_hello()).expect("encode Hello");
    assert_fixture("hello/v1.cbor", &bytes);
    let back: Hello = decode_canonical(&read("hello/v1.cbor")).expect("Hello is canonical");
    assert_eq!(back, v1_hello());
}

#[test]
fn hello_ack_v1_fixture_is_byte_exact() {
    let ack = v1_hello()
        .negotiate(
            "1.0.0".into(),
            &[1],
            &[1],
            u32::from(sunrise_cbor::DOC_SCHEMA_FLOOR),
            REQUIRED_SERVER_BITS.0 | REQUIRED_CLIENT_BITS.0,
            1_700_000_000_000,
        )
        .expect("v1 negotiates");
    let bytes = encode_canonical(&ack).expect("encode HelloAck");
    assert_fixture("hello/ack_v1.cbor", &bytes);
    let back: HelloAck = decode_canonical(&read("hello/ack_v1.cbor")).expect("canonical");
    assert_eq!(back, ack);
}

/// §12: "every error path above produces a fixture; clients and server both
/// run a vector test that confirms decode + correct error."
#[test]
fn every_negotiation_error_path_has_a_fixture() {
    let cases: [(&str, Hello, NegotiationError); 4] = [
        (
            "wire-mismatch.cbor",
            Hello {
                wire_proto_supported: vec![99],
                ..v1_hello()
            },
            NegotiationError::WireMismatch,
        ),
        (
            "crypto-mismatch.cbor",
            Hello {
                crypto_suite_supported: vec![99],
                ..v1_hello()
            },
            NegotiationError::CryptoMismatch,
        ),
        (
            "doc-schema-too-old.cbor",
            Hello {
                doc_schema_max: 0,
                ..v1_hello()
            },
            NegotiationError::DocSchemaTooOld,
        ),
        (
            "capability-missing.cbor",
            Hello {
                capabilities: 0,
                ..v1_hello()
            },
            NegotiationError::CapabilityRequiredMissing,
        ),
    ];

    for (name, hello, want) in cases {
        let rel = format!("version-mismatch/{name}");
        let bytes = encode_canonical(&hello).expect("encode");
        assert_fixture(&rel, &bytes);

        // Decode the FIXTURE, not the value just built: the point is that the
        // bytes on disk still produce this error.
        let decoded: Hello = decode_canonical(&read(&rel)).expect("fixture is canonical");
        let err = decoded
            .negotiate(
                "1.0.0".into(),
                &[1],
                &[1],
                u32::from(sunrise_cbor::DOC_SCHEMA_FLOOR),
                REQUIRED_SERVER_BITS.0 | REQUIRED_CLIENT_BITS.0,
                0,
            )
            .expect_err("must not negotiate");
        assert_eq!(err, want, "{rel}");
    }
}
