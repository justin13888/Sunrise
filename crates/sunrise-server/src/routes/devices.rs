//! Device registration, listing, revocation, and push-token routes.
//!
//! Per `docs/06-server/api.md` §devices. Every route here scopes itself to the
//! account the bearer token resolves to: a device id in a path or body is only
//! ever used as a *filter* on that account's rows, never as a lookup key on its
//! own. That is what makes `DELETE /devices/<id>` from another account a 404
//! rather than a revocation.

use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::auth::request::{authenticate, authenticate_bootstrap, Caller, RequestContext};
use crate::error::{codes, ApiError};
use crate::push::PushRegistration;
use crate::store::{Device, NewDevice};
use crate::ServerState;

/// Mount device routes.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new()
        .route("/devices", get(list).post(register))
        .route("/devices/:device_id", axum::routing::delete(revoke))
        .route("/devices/push-tokens", post(register_push_token))
}

/// `DeviceMeta` from `docs/06-server/api.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceMeta {
    /// Crockford base-32 of 16 bytes.
    pub device_id: String,
    /// User-set name.
    pub nickname: String,
    /// Platform tag.
    pub platform: String,
    /// Reported app version, if any.
    pub app_version: Option<String>,
    /// Registration time, ms since epoch.
    pub created_at_ms: u64,
    /// Last authenticated request, ms since epoch.
    pub last_seen_at_ms: u64,
    /// Whether the device has been revoked.
    pub revoked: bool,
    /// Revocation time, ms since epoch.
    pub revoked_at_ms: Option<u64>,
}

impl From<Device> for DeviceMeta {
    fn from(d: Device) -> Self {
        Self {
            device_id: d.device_id,
            nickname: d.nickname,
            platform: d.platform,
            app_version: d.app_version,
            created_at_ms: d.created_at_ms,
            last_seen_at_ms: d.last_seen_at_ms,
            revoked: d.revoked,
            revoked_at_ms: d.revoked_at_ms,
        }
    }
}

/// `POST /api/v1/devices` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceRegisterRequest {
    /// Ed25519 device signing key, base64url no-pad. Verifies this device's
    /// `X-Sunrise-Device-Sig` on every later request.
    pub device_pub_s: String,
    /// X25519 device key, base64url no-pad.
    #[serde(default)]
    pub device_pub_d: Option<String>,
    /// Self-signed device certificate; opaque to the server.
    #[serde(default)]
    pub device_cert: Option<String>,
    /// User-visible name.
    pub nickname: String,
    /// Platform tag.
    pub platform: String,
    /// Reported app version.
    #[serde(default)]
    pub app_version: Option<String>,
}

/// `POST /api/v1/devices` response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRegisterResponse {
    /// The server-assigned device id.
    pub device_id: String,
}

/// Platforms `DeviceMeta.platform` admits.
const PLATFORMS: [&str; 6] = ["ios", "android", "macos", "windows", "linux", "web"];

/// Maximum nickname length in bytes, per `DeviceMeta`.
const MAX_NICKNAME_BYTES: usize = 64;

