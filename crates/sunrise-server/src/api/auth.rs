//! The bearer scheme, and the device binding that sits on top of it.
//!
//! Two separate things, deliberately, because they prove different facts:
//!
//! * the **bearer** proves which *account* is calling. It is verified against
//!   the issuer's JWKS and resolved to a row through `(iss, sub)`. Nothing
//!   identifying is ever read out of a request body — a handler cannot be
//!   tricked into acting on an account by being *told* one.
//! * the **device signature** proves which *device* is calling. A stolen bearer
//!   is replayable from anywhere; the signature is what makes revoking a device
//!   mean something while its OIDC token is still valid at the issuer.
//!
//! Taking [`Auth<AccountToken>`](kynos::security::auth::Auth) in a handler adds the scheme to that
//! operation's `security`, registers it under `components.securitySchemes`, and
//! adds 401 and 403 to its responses. There is no way to do one without the
//! others, which is the property that stops an authenticated route being
//! documented as an open one.

use crate::api::error::ApiError;
use crate::api::signed::SignedRejection;
use crate::auth::Verified;
use crate::state::ServerState;
use crate::store::{Account, MetadataError};
use kynos::error::rejection::AuthRejection;
use kynos::http::Parts;
use kynos::security::auth::MaybeAuth;
use kynos::security::carrier::BearerToken;
use kynos::security::{Authenticates, Authenticator, Guard};

/// A caller proven to hold a valid bearer for a resolved account.
///
/// Deliberately carries no device: the bearer cannot prove one. A route that
/// needs the calling device calls [`super::signed::verify`], which checks the
/// request signature and produces one.
#[derive(Debug, Clone)]
pub struct Principal {
    /// The verified token subject.
    pub subject: crate::auth::Subject,
    /// The account `(iss, sub)` resolves to.
    pub account: Account,
    /// What the token said about how recently and how strongly its holder
    /// authenticated.
    ///
    /// Carried on the principal rather than consulted at the verifier because
    /// only the *route* knows what it demands: nothing on this surface needs a
    /// step-up except `GET /api/v1/accounts/me/recovery_blob`, and a verifier
    /// that refused a token for lacking one would lock every other operation
    /// out of every deployment whose IdP omits the claims.
    pub step_up: crate::auth::StepUp,
    /// The bearer's `exp`, carried because a sync session outlives the request
    /// that opened it and has to end when the credential does.
    ///
    /// `None` only for the self-host verifier, which has no IdP and therefore
    /// no token to age out. Read it as "not applicable" rather than "expired
    /// now" — the latter would break self-host entirely.
    pub expires_at_ms: Option<u64>,
}

/// The account bearer token.
///
/// Its credential is the verified token, not the account: a token is a fact
/// about the caller, and turning it into an account row is a store read that
/// can fail for reasons that have nothing to do with the caller. kynos's
/// [`AuthRejection`] can say `401` or `403` and nothing else, and carries no
/// stable code, so a store that did not answer has no truthful way out of an
/// [`Authenticator`] and a refused sign-up loses its `AUTH_SIGNUP_DISABLED`.
/// The lookup therefore runs in [`resolve_bearer`], after the authenticator,
/// where a storage failure can be the retryable `503` it is on every other
/// path (ADR-0062 §1).
#[derive(Debug, kynos::SecurityScheme)]
#[security(bearer(format = "JWT"))]
#[security(
    credential = Verified,
    description = "An OIDC access token for the calling account, verified against the issuer's JWKS."
)]
pub struct AccountToken;

impl Authenticator<AccountToken, ServerState> for ServerState {
    /// Verify the bearer, without resolving it to an account.
    ///
    /// Every *credential* failure is `unauthenticated`. The server can tell a
    /// bad signature from an expired token from an unknown issuer, and says so
    /// in its own logs; a client is told none of it, because the difference is
    /// only useful to someone probing which tokens exist.
    async fn authenticate(
        &self,
        presented: BearerToken,
        _context: &ServerState,
    ) -> Result<Verified, AuthRejection> {
        verify(self, presented.as_str()).await
    }

    /// No scopes are defined on this surface.
    ///
    /// Authorization here is per-account row ownership, enforced in SQL by
    /// every lookup taking the account id as a filter rather than trusting an
    /// id the caller supplied. A scope string would be a second, weaker place
    /// to express the same thing.
    async fn authorize(
        &self,
        _credential: &Verified,
        _scopes: &[kynos::security::Unscoped],
        _context: &ServerState,
    ) -> Result<(), AuthRejection> {
        Ok(())
    }
}

