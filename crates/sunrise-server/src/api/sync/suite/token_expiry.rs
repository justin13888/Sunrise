//! A session ends with its token, and a refresh extends it. Was
//! `ws_token_expiry`.

use super::*;

/// Was `an_idle_session_is_closed_when_its_token_expires` and
/// `an_inbound_frame_on_an_expired_token_ends_the_session`.
///
/// One test now, because the difference between them is gone: every
/// operation resolves the session, and an expired one resolves to nothing
/// whether or not anything arrived.
#[tokio::test]
async fn an_expired_token_ends_the_session() {
    let clock = TestClock::at(T0_MS);
    let verifier = StaticVerifier::default().with_expiring(
        "good",
        Subject::new(ISSUER, "alice"),
        T0_MS + 60_000,
    );
    let state = ServerState::with_clock(ServerConfig::default(), clock.clone())
        .with_verifier(Arc::new(verifier));
    let client = Client::from_state(state);
    let id = session_as(&client, "Bearer good").await;

    clock.set(T0_MS + 60_001);
    assert_eq!(
        subscribe_as(&client, "Bearer good", &id, None).await,
        StatusCode::UNAUTHORIZED
    );
}

/// Was `a_session_with_no_deadline_is_never_closed`.
#[tokio::test]
async fn a_session_with_no_deadline_is_never_closed() {
    let clock = TestClock::at(T0_MS);
    let state = ServerState::with_clock(ServerConfig::default(), clock.clone());
    let client = Client::from_state(state);
    let id = establish(&client).await;

    clock.set(T0_MS + 10_000_000);
    assert_eq!(
        subscribe_as(&client, BEARER, &id, None).await,
        StatusCode::NO_CONTENT
    );
}