async fn caller_of(
    state: &ServerState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Caller, ApiError> {
    authenticate(
        state,
        RequestContext {
            method,
            uri,
            headers,
            body,
        },
    )
    .await
}

/// As [`caller_of`], for the route a client with no registered device calls.
async fn bootstrap_caller_of(
    state: &ServerState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Caller, ApiError> {
    authenticate_bootstrap(
        state,
        RequestContext {
            method,
            uri,
            headers,
            body,
        },
    )
    .await
}

async fn list(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Vec<DeviceMeta>>, ApiError> {
    let caller = caller_of(&state, &method, &uri, &headers, &body).await?;
    state.metrics.incr("sunrise_devices_list_total");
    let devices = state.store.list_devices(&caller.account.account_id)?;
    Ok(Json(devices.into_iter().map(DeviceMeta::from).collect()))
}

async fn register(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<DeviceRegisterResponse>), ApiError> {
    let caller = bootstrap_caller_of(&state, &method, &uri, &headers, &body).await?;
    let req: DeviceRegisterRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("malformed device body: {e}")))?;

    // A signing key that cannot be parsed is a device that could never satisfy
    // `header_sig_v1`; rejecting it at registration turns a permanent 403 into
    // an immediate, explicable 400.
    if !is_ed25519_pub(&req.device_pub_s) {
        return Err(ApiError::validation(
            "device_pub_s must be a base64url no-pad Ed25519 public key",
        ));
    }
    if req.nickname.trim().is_empty() || req.nickname.len() > MAX_NICKNAME_BYTES {
        return Err(ApiError::validation("nickname must be 1..=64 bytes"));
    }
    if !PLATFORMS.contains(&req.platform.as_str()) {
        return Err(ApiError::validation(format!(
            "platform must be one of {PLATFORMS:?}"
        )));
    }

    let device = state.store.register_device(
        &caller.account.account_id,
        &NewDevice {
            device_pub_s: req.device_pub_s,
            device_pub_d: req.device_pub_d,
            device_cert: req.device_cert,
            nickname: req.nickname,
            platform: req.platform,
            app_version: req.app_version,
        },
        state.clock.now_ms(),
    )?;
    state.metrics.incr("sunrise_devices_register_total");
    Ok((
        StatusCode::CREATED,
        Json(DeviceRegisterResponse {
            device_id: device.device_id,
        }),
    ))
}

async fn revoke(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(device_id): Path<String>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let caller = caller_of(&state, &method, &uri, &headers, &body).await?;
    // `docs/06-server/api.md`: revocation is "only callable by another paired
    // device". A device that can revoke itself is a device a thief can use to
    // erase the evidence of its own theft, and the legitimate "sign out here"
    // gesture is a local key wipe, not a server call.
    if caller
        .device
        .as_ref()
        .is_some_and(|d| d.device_id == device_id)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            codes::AUTH_DEVICE_NOT_OWNER,
            "a device cannot revoke itself; revoke it from another paired device",
        ));
    }
    state
        .store
        .revoke_device(&caller.account.account_id, &device_id, state.clock.now_ms())
        .map_err(|e| match e {
            crate::store::StoreError::NotFound => ApiError::new(
                StatusCode::NOT_FOUND,
                codes::DEVICE_NOT_FOUND,
                "no such active device on this account",
            ),
            other => other.into(),
        })?;
    state.metrics.incr("sunrise_devices_revoke_total");
    Ok(StatusCode::NO_CONTENT)
}

async fn register_push_token(
    State(state): State<ServerState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let caller = caller_of(&state, &method, &uri, &headers, &body).await?;
    let reg: PushRegistration = serde_json::from_slice(&body)
        .map_err(|e| ApiError::validation(format!("malformed push registration: {e}")))?;

    if sunrise_id::crockford::decode_str(&reg.device_id).is_err() {
        return Err(ApiError::validation(
            "device_id must be 26 Crockford base-32 characters",
        ));
    }
    if reg.token.is_empty() {
        return Err(ApiError::validation("token required"));
    }

    // Ownership *and* revocation in one lookup: a revoked device is not an
    // active device, so it silently stops being wakeable.
    if state
        .store
        .active_device(&caller.account.account_id, &reg.device_id)?
        .is_none()
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            codes::AUTH_DEVICE_NOT_OWNER,
            "device is not an active device of this account",
        ));
    }

    let platform = match reg.platform {
        crate::push::PushPlatform::Apns => "apns",
        crate::push::PushPlatform::Fcm => "fcm",
        crate::push::PushPlatform::WebPush => "webpush",
    };
    state
        .store
        .upsert_push_token(&reg.device_id, platform, &reg.token, state.clock.now_ms())?;
    state.metrics.incr("sunrise_push_register_total");
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({"registered": true})),
    ))
}

