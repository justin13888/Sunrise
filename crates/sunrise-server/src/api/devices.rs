//! The device lifecycle: list, register, revoke, and push-token registration.
//!
//! A device id in a path or body is only ever a *filter* on the calling
//! account's rows, never a lookup key on its own — the account half comes from
//! the verified token and the SQL carries it, so naming another account's
//! device resolves to nothing rather than to that device.

use crate::api::auth::AccountToken;
use crate::api::error::{codes, ApiError};
use crate::api::signed::{self, DeviceSig};
use crate::state::ServerState;
use crate::store::{Device, NewDevice};
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::header::Headers;
use kynos::extract::params::path::Path;
use kynos::response::status::{Created, NoContent};
use kynos::security::auth::Auth;
use serde::{Deserialize, Serialize};

/// Platforms `DeviceMeta.platform` admits.
const PLATFORMS: [&str; 6] = ["ios", "android", "macos", "windows", "linux", "web"];
/// Maximum nickname length in bytes.
const MAX_NICKNAME_BYTES: usize = 64;

/// A registered device.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct DeviceMeta {
    /// Crockford base-32 of 16 bytes.
    pub device_id: String,
    /// User-set name.
    pub nickname: String,
    /// Platform tag.
    pub platform: String,
    /// Reported app version, if any.
    pub app_version: Option<String>,
    /// Registration time, milliseconds since the epoch.
    pub created_at_ms: u64,
    /// Last authenticated request, milliseconds since the epoch.
    pub last_seen_at_ms: u64,
    /// Whether the device has been revoked.
    ///
    /// Revocation is a soft delete: the row survives so this can be reported.
    pub revoked: bool,
    /// Revocation time, milliseconds since the epoch.
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
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct DeviceRegisterRequest {
    /// Ed25519 device signing key, base64url no-pad. This is what verifies the
    /// device's `X-Sunrise-Device-Sig` on every later request.
    pub device_pub_s: String,
    /// X25519 device key, base64url no-pad.
    #[serde(default)]
    pub device_pub_d: Option<String>,
    /// Self-signed device certificate. Opaque to the server, which never parses
    /// it — trust in a device cert is established client-side by
    /// `Command::TrustDevice`, and ADR-0024 is what gives it an identity anchor.
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
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct DeviceRegisterResponse {
    /// The server-assigned device id.
    pub device_id: String,
}

/// `POST /api/v1/devices/push-tokens` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct PushRegistration {
    /// Which of the caller's devices this token belongs to. Validated against
    /// the caller's own rows; it cannot file a token under an unowned device.
    pub device_id: String,
    /// Provider tag.
    ///
    /// The enum rather than a free string: a token filed under a platform no
    /// provider serves is a notification that silently never arrives, and the
    /// parse is the cheapest place to refuse it.
    pub platform: crate::push::PushPlatform,
    /// The provider token. Stored in plaintext today — see
    /// `docs/01-architecture/threat-model.md`.
    pub token: String,
}

/// `{device_id}` in a device path.
#[derive(Debug, Clone, Deserialize, kynos::PathParams, kynos::Schema)]
pub struct DeviceIdPath {
    /// The device being addressed.
    pub device_id: String,
}

/// Whether `s` decodes to a *usable* Ed25519 public key.
///
/// `VerifyingKey::from_bytes` rather than a length check: 32 bytes is necessary
/// and not sufficient. A small-order or otherwise non-canonical point is the
/// right length and can never verify a signature, so accepting one registers a
/// device that is permanently unable to authenticate — a 401 the user cannot
/// act on, arriving long after the request that caused it.
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

/// Every device on the calling account, revoked ones included.
#[kynos::get("/api/v1/devices")]
pub async fn list(
    Auth(principal): Auth<AccountToken>,
    Inject(state): Inject<ServerState>,
    Headers(sig): Headers<DeviceSig>,
) -> Result<Json<Vec<DeviceMeta>>, ApiError> {
    signed::verify::<()>(&state, &principal, &sig, "GET", "/api/v1/devices", None)?;
    let devices = state.store.list_devices(&principal.account.account_id)?;
    state.metrics.incr("sunrise_devices_list_total");
    Ok(Json(devices.into_iter().map(DeviceMeta::from).collect()))
}

/// Register a device.
///
/// Bootstrap route: a device cannot sign before it exists, so an absent binding
/// is accepted here even where the server demands one elsewhere. A binding that
/// *is* supplied is still verified in full.
#[kynos::post("/api/v1/devices")]
pub async fn register(
    Auth(principal): Auth<AccountToken>,
    Inject(state): Inject<ServerState>,
    Headers(sig): Headers<DeviceSig>,
    Json(body): Json<DeviceRegisterRequest>,
) -> Result<Created<Json<DeviceRegisterResponse>>, ApiError> {
    if sig.device.is_some() {
        signed::verify(
            &state,
            &principal,
            &sig,
            "POST",
            "/api/v1/devices",
            Some(&body),
        )?;
    }

    // A signing key that cannot be parsed is a device that could never satisfy
    // `header_sig_v2`; rejecting it here turns a permanent 401 into an
    // immediate, explicable 400.
    if !is_ed25519_pub(&body.device_pub_s) {
        return Err(ApiError::validation(
            "device_pub_s must be a base64url no-pad Ed25519 public key",
        ));
    }
    if body.nickname.trim().is_empty() || body.nickname.len() > MAX_NICKNAME_BYTES {
        return Err(ApiError::validation("nickname must be 1..=64 bytes"));
    }
    if !PLATFORMS.contains(&body.platform.as_str()) {
        return Err(ApiError::validation(format!(
            "platform must be one of {PLATFORMS:?}"
        )));
    }

    let device = state.store.register_device(
        &principal.account.account_id,
        &NewDevice {
            device_pub_s: body.device_pub_s,
            device_pub_d: body.device_pub_d,
            device_cert: body.device_cert,
            nickname: body.nickname,
            platform: body.platform,
            app_version: body.app_version,
        },
        state.clock.now_ms(),
    )?;
    state.metrics.incr("sunrise_devices_register_total");
    let id = device.device_id.clone();
    Ok(Created::at(
        format!("/api/v1/devices/{id}"),
        Json(DeviceRegisterResponse { device_id: id }),
    ))
}

