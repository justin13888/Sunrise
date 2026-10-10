//! A newer relay's extra response fields decode in this client (issue #370).
//!
//! Every sync body on the wire is open to extension
//! (`docs/10-cross-cutting/protocol-versioning.md` §4, §6), in both directions:
//! the relay ignores a field a newer client adds, and this client ignores a
//! field a newer relay adds. The generated models are only as tolerant as the
//! description they are generated from — spargen emits
//! `#[serde(deny_unknown_fields)]` for `additionalProperties: false` — so these
//! pin the three sync response bodies against the generated types rather than
//! trusting the description to stay open.
//!
//! Each fixture is a response a hypothetical newer relay would send. The
//! assertion is a round trip: what decodes must re-encode to the same body
//! minus the unknown field, so a model that silently dropped a field it does
//! know would fail too.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use sunrise_relay_client::api::types::{OpsResponse, RefreshResponse, SessionResponse};

/// Decode `known` plus `extra`, and assert the result re-encodes to `known`.
fn decodes_ignoring<T: DeserializeOwned + Serialize>(known: &Value, extra: &[(&str, Value)]) {
    let mut wire = known.clone();
    for (key, value) in extra {
        wire[*key] = value.clone();
    }
    let decoded: T = serde_json::from_value(wire.clone())
        .unwrap_or_else(|e| panic!("a newer relay's body must decode: {e}\n{wire}"));
    let reencoded = serde_json::to_value(&decoded).expect("re-encodes");
    assert_eq!(&reencoded, known, "every known field survives the decode");
}

fn session_response() -> Value {
    json!({
        "session_id": "01J00000000000000000000000",
        "server_app_v": "0.9.0",
        "wire_proto": 1,
        "crypto_suite": 1,
        "doc_schema_floor": 1,
        "capabilities": 0x0000_0002_0000_0201_u64,
        "server_time_ms": 1_704_067_200_000_u64,
    })
}

#[test]
fn a_newer_relays_session_response_decodes() {
    decodes_ignoring::<SessionResponse>(
        &session_response(),
        &[
            ("schema_fingerprint", json!("b3:0123456789abcdef")),
            ("relay_floor", json!({ "wire_proto": 2, "features": ["x"] })),
        ],
    );
}

#[test]
fn a_newer_relays_refresh_response_decodes() {
    decodes_ignoring::<RefreshResponse>(
        &json!({ "expires_at_ms": 1_704_067_260_000_u64 }),
        &[("refresh_after_ms", json!(1_704_067_230_000_u64))],
    );
}

#[test]
fn a_newer_relays_ops_ack_decodes() {
    decodes_ignoring::<OpsResponse>(
        &json!({
            "batch_id": 7,
            "stream_id": "11111111111111111111111111111111",
            "server_first_seen_ms": 1_704_067_200_000_u64,
        }),
        &[("stored_seq_range", json!([1, 3]))],
    );
}

/// The tolerance is for *unknown* fields only: a body missing one this client
/// requires is still refused, so the tests above are not passing because the
/// models decode anything at all.
#[test]
fn a_session_response_missing_a_required_field_is_still_refused() {
    let mut wire = session_response();
    wire.as_object_mut()
        .expect("an object")
        .remove("session_id");
    wire["schema_fingerprint"] = json!("b3:0123456789abcdef");
    assert!(serde_json::from_value::<SessionResponse>(wire).is_err());
}