/// Whether `s` decodes to a usable Ed25519 public key.
fn is_ed25519_pub(s: &str) -> bool {
    use base64::Engine as _;
    let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim()) else {
        return false;
    };
    let Ok(bytes) = <[u8; 32]>::try_from(raw) else {
        return false;
    };
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_router, ServerConfig};
    use axum::body::to_bytes;
    use axum::http::Request;
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use tower::ServiceExt;

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    fn device_body(seed: u8, nickname: &str) -> serde_json::Value {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        serde_json::json!({
            "device_pub_s": b64(sk.verifying_key().as_bytes()),
            "nickname": nickname,
            "platform": "linux",
        })
    }

    fn req(method: &str, uri: &str, body: Option<&serde_json::Value>) -> Request<axum::body::Body> {
        let b = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        match body {
            Some(v) => b.body(axum::body::Body::from(v.to_string())).unwrap(),
            None => b.body(axum::body::Body::empty()).unwrap(),
        }
    }

    async fn json(res: axum::response::Response) -> serde_json::Value {
        let body = to_bytes(res.into_body(), 65536).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn register_device(app: &axum::Router, seed: u8, nickname: &str) -> String {
        let res = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/v1/devices",
                Some(&device_body(seed, nickname)),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
        json(res).await["device_id"].as_str().unwrap().to_string()
    }

    /// The list route used to return `[]` unconditionally.
    #[tokio::test]
    async fn a_registered_device_appears_in_the_list() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let id = register_device(&app, 1, "laptop").await;

        let res = app
            .oneshot(req("GET", "/api/v1/devices", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let v = json(res).await;
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["device_id"], id);
        assert_eq!(v[0]["nickname"], "laptop");
        assert_eq!(v[0]["revoked"], false);
    }

    /// Revocation has to be observable, not just accepted.
    #[tokio::test]
    async fn revocation_takes_effect() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let keep = register_device(&app, 1, "laptop").await;
        let gone = register_device(&app, 2, "old-phone").await;

        let res = app
            .clone()
            .oneshot(req("DELETE", &format!("/api/v1/devices/{gone}"), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        // The revoked device can no longer register a push token…
        let res = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/v1/devices/push-tokens",
                Some(&serde_json::json!({
                    "device_id": gone, "platform": "fcm", "token": "t"
                })),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(json(res).await["error"]["code"], "AUTH_DEVICE_NOT_OWNER");

        // …while the surviving device still can.
        let res = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/v1/devices/push-tokens",
                Some(&serde_json::json!({
                    "device_id": keep, "platform": "fcm", "token": "t"
                })),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // And the list reports the revocation rather than hiding it.
        let v = json(
            app.oneshot(req("GET", "/api/v1/devices", None))
                .await
                .unwrap(),
        )
        .await;
        let revoked: Vec<_> = v
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["revoked"] == true)
            .collect();
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0]["device_id"], gone);
    }

    #[tokio::test]
    async fn revoking_an_unknown_device_is_a_404() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let res = app
            .oneshot(req(
                "DELETE",
                "/api/v1/devices/0000000000000000000000000Z",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        assert_eq!(json(res).await["error"]["code"], "DEVICE_NOT_FOUND");
    }

    #[tokio::test]
    async fn register_rejects_bad_device_id() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "device_id_hex": "short",
            "platform": "fcm",
            "token": "abc",
        });
        let response = app
            .oneshot(req("POST", "/api/v1/devices/push-tokens", Some(&body)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn register_rejects_an_unusable_signing_key() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let body = serde_json::json!({
            "device_pub_s": "not-a-key",
            "nickname": "laptop",
            "platform": "linux",
        });
        let res = app
            .oneshot(req("POST", "/api/v1/devices", Some(&body)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn register_rejects_an_unknown_platform() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let mut body = device_body(3, "laptop");
        body["platform"] = serde_json::json!("toaster");
        let res = app
            .oneshot(req("POST", "/api/v1/devices", Some(&body)))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// A push token stored under a device id the caller does not own would let
    /// one account redirect another's wakeups.
    #[tokio::test]
    async fn a_push_token_cannot_be_filed_under_an_unowned_device() {
        let app = build_router(ServerState::new(ServerConfig::default()));
        let _ = register_device(&app, 1, "laptop").await;
        let res = app
            .oneshot(req(
                "POST",
                "/api/v1/devices/push-tokens",
                Some(&serde_json::json!({
                    "device_id": "0000000000000000000000000Z",
                    "platform": "fcm",
                    "token": "t"
                })),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
}
