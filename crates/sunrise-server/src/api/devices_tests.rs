//! The `/devices*` handlers through the real router.

use crate::api::error::codes;
use crate::api::testing::{code_of, register_device, send_signed, Client};
use crate::state::ServerState;
use crate::{ServerConfig, StaticVerifier, Subject};
use base64::Engine as _;
use ed25519_dalek::SigningKey;
use kynos::http::{Method, StatusCode};
use std::sync::Arc;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn register_body(seed: u8, nickname: &str) -> serde_json::Value {
    let sk = SigningKey::from_bytes(&[seed; 32]);
    serde_json::json!({
        "device_pub_s": b64(sk.verifying_key().as_bytes()),
        "nickname": nickname,
        "platform": "linux",
    })
}

/// Register a device and return its id.
async fn register(client: &Client, seed: u8, nickname: &str) -> String {
    let res = client
        .send(
            Method::POST,
            "/api/v1/devices",
            Some(&register_body(seed, nickname)),
        )
        .await;
    res.assert_status(StatusCode::CREATED);
    res.json()["device_id"]
        .as_str()
        .expect("a device id")
        .to_owned()
}

/// The check a length test cannot make.
///
/// `[0x05; 32]` is thirty-two bytes, base64url-decodes cleanly, and is not a
/// point on the curve — exactly the value a length-only check admits and
/// `VerifyingKey::from_bytes` refuses. Registering it mints a device whose
/// every later request fails to verify, with nothing in the registration
/// response to say why.
#[tokio::test]
async fn a_thirty_two_byte_value_that_is_not_a_point_is_refused() {
    let client = Client::new(ServerConfig::default());
    let res = client
        .send(
            Method::POST,
            "/api/v1/devices",
            Some(&serde_json::json!({
                "device_pub_s": b64(&[0x05u8; 32]),
                "nickname": "laptop",
                "platform": "linux",
            })),
        )
        .await;

    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(
        res.json()["code"],
        codes::VALIDATION_INVALID,
        "the stable code must survive the move to a problem document"
    );
}

/// A well-formed key still registers, so the check above refuses the bad
/// value rather than everything.
#[tokio::test]
async fn a_real_ed25519_key_still_registers() {
    let client = Client::new(ServerConfig::default());
    let _ = register(&client, 1, "laptop").await;
}

