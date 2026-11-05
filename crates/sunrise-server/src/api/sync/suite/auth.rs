//! Who may open a session, and that a session sees only its own tenant's
//! frames. Was `ws_auth`.

use super::*;

/// Was `unauthenticated_upgrade_is_refused`.
///
/// Against a verifier that actually checks, as the socket test's own relay
/// did. `NullVerifier` accepts an absent header by design — that is what
/// makes self-host work and what makes enabling authentication purely a
/// matter of configuring a verifier.
#[tokio::test]
async fn an_unauthenticated_session_is_refused() {
    let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
        StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
    ));
    let client = Client::from_state(state);
    client
        .send_as(Method::POST, "/api/v1/sync/session", None, Some(&hello()))
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// A session store that does not answer is the relay's failure, not the
/// caller's: establishment and every operation naming a session answer a
/// retryable `503`, never the `401` that would send a client to re-auth.
#[tokio::test]
async fn an_unreachable_session_store_is_a_retryable_503() {
    let state = ServerState::new(ServerConfig::default())
        .with_session_backend(Arc::new(crate::sync_session::conformance::Unreachable));
    let client = Client::from_state(state);

    let res = client
        .send_as(
            Method::POST,
            "/api/v1/sync/session",
            Some(BEARER),
            Some(&hello()),
        )
        .await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        res.json()["code"],
        serde_json::json!(codes::RELAY_STORAGE_UNAVAILABLE)
    );

    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({ "streams": [] })),
            &[("x-sunrise-session", "ses_00000000000000000000000000000000")],
        )
        .await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        res.json()["code"],
        serde_json::json!(codes::RELAY_STORAGE_UNAVAILABLE)
    );
}

/// Was `unknown_bearer_is_refused`.
#[tokio::test]
async fn an_unknown_bearer_is_refused() {
    let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
        StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
    ));
    let client = Client::from_state(state);

    client
        .send_as(
            Method::POST,
            "/api/v1/sync/session",
            Some("Bearer nope"),
            Some(&hello()),
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// Was `valid_bearer_is_accepted`.
#[tokio::test]
async fn a_valid_bearer_establishes_a_session() {
    let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
        StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
    ));
    let client = Client::from_state(state);
    let _ = session_as(&client, "Bearer good").await;
}

/// Was `one_tenant_never_receives_another_tenants_frames`.
///
/// The channel key is `(account_hash, stream_id)` and the account half comes
/// from the verified token, never from anything the client sends — so naming
/// another tenant's stream id reaches a different channel entirely.
#[tokio::test]
async fn one_tenant_never_receives_another_tenants_frames() {
    let verifier = StaticVerifier::default()
        .with("alice", Subject::new(ISSUER, "alice"))
        .with("bob", Subject::new(ISSUER, "bob"));
    let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(verifier));
    let client = Client::from_state(state);

    let alice = session_as(&client, "Bearer alice").await;
    let bob = session_as(&client, "Bearer bob").await;

    assert_eq!(
        subscribe_as(&client, "Bearer bob", &bob, None).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        publish_as(&client, "Bearer alice", &alice, vec![], 1).await,
        StatusCode::OK
    );

    let body = read_as(&client, "Bearer bob", &bob, &[]).await;
    assert!(
        !body.contains("\"kind\":\"ops\""),
        "another tenant's frame reached this stream: {body}"
    );
    assert!(
        body.contains("\"kind\":\"caught_up\""),
        "the stream must still open and report its own emptiness: {body}"
    );
}

/// Was `two_sessions_of_one_tenant_still_fan_out`: the isolation above is
/// per account, not per session.
#[tokio::test]
async fn two_sessions_of_one_tenant_both_receive() {
    let client = Client::new(ServerConfig::default());
    let publisher = establish(&client).await;
    let reader = establish(&client).await;

    subscribe(&client, &reader, None).await;
    assert_eq!(
        publish(&client, &publisher, vec![], 1).await,
        StatusCode::OK
    );

    let body = read(&client, &reader, &[]).await;
    assert!(
        body.contains("\"kind\":\"ops\""),
        "a sibling session's batch must fan out: {body}"
    );
}
