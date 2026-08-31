//! The device lifecycle: list, register, revoke, and push-token registration.
//!
//! A device id in a path or body is only ever a *filter* on the calling
//! account's rows, never a lookup key on its own — the account half comes from
//! the verified token and the SQL carries it, so naming another account's
//! device resolves to nothing rather than to that device.

use crate::api::auth::AccountToken;
use crate::api::error::ApiError;
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
    pub platform: String,
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

/// Whether `s` decodes as a base64url no-pad Ed25519 public key.
fn is_ed25519_pub(s: &str) -> bool {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim())
        .is_ok_and(|raw| raw.len() == 32)
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
        return Err(ApiError::Validation(
            "device_pub_s must be a base64url no-pad Ed25519 public key".into(),
        ));
    }
    if body.nickname.trim().is_empty() || body.nickname.len() > MAX_NICKNAME_BYTES {
        return Err(ApiError::Validation("nickname must be 1..=64 bytes".into()));
    }
    if !PLATFORMS.contains(&body.platform.as_str()) {
        return Err(ApiError::Validation(format!(
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
        return Err(ApiError::Forbidden(
            "a device cannot revoke itself; revoke it from another paired device".into(),
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
            crate::store::StoreError::NotFound => {
                ApiError::NotFound("no such active device on this account".into())
            }
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
        return Err(ApiError::Validation(
            "device_id must be 26 Crockford base-32 characters".into(),
        ));
    }
    // Scoped to the caller's account in SQL, so a token cannot be filed under
    // an unowned device by naming one.
    state
        .store
        .active_device(&principal.account.account_id, &body.device_id)?
        .ok_or_else(|| ApiError::Forbidden("not an active device of this account".into()))?;

    state.store.upsert_push_token(
        &body.device_id,
        &body.platform,
        &body.token,
        state.clock.now_ms(),
    )?;
    Ok(NoContent)
}