/// The nickname bound is enforced at the byte the constant names, not at a
/// literal somebody typed twice.
///
/// Both lengths are derived from [`MAX_NICKNAME_BYTES`], so moving the
/// constant moves this test with it — the divergence this guards against is
/// between crates, and `sunrise-e2e/tests/nickname_bound_agreement.rs` is
/// where the other crate's number is compared to this one.
#[tokio::test]
async fn a_nickname_at_the_bound_registers_and_one_byte_over_does_not() {
    let client = Client::new(ServerConfig::default());
    let _ = register(&client, 5, &"n".repeat(super::MAX_NICKNAME_BYTES)).await;

    let res = client
        .send(
            Method::POST,
            "/api/v1/devices",
            Some(&register_body(
                6,
                &"n".repeat(super::MAX_NICKNAME_BYTES + 1),
            )),
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(res.json()["code"], codes::VALIDATION_INVALID);
}

/// An empty token can never wake anything, and storing it overwrites one
/// that could.
#[tokio::test]
async fn an_empty_push_token_is_refused() {
    let client = Client::new(ServerConfig::default());
    let device_id = register(&client, 2, "phone").await;

    client
        .send(
            Method::POST,
            "/api/v1/devices/push-tokens",
            Some(&serde_json::json!({
                "device_id": device_id,
                "platform": "apns",
                "token": "",
            })),
        )
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

/// The platform is an enum, so one no provider serves is refused by the
/// parse rather than stored and silently never delivered to.
///
/// The status is kynos's `422`, not the handler's `400`, and the difference
/// is the point: a body that parses as JSON but does not satisfy the
/// declared schema never reaches the handler, so the rejection is the
/// framework's rather than ours. The surface this replaces took `Bytes` and
/// answered `400` for every malformed body alike, which is why this is
/// pinned rather than smoothed over.
#[tokio::test]
async fn an_unknown_push_platform_is_refused_before_the_handler() {
    let client = Client::new(ServerConfig::default());
    let device_id = register(&client, 4, "tablet").await;

    client
        .send(
            Method::POST,
            "/api/v1/devices/push-tokens",
            Some(&serde_json::json!({
                "device_id": device_id,
                "platform": "carrier-pigeon",
                "token": "t",
            })),
        )
        .await
        .assert_status(StatusCode::UNPROCESSABLE_ENTITY);

    assert!(
        !client
            .metrics
            .render()
            .contains("sunrise_push_register_total"),
        "nothing may have been filed"
    );
}

/// Both counters were dropped in the port. A counter that stops being
/// written fails nothing — it quietly stops answering the question it was
/// added for, which is why it needs a test rather than a reading.
#[tokio::test]
async fn the_device_counters_still_move() {
    let client = Client::new(ServerConfig::default());
    let device_id = register(&client, 3, "desktop").await;

    client
        .send(Method::GET, "/api/v1/devices", None)
        .await
        .assert_status(StatusCode::OK);

    client
        .send(
            Method::POST,
            "/api/v1/devices/push-tokens",
            Some(&serde_json::json!({
                "device_id": device_id,
                "platform": "apns",
                "token": "a-real-token",
            })),
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);

    // `sunrise_devices_list_total` folded into the HTTP family, which
    // counts the same requests by route.
    assert_eq!(
        client.metrics.get_with(
            "sunrise_http_requests_total",
            &[
                ("endpoint", "/api/v1/devices"),
                ("method", "GET"),
                ("status", "200"),
            ],
        ),
        1,
        "listing devices must be counted"
    );
    let rendered = client.metrics.render();
    assert!(
        rendered.contains("sunrise_push_register_total"),
        "filing a push token must be counted; got:\n{rendered}"
    );
}

// -- revocation ---------------------------------------------------------
//
// Issue #81: the relay's enforcement was read out of the handler and rested
// on two store-layer tests. Everything below drives it through HTTP.

/// A vault id, Crockford base-32 of 16 bytes, the way a `device_revoke`
/// names one.
const PHONE_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
/// A second one, for the laptop doing the revoking.
const LAPTOP_VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W80";

/// The whole point of the route: a vault can name the device it revoked.
///
/// `DELETE /api/v1/devices/{device_id}` takes the relay's own ULID, minted
/// here at registration and never carried into the vault, so a client
/// holding only the 16-byte id its `device_revoke` op names had no
/// expressible request to make.
#[tokio::test]
async fn a_device_is_revocable_by_the_id_its_vault_knows_it_by() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, _phone_key) = register_device(&client, 20, "phone", Some(PHONE_VAULT_ID)).await;
    let (laptop_id, laptop_key) =
        register_device(&client, 21, "laptop", Some(LAPTOP_VAULT_ID)).await;

    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    // The row survives revocation, so the listing reports it — and reports
    // the vault id, which is what let the client address it.
    let listed = send_signed(
        &client,
        "GET",
        "/api/v1/devices",
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    listed.assert_status(StatusCode::OK);
    let rows = listed.json();
    let phone = rows
        .as_array()
        .expect("an array")
        .iter()
        .find(|d| d["device_id"] == phone_id)
        .expect("the phone is still listed")
        .clone();
    assert_eq!(phone["revoked"], serde_json::json!(true));
    assert_eq!(phone["vault_device_id"], serde_json::json!(PHONE_VAULT_ID));
}

/// A revoked device's signed request is refused, and this drives it
/// through the HTTP surface rather than through `Store::active_device`.
///
/// The SQL that enforces it ends `AND revoked = 0`, so a revoked device
/// resolves to no device at all — which is why the refusal is a 401 about
/// the credential rather than a 403 about the action.
#[tokio::test]
async fn a_revoked_devices_signed_request_is_401() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 22, "phone", Some(PHONE_VAULT_ID)).await;
    let (laptop_id, laptop_key) =
        register_device(&client, 23, "laptop", Some(LAPTOP_VAULT_ID)).await;

    // Before: the phone is an ordinary authenticated client.
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

    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    // After: the same request, signed by the same key, resolves to no
    // device and is refused.
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

/// A device a thief holds must not be able to erase the evidence of its
/// own theft — by either of the two names it now answers to.
#[tokio::test]
async fn a_device_cannot_revoke_itself_by_either_id() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 24, "phone", Some(PHONE_VAULT_ID)).await;

    let by_relay_id = send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/{phone_id}"),
        &phone_id,
        &phone_key,
        None,
    )
    .await;
    by_relay_id.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&by_relay_id), codes::AUTH_DEVICE_NOT_OWNER);

    // The vault id is a second name for the same row, so the same rule has
    // to be read off it or the route is a way round the first one.
    let by_vault_id = send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
        &phone_id,
        &phone_key,
        None,
    )
    .await;
    by_vault_id.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(code_of(&by_vault_id), codes::AUTH_DEVICE_NOT_OWNER);

    // And it is still authenticating, which is what the refusal is for.
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

