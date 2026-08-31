//! The per-request authentication pipeline every authenticated route runs.
//!
//! Bearer → verified [`Subject`] → account row → device binding, in that
//! order. Nothing downstream reads an identifier out of the request body: the
//! account comes from the token's `(iss, sub)` and the device comes from a
//! header that must be signed by that device's registered key. A handler
//! therefore cannot be tricked into acting on an account by being *told* an
//! account id.

use axum::http::{HeaderMap, Method, StatusCode, Uri};

use super::device_sig::{self, DEVICE_HEADER, DEVICE_SIG_HEADER};
use super::{extract_bearer, Subject, Verified};
use crate::error::{codes, ApiError};
use crate::state::ServerState;
use crate::store::{Account, Device};

/// `403` code for a syntactically present but unverifiable device signature.
pub const AUTH_DEVICE_SIG_INVALID: &str = "AUTH_DEVICE_SIG_INVALID";

/// A fully authenticated caller.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The verified token subject.
    pub subject: Subject,
    /// The account `(iss, sub)` resolves to.
    pub account: Account,
    /// The calling device, when the request carried device binding.
    pub device: Option<Device>,
}

/// The parts of a request the pipeline needs.
#[derive(Debug, Clone, Copy)]
pub struct RequestContext<'a> {
    /// HTTP method.
    pub method: &'a Method,
    /// Request target.
    pub uri: &'a Uri,
    /// Request headers.
    pub headers: &'a HeaderMap,
    /// Raw request body, exactly as received — the bytes the device signed.
    pub body: &'a [u8],
}