/// Revoke a device.
///
/// **Only callable from another paired device.** A device that can revoke
/// itself is a device a thief can use to erase the evidence of its own theft,
/// and the legitimate "sign out here" gesture is a local key wipe rather than a
/// server call.
#[kynos::delete("/api/v1/devices/{device_id}")]
pub async fn revoke(
    Auth(principal): Auth<AccountToken>,
    Inject(state): Inject<ServerState>,
    Path(path): Path<DeviceIdPath>,
    Headers(sig): Headers<DeviceSig>,
) -> Result<NoContent, ApiError> {
    // The signed target is the *concrete* path the client sent, not the route
    // template: a signature over `/api/v1/devices/{device_id}` would verify for
    // every device id, which is the whole thing this binding exists to prevent.
    let caller_device = signed::verify::<()>(
        &state,
        &principal,
        &sig,
        "DELETE",
        &format!("/api/v1/devices/{}", path.device_id),
        None,
    )?;

    if caller_device
        .as_ref()
        .is_some_and(|d| d.device_id == path.device_id)
    {
        return Err(ApiError::forbidden(
            codes::AUTH_DEVICE_NOT_OWNER,
            "a device cannot revoke itself; revoke it from another paired device",
        ));
    }

    state
        .store
        .revoke_device(
            &principal.account.account_id,
            &path.device_id,
            state.clock.now_ms(),
        )
        .map_err(|e| match e {
            crate::store::StoreError::NotFound => ApiError::not_found(
                codes::DEVICE_NOT_FOUND,
                "no such active device on this account",
            ),
            other => other.into(),
        })?;
    state.metrics.incr("sunrise_devices_revoke_total");
    Ok(NoContent)
}

/// File a push token against one of the caller's devices.
#[kynos::post("/api/v1/devices/push-tokens")]
pub async fn push_tokens(
    Auth(principal): Auth<AccountToken>,
    Inject(state): Inject<ServerState>,
    Headers(sig): Headers<DeviceSig>,
    Json(body): Json<PushRegistration>,
) -> Result<NoContent, ApiError> {
    signed::verify(
        &state,
        &principal,
        &sig,
        "POST",
        "/api/v1/devices/push-tokens",
        Some(&body),
    )?;

    if sunrise_id::crockford::decode_str(&body.device_id).is_err() {
        return Err(ApiError::validation(
            "device_id must be 26 Crockford base-32 characters",
        ));
    }
    // An empty token is a registration that can never wake anything. Storing it
    // replaces a working token with one that silently drops every push, so the
    // failure surfaces as "notifications stopped" rather than as this request.
    if body.token.is_empty() {
        return Err(ApiError::validation("token required"));
    }
    // Scoped to the caller's account in SQL, so a token cannot be filed under
    // an unowned device by naming one.
    state
        .store
        .active_device(&principal.account.account_id, &body.device_id)?
        .ok_or_else(|| {
            ApiError::forbidden(
                codes::AUTH_DEVICE_NOT_OWNER,
                "device is not an active device of this account",
            )
        })?;

    let platform = match body.platform {
        crate::push::PushPlatform::Apns => "apns",
        crate::push::PushPlatform::Fcm => "fcm",
        crate::push::PushPlatform::WebPush => "webpush",
    };
    state
        .store
        .upsert_push_token(&body.device_id, platform, &body.token, state.clock.now_ms())?;
    state.metrics.incr("sunrise_push_register_total");
    Ok(NoContent)
}

#[cfg(test)]
mod tests {
    use crate::api::error::codes;
    use crate::api::testing::Client;
    use crate::ServerConfig;
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use kynos::http::{Method, StatusCode};

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

        let rendered = client.metrics.render();
        assert!(
            rendered.contains("sunrise_devices_list_total"),
            "listing devices must be counted; got:\n{rendered}"
        );
        assert!(
            rendered.contains("sunrise_push_register_total"),
            "filing a push token must be counted; got:\n{rendered}"
        );
    }

    /// Sign-up being disabled is a statement about the server, not about the
    /// caller's credential, and the surface this replaces answered 403.
    #[tokio::test]
    async fn signup_disabled_is_a_403_not_a_401() {
        let client = Client::new(ServerConfig {
            allow_signup: false,
            ..ServerConfig::default()
        });

        client
            .send(Method::GET, "/api/v1/devices", None)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
}
