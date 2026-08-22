//! `header_sig_v1` device binding, driven through the real router.
//!
//! `docs/06-server/api.md` §device-binding asks the server to check three
//! things on every authenticated request: that the `X-Sunrise-Device` id is an
//! active device of the account, that `X-Sunrise-Device-Sig` verifies under
//! that device's registered key, and that the token's device claim agrees with
//! the header. Each of those gets a test that fails only if the corresponding
//! check is missing.
//!
//! The point of the mechanism is revocation: a bearer token is a replayable
//! credential, so "revoked" has to mean something the token alone cannot
//! override. The revocation test is the one that proves it does.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use sunrise_server::auth::device_sig::canonical_string;
use sunrise_server::state::Clock;
use sunrise_server::{build_router, ServerConfig, ServerState, StaticVerifier, Subject};
use tower::ServiceExt;

/// 2024-01-01T00:00:00Z, and its HTTP-date spelling.
const T0_MS: u64 = 1_704_067_200_000;
const DATE: &str = "Mon, 01 Jan 2024 00:00:00 GMT";
const ISSUER: &str = "https://idp.example";

#[derive(Debug)]
struct FixedClock(u64);
impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Two accounts, plus a third bearer whose token carries a device claim.
fn verifier() -> StaticVerifier {
    let mut allowed = HashMap::new();
    allowed.insert("alice".to_string(), Subject::new(ISSUER, "alice"));
    allowed.insert("bob".to_string(), Subject::new(ISSUER, "bob"));
    StaticVerifier { allowed }
}

fn boot(require_device_sig: bool) -> axum::Router {
    let cfg = ServerConfig {
        require_device_sig,
        ..Default::default()
    };
    let state = ServerState::with_clock(cfg, Arc::new(FixedClock(T0_MS)))
        .with_verifier(Arc::new(verifier()));
    build_router(state)
}

/// Build a request, optionally device-bound.
struct Call<'a> {
    method: &'a str,
    path: &'a str,
    body: &'a str,
    bearer: &'a str,
    device: Option<(&'a str, &'a SigningKey)>,
    /// Sign a *different* payload than the one sent, to forge.
    sign_as: Option<(&'a str, &'a str, &'a str)>,
}

impl<'a> Call<'a> {
    fn new(method: &'a str, path: &'a str, bearer: &'a str) -> Self {
        Self {
            method,
            path,
            body: "",
            bearer,
            device: None,
            sign_as: None,
        }
    }
    fn body(mut self, b: &'a str) -> Self {
        self.body = b;
        self
    }
    fn device(mut self, id: &'a str, key: &'a SigningKey) -> Self {
        self.device = Some((id, key));
        self
    }
    fn sign_as(mut self, method: &'a str, path: &'a str, body: &'a str) -> Self {
        self.sign_as = Some((method, path, body));
        self
    }
    fn build(self) -> Request<axum::body::Body> {
        let mut b = Request::builder()
            .method(self.method)
            .uri(self.path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.bearer));
        if let Some((id, key)) = self.device {
            let (m, p, body) = self.sign_as.unwrap_or((self.method, self.path, self.body));
            let canonical = canonical_string(m, p, DATE, body.as_bytes());
            b = b
                .header("date", DATE)
                .header("x-sunrise-device", id)
                .header(
                    "x-sunrise-device-sig",
                    b64(&key.sign(canonical.as_bytes()).to_bytes()),
                );
        }
        b.body(axum::body::Body::from(self.body.to_string()))
            .unwrap()
    }
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    let body = to_bytes(res.into_body(), 65536).await.unwrap();
    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
}

/// Register a device on `bearer`'s account and return `(device_id, key)`.
async fn register(
    app: &axum::Router,
    bearer: &str,
    seed: u8,
    nickname: &str,
) -> (String, SigningKey) {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let body = serde_json::json!({
        "device_pub_s": b64(key.verifying_key().as_bytes()),
        "nickname": nickname,
        "platform": "linux",
    })
    .to_string();
    let res = app
        .clone()
        .oneshot(
            Call::new("POST", "/api/v1/devices", bearer)
                .body(&body)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        StatusCode::CREATED,
        "registering the first device must not itself require a device binding"
    );
    let id = json(res).await["device_id"].as_str().unwrap().to_string();
    (id, key)
}