/// Was `a_refresh_extends_the_session_without_a_reconnect`.
#[tokio::test]
async fn a_refresh_extends_the_session() {
    let clock = TestClock::at(T0_MS);
    let verifier = StaticVerifier::default()
        .with_expiring("first", Subject::new(ISSUER, "alice"), T0_MS + 60_000)
        .with_expiring("second", Subject::new(ISSUER, "alice"), T0_MS + 600_000);
    let state = ServerState::with_clock(ServerConfig::default(), clock.clone())
        .with_verifier(Arc::new(verifier));
    let client = Client::from_state(state);
    let id = session_as(&client, "Bearer first").await;

    let refreshed = client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some("Bearer first"),
            Some(&serde_json::json!({ "token": "second" })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    refreshed.assert_status(StatusCode::OK);
    assert_eq!(refreshed.json()["expires_at_ms"], T0_MS + 600_000);

    // Past the first deadline, the session is still live on the second.
    clock.set(T0_MS + 60_001);
    assert_eq!(
        subscribe_as(&client, "Bearer second", &id, None).await,
        StatusCode::NO_CONTENT
    );
}

/// Was `a_refresh_with_an_unverifiable_token_is_refused_but_keeps_the_session`.
#[tokio::test]
async fn an_unverifiable_refresh_is_refused_but_keeps_the_session() {
    let clock = TestClock::at(T0_MS);
    let verifier = StaticVerifier::default().with_expiring(
        "good",
        Subject::new(ISSUER, "alice"),
        T0_MS + 60_000,
    );
    let state =
        ServerState::with_clock(ServerConfig::default(), clock).with_verifier(Arc::new(verifier));
    let client = Client::from_state(state);
    let id = session_as(&client, "Bearer good").await;

    client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some("Bearer good"),
            Some(&serde_json::json!({ "token": "garbage" })),
            &[("x-sunrise-session", &id)],
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    // Still running on the credential it already had.
    assert_eq!(
        subscribe_as(&client, "Bearer good", &id, None).await,
        StatusCode::NO_CONTENT
    );
}

/// Was `a_refresh_naming_another_principal_ends_the_session`.
///
/// The channel namespace was fixed at establishment and is never
/// re-derived, so a token naming someone else must not be allowed to keep
/// reading this account's stream.
#[tokio::test]
async fn a_refresh_naming_another_principal_ends_the_session() {
    let clock = TestClock::at(T0_MS);
    let verifier = StaticVerifier::default()
        .with_expiring("alice", Subject::new(ISSUER, "alice"), T0_MS + 60_000)
        .with_expiring("bob", Subject::new(ISSUER, "bob"), T0_MS + 600_000);
    let state =
        ServerState::with_clock(ServerConfig::default(), clock).with_verifier(Arc::new(verifier));
    let client = Client::from_state(state);
    let id = session_as(&client, "Bearer alice").await;

    client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some("Bearer alice"),
            Some(&serde_json::json!({ "token": "bob" })),
            &[("x-sunrise-session", &id)],
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    assert!(
        client.sessions.get(&id, T0_MS).await.unwrap().is_none(),
        "the session must be gone, not merely refused"
    );
}

/// The other half of the refresh identity check, and the half nothing
/// reached.
///
/// [`a_refresh_naming_another_principal_ends_the_session`] uses two
/// different `sub`s, so `principal_key()` alone decides it and the
/// `device_id` clause beside it never has to be the reason. Here the
/// principal is identical and only the token's own
/// `https://sunrise.app/device_id` claim differs: one device must not be
/// able to hand its token to a session another device opened and keep that
/// session's stream alive.
#[tokio::test]
async fn a_refresh_naming_another_device_ends_the_session() {
    let clock = TestClock::at(T0_MS);
    let alice = Subject::new(ISSUER, "alice");
    let verifier = StaticVerifier::default()
        .with_device_id("phone", alice.clone(), "dev_phone")
        .with_device_id("phone-renewed", alice.clone(), "dev_phone")
        .with_device_id("laptop", alice, "dev_laptop");
    let state =
        ServerState::with_clock(ServerConfig::default(), clock).with_verifier(Arc::new(verifier));
    let client = Client::from_state(state);
    let id = session_as(&client, "Bearer phone").await;

    // The control: the same principal and the same device claim renews.
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some("Bearer phone"),
            Some(&serde_json::json!({ "token": "phone-renewed" })),
            &[("x-sunrise-session", &id)],
        )
        .await
        .assert_status(StatusCode::OK);

    client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some("Bearer phone"),
            Some(&serde_json::json!({ "token": "laptop" })),
            &[("x-sunrise-session", &id)],
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);

    assert!(
        client.sessions.get(&id, T0_MS).await.unwrap().is_none(),
        "the session must be gone, not merely refused"
    );
}

/// A refused removal on a refresh-identity mismatch is the relay's
/// failure: `503`, with the session still held, and the client's retry is
/// what ends it. A `401` here would report a session ended that is not.
#[tokio::test]
async fn a_refused_removal_on_a_refresh_mismatch_is_a_503_and_the_retry_ends_it() {
    let clock = TestClock::at(T0_MS);
    let verifier = StaticVerifier::default()
        .with_expiring("alice", Subject::new(ISSUER, "alice"), T0_MS + 60_000)
        .with_expiring("bob", Subject::new(ISSUER, "bob"), T0_MS + 600_000);
    let faulty = Arc::new(crate::sync_session::conformance::Faulty::default());
    let state = ServerState::with_clock(ServerConfig::default(), clock)
        .with_verifier(Arc::new(verifier))
        .with_session_backend(faulty.clone());
    let client = Client::from_state(state);
    let id = session_as(&client, "Bearer alice").await;
    let body = serde_json::json!({ "token": "bob" });
    let headers = [("x-sunrise-session", id.as_str())];
    let refresh = || {
        client.send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some("Bearer alice"),
            Some(&body),
            &headers,
        )
    };

    faulty.remove.store(true, Ordering::SeqCst);
    let res = refresh().await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        res.json()["code"],
        serde_json::json!(codes::RELAY_STORAGE_UNAVAILABLE)
    );
    assert!(
        client.sessions.get(&id, T0_MS).await.unwrap().is_some(),
        "the removal was refused, so the session is still held"
    );

    faulty.remove.store(false, Ordering::SeqCst);
    refresh().await.assert_status(StatusCode::UNAUTHORIZED);
    assert!(
        client.sessions.get(&id, T0_MS).await.unwrap().is_none(),
        "the retry ends the session"
    );
}

