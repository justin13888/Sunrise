//! `sunrise-device-sig-v2`, anchored to a frozen signature.
//!
//! The version tag is the first line of the string a device signs and appears
//! nowhere on the wire, so it is only observable through a signature. A server
//! and a client that spell it differently agree on every header, every hash and
//! every key, and the server rejects every request as a forgery.
//!
//! The two signatures below were produced once by this implementation under a
//! fixed Ed25519 seed — the algorithm is deterministic — and pinned in
//! `sunrise-crypto-test-vectors`, a crate that depends on nothing. The
//! canonical string is pinned beside them so a reader can see which of its five
//! lines moved when one fails.
//!
//! A failure here is an ADR-0022 protocol change, not a test fix.

use ed25519_dalek::SigningKey;
use sunrise_crypto_test_vectors::protocol::device_sig_v2 as v;
use sunrise_http_sig::{
    canonical_json, canonical_string, device_pub_b64, sign, verify, CANONICAL_V2_TAG,
};

/// The instant the frozen `Date` header names, so the skew check passes.
const NOW_MS: u64 = 1_700_000_000_000;

fn key() -> SigningKey {
    SigningKey::from_bytes(&v::SIGNING_SECRET)
}

#[test]
fn the_canonical_string_is_byte_exact() {
    assert_eq!(
        canonical_string("GET", "/api/v1/meta", v::DATE, b""),
        v::CANONICAL_NO_BODY,
        "the sunrise-device-sig-v2 canonical string drifted"
    );
    // The tag is the first line, spelled out, so a rename is visible here as
    // well as through the signatures below.
    assert!(v::CANONICAL_NO_BODY.starts_with(CANONICAL_V2_TAG));
}

#[test]
fn the_frozen_signatures_are_byte_exact() {
    let sk = key();
    assert_eq!(
        device_pub_b64(&sk.verifying_key().to_bytes()),
        v::DEVICE_PUB_B64
    );

    assert_eq!(
        sign::<()>(&sk, "GET", "/api/v1/meta", v::DATE, None).expect("sign"),
        v::SIG_NO_BODY,
        "the bodyless sunrise-device-sig-v2 signature drifted"
    );

    let body = serde_json::json!({ "b": 2, "a": 1 });
    assert_eq!(
        canonical_json(&body).expect("canonicalize"),
        v::CANONICAL_BODY.as_bytes(),
        "RFC 8785 canonical JSON drifted"
    );
    assert_eq!(
        sign(&sk, "POST", v::PATH_WITH_BODY, v::DATE, Some(&body)).expect("sign"),
        v::SIG_WITH_BODY,
        "the bodied sunrise-device-sig-v2 signature drifted"
    );
}

/// The unconditional half: the frozen header verifies against the frozen
/// public key, with no signing key in the assertion at all. This is the
/// assertion a second implementation of the client has to satisfy.
#[test]
fn the_frozen_signature_verifies() {
    verify::<()>(
        v::DEVICE_PUB_B64,
        v::SIG_NO_BODY,
        "GET",
        "/api/v1/meta",
        v::DATE,
        None,
        NOW_MS,
    )
    .expect("the frozen bodyless signature verifies");

    let body = serde_json::json!({ "b": 2, "a": 1 });
    verify(
        v::DEVICE_PUB_B64,
        v::SIG_WITH_BODY,
        "POST",
        v::PATH_WITH_BODY,
        v::DATE,
        Some(&body),
        NOW_MS,
    )
    .expect("the frozen bodied signature verifies");
}
