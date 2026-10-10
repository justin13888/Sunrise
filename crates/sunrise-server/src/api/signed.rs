//! Device binding for the typed surface: one extractor that parses and verifies.
//!
//! `header_sig_v2` signs the canonical JSON of the request *value*
//! ([ADR-0022](../../../../docs/11-adr/0022-device-signature-canonical-json.md)),
//! so verification necessarily happens *after* the body is parsed. That ordering
//! is what makes a combined extractor the right shape rather than a convenience:
//! parsing and verifying are one step, so they are one type.
//!
//! # Why the value and the check travel together
//!
//! [`Signed<T>`] hands a handler the parsed body, the authenticated
//! [`Principal`] and the resolved [`Device`] *only* once the bearer, the three
//! binding headers and the signature have all checked out.
//!
//! Precisely: the only route from a *request* to a `Signed<T>` is
//! [`FromRequest::from_request`], and every path through it verifies. A handler
//! declaring `Signed<PushRegistration>` therefore cannot be handed an
//! unverified body — not because the struct is unforgeable in Rust, but because
//! the framework has no other way to build one. What the previous shape left to
//! review — did this handler remember to call `verify`, with the right target,
//! before touching the value — has no place left to go wrong.
//!
//! An earlier revision of this module used `Headers<DeviceSig>` plus a hand
//! call to [`verify`], on the stated grounds that "kynos seals `Describe` —
//! `OperationCx` is private — so a custom extractor cannot describe itself".
//! **That was wrong.** `Describe` is a plain public trait with no sealed
//! supertrait, and `OperationCx` exposes `new`, `finish`, `add_parameter`,
//! `set_request_body`, `add_security` and the rest. kynos's own `LastEventId`
//! is a hand-written extractor, and its `examples/parameters.rs` advertises one.
//! Nothing was ever in the way.
//!
//! # What it declares
//!
//! Everything it reads, by composing the extractors that already describe each
//! part: `Auth<AccountToken>` for the bearer and its security scheme,
//! `Headers<DeviceSig>` for the three binding headers, and `Json<T>` for the
//! body. The description is therefore identical to the one the split shape
//! produced — the same parameters under the same wire names — and the
//! compile-time guarantee is new.
//!
//! Authentication runs once. Composing `Auth` rather than re-verifying the
//! bearer is what keeps it that way: a handler taking both `Auth<AccountToken>`
//! and a self-authenticating body extractor would verify every token twice.
//!
//! # The canonical target
//!
//! The signature covers the method and the *concrete* target the client sent,
//! read from the request. kynos routes on whole paths and does not rewrite the
//! URI the way axum's `Router::nest` did — which is what forced the axum server to reach for
//! `OriginalUri` — so what arrives here is what was sent.
//!
//! **No signed operation takes a query parameter.** Adding one means extending the canonical
//! target on both sides; today `path_and_query` and the path agree, and a query appearing
//! without the client half changing would break verification loudly rather than silently. The
//! one query on the surface, `GET /api/v1/health?deep=1`, is unsigned and never reaches these.

