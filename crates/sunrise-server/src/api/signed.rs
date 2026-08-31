//! Device binding for the typed surface: declared headers plus one checker.
//!
//! `header_sig_v2` signs the canonical JSON of the request *value*
//! ([ADR-0022](../../../../docs/11-adr/0022-device-signature-canonical-json.md)),
//! so verification necessarily happens *after* the body is parsed.
//!
//! # Why the headers are declared rather than hidden
//!
//! The first shape of this was a `Signed<T>` body extractor that parsed and
//! verified in one step, so a handler could not reach an unverified value.
//! kynos seals `Describe` — `OperationCx` is private — so a custom extractor
//! cannot describe itself, and an operation whose body extractor is invisible
//! to the document is exactly the silent gap
//! [ADR-0021](../../../../docs/11-adr/0021-kynos-openapi-server.md) adopted the
//! framework to remove.
//!
//! Declaring [`DeviceSig`] through `Headers<T>` is better than the shape it
//! replaced rather than merely permitted: the three headers appear in the
//! description as parameters, so a generated client knows they exist. What is
//! lost is the compiler forcing the check; what replaces it is [`verify`] being
//! the only way to obtain a [`Device`], and every authenticated operation
//! needing one.
//!
//! # The canonical target
//!
//! The signature covers the method and target, and both are taken from the
//! operation's own declaration rather than read back off the request. That is
//! stricter than v1 managed: axum's `Router::nest` rewrote `Uri` to the path
//! *inside* the nest, so v1 had to reach for `OriginalUri` to see what the
//! client actually sent, and getting that wrong silently changes what is
//! verified.
//!
//! **No operation on this surface takes a query parameter.** Adding one means
//! extending the canonical target to include it on both sides; `path` is a
//! literal at each call site so that cannot happen by accident.

use crate::api::auth::Principal;
use crate::api::error::ApiError;
use crate::state::ServerState;
use crate::store::Device;

/// The device-binding headers, declared so they appear in the description.
#[derive(Debug, Clone, kynos::HeaderParams)]
pub struct DeviceSig {
    /// Which of the account's devices signed this request.
    ///
    /// Renamed explicitly: the derive falls back to the field identifier
    /// verbatim, so without this the extractor would read a header literally
    /// named `x_sunrise_device` and describe that name to clients -- wrong on
    /// the wire and wrong in the document, and passing every test that only
    /// ever talks to itself.
    #[header(rename = "X-Sunrise-Device")]
    pub device: Option<String>,
    /// The detached Ed25519 signature, base64url no-pad.
    #[header(rename = "X-Sunrise-Device-Sig")]
    pub signature: Option<String>,
    /// Inside the signature, so it cannot be adjusted in flight.
    #[header(rename = "Date")]
    pub date: Option<String>,
}

/// Check a request's device signature and resolve the signing device.
///
/// `value` is the parsed request body, or `None` for an operation with no body
/// — which hashes the empty string, so stripping a body is not a way to produce
/// a signature that verifies.
///
/// The device is resolved from the *account row*, never from what the request
/// says about itself: the header names which of the account's devices to check
/// against, and an id that is not an active row on that account resolves to
/// nothing. A caller cannot borrow another account's device by naming it.
///
/// # Errors
/// [`ApiError::Unauthenticated`] when binding is required and absent, when the named device
/// is not active on this account, or when the signature does not verify. Every
/// case collapses to one rejection: the distinction is useful to the server's
/// log and to nobody else.
pub fn verify<T: serde::Serialize>(
    state: &ServerState,
    principal: &Principal,
    sig: &DeviceSig,
    method: &str,
    path: &str,
    value: Option<&T>,
) -> Result<Option<Device>, ApiError> {
    let (Some(device_id), Some(signature)) = (sig.device.as_deref(), sig.signature.as_deref())
    else {
        if state.config.require_device_sig {
            return Err(ApiError::Unauthenticated);
        }
        // Self-host single-tenant: there are no device rows to bind to, and
        // `ServerConfig::validate` refuses `require_device_sig` in that mode,
        // so an absent binding here is the configured state, not a gap.
        return Ok(None);
    };

    // Cross-check the token's own claim when the issuer stamps one, so a stolen
    // bearer cannot be replayed from a different device.
    if let Some(claimed) = principal.subject.device_id.as_deref() {
        if claimed != device_id {
            return Err(ApiError::Unauthenticated);
        }
    }

    let device = state
        .store
        .active_device(&principal.account.account_id, device_id)
        .map_err(|_| ApiError::Unauthenticated)?
        .ok_or(ApiError::Unauthenticated)?;

    let date = sig.date.as_deref().ok_or(ApiError::Unauthenticated)?;
    sunrise_http_sig::verify(
        &device.device_pub_s,
        signature,
        method,
        path,
        date,
        value,
        state.clock.now_ms(),
    )
    .map_err(|e| {
        state.metrics.incr("sunrise_device_sig_rejected_total");
        tracing::warn!(
            ev = "srv.auth.device_sig_rejected",
            err_kind = "user",
            reason = %e,
            "device signature rejected"
        );
        ApiError::Unauthenticated
    })?;

    let _ = state
        .store
        .touch_device(&device.device_id, state.clock.now_ms());
    Ok(Some(device))
}
