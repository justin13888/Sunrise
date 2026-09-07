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

use crate::state::ServerState;
use crate::store::Account;
use kynos::error::rejection::AuthRejection;
use kynos::security::carrier::BearerToken;
use kynos::security::{Authenticates, Authenticator};

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
    /// The bearer's `exp`, carried because a sync session outlives the request
    /// that opened it and has to end when the credential does.
    ///
    /// `None` only for the self-host verifier, which has no IdP and therefore
    /// no token to age out. Read it as "not applicable" rather than "expired
    /// now" — the latter would break self-host entirely.
    pub expires_at_ms: Option<u64>,
}

/// The account bearer token.
#[derive(Debug, kynos::SecurityScheme)]
#[security(bearer(format = "JWT"))]
#[security(
    credential = Principal,
    description = "An OIDC access token for the calling account, verified against the issuer's JWKS."
)]
pub struct AccountToken;

impl Authenticator<AccountToken, ServerState> for ServerState {
    /// Verify the bearer and resolve it to an account row.
    ///
    /// Every *credential* failure is `unauthenticated`. The server can tell a
    /// bad signature from an expired token from an unknown issuer, and says so
    /// in its own logs; a client is told none of it, because the difference is
    /// only useful to someone probing which tokens exist.
    ///
    /// Sign-up being disabled is the one failure that is not a credential
    /// failure and must not be reported as one. The token is valid and the
    /// caller is who they say they are; the server is declining to open a new
    /// account, which is a statement about its own configuration and reveals
    /// nothing about who exists. Answering 401 tells a legitimate user to go
    /// and fix a credential that was never wrong.
    ///
    /// The stable `AUTH_SIGNUP_DISABLED` code does not ride along on this one
    /// path: `Authenticator` is required to fail with [`AuthRejection`], whose
    /// problem document kynos renders itself so that the challenge on the wire
    /// and the one the operation declares cannot disagree. The status is the
    /// half of that contract a client acts on, and it is restored here.
    async fn authenticate(
        &self,
        presented: BearerToken,
        _context: &ServerState,
    ) -> Result<Principal, AuthRejection> {
        resolve_bearer(self, presented.as_str()).await
    }

    /// No scopes are defined on this surface.
    ///
    /// Authorization here is per-account row ownership, enforced in SQL by
    /// every lookup taking the account id as a filter rather than trusting an
    /// id the caller supplied. A scope string would be a second, weaker place
    /// to express the same thing.
    async fn authorize(
        &self,
        _credential: &Principal,
        _scopes: &'static [&'static str],
        _context: &ServerState,
    ) -> Result<(), AuthRejection> {
        Ok(())
    }
}

/// Verify `bearer` and resolve it to an account row.
///
/// Split out of [`Authenticator::authenticate`] because there are two ways in.
/// The other is an **absent** `Authorization` header, which
/// `docs/06-server/auth.md` gives a specific meaning: it verifies the empty
/// string. `NullVerifier` — self-host, single-tenant — accepts that; every real
/// verifier rejects it. The property that follows is the one worth preserving:
/// *enabling authentication is purely a matter of configuring a verifier*, with
/// no route, extractor or header check to change alongside it.
///
/// kynos's `Auth<S>` refuses an absent header at the carrier, before any
/// verifier is consulted, so taking it alone would have quietly made self-host
/// unusable — the CLI presents no bearer — and made "configure a verifier" no
/// longer sufficient. [`super::signed`] therefore takes `MaybeAuth` and routes
/// the absent case here.
///
/// # Errors
/// [`AuthRejection::Forbidden`] when sign-up is disabled, and `unauthenticated`
/// for every credential failure.
pub async fn resolve_bearer(state: &ServerState, bearer: &str) -> Result<Principal, AuthRejection> {
    let verified = state
        .token_verifier
        .verify(bearer)
        .await
        .map_err(|_| AuthRejection::unauthenticated())?;
    let account = state
        .store
        .resolve_account(
            &verified.subject,
            state.config.allow_signup,
            state.clock.now_ms(),
        )
        .map_err(|e| match e {
            crate::store::StoreError::SignupDisabled => AuthRejection::Forbidden,
            _ => AuthRejection::unauthenticated(),
        })?;
    Ok(Principal {
        subject: verified.subject,
        account,
        expires_at_ms: verified.expires_at_ms,
    })
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