/// A device that re-registers is a second row, and all of its rows are the
/// device the caller is revoking.
///
/// `docs/06-server/api.md` records the re-registration as deliberate — it
/// is what lets a client run the bootstrap on every start. Revoking one row
/// and leaving the rest would leave the device authenticating under a row
/// its owner has no way to name.
#[tokio::test]
async fn revoking_by_vault_id_revokes_every_row_that_device_registered() {
    let client = Client::new(ServerConfig::default());
    let (first, first_key) = register_device(&client, 25, "phone", Some(PHONE_VAULT_ID)).await;
    let (second, second_key) = register_device(&client, 26, "phone", Some(PHONE_VAULT_ID)).await;
    assert_ne!(first, second, "re-registering mints a second row");
    let (laptop_id, laptop_key) =
        register_device(&client, 27, "laptop", Some(LAPTOP_VAULT_ID)).await;

    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    for (id, key) in [(&first, &first_key), (&second, &second_key)] {
        send_signed(&client, "GET", "/api/v1/accounts/me", id, key, None)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }
}

/// `DELETE /api/v1/devices/{device_id}` — the success branch, which no
/// test drove.
///
/// Only the 403 self-revoke refusal reached this route, so replacing the
/// store call and its `NotFound` arm with `Ok(())` failed nothing, and
/// `sunrise_devices_revoke_total` was never observed at all. The e2e test
/// that names this route revokes through `Store` directly and never issues
/// the HTTP `DELETE`.
#[tokio::test]
async fn revoking_another_device_by_its_relay_id_is_204_and_is_counted() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 33, "phone", Some(PHONE_VAULT_ID)).await;
    let (laptop_id, laptop_key) =
        register_device(&client, 34, "laptop", Some(LAPTOP_VAULT_ID)).await;

    assert_eq!(
        client.metrics.get("sunrise_devices_revoke_total"),
        0,
        "nothing has been revoked yet"
    );

    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/{phone_id}"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    assert_eq!(
        client.metrics.get("sunrise_devices_revoke_total"),
        1,
        "a revocation must be counted"
    );

    // And it really revoked: the row is a soft delete the listing reports,
    // and the device no longer authenticates.
    let listed = send_signed(
        &client,
        "GET",
        "/api/v1/devices",
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    listed.assert_status(StatusCode::OK);
    let phone = listed
        .json()
        .as_array()
        .expect("an array")
        .iter()
        .find(|d| d["device_id"] == phone_id)
        .expect("the phone is still listed")
        .clone();
    assert_eq!(phone["revoked"], serde_json::json!(true));

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

/// The `StoreError::NotFound` arm of the same route, equally undriven.
///
/// A relay device id that is not an active row on this account is a 404
/// carrying `DEVICE_NOT_FOUND` — for an id that was never issued, and for
/// one that was already revoked alike, so the route answers nothing about
/// which devices exist. Nothing is counted on either.
#[tokio::test]
async fn revoking_an_unknown_or_already_revoked_relay_id_is_404() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, _phone_key) = register_device(&client, 35, "phone", Some(PHONE_VAULT_ID)).await;
    let (laptop_id, laptop_key) =
        register_device(&client, 36, "laptop", Some(LAPTOP_VAULT_ID)).await;

    let res = send_signed(
        &client,
        "DELETE",
        "/api/v1/devices/dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&res), codes::DEVICE_NOT_FOUND);
    assert_eq!(
        client.metrics.get("sunrise_devices_revoke_total"),
        0,
        "a refusal must not be counted as a revocation"
    );

    // A second revoke of a device that really was revoked is the same
    // answer, so the two are indistinguishable to a caller.
    let target = format!("/api/v1/devices/{phone_id}");
    send_signed(&client, "DELETE", &target, &laptop_id, &laptop_key, None)
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let res = send_signed(&client, "DELETE", &target, &laptop_id, &laptop_key, None).await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&res), codes::DEVICE_NOT_FOUND);
    assert_eq!(client.metrics.get("sunrise_devices_revoke_total"), 1);
}

/// The malformed-vault-id check exists on *both* sides and only the
/// registration side was tested.
///
/// An id that is not 26 Crockford base-32 characters can never equal one a
/// real `device_revoke` names, so the revoke route refuses it as a 400
/// rather than looking it up and answering the 404 that says "no active
/// row carries it" — which a client would read as "the device is gone".
#[tokio::test]
async fn a_malformed_vault_device_id_is_refused_at_revocation() {
    let client = Client::new(ServerConfig::default());
    let (laptop_id, laptop_key) =
        register_device(&client, 37, "laptop", Some(LAPTOP_VAULT_ID)).await;

    let res = send_signed(
        &client,
        "DELETE",
        "/api/v1/devices/by-vault-id/not-a-vault-id",
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&res), codes::VALIDATION_INVALID);
}

