//! The device-signature binding through the real router.

use crate::api::error::codes::{AUTH_DEVICE_SIG_INVALID, AUTH_TOKEN_INVALID};
use crate::api::testing::{code_of, now_rfc2822, send_signed, Client, BEARER};
use crate::state::ServerState;
use crate::{ServerConfig, StaticVerifier, Subject};
use base64::Engine as _;
use ed25519_dalek::SigningKey;
use kynos::http::{Method, StatusCode};
use std::sync::Arc;

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Register a device and hand back its id and signing key.
///
/// A thin alias over the shared helper: this module's tests are about the
/// binding rather than about what a device registers, so they take the
/// default nickname and no vault id.
async fn paired(client: &Client, seed: u8) -> (String, SigningKey) {
    crate::api::testing::register_device(client, seed, "laptop", None).await
}

/// A correctly signed request reaches the handler.
///
/// Also the first caller `sunrise_http_sig::sign` has ever had: the client
/// half of the scheme was written and tested against itself, and never once
/// exercised against the server half until here.
#[tokio::test]
async fn a_correctly_signed_request_is_accepted() {
    let client = Client::new(ServerConfig::default());
    let (device_id, sk) = paired(&client, 7).await;

    send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &device_id,
        &sk,
        None::<&serde_json::Value>,
    )
    .await
    .assert_status(StatusCode::OK);
}

/// A signature from the wrong key never reaches the handler.
///
/// The extractor refuses it, which is the point of the shape: there is no
/// handler body left to forget the check in.
#[tokio::test]
async fn a_signature_from_the_wrong_key_is_refused() {
    let client = Client::new(ServerConfig::default());
    let (device_id, _) = paired(&client, 8).await;
    let impostor = SigningKey::from_bytes(&[9u8; 32]);

    let res = send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &device_id,
        &impostor,
        None::<&serde_json::Value>,
    )
    .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    // The device is real and active; only the signature is wrong, so the
    // caller is told which of the two credentials to fix.
    assert_eq!(code_of(&res), AUTH_DEVICE_SIG_INVALID);
}

/// A clock 400 s out is the commonest real cause of this refusal, and the
/// fix is "set your clock", not "log in again". Before the code existed
/// the client saw a bare `AUTH_TOKEN_INVALID` and refreshed a bearer that
/// was never the problem, into the same rejection, forever.
#[tokio::test]
async fn a_skewed_clock_is_told_its_clock_is_wrong() {
    let client = Client::new(ServerConfig::default());
    let (device_id, sk) = paired(&client, 14).await;

    let secs = i64::try_from(client.clock_now_ms() / 1000).expect("a sane clock")
        + sunrise_http_sig::MAX_CLOCK_SKEW_SECS
        + 100;
    let date = jiff::Timestamp::from_second(secs)
        .expect("a valid timestamp")
        .strftime("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let signature =
        sunrise_http_sig::sign::<serde_json::Value>(&sk, "GET", "/api/v1/accounts/me", &date, None)
            .expect("the client half signs");

    let res = client
        .send_with(
            Method::GET,
            "/api/v1/accounts/me",
            Some(BEARER),
            None,
            &[
                ("x-sunrise-device", &device_id),
                ("x-sunrise-device-sig", &signature),
                ("date", &date),
            ],
        )
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&res), AUTH_DEVICE_SIG_INVALID);
}

/// The guard on the whole distinction.
///
/// A correct signature over a device that is not an active row on this
/// account must be indistinguishable from a bad bearer — otherwise the
/// code answers "does this device exist here?" for a caller who has proved
/// nothing, which is exactly the enumeration oracle the collapse existed
/// to prevent.
#[tokio::test]
async fn an_unknown_device_is_still_indistinguishable_from_a_bad_bearer() {
    let client = Client::new(ServerConfig::default());
    let sk = SigningKey::from_bytes(&[15u8; 32]);

    let res = send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
        &sk,
        None::<&serde_json::Value>,
    )
    .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&res), AUTH_TOKEN_INVALID);
}

/// The other pre-lookup case that *does* name the signature: the server
/// has already published `device_binding_required` in `GET /meta`, so
/// saying "you did not sign" discloses nothing it has not advertised.
#[tokio::test]
async fn an_absent_binding_where_one_is_required_names_the_signature() {
    let client = Client::new(ServerConfig {
        require_device_sig: Some(true),
        ..ServerConfig::default()
    });

    let res = client.send(Method::GET, "/api/v1/accounts/me", None).await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&res), AUTH_DEVICE_SIG_INVALID);
}