use crate::api::auth::{AccountToken, Principal};
use crate::api::error::ApiError;
use crate::state::ServerState;
use crate::store::Device;
use kynos::error::rejection::{AuthRejection, BodyRejection};
use kynos::extract::body::binary::Binary;
use kynos::extract::body::json::Json;
use kynos::extract::describe::{Describe, RequestContent};
use kynos::extract::media::OctetStream;
use kynos::extract::params::header::Headers;
use kynos::extract::{FromRequest, FromRequestParts};
use kynos::http::{Parts, Request};
use kynos::response::{IntoResponse, Responses};
use kynos::router::operation::OperationCx;
use kynos::schema::registry::Registry;
use kynos::schema::Schema;
use kynos::security::auth::{Auth, MaybeAuth};

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
/// # Effects
///
/// Not a pure check. A verification that succeeds **writes**: the resolved
/// device's last-seen stamp is bumped through `Store::touch_device`, so every
/// signed route pays one SQLite `UPDATE` inside the request, on the store's
/// single mutex-guarded connection. That is the cost this imposes on the whole
/// signed surface, and it is worth knowing before adding another route to it.
///
/// That write's result is deliberately discarded. A failed touch leaves the
/// stamp stale and the request still succeeds — device liveness is a
/// diagnostic, not something to fail an otherwise valid request over — but it
/// also means the failure is silent and never reaches a log.
///
/// A bad signature also counts in `sunrise_device_sig_rejected_total{reason}` and is logged.
///
/// # Errors
/// A `401` [`ApiError::Unauthenticated`] in every failing case; only the *code*
/// it carries varies. `AUTH_DEVICE_SIG_INVALID` says "the signature, not the
/// bearer" and is returned once the named device has resolved to an active row
/// on this account and the binding still did not check out — and, before any
/// lookup, when `require_device_sig` is set and the caller did not present a
/// complete binding, meaning either header missing, not only both.
/// `AUTH_TOKEN_INVALID` covers the rest of the pre-lookup ground, including a
/// device id that is not on this account, which must stay indistinguishable
/// from a bad bearer or the code becomes an enumeration oracle. See
/// [`ApiError::device_sig_invalid`] for the full rule and for the bearer-
/// validity disclosure the pre-lookup case carries.
///
/// The one failure that is not a `401` is the metadata store not answering the
/// device lookup: `503 RELAY_STORAGE_UNAVAILABLE`, which the client retries
/// rather than refreshing a bearer that was never the problem (ADR-0062 §1).
///
/// The body is canonicalized before the returned future is built, so the
/// future holds bytes rather than a borrow of `T`.
pub fn verify<'a, T: serde::Serialize>(
    state: &'a ServerState,
    principal: &'a Principal,
    sig: &'a DeviceSig,
    method: &'a str,
    path: &'a str,
    value: Option<&T>,
) -> impl std::future::Future<Output = Result<Option<Device>, ApiError>> + Send + 'a {
    let canonical = value.map_or_else(
        || Ok(Vec::new()),
        |v| sunrise_http_sig::canonical_json(v).map_err(|_| ApiError::unauthenticated()),
    );
    async move { verify_bytes(state, principal, sig, method, path, &canonical?).await }
}

/// [`verify`], against a body that is already in its canonical form.
///
/// ADR-0022 defines the rule over the body's **canonical form**, and JSON
/// reaches that through RFC 8785 first. A chunk of ciphertext has no key order,
/// whitespace or escaping to normalise away, so the bytes already are it and
/// this is the entry point a binary body uses. There is no second scheme —
/// `header_sig_v2` covers both, which is what lets the blob store's own content
/// hashing assert the same property from the other direction.
///
/// # Errors
/// As [`verify`], as are the effects: this is the function that performs them.
pub async fn verify_bytes(
    state: &ServerState,
    principal: &Principal,
    sig: &DeviceSig,
    method: &str,
    path: &str,
    canonical_body: &[u8],
) -> Result<Option<Device>, ApiError> {
    use sunrise_telemetry::FutureExt as _;
    // `auth.verify_signature`, with the device lookup beneath it. Neither the
    // signature nor the device id is span data.
    let span = sunrise_telemetry::span("auth.verify_signature", []);
    let verified = verify_binding(state, principal, sig, method, path, canonical_body)
        .with_context(span.context())
        .await;
    if verified.is_err() {
        span.fail("device signature refused");
    }
    verified
}