impl RequestContext<'_> {
    fn path_and_query(&self) -> &str {
        self.uri
            .path_and_query()
            .map_or_else(|| self.uri.path(), |p| p.as_str())
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn device_not_owner(message: &str) -> ApiError {
    ApiError::new(
        StatusCode::FORBIDDEN,
        codes::AUTH_DEVICE_NOT_OWNER,
        message.to_string(),
    )
}

/// Verify the bearer and resolve the account, with no device binding.
///
/// This is what the `/sync` upgrade uses: a WebSocket upgrade carries no body
/// to sign, and its per-message auth is the token's `exp`. Resolving the
/// account here is what applies `allow_signup` to sync as well as to REST — a
/// server with sign-up off must not relay for an account it never provisioned.
pub async fn authenticate_token(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<(Verified, Account), ApiError> {
    // An absent header verifies the empty string. `NullVerifier` (self-host,
    // single-tenant) accepts it; every real verifier rejects it. Enabling auth
    // is therefore purely a matter of configuring a verifier.
    let bearer = extract_bearer(headers).unwrap_or("");
    // Both failure arms log at `warn` with the stable code and nothing else.
    // "Which token" is never a loggable question — `bearer` is a credential,
    // and `ApiError::message` is written to be safe here (see error.rs).
    let verified = match state.token_verifier.verify(bearer).await {
        Ok(v) => v,
        Err(e) => {
            let api: ApiError = e.into();
            tracing::warn!(
                ev = "srv.auth.rejected",
                err_code = api.code,
                err_kind = "user",
                retryable = false,
                status = api.status.as_u16(),
                "bearer token rejected"
            );
            return Err(api);
        }
    };
    let account = match state.store.resolve_account(
        &verified.subject,
        state.config.allow_signup,
        state.clock.now_ms(),
    ) {
        Ok(a) => a,
        Err(e) => {
            let api: ApiError = e.into();
            tracing::warn!(
                ev = "srv.auth.rejected",
                err_code = api.code,
                err_kind = "user",
                retryable = false,
                status = api.status.as_u16(),
                "account not resolved"
            );
            return Err(api);
        }
    };
    tracing::debug!(
        ev = "srv.auth.ok",
        account_h = %crate::logging::account_h(&account.account_id),
        tier = %account.tier,
        "bearer accepted"
    );
    Ok((verified, account))
}

/// Authenticate a `/sync` upgrade and bind it to a registered device.
///
/// The upgrade carries no body, so there is nothing for `X-Sunrise-Device-Sig`
/// to cover and no signature is demanded. What this adds over
/// [`authenticate_token`] is the half that was missing entirely: the device is
/// looked up in the account's `devices` table, so an unregistered or **revoked**
/// device cannot open a sync session.
///
/// `docs/06-server/auth.md` promises that revoking a device stops it
/// authenticating "even if the OIDC token is still valid". That held for REST,
/// which runs `bind_device` on every request, and did not hold here: `/sync`
/// resolved the account and never consulted `devices`, so a revoked device kept
/// an authenticated socket -- and kept receiving the account's fan-out -- until
/// its token happened to expire.
///
/// The device id comes from the `X-Sunrise-Device` header, falling back to the
/// token's own `device_id` claim. When both are present they must agree, by the
/// same argument [`bind_device`] makes: a stolen token must not be replayable
/// from another device. When neither is present the session is refused unless
/// the server runs without device binding, which is what keeps the self-host
/// single-tenant path usable before any device is registered.
pub async fn authenticate_sync(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<(Verified, Account, Option<Device>), ApiError> {
    let (verified, account) = authenticate_token(state, headers).await?;

    let claimed = verified.subject.device_id.clone();
    let supplied = header(headers, DEVICE_HEADER).map(ToOwned::to_owned);
    if let (Some(h), Some(c)) = (supplied.as_deref(), claimed.as_deref()) {
        if h != c {
            return Err(device_not_owner(
                "token device_id claim does not match X-Sunrise-Device",
            ));
        }
    }

    let Some(device_id) = supplied.or(claimed) else {
        if state.config.require_device_sig {
            return Err(device_not_owner(
                "this server requires an X-Sunrise-Device header on /sync",
            ));
        }
        return Ok((verified, account, None));
    };

    let device = state
        .store
        .active_device(&account.account_id, &device_id)?
        .ok_or_else(|| device_not_owner("device is not an active device of this account"))?;
    let _ = state
        .store
        .touch_device(&device.device_id, state.clock.now_ms());
    Ok((verified, account, Some(device)))
}

/// Whether `device_id` is still an active, unrevoked device of `account_id`.
///
/// The live-session counterpart to the lookup [`authenticate_sync`] does at the
/// upgrade. A revocation that only took effect at the *next* connection would
/// leave untouched the socket that matters most: the one the revoked device is
/// holding open right now.
pub fn device_still_active(
    state: &ServerState,
    account_id: &str,
    device_id: &str,
) -> Result<bool, ApiError> {
    Ok(state.store.active_device(account_id, device_id)?.is_some())
}

/// Run the full pipeline for a REST request.
pub async fn authenticate(
    state: &ServerState,
    ctx: RequestContext<'_>,
) -> Result<Caller, ApiError> {
    authenticate_with(state, ctx, state.config.require_device_sig).await
}

/// As [`authenticate`], but never *demands* a device binding.
///
/// The two bootstrap routes need this. `POST /accounts` and `POST /devices` are
/// what a freshly-installed client calls after OIDC login, before it owns any
/// registered device — demanding a signature from a device that by definition
/// does not exist yet would make `require_device_sig` unbootstrappable. A
/// binding that *is* supplied is still checked in full, so this weakens nothing
/// for a client that already has a device.
pub async fn authenticate_bootstrap(
    state: &ServerState,
    ctx: RequestContext<'_>,
) -> Result<Caller, ApiError> {
    authenticate_with(state, ctx, false).await
}

async fn authenticate_with(
    state: &ServerState,
    ctx: RequestContext<'_>,
    require_binding: bool,
) -> Result<Caller, ApiError> {
    let (verified, account) = authenticate_token(state, ctx.headers).await?;
    let device = bind_device(state, &verified.subject, &account, ctx, require_binding)?;
    Ok(Caller {
        subject: verified.subject,
        account,
        device,
    })
}

/// Resolve and validate `X-Sunrise-Device` / `X-Sunrise-Device-Sig`.
///
/// Absence is tolerated unless `require_binding` says otherwise, which is what
/// lets a self-host single binary work before any device has been registered.
/// A *present* binding is always fully checked: a bad signature is never merely
/// ignored, whatever `require_binding` says.
fn bind_device(
    state: &ServerState,
    subject: &Subject,
    account: &Account,
    ctx: RequestContext<'_>,
    require_binding: bool,
) -> Result<Option<Device>, ApiError> {
    let device_id = header(ctx.headers, DEVICE_HEADER);
    let signature = header(ctx.headers, DEVICE_SIG_HEADER);

    let Some(device_id) = device_id else {
        if require_binding {
            return Err(device_not_owner(
                "this server requires an X-Sunrise-Device header on authenticated requests",
            ));
        }
        return Ok(None);
    };

    // Defence in depth per `docs/06-server/auth.md` §device-binding: when the
    // IdP stamped a device id into the token it must agree with the header, so
    // a stolen token cannot be replayed from a different device.
    if let Some(claimed) = subject.device_id.as_deref() {
        if claimed != device_id {
            return Err(device_not_owner(
                "token device_id claim does not match X-Sunrise-Device",
            ));
        }
    }

    // Revoked devices fall out here: `active_device` filters them, so a device
    // removed from another session stops authenticating on its next request.
    let device = state
        .store
        .active_device(&account.account_id, device_id)?
        .ok_or_else(|| device_not_owner("device is not an active device of this account"))?;

    match signature {
        Some(sig) => {
            let date = header(ctx.headers, "date").ok_or_else(|| {
                ApiError::new(
                    StatusCode::FORBIDDEN,
                    AUTH_DEVICE_SIG_INVALID,
                    "X-Sunrise-Device-Sig requires a Date header",
                )
            })?;
            device_sig::verify(
                &device.device_pub_s,
                sig,
                ctx.method.as_str(),
                ctx.path_and_query(),
                date,
                ctx.body,
                state.clock.now_ms(),
            )
            .map_err(|e| {
                state.metrics.incr("sunrise_device_sig_rejected_total");
                ApiError::new(
                    StatusCode::FORBIDDEN,
                    AUTH_DEVICE_SIG_INVALID,
                    e.to_string(),
                )
            })?;
        }
        None if require_binding => {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                AUTH_DEVICE_SIG_INVALID,
                "this server requires an X-Sunrise-Device-Sig header",
            ));
        }
        None => {}
    }

    // Best-effort liveness stamp; a failure here must not fail the request.
    let _ = state
        .store
        .touch_device(&device.device_id, state.clock.now_ms());
    Ok(Some(device))
}