/// Verify the request's bearer and resolve it to an account row.
///
/// There are two ways in. The usual one is a presented `Authorization` header,
/// which kynos's carrier reads and [`Authenticator::authenticate`] verifies.
/// The other is an **absent** header, which `docs/06-server/auth.md` gives a
/// specific meaning: it verifies the empty string. `NullVerifier` — self-host,
/// single-tenant — accepts that; every real verifier rejects it. The property
/// that follows is the one worth preserving: *enabling authentication is purely
/// a matter of configuring a verifier*, with no route, extractor or header
/// check to change alongside it.
///
/// kynos's `Auth<S>` refuses an absent header at the carrier, before any
/// verifier is consulted, so taking it alone would have quietly made self-host
/// unusable — the CLI presents no bearer — and made "configure a verifier" no
/// longer sufficient. This therefore takes `MaybeAuth` and routes the absent
/// case to the verifier itself.
///
/// # Errors
/// [`SignedRejection::Auth`], `401`, for every credential failure.
/// [`SignedRejection::Binding`] when the account lookup refused or failed:
/// `403 AUTH_SIGNUP_DISABLED` when sign-up is disabled, and
/// `503 RELAY_STORAGE_UNAVAILABLE` when the metadata store did not answer, so
/// the client retries rather than refreshing a bearer that was never the
/// problem.
pub async fn resolve_bearer(
    parts: &Parts,
    state: &ServerState,
) -> Result<Principal, SignedRejection> {
    use sunrise_telemetry::FutureExt as _;

    // `auth.verify_token`, with the account lookup beneath it. The bearer is
    // never span data: the span carries its name and, on refusal, a status.
    let span = sunrise_telemetry::span("auth.verify_token", []);
    let resolved = verify_and_resolve(parts, state)
        .with_context(span.context())
        .await;
    if resolved.is_err() {
        span.fail("bearer refused");
    }
    resolved
}

async fn verify_and_resolve(
    parts: &Parts,
    state: &ServerState,
) -> Result<Principal, SignedRejection> {
    // Run through `MaybeAuth`'s `Guard` check directly: a guard has a handler
    // slot of its own rather than being an extractor, and the signed
    // extractors *are* the signed operations' authentication, so they run the
    // same check from inside.
    let MaybeAuth(presented) =
        <MaybeAuth<AccountToken> as Guard<ServerState>>::guard(parts, state).await?;
    let verified = match presented {
        Some(verified) => verified,
        None => verify(state, "").await?,
    };
    let account = state
        .store
        .resolve_account(
            &verified.subject,
            state.config.allow_signup,
            state.clock.now_ms(),
        )
        .await
        .map_err(|e| match e {
            // Sign-up being disabled is not a credential failure and must not
            // be reported as one. The token is valid and the caller is who
            // they say they are; the server is declining to open a new
            // account, which is a statement about its own configuration and
            // reveals nothing about who exists. Answering 401 tells a
            // legitimate user to go and fix a credential that was never wrong,
            // so it is `403 AUTH_SIGNUP_DISABLED`.
            //
            // A store that did not answer is the server's fault, so it is the
            // retryable `503 RELAY_STORAGE_UNAVAILABLE` it is on every other
            // request path, logged as `srv.store.failed` by the conversion.
            MetadataError::SignupDisabled | MetadataError::Unavailable(_) => {
                SignedRejection::Binding(ApiError::from(e))
            }
            MetadataError::NotFound | MetadataError::RecoveryBlobExists => {
                SignedRejection::Auth(AuthRejection::unauthenticated())
            }
        })?;
    Ok(Principal {
        subject: verified.subject,
        account,
        step_up: verified.step_up,
        expires_at_ms: verified.expires_at_ms,
    })
}

/// Verify `bearer` against the configured verifier.
async fn verify(state: &ServerState, bearer: &str) -> Result<Verified, AuthRejection> {
    state
        .token_verifier
        .verify(bearer)
        .await
        .map_err(|_| AuthRejection::unauthenticated())
}

/// The context owns its own authenticator, because the verification a bearer
/// needs *is* the server's state: the JWKS verifier that checks the token, the
/// store that resolves it to a row, and the clock both are checked against.
/// Splitting them into a separate authenticator value would give the same three
/// fields a second owner.
impl Authenticates<AccountToken> for ServerState {
    type Authenticator = Self;

    fn authenticator(&self) -> &Self {
        self
    }
}
