//! The account routes and the snapshot pin against the loopback relay:
//! what goes on the wire through the generated client, and how each reply is
//! read back (#774).

use super::*;
use crate::transport::{IdentityChainPinned, KeyBundlePinned, ReinstateOutcome};
use base64::Engine as _;

fn key_bundle_pin() -> String {
    format!(
        r#"{{"blob_id":"blb_{}","size_bytes":9,"manifest_digest":"{}","published_at_ms":1}}"#,
        "0f".repeat(16),
        "ab".repeat(32)
    )
}

/// The key bundle: a pin read back as bytes, the relay's "no pin" told apart
/// from a relay that predates the route, and a superseded pin told apart
/// from a refusal. Every request carries a binding that verifies over the
/// value the relay parsed.
#[tokio::test]
async fn the_key_bundle_is_read_pinned_and_its_404s_told_apart() {
    let relay = relay::start(vec![
        relay::Canned::json(200, &key_bundle_pin()),
        relay::Canned::json(404, r#"{"code":"BLOB_NOT_FOUND"}"#),
        relay::Canned::empty(404),
        relay::Canned::json(200, &key_bundle_pin()),
        relay::Canned::json(409, r#"{"code":"KEY_BUNDLE_SUPERSEDED"}"#),
        relay::Canned::json(503, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#),
    ])
    .await;
    let mut t = dial(&relay);

    let pin = t
        .key_bundle_latest()
        .await
        .expect("a pin")
        .expect("there is one");
    assert_eq!(pin.blob_id, [0x0f; 16]);
    assert_eq!(pin.manifest_digest, [0xab; 32]);
    assert_eq!(t.key_bundle_latest().await.expect("no pin"), None);
    assert!(
        matches!(
            t.key_bundle_latest().await,
            Err(TransportError::Unsupported)
        ),
        "a 404 without the route's own code is a relay that predates it"
    );

    assert_eq!(
        t.key_bundle_pin(&[0x0f; 16], &[0xab; 32], Some(&[0x01; 16]))
            .await
            .expect("pinned"),
        KeyBundlePinned::Pinned
    );
    assert_eq!(
        t.key_bundle_pin(&[0x0f; 16], &[0xab; 32], None)
            .await
            .expect("not a failure"),
        KeyBundlePinned::Superseded
    );
    let err = t
        .key_bundle_pin(&[0x0f; 16], &[0xab; 32], None)
        .await
        .expect_err("an outage is a refusal");
    assert_eq!(code_of(&err), "RELAY_STORAGE_UNAVAILABLE");

    let seen = relay.seen();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, "/api/v1/accounts/me/key_bundle");
    assert_json_binding(&seen[0]);
    assert_eq!(seen[3].method, "PUT");
    assert_json_binding(&seen[3]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&seen[3].body).expect("JSON"),
        serde_json::json!({
            "blob_id": format!("blb_{}", "0f".repeat(16)),
            "manifest_digest": "ab".repeat(32),
            "replaces": format!("blb_{}", "01".repeat(16)),
        })
    );
    // A pin with nothing to replace says so; whether as an absent member or
    // a `null` is the generated model's choice, and both are what the
    // description allows.
    let unreplaced: serde_json::Value = serde_json::from_slice(&seen[4].body).expect("JSON");
    assert!(
        unreplaced
            .get("replaces")
            .is_none_or(serde_json::Value::is_null),
        "{unreplaced}"
    );
    assert_json_binding(&seen[4]);
}

/// The identity chain: a record decoded from base64url, the relay's "no
/// record" told apart from a relay that predates the route, and a record
/// that does not decode refused rather than read as empty.
#[tokio::test]
async fn the_identity_chain_is_read_pinned_and_its_404s_told_apart() {
    let chain = b"an identity chain record";
    let pinned = format!(
        r#"{{"chain":"{}","digest":"{}","published_at_ms":1}}"#,
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(chain),
        "cd".repeat(32)
    );
    let relay = relay::start(vec![
        relay::Canned::json(200, &pinned),
        relay::Canned::json(404, r#"{"code":"IDENTITY_CHAIN_NOT_FOUND"}"#),
        relay::Canned::empty(404),
        relay::Canned::json(
            200,
            &format!(
                r#"{{"chain":"not base64!","digest":"{}","published_at_ms":1}}"#,
                "cd".repeat(32)
            ),
        ),
        relay::Canned::json(200, &pinned),
        relay::Canned::json(409, r#"{"code":"IDENTITY_CHAIN_SUPERSEDED"}"#),
    ])
    .await;
    let mut t = dial(&relay);

    assert_eq!(
        t.identity_chain_latest().await.expect("a record"),
        Some(chain.to_vec())
    );
    assert_eq!(t.identity_chain_latest().await.expect("no record"), None);
    assert!(matches!(
        t.identity_chain_latest().await,
        Err(TransportError::Unsupported)
    ));
    assert!(
        matches!(
            t.identity_chain_latest().await,
            Err(TransportError::Protocol(m)) if m.contains("malformed")
        ),
        "a record that does not decode is not an empty one"
    );

    assert_eq!(
        t.identity_chain_pin(chain, Some(&[0xcd; 32]))
            .await
            .expect("pinned"),
        IdentityChainPinned::Pinned
    );
    assert_eq!(
        t.identity_chain_pin(chain, None)
            .await
            .expect("not a failure"),
        IdentityChainPinned::Superseded
    );

    let seen = relay.seen();
    assert_eq!(seen[0].path, "/api/v1/accounts/me/identity_chain");
    assert_json_binding(&seen[0]);
    assert_eq!(seen[4].method, "PUT");
    assert_json_binding(&seen[4]);
    let sent: serde_json::Value = serde_json::from_slice(&seen[4].body).expect("JSON");
    assert_eq!(
        sent["chain"],
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(chain)
    );
    assert_eq!(sent["replaces"], "cd".repeat(32));
}

/// The recovery blob replacement: a relay without the route is told apart
/// from one that refused it, and the refusal keeps the relay's own code and
/// the `Retry-After` it asked for.
#[tokio::test]
async fn a_recovery_blob_replacement_keeps_unsupported_and_refused_apart() {
    let relay = relay::start(vec![
        relay::Canned::json(200, r#"{"retired":true}"#),
        relay::Canned::empty(405),
        relay::Canned::json(429, r#"{"code":"RATE_LIMITED"}"#).with_header("retry-after", "7"),
    ])
    .await;
    let mut t = dial(&relay);

    t.recovery_blob_replace(b"sealed").await.expect("replaced");
    assert!(matches!(
        t.recovery_blob_replace(b"sealed").await,
        Err(TransportError::Unsupported)
    ));
    let err = t
        .recovery_blob_replace(b"sealed")
        .await
        .expect_err("rate limited");
    assert_eq!(code_of(&err), "RATE_LIMITED");
    assert!(
        matches!(
            &err,
            TransportError::Server { retry_after: Some(d), .. } if d.as_secs() == 7
        ),
        "the relay's Retry-After survives the generated client: {err:?}"
    );

    let seen = relay.seen();
    assert_eq!(seen[0].method, "PUT");
    assert_eq!(seen[0].path, "/api/v1/accounts/me/recovery_blob");
    assert_json_binding(&seen[0]);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&seen[0].body).expect("JSON"),
        serde_json::json!({
            "recovery_blob": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sealed")
        })
    );
}

/// The snapshot pin: read back, absent as a 404, and pinned.
#[tokio::test]
async fn a_snapshot_pin_is_read_absent_and_pinned() {
    let stream = [0x5a; 16];
    let pin = format!(
        r#"{{"stream_id":"{}","blob_id":"blb_{}","size_bytes":42,"published_at_ms":1}}"#,
        "5a".repeat(16),
        "0f".repeat(16)
    );
    let relay = relay::start(vec![
        relay::Canned::json(200, &pin),
        relay::Canned::empty(404),
        relay::Canned::json(200, &pin),
    ])
    .await;
    let mut t = dial(&relay);

    let read = t
        .snapshot_latest(&stream)
        .await
        .expect("a pin")
        .expect("there is one");
    assert_eq!(read.blob_id, [0x0f; 16]);
    assert_eq!(read.size_bytes, 42);
    assert_eq!(t.snapshot_latest(&stream).await.expect("no pin"), None);
    let pinned = t.snapshot_pin(&stream, &[0x0f; 16]).await.expect("pinned");
    assert_eq!(pinned.size_bytes, 42);

    let seen = relay.seen();
    assert_eq!(
        seen[0].path,
        format!("/api/v1/snapshots/{}", "5a".repeat(16))
    );
    assert_json_binding(&seen[0]);
    assert_eq!(seen[2].method, "PUT");
    assert_json_binding(&seen[2]);
}

/// A refusal on these routes is read from what the relay sent, not from the
/// generated client's typed error: a documented status whose body is not a
/// problem document reaches the generated client's caller with no status at
/// all, and this transport still has to map it by status.
#[tokio::test]
async fn a_refusal_with_no_problem_document_still_maps_by_status() {
    let relay = relay::start(vec![relay::Canned::empty(401)]).await;
    let mut t = dial(&relay);
    let err = t
        .key_bundle_pin(&[0x0f; 16], &[0xab; 32], None)
        .await
        .expect_err("refused");
    assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
}

/// The reinstatement outcomes are kept apart (ADR-0056 §5): accepted again,
/// refused because this device is not the recorded revoker, and no such row
/// are each terminal and each a different thing to tell the user, while a
/// `403` with any other code is a binding the relay refused and stays an
/// error to retry.
#[tokio::test]
async fn a_reinstatement_keeps_reinstated_not_revoker_unknown_and_refused_apart() {
    let relay = relay::start(vec![
        relay::Canned::empty(204),
        relay::Canned::json(403, r#"{"code":"DEVICE_NOT_REVOKER"}"#),
        relay::Canned::json(404, r#"{"code":"DEVICE_NOT_FOUND"}"#),
        relay::Canned::json(403, r#"{"code":"AUTH_DEVICE_NOT_OWNER"}"#),
        relay::Canned::json(500, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#),
    ])
    .await;
    let mut t = dial(&relay);
    let device = [0x11u8; 16];

    assert_eq!(
        t.reinstate_device(device).await.expect("reinstated"),
        ReinstateOutcome::Reinstated
    );
    assert_eq!(
        t.reinstate_device(device).await.expect("not the revoker"),
        ReinstateOutcome::NotRevoker
    );
    assert_eq!(
        t.reinstate_device(device).await.expect("no such row"),
        ReinstateOutcome::Unknown
    );
    for _ in 0..2 {
        let err = t
            .reinstate_device(device)
            .await
            .expect_err("the relay would not");
        assert_eq!(code_of(&err), "AUTH_DEVICE_REINSTATE_FAILED");
    }

    let seen = relay.seen();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(
        seen[0].path,
        format!(
            "/api/v1/devices/by-vault-id/{}/reinstate",
            sunrise_id::crockford::encode_bytes(&device)
        ),
        "the route names the vault id, not the ULID the relay minted"
    );
    assert_json_binding(&seen[0]);
}