/// Every `update` a request makes on a live session answers a retryable
/// `503` when the store refuses it: the refresh's new deadline, the
/// subscribe's stream set, and the stream open's served mark.
#[tokio::test]
async fn a_refused_session_update_is_a_retryable_503() {
    let faulty = Arc::new(crate::sync_session::conformance::Faulty::default());
    let state = ServerState::new(ServerConfig::default()).with_session_backend(faulty.clone());
    let client = Client::from_state(state);
    let id = establish(&client).await;
    // Leaves the set unserved, so the stream open below has to update it.
    subscribe(&client, &id, None).await;

    faulty.update.store(true, Ordering::SeqCst);
    let refreshed = client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some(BEARER),
            Some(&serde_json::json!({ "token": "anything" })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    let subscribed = client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({
                "streams": [{ "stream_id": stream_hex(), "cursors": [] }]
            })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    let opened = open_events(&client, &id, &[]).await;
    for (what, res) in [
        ("refresh", refreshed),
        ("subscribe", subscribed),
        ("stream open", opened),
    ] {
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            res.json()["code"],
            serde_json::json!(codes::RELAY_STORAGE_UNAVAILABLE),
            "{what}"
        );
    }
}

/// A live stream that cannot re-read its session on a recheck has not
/// seen it expire: it closes with the retryable storage code, never
/// `AUTH_TOKEN_EXPIRED`, which would send the client to re-authenticate.
#[tokio::test]
async fn a_stream_that_loses_its_session_store_closes_retryably() {
    let faulty = Arc::new(crate::sync_session::conformance::Faulty::default());
    let state = ServerState::new(ServerConfig {
        device_recheck_ms: 20,
        ..ServerConfig::default()
    })
    .with_session_backend(faulty.clone());
    let client = Client::from_state(state);
    let id = establish(&client).await;
    subscribe(&client, &id, None).await;

    // The open has to resolve the session first; the store goes away
    // under the stream it served.
    let (body, ()) = tokio::join!(read(&client, &id, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        faulty.get.store(true, Ordering::SeqCst);
    });
    assert!(
        body.contains("\"kind\":\"caught_up\""),
        "the stream was served before the store went away: {body}"
    );
    assert!(
        body.contains("RELAY_STORAGE_UNAVAILABLE"),
        "the stream must close naming the store: {body}"
    );
    assert!(!body.contains("AUTH_TOKEN_EXPIRED"), "{body}");
}

/// Was `an_ack_for_a_token_with_no_expiry_reports_zero`.
#[tokio::test]
async fn a_refresh_for_a_token_with_no_expiry_reports_zero() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;

    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some(BEARER),
            Some(&serde_json::json!({ "token": "anything" })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    res.assert_status(StatusCode::OK);
    assert_eq!(
        res.json()["expires_at_ms"],
        0,
        "the self-host verifier issues no deadline"
    );
}

/// Was `a_refresh_with_an_undecodable_payload_is_refused`.
#[tokio::test]
async fn a_refresh_with_an_undecodable_body_is_refused() {
    let client = Client::new(ServerConfig::default());
    let id = establish(&client).await;

    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/session/refresh",
            Some(BEARER),
            Some(&serde_json::json!({ "not_a_token": 1 })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    // kynos's, not this surface's: a body that is JSON but does not match
    // the declared schema never reaches the handler, so `RefreshRequest`'s
    // missing `token` is refused at the extractor. The unknown
    // `not_a_token` is not what refuses it: every sync body ignores
    // unknown fields (issue #370, pinned by
    // `a_newer_clients_refresh_with_an_unknown_field_is_accepted`), so a
    // misspelt required field still fails here, as a missing one.
    // `is_client_error` could not tell that from the handler accepting the
    // body and failing later.
    res.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
}
