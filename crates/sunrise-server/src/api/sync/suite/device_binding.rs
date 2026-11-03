//! A session bound to a registered, unrevoked device. Was `ws_device_binding`.

use super::*;

/// Was `an_unregistered_device_cannot_open_a_sync_session`.
///
/// It used to claim it also covered `a_revoked_device_cannot_open_a_sync_session`
/// — a test that has never existed in this tree (`#81`). The claim was
/// wrong on its own terms as well as dangling: this device id was never
/// registered, so the lookup misses for a reason a revoked device's does
/// not share. The revoked case is
/// [`a_revoked_device_cannot_open_a_session`] below, and it revokes a
/// device that really did register.
#[tokio::test]
async fn an_unregistered_device_cannot_open_a_session() {
    let client = Client::new(ServerConfig::default());
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/session",
            Some(BEARER),
            Some(&hello()),
            &[
                ("x-sunrise-device", "01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
                ("x-sunrise-device-sig", "AAAA"),
                ("date", "Mon, 01 Jan 2024 00:00:00 GMT"),
            ],
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// The test the comment above claimed for four months and nobody wrote.
///
/// A device that registered, signed, and was then revoked cannot open a
/// sync session: `verify_bytes` resolves it through `active_device`, whose
/// SQL ends `AND revoked = 0`, so it resolves to no device and the request
/// is refused as a credential failure rather than as a forbidden action.
#[tokio::test]
async fn a_revoked_device_cannot_open_a_session() {
    let client = Client::new(ServerConfig::default());
    let (phone, phone_key) =
        register_device(&client, 40, "phone", Some(SESSION_PHONE_VAULT_ID)).await;
    let (laptop, laptop_key) =
        register_device(&client, 41, "laptop", Some(SESSION_LAPTOP_VAULT_ID)).await;

    // While it is active, the phone opens a session.
    send_signed(
        &client,
        "POST",
        "/api/v1/sync/session",
        &phone,
        &phone_key,
        Some(&hello()),
    )
    .await
    .assert_status(StatusCode::CREATED);

    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{SESSION_PHONE_VAULT_ID}"),
        &laptop,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    send_signed(
        &client,
        "POST",
        "/api/v1/sync/session",
        &phone,
        &phone_key,
        Some(&hello()),
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);
}

/// A revoked device cannot publish, on a session it already holds.
///
/// This is the half that matters for `#80`: the session survives the
/// revocation as a row, and the upload is still refused, because every
/// `Signed<…>` route re-resolves the device on every request rather than
/// trusting what establishment recorded.
#[tokio::test]
async fn a_revoked_devices_signed_upload_is_refused() {
    let client = Client::new(ServerConfig::default());
    let (phone, phone_key) =
        register_device(&client, 42, "phone", Some(SESSION_PHONE_VAULT_ID)).await;
    let (laptop, laptop_key) =
        register_device(&client, 43, "laptop", Some(SESSION_LAPTOP_VAULT_ID)).await;

    let opened = send_signed(
        &client,
        "POST",
        "/api/v1/sync/session",
        &phone,
        &phone_key,
        Some(&hello()),
    )
    .await;
    opened.assert_status(StatusCode::CREATED);
    let session = opened.json()["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned();

    let batch = serde_json::json!({
        "stream_id": stream_hex(),
        "batch_id": 1,
        "ops": [envelope([0xaa; 16], 1)],
    });
    send_signed_with(
        &client,
        "POST",
        "/api/v1/sync/ops",
        &phone,
        &phone_key,
        Some(&batch),
        &[("x-sunrise-session", session.as_str())],
    )
    .await
    .assert_status(StatusCode::OK);

    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{SESSION_PHONE_VAULT_ID}"),
        &laptop,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    let batch = serde_json::json!({
        "stream_id": stream_hex(),
        "batch_id": 2,
        "ops": [envelope([0xaa; 16], 2)],
    });
    send_signed_with(
        &client,
        "POST",
        "/api/v1/sync/ops",
        &phone,
        &phone_key,
        Some(&batch),
        &[("x-sunrise-session", session.as_str())],
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);
}

/// Revoking a device ends the stream it is already holding.
///
/// `device_recheck_ms` is described in `config/model.rs` as "the bound on how
/// long a revoked device keeps receiving fan-out on a socket it already
/// holds", which is a security bound that had no test behind it (`#81`).
/// It is driven down here rather than waited out: the production default
/// is 30 s.
#[tokio::test]
async fn an_open_stream_is_torn_down_when_its_device_is_revoked() {
    let client = Client::new(ServerConfig {
        device_recheck_ms: 20,
        ..ServerConfig::default()
    });
    let (phone, phone_key) =
        register_device(&client, 44, "phone", Some(SESSION_PHONE_VAULT_ID)).await;
    let (laptop, laptop_key) =
        register_device(&client, 45, "laptop", Some(SESSION_LAPTOP_VAULT_ID)).await;

    let opened = send_signed(
        &client,
        "POST",
        "/api/v1/sync/session",
        &phone,
        &phone_key,
        Some(&hello()),
    )
    .await;
    opened.assert_status(StatusCode::CREATED);
    let session = opened.json()["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned();
    subscribe(&client, &session, None).await;

    // The revocation lands while the stream is open, which is the case the
    // recheck exists for — refusing the *next* connection would not end
    // this one.
    let (body, revoked) = tokio::join!(read_as(&client, BEARER, &session, &[]), async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        send_signed(
            &client,
            "DELETE",
            &format!("/api/v1/devices/by-vault-id/{SESSION_PHONE_VAULT_ID}"),
            &laptop,
            &laptop_key,
            None,
        )
        .await
        .status
    });
    assert_eq!(revoked, StatusCode::NO_CONTENT);
    assert!(
        body.contains("AUTH_DEVICE_REVOKED"),
        "the stream must close naming the revocation, not merely stop: {body}"
    );
}