async fn verify_binding(
    state: &ServerState,
    principal: &Principal,
    sig: &DeviceSig,
    method: &str,
    path: &str,
    canonical_body: &[u8],
) -> Result<Option<Device>, ApiError> {
    let (Some(device_id), Some(signature)) = (sig.device.as_deref(), sig.signature.as_deref())
    else {
        if state.config.device_sig_required() {
            // The one pre-lookup case that names the signature, and it covers a
            // *partial* binding too: the `let else` above wants both headers,
            // so one without the other lands here. Nothing about the *account*
            // is disclosed -- no lookup has happened yet. That the *bearer* is
            // valid is disclosed, and is accepted rather than closed: the
            // bootstrap routes carry the same disclosure unconditionally and
            // more strongly. See `ApiError::device_sig_invalid` and ADR-0035.
            return Err(ApiError::device_sig_invalid());
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
            return Err(ApiError::unauthenticated());
        }
    }

    // Deliberately `unauthenticated`, not `device_sig_invalid`: a caller who
    // guessed a device id must not be able to tell "not on this account" from
    // "your bearer is no good". Everything below this line has proved it is an
    // active device of the authenticated account, which is what earns it the
    // finer code.
    let device = state
        .store
        .active_device(&principal.account.account_id, device_id)
        .await?
        .ok_or_else(ApiError::unauthenticated)?;

    let date = sig
        .date
        .as_deref()
        .ok_or_else(ApiError::device_sig_invalid)?;
    sunrise_http_sig::verify_canonical(
        &device.device_pub_s,
        signature,
        method,
        path,
        date,
        canonical_body,
        state.clock.now_ms(),
    )
    .map_err(|e| {
        state.metrics.incr_with(
            "sunrise_device_sig_rejected_total",
            &[("reason", e.reason())],
        );
        tracing::warn!(
            ev = "srv.auth.device_sig_rejected",
            err_code = %sunrise_error::ErrorCode::AuthDeviceSigInvalid,
            err_kind = "user",
            cause = %e,
            "device signature rejected"
        );
        ApiError::device_sig_invalid()
    })?;

    let _ = state
        .store
        .touch_device(&device.device_id, state.clock.now_ms())
        .await;
    Ok(Some(device))
}

/// Whether a route demands a binding or merely checks one that is offered.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Binding {
    /// The server's configured policy decides, via [`verify`].
    Required,
    /// A device cannot sign before it exists, so an absent binding is accepted
    /// here even where the server demands one elsewhere. One that *is* supplied
    /// is still verified in full — the exemption is for the missing signature,
    /// not for a wrong one.
    Bootstrap,
}

/// What every signed extractor resolves to.
///
/// Obtaining one is proof that the bearer verified, the account resolved, and
/// the binding satisfied whatever the route demands of it.
#[derive(Debug, Clone)]
pub struct Caller {
    /// The authenticated account.
    pub principal: Principal,
    /// The device that signed, when one did. `None` is the self-host
    /// single-tenant case, where there are no device rows to bind to and
    /// `ServerConfig::validate` refuses `require_device_sig`.
    pub device: Option<Device>,
}

/// A verified request body, with the caller that sent it.
///
/// The body is parsed and the signature checked over its canonical form before
/// this exists, so `value` has never been reachable unverified.
#[derive(Debug, Clone)]
pub struct Signed<T> {
    /// Who sent it.
    pub caller: Caller,
    /// The parsed body.
    pub value: T,
}

/// [`Signed`] for the two bootstrap operations.
///
/// `POST /accounts` and `POST /devices` keep the exemption ADR-0022 records: a
/// device cannot sign before it exists. A supplied binding is still verified.
#[derive(Debug, Clone)]
pub struct SignedBootstrap<T> {
    /// Who sent it.
    pub caller: Caller,
    /// The parsed body.
    pub value: T,
}

/// The caller for an operation with no request body.
///
/// Hashes the empty string, so stripping a body is not a way to produce a
/// signature that verifies.
#[derive(Debug, Clone)]
pub struct SignedParts(pub Caller);

