//! What `POST /sync/session` negotiates: versions, the refusals, and the
//! capability bits the relay agrees. Was `ws_handshake`.

use super::*;

/// Was `ws_handshake_round_trip`.
#[tokio::test]
async fn a_session_negotiates_the_same_versions_the_handshake_did() {
    let client = Client::new(ServerConfig::default());
    let res = client
        .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
        .await;
    res.assert_status(StatusCode::CREATED);
    let body = res.json();

    assert_eq!(body["wire_proto"], u32::from(WIRE_PROTO_V));
    assert_eq!(
        body["crypto_suite"],
        u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)
    );
    assert!(
        body["session_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("ses_")),
        "a session id must come back: {body}"
    );
}

/// A client the server cannot agree with is refused for the same reason it
/// was refused at the socket.
#[tokio::test]
async fn an_unnegotiable_client_is_refused() {
    let client = Client::new(ServerConfig::default());
    let mut body = hello();
    body["wire_proto_supported"] = serde_json::json!([9999]);

    client
        .send(Method::POST, "/api/v1/sync/session", Some(&body))
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// The four negotiation failures are four different user-facing outcomes,
/// and every one of them used to arrive as `VALIDATION_INVALID` with the
/// distinction only in a message string. `as_error_code` had always
/// computed it; nothing called it. Telling a self-hoster to update their
/// app when their *relay* is the stale half is the concrete failure.
#[tokio::test]
async fn every_negotiation_refusal_carries_its_own_code() {
    let client = Client::new(ServerConfig::default());
    let cases: [(&str, serde_json::Value, &str); 4] = [
        (
            "wire_proto_supported",
            serde_json::json!([9999]),
            "SYNC_PROTOCOL_VERSION_MISMATCH",
        ),
        (
            "crypto_suite_supported",
            serde_json::json!([9999]),
            "CRYPTO_SUITE_MISMATCH",
        ),
        // The server's floor is 1, so a client that can read nothing at
        // all is the device whose data predates what this relay accepts.
        ("doc_schema_max", serde_json::json!(0), "DOC_SCHEMA_TOO_OLD"),
        (
            "capabilities",
            serde_json::json!(0),
            "CAPABILITY_REQUIRED_MISSING",
        ),
    ];

    for (field, value, expected) in cases {
        let mut body = hello();
        body[field] = value;
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&body))
            .await;
        res.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            res.json()["code"],
            serde_json::json!(expected),
            "a refusal on {field} must carry {expected}, not a shared code"
        );
        // The metric splits on the same code, so a dashboard can tell a
        // stale app from a stale relay too.
        let reason = [("reason", expected)];
        let refused = "sunrise_sync_negotiate_refused_total";
        assert_eq!(client.metrics.get_with(refused, &reason), 1, "{expected}");
    }
}

/// Was `the_relay_advertises_the_token_refresh_capability`.
#[tokio::test]
async fn the_relay_advertises_the_token_refresh_capability() {
    let client = Client::new(ServerConfig::default());
    let res = client
        .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
        .await;
    res.assert_status(StatusCode::CREATED);

    let agreed = res.json()["capabilities"].as_u64().expect("a bitfield");
    assert!(
        CapabilityBits(agreed).has(Capability::SrvTokenRefresh),
        "a client only asks for a refresh if it sees this bit come back"
    );
}

/// ADR-0045 §5: bit 9 tells a client this relay routes a newer envelope
/// container. It is agreed by AND, so the client offers it here.
#[tokio::test]
async fn the_relay_advertises_the_envelope_floor_capability() {
    let client = Client::new(ServerConfig::default());
    let mut hello = hello();
    hello["capabilities"] = serde_json::json!(
        sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0
            | CapabilityBits::EMPTY.with(Capability::SrvEnvelopeFloor).0
    );
    let res = client
        .send(Method::POST, "/api/v1/sync/session", Some(&hello))
        .await;
    res.assert_status(StatusCode::CREATED);

    let agreed = res.json()["capabilities"].as_u64().expect("a bitfield");
    assert!(
        CapabilityBits(agreed).has(Capability::SrvEnvelopeFloor),
        "a client writes a newer container only to a relay that agreed this bit"
    );
}
