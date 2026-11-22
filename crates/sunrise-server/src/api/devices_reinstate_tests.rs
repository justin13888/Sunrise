//! `POST /api/v1/devices/by-vault-id/{id}/reinstate` through the real router
//! (ADR-0056 §5): the relay accepts a reinstatement only on the signed word of
//! the device it recorded as the revoker.

use crate::api::error::codes;
use crate::api::testing::{code_of, register_device, send_signed, Client, Res, BEARER};
use crate::state::{Clock, ServerState};
use crate::ServerConfig;
use kynos::http::{Method, StatusCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const PHONE: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
const LAPTOP: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W80";
const DESKTOP: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W82";

const T0_MS: u64 = 1_800_000_000_000;

/// A clock the test moves. A reinstatement must be signed in a later second
/// than the revoke it undoes, and the `Date` header carries whole seconds, so
/// a test on the wall clock would pass or fail by where in a second it ran.
#[derive(Debug)]
struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

fn relay() -> (Client, Arc<TestClock>) {
    let clock = Arc::new(TestClock(AtomicU64::new(T0_MS)));
    let state = ServerState::with_clock(ServerConfig::default(), clock.clone());
    (Client::from_state(state), clock)
}

/// A request signed once, so the test can send the same bytes again later,
/// the way an eavesdropper replays one.
struct Captured {
    target: String,
    device_id: String,
    date: String,
    signature: String,
}

impl Captured {
    fn sign(
        client: &Client,
        target: &str,
        device_id: &str,
        key: &ed25519_dalek::SigningKey,
    ) -> Self {
        let date = sunrise_http_sig::date_header(client.clock_now_ms());
        let signature =
            sunrise_http_sig::sign::<serde_json::Value>(key, "POST", target, &date, None)
                .expect("the client half signs");
        Self {
            target: target.to_owned(),
            device_id: device_id.to_owned(),
            date,
            signature,
        }
    }

    async fn send(&self, client: &Client) -> Res {
        client
            .send_with(
                Method::POST,
                &self.target,
                Some(BEARER),
                None,
                &[
                    ("x-sunrise-device", self.device_id.as_str()),
                    ("x-sunrise-device-sig", self.signature.as_str()),
                    ("date", self.date.as_str()),
                ],
            )
            .await
    }
}

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
    let (client, clock) = relay();
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
    clock.advance(1_000);
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
    let (client, clock) = relay();
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
    clock.advance(1_000);

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

/// `header_sig_v2` has no nonce, so a captured reinstatement stays valid for
/// the ±300 s `Date` window. Replayed after its revoker revoked the device
/// again inside that window, it is refused `409` and the device stays out. A
/// reinstatement signed in the same second as the revoke is refused the same
/// way, and succeeds once signed again a second later.
#[tokio::test]
async fn a_replayed_reinstatement_does_not_undo_a_later_revoke() {
    let (client, clock) = relay();
    let (phone_id, phone_key) = register_device(&client, 70, "phone", Some(PHONE)).await;
    let (laptop_id, laptop_key) = register_device(&client, 71, "laptop", Some(LAPTOP)).await;
    let revoke_target = revoke_path(PHONE);
    let revoke = || {
        send_signed(
            &client,
            "DELETE",
            &revoke_target,
            &laptop_id,
            &laptop_key,
            None,
        )
    };
    let phone_status = || async {
        send_signed(
            &client,
            "GET",
            "/api/v1/accounts/me",
            &phone_id,
            &phone_key,
            None,
        )
        .await
        .status
    };

    revoke().await.assert_status(StatusCode::NO_CONTENT);
    let same_second = Captured::sign(&client, &reinstate_path(PHONE), &laptop_id, &laptop_key);
    let refused = same_second.send(&client).await;
    refused.assert_status(StatusCode::CONFLICT);
    assert_eq!(code_of(&refused), codes::DEVICE_REINSTATE_PREDATES_REVOKE);

    clock.advance(1_000);
    let captured = Captured::sign(&client, &reinstate_path(PHONE), &laptop_id, &laptop_key);
    captured
        .send(&client)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(phone_status().await, StatusCode::OK);

    clock.advance(1_000);
    revoke().await.assert_status(StatusCode::NO_CONTENT);
    clock.advance(60_000);
    let replayed = captured.send(&client).await;
    replayed.assert_status(StatusCode::CONFLICT);
    assert_eq!(code_of(&replayed), codes::DEVICE_REINSTATE_PREDATES_REVOKE);
    assert_eq!(
        phone_status().await,
        StatusCode::UNAUTHORIZED,
        "the replay left the device revoked"
    );
    assert_eq!(client.metrics.get("sunrise_devices_reinstate_total"), 1);

    Captured::sign(&client, &reinstate_path(PHONE), &laptop_id, &laptop_key)
        .send(&client)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    assert_eq!(phone_status().await, StatusCode::OK);
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

/// `POST /api/v1/devices` for the device `seed` derives, unsigned, as the
/// bootstrap sends it.
async fn register_raw(client: &Client, seed: u8, vault_device_id: &str) -> Res {
    use base64::Engine as _;
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let body = serde_json::json!({
        "device_pub_s": base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(key.verifying_key().as_bytes()),
        "vault_device_id": vault_device_id,
        "nickname": "phone",
        "platform": "linux",
    });
    client
        .send(Method::POST, "/api/v1/devices", Some(&body))
        .await
}

/// A revoked device holding a bearer cannot register its way back (#1300):
/// registration asks for nothing more than the bearer, so without the refusal
/// the device would sign under a fresh active row. Its revoker's
/// reinstatement is the way back.
#[tokio::test]
async fn a_revoked_device_cannot_register_again_until_reinstated() {
    let (client, clock) = relay();
    let (phone_id, phone_key) = register_device(&client, 90, "phone", Some(PHONE)).await;
    let (laptop_id, laptop_key) = register_device(&client, 91, "laptop", Some(LAPTOP)).await;

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

    // Its own key, a fresh key under its vault id, and its own key under a
    // vault id nothing carries.
    for (seed, vault_id) in [(90, PHONE), (92, PHONE), (90, DESKTOP)] {
        let refused = register_raw(&client, seed, vault_id).await;
        refused.assert_status(StatusCode::FORBIDDEN);
        assert_eq!(code_of(&refused), codes::DEVICE_REVOKED_REGISTER);
    }
    assert_eq!(client.metrics.get("sunrise_devices_register_total"), 2);
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

    clock.advance(1_000);
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
    register_raw(&client, 90, PHONE)
        .await
        .assert_status(StatusCode::CREATED);
}