/// And a *partial* binding takes the same path, which is easy to describe
/// wrongly: `verify_bytes` destructures the device and the signature
/// together, so one header without the other never reaches the lookup.
#[tokio::test]
async fn a_half_present_binding_is_the_same_pre_lookup_refusal() {
    let client = Client::new(ServerConfig {
        require_device_sig: Some(true),
        ..ServerConfig::default()
    });
    let (device_id, _) = paired(&client, 23).await;

    // A device id with no signature beside it.
    let res = client
        .send_with(
            Method::GET,
            "/api/v1/accounts/me",
            Some(BEARER),
            None,
            &[("x-sunrise-device", &device_id)],
        )
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&res), AUTH_DEVICE_SIG_INVALID);

    // And a signature with no device id naming whose it is.
    let res = client
        .send_with(
            Method::GET,
            "/api/v1/accounts/me",
            Some(BEARER),
            None,
            &[("x-sunrise-device-sig", "not-a-real-signature")],
        )
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&res), AUTH_DEVICE_SIG_INVALID);
}

/// A signature is not transferable between targets.
///
/// The method and the concrete path are inside the canonical string, so one
/// signed for `/accounts/me` does not verify for `/devices`.
#[tokio::test]
async fn a_signature_does_not_transfer_between_targets() {
    let client = Client::new(ServerConfig::default());
    let (device_id, sk) = paired(&client, 10).await;
    let date = now_rfc2822(&client);

    let signature =
        sunrise_http_sig::sign::<serde_json::Value>(&sk, "GET", "/api/v1/accounts/me", &date, None)
            .expect("signing");

    client
        .send_with(
            Method::GET,
            "/api/v1/devices",
            Some(BEARER),
            None,
            &[
                ("x-sunrise-device", &device_id),
                ("x-sunrise-device-sig", &signature),
                ("date", &date),
            ],
        )
        .await
        .assert_status(StatusCode::UNAUTHORIZED);
}

/// The cross-check the module documents as "so a stolen bearer cannot be
/// replayed from a different device", which nothing reached.
///
/// `Subject::device_id` is a public field with no builder, so no test ever
/// set it and deleting the clause in `verify_bytes` failed nothing. The
/// refusal is `AUTH_TOKEN_INVALID` rather than `AUTH_DEVICE_SIG_INVALID`:
/// the check runs before the device lookup, and the finer code is earned
/// only once the named device has resolved to an active row.
#[tokio::test]
async fn a_token_claiming_another_device_is_refused() {
    let claimed = "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
    let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
        StaticVerifier::default().with_device_id(
            "test",
            Subject::new("https://idp.example", "alice"),
            claimed,
        ),
    ));
    let client = Client::from_state(state);
    let (device_id, sk) = paired(&client, 33).await;
    assert_ne!(
        device_id, claimed,
        "the registered device must not be the one the token names"
    );

    let res = send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &device_id,
        &sk,
        None::<&serde_json::Value>,
    )
    .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    assert_eq!(code_of(&res), AUTH_TOKEN_INVALID);
}

/// The control for the test above: the identical request under a token
/// that claims *no* device is served. Without it a clause that refused
/// unconditionally would look like the cross-check working.
#[tokio::test]
async fn a_token_claiming_no_device_still_binds_to_the_header() {
    let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
        StaticVerifier::default().with("test", Subject::new("https://idp.example", "alice")),
    ));
    let client = Client::from_state(state);
    let (device_id, sk) = paired(&client, 34).await;

    send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &device_id,
        &sk,
        None::<&serde_json::Value>,
    )
    .await
    .assert_status(StatusCode::OK);
}

/// The bootstrap exemption covers an absent binding, not a wrong one.
#[tokio::test]
async fn a_bootstrap_route_still_checks_a_binding_that_is_offered() {
    let client = Client::new(ServerConfig::default());
    let (device_id, _) = paired(&client, 11).await;
    let impostor = SigningKey::from_bytes(&[12u8; 32]);

    let body = serde_json::json!({
        "device_pub_s": b64(SigningKey::from_bytes(&[13u8; 32]).verifying_key().as_bytes()),
        "nickname": "second",
        "platform": "linux",
    });

    send_signed(
        &client,
        "POST",
        "/api/v1/devices",
        &device_id,
        &impostor,
        Some(&body),
    )
    .await
    .assert_status(StatusCode::UNAUTHORIZED);
}