#[tokio::test]
async fn a_correctly_signed_request_from_a_registered_device_is_accepted() {
    let app = boot(true);
    let (id, key) = register(&app, "alice", 1, "laptop").await;
    let res = app
        .oneshot(
            Call::new("GET", "/api/v1/devices", "alice")
                .device(&id, &key)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// A valid bearer alone must not be enough on a server that requires binding —
/// that is the entire value of `header_sig_v1`.
#[tokio::test]
async fn a_bearer_without_a_device_signature_is_refused_when_binding_is_required() {
    let app = boot(true);
    let (_id, _key) = register(&app, "alice", 1, "laptop").await;
    let res = app
        .oneshot(Call::new("GET", "/api/v1/devices", "alice").build())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

/// …while a server that does not require it still serves the same request.
#[tokio::test]
async fn the_same_request_succeeds_when_binding_is_optional() {
    let app = boot(false);
    let res = app
        .oneshot(Call::new("GET", "/api/v1/devices", "alice").build())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// A signature from a key the device never registered is rejected even though
/// the device id is real and active.
#[tokio::test]
async fn a_signature_from_the_wrong_key_is_rejected() {
    let app = boot(true);
    let (id, _key) = register(&app, "alice", 1, "laptop").await;
    let impostor = SigningKey::from_bytes(&[99u8; 32]);
    let res = app
        .oneshot(
            Call::new("GET", "/api/v1/devices", "alice")
                .device(&id, &impostor)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(res).await["error"]["code"], "AUTH_DEVICE_SIG_INVALID");
}

/// A signature captured from one request must not authorise another. Here the
/// device signs a harmless GET and the bytes are replayed onto a POST.
#[tokio::test]
async fn a_signature_is_not_transferable_between_requests() {
    let app = boot(true);
    let (id, key) = register(&app, "alice", 1, "laptop").await;
    let (victim, _k) = register(&app, "alice", 2, "phone").await;
    let res = app
        .oneshot(
            Call::new("DELETE", &format!("/api/v1/devices/{victim}"), "alice")
                .device(&id, &key)
                // Signed over an innocuous GET instead.
                .sign_as("GET", "/api/v1/devices", "")
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(res).await["error"]["code"], "AUTH_DEVICE_SIG_INVALID");
}

/// The binding is per-account: one account naming another's device id, even
/// with a perfect signature, gets nothing.
#[tokio::test]
async fn one_account_cannot_present_anothers_device() {
    let app = boot(true);
    let (alice_device, alice_key) = register(&app, "alice", 1, "laptop").await;
    let res = app
        .oneshot(
            Call::new("GET", "/api/v1/devices", "bob")
                .device(&alice_device, &alice_key)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(res).await["error"]["code"], "AUTH_DEVICE_NOT_OWNER");
}

/// The headline property: after revocation from another paired device, the
/// revoked device's own perfectly-signed request stops working — even though
/// its bearer token is still entirely valid.
#[tokio::test]
async fn a_revoked_device_stops_authenticating_while_its_token_is_still_valid() {
    let app = boot(true);
    let (survivor, survivor_key) = register(&app, "alice", 1, "laptop").await;
    let (stolen, stolen_key) = register(&app, "alice", 2, "stolen-phone").await;

    // The stolen device works before revocation.
    let before = app
        .clone()
        .oneshot(
            Call::new("GET", "/api/v1/devices", "alice")
                .device(&stolen, &stolen_key)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(before.status(), StatusCode::OK);

    let revoke = app
        .clone()
        .oneshot(
            Call::new("DELETE", &format!("/api/v1/devices/{stolen}"), "alice")
                .device(&survivor, &survivor_key)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT);

    let after = app
        .oneshot(
            Call::new("GET", "/api/v1/devices", "alice")
                .device(&stolen, &stolen_key)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(
        after.status(),
        StatusCode::FORBIDDEN,
        "the same bearer and the same valid signature must stop working once revoked"
    );
    assert_eq!(json(after).await["error"]["code"], "AUTH_DEVICE_NOT_OWNER");
}

/// A thief holding one device must not be able to revoke the owner's other
/// devices' ability to revoke *it* by revoking itself out of the audit trail.
#[tokio::test]
async fn a_device_cannot_revoke_itself() {
    let app = boot(true);
    let (id, key) = register(&app, "alice", 1, "laptop").await;
    let res = app
        .oneshot(
            Call::new("DELETE", &format!("/api/v1/devices/{id}"), "alice")
                .device(&id, &key)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

/// Defence in depth: when the IdP stamps a device id into the token, the
/// header must agree with it.
#[tokio::test]
async fn a_token_device_claim_must_match_the_header() {
    let cfg = ServerConfig {
        require_device_sig: true,
        ..Default::default()
    };
    // A bearer whose token claims device "SOMEONE-ELSE".
    let mut allowed = HashMap::new();
    let mut claimed = Subject::new(ISSUER, "alice");
    claimed.device_id = Some("SOMEONE-ELSE".into());
    allowed.insert("alice-claimed".to_string(), claimed);
    allowed.insert("alice".to_string(), Subject::new(ISSUER, "alice"));
    let state = ServerState::with_clock(cfg, Arc::new(FixedClock(T0_MS)))
        .with_verifier(Arc::new(StaticVerifier { allowed }));
    let app = build_router(state);

    let (id, key) = register(&app, "alice", 1, "laptop").await;
    let res = app
        .oneshot(
            Call::new("GET", "/api/v1/devices", "alice-claimed")
                .device(&id, &key)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(res).await["error"]["code"], "AUTH_DEVICE_NOT_OWNER");
}

/// A device signature is verified whenever it is *present*, even on a server
/// that does not demand one — otherwise turning the requirement off would turn
/// the check off.
#[tokio::test]
async fn a_bad_signature_is_rejected_even_when_binding_is_optional() {
    let app = boot(false);
    let (id, _key) = register(&app, "alice", 1, "laptop").await;
    let impostor = SigningKey::from_bytes(&[99u8; 32]);
    let res = app
        .oneshot(
            Call::new("GET", "/api/v1/devices", "alice")
                .device(&id, &impostor)
                .build(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}
