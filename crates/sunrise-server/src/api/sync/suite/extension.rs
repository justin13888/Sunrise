//! A newer client's extra fields (issue #370).
//!
//! Every sync body on the wire is open to extension
//! (protocol-versioning.md §4, §6): a field this relay predates is ignored,
//! never a decode failure. Each request body is pinned here with the field
//! a plausible newer client would add, and each test asserts the request
//! did its job — not merely that it was not refused — so a relay that
//! accepted the body and dropped a field it does know would still fail.

use super::*;

/// The `Hello`: ADR-0045's schema fingerprint and a relay floor are the
/// negotiation fields a newer client is expected to add next to the
/// capability bitfield.
#[tokio::test]
async fn a_newer_clients_hello_with_unknown_fields_establishes_a_session() {
    let client = Client::new(ServerConfig::default());
    let mut body = hello();
    body["schema_fingerprint"] = serde_json::json!("b3:0123456789abcdef");
    body["relay_floor"] = serde_json::json!({ "wire_proto": 2, "features": ["x"] });

    let res = client
        .send(Method::POST, "/api/v1/sync/session", Some(&body))
        .await;
    res.assert_status(StatusCode::CREATED);
    let ack = res.json();
    assert!(ack["session_id"].is_string(), "{ack}");
    assert_eq!(ack["wire_proto"], u32::from(WIRE_PROTO_V), "{ack}");
    assert!(
        CapabilityBits(ack["capabilities"].as_u64().expect("capabilities"))
            .has(Capability::SrvTokenRefresh),
        "the bitfield next to the unknown fields must still be read: {ack}"
    );
}

#[tokio::test]
async fn a_newer_clients_refresh_with_an_unknown_field_is_accepted() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;

    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some(BEARER),
            Some(&serde_json::json!({ "token": "anything", "token_kind": "dpop" })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    res.assert_status(StatusCode::OK);
    assert_eq!(res.json()["expires_at_ms"], 0);
}

/// Unknown fields at all three levels of a subscribe — the request, a
/// stream, and a cursor — and the cursor is still honoured: only the op it
/// does not cover replays.
#[tokio::test]
async fn a_newer_clients_subscribe_with_unknown_fields_still_narrows_the_replay() {
    let client = Client::new(ServerConfig::default());
    let writer = establish(&client).await;
    let device = [7u8; 16];
    for seq in 1..=3u64 {
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, seq)], seq).await,
            StatusCode::OK
        );
    }

    let reader = establish(&client).await;
    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({
                "priority": "foreground",
                "streams": [{
                    "stream_id": stream_hex(),
                    "doc_schema": 99,
                    "cursors": [{
                        "device_id": hex::encode(device),
                        "last_applied_seq": 2,
                        "last_applied_hlc": 12345,
                    }],
                }],
            })),
            &[("x-sunrise-session", &reader)],
        )
        .await;
    res.assert_status(StatusCode::NO_CONTENT);

    let body = read(&client, &reader, &[]).await;
    assert_eq!(
        body.matches("\"kind\":\"ops\"").count(),
        1,
        "the cursor beside the unknown fields must still narrow the replay: {body}"
    );
}

#[tokio::test]
async fn a_newer_clients_batch_with_an_unknown_field_is_acked() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": stream_hex(),
                "batch_id": 7,
                "ops": [envelope([7u8; 16], 1)],
                "compression": "zstd-dict-3",
            })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    res.assert_status(StatusCode::OK);
    let ack = res.json();
    assert_eq!(ack["batch_id"], 7, "{ack}");
    assert_eq!(ack["stream_id"], stream_hex(), "{ack}");
}

/// A relay that demands device signatures, which is the default once an
/// OIDC issuer is configured.
fn signing_relay() -> Client {
    Client::new(ServerConfig {
        require_device_sig: Some(true),
        ..ServerConfig::default()
    })
}

/// The four request bodies again, each **signed** by a registered device
/// on a relay that requires the binding, the way a real newer client
/// sends them.
///
/// The unsigned tests above cannot see this failure: the signature covers
/// the client's whole value, the extra field included, so a relay that
/// re-canonicalised the parsed `T` — which has dropped that field — would
/// hash something the client never signed and refuse every one of these as
/// `AUTH_DEVICE_SIG_INVALID`.
#[tokio::test]
async fn a_newer_clients_signed_requests_with_unknown_fields_are_verified() {
    let client = signing_relay();
    let (phone, key) = register_device(&client, 60, "phone", Some(SESSION_PHONE_VAULT_ID)).await;

    let mut hello = hello();
    hello["schema_fingerprint"] = serde_json::json!("b3:0123456789abcdef");
    hello["relay_floor"] = serde_json::json!({ "wire_proto": 2, "features": ["x"] });
    let opened = send_signed(
        &client,
        "POST",
        "/api/v1/sync/session",
        &phone,
        &key,
        Some(&hello),
    )
    .await;
    opened.assert_status(StatusCode::CREATED);
    let session = opened.json()["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let on_session = [("x-sunrise-session", session.as_str())];

    send_signed_with(
        &client,
        "POST",
        "/api/v1/sync/subscribe",
        &phone,
        &key,
        Some(&serde_json::json!({
            "priority": "foreground",
            "streams": [{
                "stream_id": stream_hex(),
                "doc_schema": 99,
                "cursors": [{
                    "device_id": hex::encode([7u8; 16]),
                    "last_applied_seq": 0,
                    "last_applied_hlc": 12345,
                }],
            }],
        })),
        &on_session,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    let published = send_signed_with(
        &client,
        "POST",
        "/api/v1/sync/ops",
        &phone,
        &key,
        Some(&serde_json::json!({
            "stream_id": stream_hex(),
            "batch_id": 7,
            "ops": [envelope([7u8; 16], 1)],
            "compression": "zstd-dict-3",
        })),
        &on_session,
    )
    .await;
    published.assert_status(StatusCode::OK);
    assert_eq!(published.json()["batch_id"], 7, "{}", published.json());

    send_signed_with(
        &client,
        "POST",
        "/api/v1/sync/session/refresh",
        &phone,
        &key,
        Some(&serde_json::json!({ "token": "anything", "token_kind": "dpop" })),
        &on_session,
    )
    .await
    .assert_status(StatusCode::OK);
}

/// The other half of the property: the unknown field is *inside* the
/// signature, not merely tolerated beside it. A field this relay ignores
/// still cannot be altered in flight.
#[tokio::test]
async fn an_unknown_field_altered_in_flight_fails_the_signature() {
    let client = signing_relay();
    let (phone, key) = register_device(&client, 61, "phone", Some(SESSION_PHONE_VAULT_ID)).await;

    let mut signed = hello();
    signed["relay_floor"] = serde_json::json!({ "wire_proto": 2 });
    let mut sent = signed.clone();
    sent["relay_floor"] = serde_json::json!({ "wire_proto": 3 });

    let date = now_rfc2822(&client);
    let signature =
        sunrise_http_sig::sign(&key, "POST", "/api/v1/sync/session", &date, Some(&signed))
            .expect("the client half signs");
    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/session",
            Some(BEARER),
            Some(&sent),
            &[
                ("x-sunrise-device", phone.as_str()),
                ("x-sunrise-device-sig", signature.as_str()),
                ("date", date.as_str()),
            ],
        )
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&res), codes::AUTH_DEVICE_SIG_INVALID);
}
