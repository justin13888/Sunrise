//! `POST /api/v1/devices/by-vault-id/{id}/reinstate` through the real router
//! (ADR-0056 §5): the relay accepts a reinstatement only on the signed word of
//! the device it recorded as the revoker.

use crate::api::error::codes;
use crate::api::testing::{code_of, register_device, send_signed, Client};
use crate::ServerConfig;
use kynos::http::{Method, StatusCode};

const PHONE: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
const LAPTOP: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W80";
const DESKTOP: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W82";

fn revoke_path(vault_id: &str) -> String {
    format!("/api/v1/devices/by-vault-id/{vault_id}")
}

fn reinstate_path(vault_id: &str) -> String {
    format!("/api/v1/devices/by-vault-id/{vault_id}/reinstate")
}

/// The revoker's signed reinstatement lets the revoked device back in, is
/// counted once, and a retry after it landed is still `204`.
#[tokio::test]
async fn the_recorded_revoker_reinstates_and_a_retry_is_204() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 60, "phone", Some(PHONE)).await;
    let (laptop_id, laptop_key) = register_device(&client, 61, "laptop", Some(LAPTOP)).await;

    send_signed(
        &client,
        "DELETE",
        &revoke_path(PHONE),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &phone_id,
        &phone_key,
        None,
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);

    send_signed(
        &client,
        "POST",
        &reinstate_path(PHONE),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &phone_id,
        &phone_key,
        None,
    )
    .await
    .assert_status(StatusCode::OK);
    assert_eq!(client.metrics.get("sunrise_devices_reinstate_total"), 1);

    send_signed(
        &client,
        "POST",
        &reinstate_path(PHONE),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(
        client.metrics.get("sunrise_devices_reinstate_total"),
        1,
        "a retry that found the row active reinstated nothing"
    );

    let listed = send_signed(
        &client,
        "GET",
        "/api/v1/devices",
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .json();
    let phone = listed
        .as_array()
        .expect("an array")
        .iter()
        .find(|d| d["device_id"] == phone_id)
        .expect("the phone is listed")
        .clone();
    assert_eq!(phone["revoked"], serde_json::json!(false));
    assert_eq!(phone["revoked_at_ms"], serde_json::Value::Null);
}

/// The revoker is recorded by the key it signs with, not by its row: a client
/// re-registers on every start and gets a new row id each time, and the
/// reinstatement it sends from the new row must still be its own.
#[tokio::test]
async fn the_revoker_reinstates_from_a_row_it_registered_since() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 68, "phone", Some(PHONE)).await;
    let (laptop_id, laptop_key) = register_device(&client, 69, "laptop", Some(LAPTOP)).await;

    send_signed(
        &client,
        "DELETE",
        &revoke_path(PHONE),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    // The same key again: a second row, as every client start produces.
    let (relaunched_id, relaunched_key) =
        register_device(&client, 69, "laptop", Some(LAPTOP)).await;
    assert_ne!(
        relaunched_id, laptop_id,
        "re-registering mints a second row"
    );

    send_signed(
        &client,
        "POST",
        &reinstate_path(PHONE),
        &relaunched_id,
        &relaunched_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
    send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &phone_id,
        &phone_key,
        None,
    )
    .await
    .assert_status(StatusCode::OK);
}

/// Another device on the same account cannot undo a revocation it did not
/// make: that is the third-party un-revoke ADR-0056 §3 rejects.
#[tokio::test]
async fn a_device_that_did_not_revoke_it_is_refused() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 62, "phone", Some(PHONE)).await;
    let (laptop_id, laptop_key) = register_device(&client, 63, "laptop", Some(LAPTOP)).await;
    let (desktop_id, desktop_key) = register_device(&client, 64, "desktop", Some(DESKTOP)).await;

    send_signed(
        &client,
        "DELETE",
        &revoke_path(PHONE),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    let refused = send_signed(
        &client,
        "POST",
        &reinstate_path(PHONE),
        &desktop_id,
        &desktop_key,
        None,
    )
    .await;
    refused.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&refused), codes::DEVICE_NOT_REVOKER);
    send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &phone_id,
        &phone_key,
        None,
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(client.metrics.get("sunrise_devices_reinstate_total"), 0);
}

/// An unsigned revoke records no device, so nobody can reinstate what it
/// revoked; and an unsigned reinstatement is refused whatever the row says.
#[tokio::test]
async fn an_unsigned_revoke_or_reinstatement_leaves_the_device_revoked() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 65, "phone", Some(PHONE)).await;
    let (laptop_id, laptop_key) = register_device(&client, 66, "laptop", Some(LAPTOP)).await;

    client
        .send(Method::DELETE, &revoke_path(PHONE), None)
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let refused = send_signed(
        &client,
        "POST",
        &reinstate_path(PHONE),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    refused.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&refused), codes::DEVICE_NOT_REVOKER);

    let unsigned = client
        .send(Method::POST, &reinstate_path(PHONE), None)
        .await;
    unsigned.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&unsigned), codes::DEVICE_NOT_REVOKER);

    send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &phone_id,
        &phone_key,
        None,
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);
}

/// A vault id no row carries is `404`, and a malformed one is `400`.
#[tokio::test]
async fn an_unknown_or_malformed_vault_id_is_refused() {
    let client = Client::new(ServerConfig::default());
    let (laptop_id, laptop_key) = register_device(&client, 67, "laptop", Some(LAPTOP)).await;

    let unknown = send_signed(
        &client,
        "POST",
        &reinstate_path(PHONE),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    unknown.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&unknown), codes::DEVICE_NOT_FOUND);

    let malformed = send_signed(
        &client,
        "POST",
        &reinstate_path("not-a-vault-id"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    malformed.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&malformed), codes::VALIDATION_INVALID);
}
