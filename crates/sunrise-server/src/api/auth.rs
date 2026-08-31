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
//! Taking [`Auth<AccountToken>`] in a handler adds the scheme to that
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
    /// Every failure is `unauthenticated`. The server can tell a bad signature
    /// from an expired token from an unknown issuer, and says so in its own
    /// logs; a client is told none of it, because the difference is only useful
    /// to someone probing which tokens exist.
    async fn authenticate(
        &self,
        presented: BearerToken,
        _context: &ServerState,
    ) -> Result<Principal, AuthRejection> {
        let verified = self
            .token_verifier
            .verify(presented.as_str())
            .await
            .map_err(|_| AuthRejection::unauthenticated())?;
        let account = self
            .store
            .resolve_account(
                &verified.subject,
                self.config.allow_signup,
                self.clock.now_ms(),
            )
            .map_err(|_| AuthRejection::unauthenticated())?;
        Ok(Principal {
            subject: verified.subject,
            account,
        })
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