/// A vault id no active row carries is a 404, and a client must not read
/// that as "the device is now refused".
///
/// The three cases behind it are a second revoke, another account's device,
/// and — the dangerous one — a device that registered before it sent a
/// `vault_device_id` at all, which this relay is still accepting under a
/// row the caller cannot name.
#[tokio::test]
async fn a_vault_id_no_active_row_carries_is_404() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 28, "phone", None).await;
    let (laptop_id, laptop_key) =
        register_device(&client, 29, "laptop", Some(LAPTOP_VAULT_ID)).await;

    let res = send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await;
    res.assert_status(StatusCode::NOT_FOUND);
    assert_eq!(code_of(&res), codes::DEVICE_NOT_FOUND);

    // And the phone really is still accepted, which is the fact the 404
    // hides and the reason the client logs it as a warning.
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

    // A second revoke of a device that *was* revoked lands in the same
    // place, so the two are indistinguishable to a caller — deliberately.
    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{LAPTOP_VAULT_ID}"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::FORBIDDEN);
}

/// An id that is not a well-formed vault id can never equal one a real
/// `device_revoke` names, so accepting it at registration would file a
/// device that looks revocable and is not.
#[tokio::test]
async fn a_malformed_vault_device_id_is_refused_at_registration() {
    let client = Client::new(ServerConfig::default());
    let res = client
        .send(
            Method::POST,
            "/api/v1/devices",
            Some(&serde_json::json!({
                "device_pub_s": b64(SigningKey::from_bytes(&[30u8; 32])
                    .verifying_key()
                    .as_bytes()),
                "vault_device_id": "not-a-vault-id",
                "nickname": "phone",
                "platform": "linux",
            })),
        )
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    assert_eq!(code_of(&res), codes::VALIDATION_INVALID);
}

/// The gap, pinned rather than left to be discovered.
///
/// This relay has no issuer, so `require_device_sig` resolves off — the
/// state a self-host relay, or an operator's explicit
/// `require_device_sig = false`, leaves it in. With no `X-Sunrise-Device-Sig`,
/// `verify_bytes` returns `Ok(None)`: no device is resolved, so no
/// revocation check runs at all. A revoked device that simply stops signing
/// keeps working, and nothing in the relay notices.
///
/// This asserts the hole so that closing it fails a test rather than
/// passing silently, and so that nobody reads the tests above as a
/// guarantee that holds for a deployment leaving the flag alone.
#[tokio::test]
async fn without_a_device_signature_a_revoked_device_is_not_device_bound() {
    let client = Client::new(ServerConfig::default());
    let (phone_id, phone_key) = register_device(&client, 31, "phone", Some(PHONE_VAULT_ID)).await;
    let (laptop_id, laptop_key) =
        register_device(&client, 32, "laptop", Some(LAPTOP_VAULT_ID)).await;

    send_signed(
        &client,
        "DELETE",
        &format!("/api/v1/devices/by-vault-id/{PHONE_VAULT_ID}"),
        &laptop_id,
        &laptop_key,
        None,
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);

    // Signed: refused, as the tests above establish.
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

    // Unsigned, same bearer: served. Nothing bound the request to the
    // revoked device, so there was nothing to refuse.
    client
        .send(Method::GET, "/api/v1/accounts/me", None)
        .await
        .assert_status(StatusCode::OK);
}

/// No credential at all, against a verifier that actually checks.
///
/// The default `NullVerifier` accepts an absent bearer on purpose — that is
/// what makes self-host work and what makes enabling authentication purely
/// a matter of configuring a verifier — so every `None` request elsewhere
/// in this module proves nothing about whether these routes are
/// authenticated at all. Blobs solved this at
/// `every_blob_route_requires_a_bearer` and sync at
/// `an_unauthenticated_session_is_refused`; the device routes had no
/// equivalent, so nothing here would have failed if the security scheme
/// came off one of them.
#[tokio::test]
async fn every_device_route_requires_a_bearer() {
    let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
        StaticVerifier::default().with("test", Subject::new("https://idp.example", "alice")),
    ));
    let client = Client::from_state(state);
    for (method, path) in [
        (Method::GET, "/api/v1/devices"),
        (Method::POST, "/api/v1/devices"),
        (Method::POST, "/api/v1/devices/push-tokens"),
        (
            Method::DELETE,
            "/api/v1/devices/dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
        ),
        (
            Method::DELETE,
            "/api/v1/devices/by-vault-id/01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
        ),
    ] {
        let res = client.send_as(method.clone(), path, None, None).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} must refuse an unauthenticated caller"
        );
    }
}

/// Sign-up being disabled is a statement about the server, not about the
/// caller's credential, and the surface this replaces answered 403. It
/// carries the code `api.md` documents for it.
#[tokio::test]
async fn signup_disabled_is_a_403_not_a_401() {
    let client = Client::new(ServerConfig {
        allow_signup: false,
        ..ServerConfig::default()
    });

    let res = client.send(Method::GET, "/api/v1/devices", None).await;
    res.assert_status(StatusCode::FORBIDDEN);
    assert_eq!(
        crate::api::testing::code_of(&res),
        crate::api::error::codes::AUTH_SIGNUP_DISABLED
    );
}