/// Why a signed request was refused.
///
/// Three sources, kept apart so each keeps its own rendering: kynos owns what a
/// bad credential and an unreadable body look like, and collapsing them into
/// one application error would make the document claim statuses the framework
/// does not send.
#[derive(Debug, thiserror::Error)]
pub enum SignedRejection {
    /// The bearer was absent, invalid, or refused.
    #[error(transparent)]
    Auth(#[from] AuthRejection),
    /// The body was the wrong media type, unreadable, or did not fit `T`.
    #[error(transparent)]
    Body(#[from] BodyRejection),
    /// The binding did not check out.
    #[error(transparent)]
    Binding(#[from] ApiError),
}

impl IntoResponse for SignedRejection {
    fn into_response(self) -> kynos::http::Response {
        match self {
            Self::Auth(e) => e.into_response(),
            Self::Body(e) => e.into_response(),
            Self::Binding(e) => e.into_response(),
        }
    }
}

impl Responses for SignedRejection {
    /// The union of what the three can send.
    ///
    /// Merged rather than picked: an operation that can answer 401 from the
    /// credential check and 422 from the body must describe both, and listing
    /// one would make the description wrong about the other.
    fn responses(registry: &mut Registry) -> kynos::openapi::Responses {
        let mut merged = <AuthRejection as Responses>::responses(registry);
        for source in [
            <BodyRejection as Responses>::responses(registry),
            <ApiError as Responses>::responses(registry),
        ] {
            for (status, response) in source.responses {
                merged.responses.insert(status, response);
            }
        }
        merged
    }
}

/// Authenticate, read the binding headers, and resolve the caller.
///
/// Shared by all three extractors so the order of checks is stated once.
async fn caller_of(
    parts: &mut Parts,
    state: &ServerState,
) -> Result<(Principal, DeviceSig), SignedRejection> {
    // `MaybeAuth` rather than `Auth`, so an absent `Authorization` header
    // reaches the verifier as the empty string rather than being refused at the
    // carrier -- see `resolve_bearer` for why that distinction is load-bearing.
    let MaybeAuth(presented) = MaybeAuth::<AccountToken>::from_request_parts(parts, state).await?;
    let principal = match presented {
        Some(principal) => principal,
        None => crate::api::auth::resolve_bearer(state, "").await?,
    };
    let Headers(sig) = Headers::<DeviceSig>::from_request_parts(parts, state)
        .await
        .map_err(|_| ApiError::unauthenticated())?;
    Ok((principal, sig))
}

/// The method and concrete target a signature covers.
fn target_of(parts: &Parts) -> (String, String) {
    let method = parts.method.as_str().to_owned();
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), |p| p.as_str().to_owned());
    (method, target)
}

/// Parse the body, then verify the binding over it.
async fn signed_of<T>(
    request: Request,
    state: &ServerState,
    binding: Binding,
) -> Result<(Caller, T), SignedRejection>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Send,
{
    let (mut parts, body) = request.into_parts();
    let (principal, sig) = caller_of(&mut parts, state).await?;
    let (method, target) = target_of(&parts);

    // The body is read *after* the credential, so an unauthenticated caller
    // cannot make the server parse an arbitrary document.
    let Json(value) = Json::<T>::from_request(Request::from_parts(parts, body), state).await?;

    let device = if binding == Binding::Bootstrap && sig.device.is_none() {
        None
    } else {
        verify(state, &principal, &sig, &method, &target, Some(&value)).await?
    };
    Ok((Caller { principal, device }, value))
}

impl<T> FromRequest<ServerState> for Signed<T>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Send,
{
    type Rejection = SignedRejection;

    async fn from_request(
        request: Request,
        context: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        let (caller, value) = signed_of(request, context, Binding::Required).await?;
        Ok(Self { caller, value })
    }
}

impl<T> FromRequest<ServerState> for SignedBootstrap<T>
where
    T: serde::de::DeserializeOwned + serde::Serialize + Send,
{
    type Rejection = SignedRejection;

    async fn from_request(
        request: Request,
        context: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        let (caller, value) = signed_of(request, context, Binding::Bootstrap).await?;
        Ok(Self { caller, value })
    }
}

impl FromRequestParts<ServerState> for SignedParts {
    type Rejection = SignedRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        context: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        let (principal, sig) = caller_of(parts, context).await?;
        let (method, target) = target_of(parts);
        let device = verify::<()>(context, &principal, &sig, &method, &target, None).await?;
        Ok(Self(Caller { principal, device }))
    }
}

