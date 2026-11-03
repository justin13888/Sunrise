//! The device lifecycle: list, register, revoke, and push-token registration.
//!
//! A device id in a path or body is only ever a *filter* on the calling
//! account's rows, never a lookup key on its own — the account half comes from
//! the verified token and the SQL carries it, so naming another account's
//! device resolves to nothing rather than to that device.

use crate::api::error::{codes, ApiError};
use crate::api::signed::{Signed, SignedBootstrap, SignedParts};
use crate::state::ServerState;
use crate::store::{Device, NewDevice};
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::path::Path;
use kynos::response::status::{Created, NoContent};
use serde::{Deserialize, Serialize};

/// Platforms `DeviceMeta.platform` admits.
const PLATFORMS: [&str; 6] = ["ios", "android", "macos", "windows", "linux", "web"];
/// Maximum nickname length in bytes.
///
/// Must equal `sunrise_crypto::MAX_NICKNAME_BYTES`, which is what
/// `DeviceCert`'s encoder and decoder enforce on the same field. Lowering this
/// alone makes the pairing path mint certs this route refuses; raising it alone
/// registers a device whose cert no peer can decode. This crate has no direct
/// dependency on `sunrise-crypto` — it is reached only transitively, through
/// `sunrise-pairing` and `sunrise-storage`, and never called; the relay never
/// parses a cert, it stores it as opaque text — so the agreement is asserted
/// from `sunrise-e2e`, which depends on both: see
/// `sunrise-e2e/tests/nickname_bound_agreement.rs`. It is `pub` so that test
/// can read it.
pub const MAX_NICKNAME_BYTES: usize = 64;

/// A registered device.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct DeviceMeta {
    /// Crockford base-32 of 16 bytes.
    pub device_id: String,
    /// The id this device answers to inside the vault, when it supplied one.
    ///
    /// The relay's own `device_id` above is a ULID minted here at
    /// registration; nothing carries it back into the vault, so no device
    /// holds a peer's. A `device_revoke` op names *this* id, which is what
    /// makes it the only correlation key a client can act on.
    pub vault_device_id: Option<String>,
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
            vault_device_id: d.vault_device_id,
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
    /// This device's vault-side id, Crockford base-32 of 16 bytes.
    ///
    /// Optional because it is additive to a shipped route, and a client that
    /// predates it still registers. A device that omits it cannot afterwards
    /// be revoked through `DELETE /api/v1/devices/by-vault-id/{id}`, because
    /// there is nothing on the row for that route to match.
    #[serde(default)]
    pub vault_device_id: Option<String>,
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

