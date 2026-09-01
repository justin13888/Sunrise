//! `POST /api/v1/accounts` and `GET /api/v1/accounts/me`.
//!
//! Both resolve the caller from the verified token's `(iss, sub)`. **Nothing
//! identifying is read out of a request body** — the account id used to be the
//! first sixteen characters of the caller's own submitted public key, and
//! `GET /accounts/me` used to answer `200 {"identity_id":"unauthenticated"}` to
//! anybody. Those are regression-tested in `routes::accounts` and the property
//! is preserved here by construction: the only account id in scope comes from
//! [`Principal`], which only [`Auth`] can produce.
//!
//! # Why the request types live here
//!
//! `sunrise-onboarding` declares an `AccountCreateRequest` and an `AccountInfo`
//! that nothing has ever sent or received — the bootstrap flow was specified,
//! served, and never invoked, which is the void ADR-0021 adopted a client
//! generator to fill. The typed surface is the contract now, so the shapes are
//! declared where they are described, and carry the two things the old ones
//! could not: a `Schema` for the document, and `deny_unknown_fields`.
//!
//! That last one is load-bearing rather than tidiness. `header_sig_v2` signs a
//! re-serialisation of the parsed value, and a signature over a lossy parse is
//! a signature over something the client did not send. Rejecting the unknown
//! member turns an inscrutable signature mismatch into a 400 that names it.

use crate::api::error::ApiError;
use crate::api::signed::{SignedBootstrap, SignedParts};
use crate::state::ServerState;
use crate::store::Account;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::response::status::Created;
use serde::{Deserialize, Serialize};

/// `POST /api/v1/accounts` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct AccountCreateRequest {
    /// User email. Normalized server-side, and only a fallback: the IdP owns
    /// the address, and this is read solely when the verifier emits no `email`
    /// claim at all.
    pub email: String,
    /// Identity Ed25519 public key, base64url no-pad.
    pub identity_signing_pub: String,
    /// Identity X25519 public key, base64url no-pad.
    pub identity_dh_pub: String,
    /// Encrypted recovery blob. Opaque to the server, base64url no-pad.
    pub recovery_blob: String,
    /// Terms acceptance timestamp, milliseconds since the epoch.
    pub terms_at_ms: u64,
}

/// An account, as this surface reports it.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct AccountInfo {
    /// 16-byte identity id.
    pub identity_id: String,
    /// Normalized email.
    pub email: String,
    /// Subscription tier.
    pub tier: String,
    /// Number of registered, unrevoked devices.
    pub device_count: u32,
    /// Account creation time, milliseconds since the epoch.
    pub created_at_ms: u64,
}

fn info(state: &ServerState, account: &Account) -> Result<AccountInfo, ApiError> {
    Ok(AccountInfo {
        identity_id: account.account_id.clone(),
        email: account.email.clone().unwrap_or_default(),
        tier: account.tier.clone(),
        device_count: state.store.active_device_count(&account.account_id)?,
        created_at_ms: account.created_at_ms,
    })
}

/// Set the account's identity keys and recovery blob.
///
/// Idempotent: the store coalesces, so a client that retries after a dropped
/// response does not get a second account. The account itself already exists by
/// the time this runs — it is minted when the token first resolves.
#[kynos::post("/api/v1/accounts", operation_id = "createAccount")]
pub async fn create(
    Inject(state): Inject<ServerState>,
    // Bootstrap exemption: a device cannot sign before it exists, so this route
    // accepts an absent binding even where the server demands one elsewhere. A
    // binding that *is* supplied is verified in full, so this weakens nothing
    // for a client that already has a device.
    SignedBootstrap {
        caller,
        value: body,
    }: SignedBootstrap<AccountCreateRequest>,
) -> Result<Created<Json<AccountInfo>>, ApiError> {
    let principal = &caller.principal;

    if body.email.trim().is_empty() {
        return Err(ApiError::validation("email required"));
    }
    if body.identity_signing_pub.trim().is_empty() || body.identity_dh_pub.trim().is_empty() {
        return Err(ApiError::validation("identity keys required"));
    }

    let account = state.store.set_identity(
        &principal.account.account_id,
        body.identity_signing_pub.trim(),
        body.identity_dh_pub.trim(),
        Some(body.recovery_blob.as_str()).filter(|b| !b.is_empty()),
        body.terms_at_ms,
    )?;

    state.metrics.incr("sunrise_account_create_total");
    let mut out = info(&state, &account)?;
    // The IdP owns the email; the body's copy is the self-host fallback.
    out.email = principal
        .subject
        .email
        .clone()
        .unwrap_or_else(|| body.email.trim().to_ascii_lowercase());
    Ok(Created::at("/api/v1/accounts/me", Json(out)))
}

/// The calling account.
#[kynos::get("/api/v1/accounts/me", operation_id = "getAccount")]
pub async fn me(
    Inject(state): Inject<ServerState>,
    SignedParts(caller): SignedParts,
) -> Result<Json<AccountInfo>, ApiError> {
    Ok(Json(info(&state, &caller.principal.account)?))
}