/// Declare the bearer, the three binding headers, and the body.
///
/// Composed from the extractors that already describe each part, so the
/// document says exactly what the split shape used to say.
fn describe_signed<T: Schema>(operation: &mut OperationCx<'_>) {
    <Auth<AccountToken> as Describe>::describe(operation);
    <Headers<DeviceSig> as Describe>::describe(operation);
    <Json<T> as Describe>::describe(operation);
}

impl<T: Schema> Describe for Signed<T> {
    fn describe(operation: &mut OperationCx<'_>) {
        describe_signed::<T>(operation);
    }
}

impl<T: Schema> Describe for SignedBootstrap<T> {
    fn describe(operation: &mut OperationCx<'_>) {
        describe_signed::<T>(operation);
    }
}

impl Describe for SignedParts {
    fn describe(operation: &mut OperationCx<'_>) {
        <Auth<AccountToken> as Describe>::describe(operation);
        <Headers<DeviceSig> as Describe>::describe(operation);
    }
}

impl<T: Schema> RequestContent for Signed<T> {
    fn media_types() -> Vec<&'static str> {
        <Json<T> as RequestContent>::media_types()
    }

    fn request_body(registry: &mut Registry) -> kynos::openapi::RequestBody {
        <Json<T> as RequestContent>::request_body(registry)
    }
}

impl<T: Schema> RequestContent for SignedBootstrap<T> {
    fn media_types() -> Vec<&'static str> {
        <Json<T> as RequestContent>::media_types()
    }

    fn request_body(registry: &mut Registry) -> kynos::openapi::RequestBody {
        <Json<T> as RequestContent>::request_body(registry)
    }
}

/// A verified body of raw bytes, with the caller that sent it.
///
/// The chunk upload's body is opaque ciphertext, so there is no JSON value to
/// canonicalize and none is invented: [`verify_bytes`] signs the bytes as they
/// arrived, which ADR-0022 records as the same rule rather than a second one.
#[derive(Debug, Clone)]
pub struct SignedBinary {
    /// Who sent it.
    pub caller: Caller,
    /// The body, exactly as received.
    pub bytes: Vec<u8>,
}

impl FromRequest<ServerState> for SignedBinary {
    type Rejection = SignedRejection;

    async fn from_request(
        request: Request,
        context: &ServerState,
    ) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = request.into_parts();
        let (principal, sig) = caller_of(&mut parts, context).await?;
        let (method, target) = target_of(&parts);

        let binary =
            Binary::<OctetStream>::from_request(Request::from_parts(parts, body), context).await?;
        let bytes = binary.bytes.to_vec();
        let device = verify_bytes(context, &principal, &sig, &method, &target, &bytes).await?;
        Ok(Self {
            caller: Caller { principal, device },
            bytes,
        })
    }
}

impl Describe for SignedBinary {
    fn describe(operation: &mut OperationCx<'_>) {
        <Auth<AccountToken> as Describe>::describe(operation);
        <Headers<DeviceSig> as Describe>::describe(operation);
        <Binary<OctetStream> as Describe>::describe(operation);
    }
}

impl RequestContent for SignedBinary {
    fn media_types() -> Vec<&'static str> {
        <Binary<OctetStream> as RequestContent>::media_types()
    }

    fn request_body(registry: &mut Registry) -> kynos::openapi::RequestBody {
        <Binary<OctetStream> as RequestContent>::request_body(registry)
    }
}

#[cfg(test)]
#[path = "signed_tests.rs"]
mod tests;