/// `{vault_device_id}` in a device path.
#[derive(Debug, Clone, Deserialize, kynos::PathParams, kynos::Schema)]
pub struct VaultDeviceIdPath {
    /// The vault-side id of the device being addressed.
    pub vault_device_id: String,
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
#[kynos::get("/api/v1/devices", operation_id = "listDevices")]
pub async fn list(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<Vec<DeviceMeta>>, ApiError> {
    let devices = state
        .store
        .list_devices(&caller.principal.account.account_id)
        .await?;
    Ok(Json(devices.into_iter().map(DeviceMeta::from).collect()))
}

/// Register a device.
///
/// Bootstrap route: a device cannot sign before it exists, so an absent binding
/// is accepted here even where the server demands one elsewhere. A binding that
/// *is* supplied is still verified in full.
#[kynos::post("/api/v1/devices", operation_id = "registerDevice")]
pub async fn register(
    Inject(state): Inject<ServerState>,
    SignedBootstrap {
        caller,
        value: body,
    }: SignedBootstrap<DeviceRegisterRequest>,
) -> Result<Created<Json<DeviceRegisterResponse>>, ApiError> {
    // A signing key that cannot be parsed is a device that could never satisfy
    // `header_sig_v2`; rejecting it here turns a permanent 401 into an
    // immediate, explicable 400.
    if !is_ed25519_pub(&body.device_pub_s) {
        return Err(ApiError::validation(
            "device_pub_s must be a base64url no-pad Ed25519 public key",
        ));
    }
    if body.nickname.trim().is_empty() || body.nickname.len() > MAX_NICKNAME_BYTES {
        return Err(ApiError::validation(format!(
            "nickname must be 1..={MAX_NICKNAME_BYTES} bytes"
        )));
    }
    if !PLATFORMS.contains(&body.platform.as_str()) {
        return Err(ApiError::validation(format!(
            "platform must be one of {PLATFORMS:?}"
        )));
    }
    // Refused at registration rather than stored and found unmatchable later:
    // an id that is not a well-formed vault id can never equal one a real
    // `device_revoke` names, so accepting it would register a device that
    // looks revocable and is not.
    if let Some(vault_device_id) = body.vault_device_id.as_deref() {
        if sunrise_id::crockford::decode_str(vault_device_id).is_err() {
            return Err(ApiError::validation(
                "vault_device_id must be 26 Crockford base-32 characters",
            ));
        }
    }

    let device = state
        .store
        .register_device(
            &caller.principal.account.account_id,
            &NewDevice {
                device_pub_s: body.device_pub_s,
                device_pub_d: body.device_pub_d,
                device_cert: body.device_cert,
                vault_device_id: body.vault_device_id,
                nickname: body.nickname,
                platform: body.platform,
                app_version: body.app_version,
            },
            state.clock.now_ms(),
        )
        .await?;
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
#[kynos::delete("/api/v1/devices/{device_id}", operation_id = "revokeDevice")]
pub async fn revoke(
    Inject(state): Inject<ServerState>,
    Path(path): Path<DeviceIdPath>,
    // The signed target is the *concrete* path the client sent, not the route
    // template: a signature over `/api/v1/devices/{device_id}` would verify for
    // every device id, which is the whole thing this binding exists to prevent.
    // `SignedParts` reads the target off the request, so that is what it covers.
    SignedParts(caller): SignedParts,
) -> Result<NoContent, ApiError> {
    if caller
        .device
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
            &caller.principal.account.account_id,
            &path.device_id,
            state.clock.now_ms(),
        )
        .await
        .map_err(|e| match e {
            crate::store::MetadataError::NotFound => ApiError::not_found(
                codes::DEVICE_NOT_FOUND,
                "no such active device on this account",
            ),
            other => other.into(),
        })?;
    state.metrics.incr("sunrise_devices_revoke_total");
    Ok(NoContent)
}

/// Revoke a device by the id it answers to inside the vault.
///
/// The route above names the relay's own ULID, and a vault has no way to learn
/// a peer's: the ULID is minted here at registration and travels back only to
/// the device that registered. What a vault holds for a peer is the 16-byte id
/// a `device_revoke` op names, so before this route existed a revocation was
/// not expressible against this API at all, whatever the client did
/// ([#80](https://github.com/justin13888/Sunrise/issues/80)).
///
/// Every active row carrying the id is revoked, not one: a device
/// re-registering is a second row rather than an error, and all of them are the
/// device the caller means.
///
/// `404` is the answer for a vault id no active row on this account carries.
/// That covers a second revoke, another account's device, **and** a device that
/// registered before it sent a `vault_device_id` — the last of which is still
/// accepted by this relay under a row the caller cannot name, so a client must
/// not read this `404` as "the device is now refused".
#[kynos::delete(
    "/api/v1/devices/by-vault-id/{vault_device_id}",
    operation_id = "revokeDeviceByVaultId"
)]
pub async fn revoke_by_vault_id(
    Inject(state): Inject<ServerState>,
    Path(path): Path<VaultDeviceIdPath>,
    SignedParts(caller): SignedParts,
) -> Result<NoContent, ApiError> {
    if sunrise_id::crockford::decode_str(&path.vault_device_id).is_err() {
        return Err(ApiError::validation(
            "vault_device_id must be 26 Crockford base-32 characters",
        ));
    }
    // The same rule as `revoke`, read off the other id: a device a thief holds
    // must not be able to erase the evidence of its own theft, and it would
    // otherwise reach the same row by its vault name.
    if caller
        .device
        .as_ref()
        .and_then(|d| d.vault_device_id.as_deref())
        .is_some_and(|id| id == path.vault_device_id)
    {
        return Err(ApiError::forbidden(
            codes::AUTH_DEVICE_NOT_OWNER,
            "a device cannot revoke itself; revoke it from another paired device",
        ));
    }

    state
        .store
        .revoke_devices_by_vault_id(
            &caller.principal.account.account_id,
            &path.vault_device_id,
            state.clock.now_ms(),
        )
        .await
        .map_err(|e| match e {
            crate::store::MetadataError::NotFound => ApiError::not_found(
                codes::DEVICE_NOT_FOUND,
                "no active device on this account carries that vault device id",
            ),
            other => other.into(),
        })?;
    state.metrics.incr("sunrise_devices_revoke_total");
    Ok(NoContent)
}

/// File a push token against one of the caller's devices.
#[kynos::post("/api/v1/devices/push-tokens", operation_id = "registerPushToken")]
pub async fn push_tokens(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<PushRegistration>,
) -> Result<NoContent, ApiError> {
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
        .active_device(&caller.principal.account.account_id, &body.device_id)
        .await?
        .ok_or_else(|| {
            ApiError::forbidden(
                codes::AUTH_DEVICE_NOT_OWNER,
                "device is not an active device of this account",
            )
        })?;

    // The same spelling the dispatcher looks tokens up by.
    state
        .store
        .upsert_push_token(
            &body.device_id,
            body.platform.store_tag(),
            &body.token,
            state.clock.now_ms(),
        )
        .await?;
    state.metrics.incr("sunrise_push_register_total");
    Ok(NoContent)
}

#[cfg(test)]
#[path = "devices_tests.rs"]
mod tests;
