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
//! URI the way axum's `Router::nest` did — which is what forced v1 to reach for
//! `OriginalUri` — so what arrives here is what was sent.
//!
//! **No operation on this surface takes a query parameter.** Adding one means
//! extending the canonical target on both sides; today `path_and_query` and the
//! path agree, and a query appearing without the client half changing would
//! break verification loudly rather than silently.

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
/// # Errors
/// A `401` [`ApiError::Unauthenticated`] in every failing case. The *code* it
/// carries is two-tier: `AUTH_DEVICE_SIG_INVALID` once the named device has
/// resolved to an active row on this account and only the signature is wrong,
/// `AUTH_TOKEN_INVALID` for everything upstream of that lookup — including a
/// device id that is not on this account, which must stay indistinguishable
/// from a bad bearer or the code becomes an enumeration oracle.
pub fn verify<T: serde::Serialize>(
    state: &ServerState,
    principal: &Principal,
    sig: &DeviceSig,
    method: &str,
    path: &str,
    value: Option<&T>,
) -> Result<Option<Device>, ApiError> {
    let canonical = match value {
        Some(v) => sunrise_http_sig::canonical_json(v).map_err(|_| ApiError::unauthenticated())?,
        None => Vec::new(),
    };
    verify_bytes(state, principal, sig, method, path, &canonical)
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
/// As [`verify`].
pub fn verify_bytes(
    state: &ServerState,
    principal: &Principal,
    sig: &DeviceSig,
    method: &str,
    path: &str,
    canonical_body: &[u8],
) -> Result<Option<Device>, ApiError> {
    let (Some(device_id), Some(signature)) = (sig.device.as_deref(), sig.signature.as_deref())
    else {
        if state.config.require_device_sig {
            // The one pre-lookup case that names the signature. Nothing is
            // disclosed: the server has already told every caller it demands a
            // binding, in `GET /meta`'s `device_binding_required`.
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
        .map_err(|_| ApiError::unauthenticated())?
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
        state.metrics.incr("sunrise_device_sig_rejected_total");
        tracing::warn!(
            ev = "srv.auth.device_sig_rejected",
            err_code = %sunrise_error::ErrorCode::AuthDeviceSigInvalid,
            err_kind = "user",
            reason = %e,
            "device signature rejected"
        );
        ApiError::device_sig_invalid()
    })?;

    let _ = state
        .store
        .touch_device(&device.device_id, state.clock.now_ms());
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
        verify(state, &principal, &sig, &method, &target, Some(&value))?
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
        let device = verify::<()>(context, &principal, &sig, &method, &target, None)?;
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
        let device = verify_bytes(context, &principal, &sig, &method, &target, &bytes)?;
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
mod tests {
    use crate::api::error::codes::{AUTH_DEVICE_SIG_INVALID, AUTH_TOKEN_INVALID};
    use crate::api::testing::{Client, BEARER};
    use crate::ServerConfig;
    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use kynos::http::{Method, StatusCode};

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }

    /// The problem document's `code` extension member — the thing a client
    /// switches on, and the whole point of the two-tier rule.
    fn code_of(res: &crate::api::testing::Res) -> String {
        res.json()["code"]
            .as_str()
            .unwrap_or_else(|| panic!("a code member: {}", res.json()))
            .to_owned()
    }

    /// The server's own now, formatted as the `Date` the scheme signs.
    fn now_rfc2822(client: &Client) -> String {
        let secs = i64::try_from(client.clock_now_ms() / 1000).expect("a sane clock");
        jiff::Timestamp::from_second(secs)
            .expect("a valid timestamp")
            .strftime("%a, %d %b %Y %H:%M:%S GMT")
            .to_string()
    }

    /// Register a device and hand back its id and signing key.
    async fn paired(client: &Client, seed: u8) -> (String, SigningKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let res = client
            .send(
                Method::POST,
                "/api/v1/devices",
                Some(&serde_json::json!({
                    "device_pub_s": b64(sk.verifying_key().as_bytes()),
                    "nickname": "laptop",
                    "platform": "linux",
                })),
            )
            .await;
        res.assert_status(StatusCode::CREATED);
        let id = res.json()["device_id"]
            .as_str()
            .expect("a device id")
            .to_owned();
        (id, sk)
    }

    /// Send a request bound to `device_id` by `key`.
    async fn send_signed(
        client: &Client,
        method: &str,
        target: &str,
        device_id: &str,
        key: &SigningKey,
        body: Option<&serde_json::Value>,
    ) -> crate::api::testing::Res {
        let date = now_rfc2822(client);
        let signature = sunrise_http_sig::sign(key, method, target, &date, body)
            .expect("the client half signs");
        client
            .send_with(
                method.parse().expect("a method"),
                target,
                Some(BEARER),
                body,
                &[
                    ("x-sunrise-device", device_id),
                    ("x-sunrise-device-sig", &signature),
                    ("date", &date),
                ],
            )
            .await
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
        let signature = sunrise_http_sig::sign::<serde_json::Value>(
            &sk,
            "GET",
            "/api/v1/accounts/me",
            &date,
            None,
        )
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
            require_device_sig: true,
            ..ServerConfig::default()
        });

        let res = client.send(Method::GET, "/api/v1/accounts/me", None).await;
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

        let signature = sunrise_http_sig::sign::<serde_json::Value>(
            &sk,
            "GET",
            "/api/v1/accounts/me",
            &date,
            None,
        )
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
}
