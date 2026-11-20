//! The `header_sig_v2` round trip and its bindings, over generated requests.
//!
//! The unit tests in `src/lib.rs` pin the round trip and each binding on one
//! fixed method, path, body and date. That proves the scheme works for that
//! request; it does not prove the canonical string keeps every input apart for
//! any request. A canonical string that, say, let a path ending in a newline
//! run into the date, or hashed a body whose canonical form collapsed two
//! values, would pass every fixed case and still sign two requests alike.
//!
//! So this draws the four inputs a client chooses - method, origin-form target,
//! JSON body (or none) and `Date` - and asserts that what [`sign`] produces
//! [`verify`] accepts, and that replacing any one of the four with a different
//! value is refused as [`SigError::BadSignature`].
//!
//! The replacement `Date` sits inside [`MAX_CLOCK_SKEW_SECS`] of the verifier's
//! clock, so the skew check passes and the refusal can only be the signature's.
//! A case whose replacement path equals its path, or whose replacement body has
//! the original's canonical bytes, is drawn again: two values with one
//! canonical form are the same request to this
//! scheme by design, and that collapse is the subject of
//! `a_non_finite_float_collapses_to_null_which_is_why_floats_are_not_signed`,
//! not of this property.

use ed25519_dalek::SigningKey;
use proptest::prelude::*;
use serde_json::Value;
use sunrise_http_sig::{
    canonical_json, date_header, device_pub_b64, sign, verify, SigError, MAX_CLOCK_SKEW_SECS,
};

const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];

/// The latest second drawn, 2099-12-31T23:59:59Z: four-digit years, which is
/// what an RFC 2822 `Date` can carry.
const LAST_SECOND: u64 = 4_102_444_799;

/// The workspace convention for a proptest under `tests/`: seed and case count
/// from `sunrise-test-seed`, counterexamples persisted to the tracked path. See
/// docs/10-cross-cutting/testing.md section 2.
fn config() -> ProptestConfig {
    ProptestConfig {
        cases: sunrise_test_seed::proptest_cases(256),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/sign_verify_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    }
}

/// An origin-form target: an absolute path, optionally with a query.
fn path_strategy() -> impl Strategy<Value = String> {
    "/[A-Za-z0-9._~%-]{0,12}(/[A-Za-z0-9._~%-]{1,12}){0,4}(\\?[A-Za-z0-9=&%._-]{0,16})?"
}

/// A JSON value with no floats: no request type on this surface carries one.
/// Integers cover both signs and the full `u64` range the wire types use.
fn json_strategy() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<String>().prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 32, 5, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            proptest::collection::vec((any::<String>(), inner), 0..5)
                .prop_map(|members| Value::Object(members.into_iter().collect())),
        ]
    })
}

/// A request body, absent one time in ten: absence hashes the empty string,
/// and that case is a body like any other here.
fn body_strategy() -> impl Strategy<Value = Option<Value>> {
    proptest::option::weighted(0.9, json_strategy())
}

/// The canonical bytes `sign` and `verify` hash for `body`.
fn canonical(body: Option<&Value>) -> Vec<u8> {
    body.map_or_else(Vec::new, |v| {
        canonical_json(v).expect("a float-free JSON value canonicalizes")
    })
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn a_signature_verifies_and_binds_each_of_its_inputs(
        seed in any::<[u8; 32]>(),
        method_index in 0..METHODS.len(),
        method_offset in 1..METHODS.len(),
        path in path_strategy(),
        other_path in path_strategy(),
        body in body_strategy(),
        other_body in body_strategy(),
        second in MAX_CLOCK_SKEW_SECS.unsigned_abs()..=LAST_SECOND,
        date_shift in (1..=MAX_CLOCK_SKEW_SECS).prop_flat_map(|s| prop_oneof![Just(s), Just(-s)]),
    ) {
        prop_assume!(path != other_path);
        prop_assume!(canonical(body.as_ref()) != canonical(other_body.as_ref()));

        let key = SigningKey::from_bytes(&seed);
        let device_pub = device_pub_b64(&key.verifying_key().to_bytes());
        let method = METHODS[method_index];
        let other_method = METHODS[(method_index + method_offset) % METHODS.len()];
        let now_ms = second * 1000;
        let date = date_header(now_ms);
        let other_date = date_header(second.saturating_add_signed(date_shift) * 1000);
        prop_assert_ne!(&date, &other_date);

        let sig = sign(&key, method, &path, &date, body.as_ref()).unwrap();

        prop_assert_eq!(
            verify(&device_pub, &sig, method, &path, &date, body.as_ref(), now_ms),
            Ok(()),
            "the signed request must verify"
        );
        prop_assert_eq!(
            verify(&device_pub, &sig, other_method, &path, &date, body.as_ref(), now_ms),
            Err(SigError::BadSignature),
            "the method is bound"
        );
        prop_assert_eq!(
            verify(&device_pub, &sig, method, &other_path, &date, body.as_ref(), now_ms),
            Err(SigError::BadSignature),
            "the target is bound"
        );
        prop_assert_eq!(
            verify(&device_pub, &sig, method, &path, &date, other_body.as_ref(), now_ms),
            Err(SigError::BadSignature),
            "the body is bound"
        );
        prop_assert_eq!(
            verify(&device_pub, &sig, method, &path, &other_date, body.as_ref(), now_ms),
            Err(SigError::BadSignature),
            "the date is bound, inside the skew window"
        );
    }
}
