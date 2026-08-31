//! Server authentication: OIDC token verification and device binding.
//!
//! Per `docs/06-server/auth.md`, Sunrise issues no tokens of its own. Every
//! authenticated request carries an OIDC access token minted by the configured
//! issuer, and the server's whole job is to *verify* it and map it onto an
//! account by `(iss, sub)`.
//!
//! Layout:
//!
//! - [`TokenVerifier`] — the seam. Everything auth-shaped goes through it.
//! - [`oidc::OidcVerifier`] — the real thing: JWKS-backed signature check plus
//!   `iss` / `aud` / `exp` / `nbf` validation.
//! - [`http`] — the injectable HTTP surface the verifier fetches JWKS over, so
//!   the verifier is testable without a network.
//! - [`NullVerifier`] — self-host single-tenant escape hatch.
//!
//! Request signing is not here. `header_sig_v1` and the axum-shaped pipeline
//! that ran it were removed with the router they served: ADR-0022 replaced the
//! construction, `sunrise-http-sig` owns it so the relay and the generated
//! client cannot disagree about it, and `api::signed` is where a request meets
//! it.

pub mod http;
pub mod oidc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Principal extracted from a verified token.
///
/// The identity that matters is the pair `(issuer, subject)`: `docs/06-server/
/// auth.md` keys the account row on it, and it is the only identifier a client
/// cannot influence. Everything downstream — the relay's channel namespace, the
/// account lookup — is derived from [`Subject::principal_key`] rather than from
/// anything in the request body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subject {
    /// OIDC `iss` claim. Distinguishes IdPs when a self-host deployment
    /// federates more than one.
    pub issuer: String,
    /// OIDC `sub` claim — the IdP's stable user identifier.
    pub subject: String,
    /// OIDC `email` claim, if the token carried one. Discovery only; auth never
    /// depends on it.
    pub email: Option<String>,
    /// The `https://sunrise.app/device_id` claim, if present. Checked against
    /// the `X-Sunrise-Device` header as defence in depth.
    pub device_id: Option<String>,
}

impl Subject {
    /// Construct a subject from the two claims that identify it.
    #[must_use]
    pub fn new(issuer: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            subject: subject.into(),
            email: None,
            device_id: None,
        }
    }

    /// Attach an email claim.
    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// A collision-free flattening of `(issuer, subject)`.
    ///
    /// The separator is US (`0x1f`), which cannot appear in a URL-shaped `iss`
    /// or in a `sub`; concatenating without one would let an issuer choose a
    /// `sub` that impersonates another issuer's principal.
    #[must_use]
    pub fn principal_key(&self) -> String {
        format!("{}\u{1f}{}", self.issuer, self.subject)
    }
}

/// A verified token: who it names, and when it stops being valid.
///
/// The expiry is the reason this exists rather than [`TokenVerifier::verify`]
/// returning a bare [`Subject`]. `exp` was validated and then dropped on the
/// floor, so nothing downstream could tell a session how long its credential
/// had left — which is the whole of mid-session expiry (issue #7). A `Subject`
/// is an *identity*, and identity does not expire; the deadline belongs beside
/// it, not inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The principal the token names.
    pub subject: Subject,
    /// Wall-clock milliseconds at which the token's `exp` falls.
    ///
    /// `None` means **this verifier issues no deadline at all**, not "already
    /// expired" and not "expires soon". Only [`NullVerifier`] does that: the
    /// self-host single-tenant path has no IdP and therefore no token to age
    /// out. A `None` deadline is never enforced, so read it as "not
    /// applicable" — anything that treats it as "expired now" would break
    /// self-host, and anything that treats it as "never verify again" would be
    /// wrong for a real verifier, which always sets it.
    pub expires_at_ms: Option<u64>,
}

impl Verified {
    /// A verification result with no expiry deadline.
    #[must_use]
    pub const fn new(subject: Subject) -> Self {
        Self {
            subject,
            expires_at_ms: None,
        }
    }

    /// Attach the deadline the token's `exp` falls at.
    #[must_use]
    pub const fn expiring_at(mut self, at_ms: u64) -> Self {
        self.expires_at_ms = Some(at_ms);
        self
    }

    /// Whether the token is already past its deadline at `now_ms`.
    ///
    /// A verifier that issues no deadline is never expired.
    #[must_use]
    pub fn is_expired_at(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| now_ms >= at)
    }
}

/// Token verification error.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Token absent or malformed.
    #[error("missing or malformed token")]
    Missing,
    /// Signature / claim validation failed.
    #[error("invalid token: {0}")]
    Invalid(String),
    /// Token is well-formed and correctly signed but past its `exp`.
    #[error("token expired")]
    Expired,
    /// OIDC provider unreachable / JWKS fetch failed.
    #[error("oidc transport: {0}")]
    Transport(String),
}

/// Token verifier trait. Each request handler that needs auth obtains the
/// verifier via the [`crate::ServerState`] and calls `verify`.
#[async_trait]
pub trait TokenVerifier: Send + Sync + std::fmt::Debug {
    /// Verify a bearer token; on success, return the subject and its deadline.
    async fn verify(&self, bearer: &str) -> Result<Verified, AuthError>;

    /// Whether this verifier maps every caller to a single shared identity.
    ///
    /// Only [`NullVerifier`] does. The server refuses to bind a non-loopback
    /// address while one is installed, because doing so publishes one shared
    /// account namespace to the network. Defaulting to `false` means a new
    /// verifier is treated as multi-tenant unless it says otherwise — the safe
    /// direction for a defaulted method.
    fn is_single_tenant(&self) -> bool {
        false
    }
}

/// The issuer string [`NullVerifier`] stamps on its one synthetic principal.
pub const SELF_HOST_ISSUER: &str = "urn:sunrise:self-host";

/// Self-host single-tenant verifier — every request maps to the same
/// synthetic subject. NEVER use in multi-tenant deployments.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullVerifier;

#[async_trait]
impl TokenVerifier for NullVerifier {
    async fn verify(&self, _bearer: &str) -> Result<Verified, AuthError> {
        // No IdP, so no `exp` and no deadline. A self-host session is bounded
        // by the process, not by a credential.
        Ok(Verified::new(Subject::new(SELF_HOST_ISSUER, "self-host")))
    }

    fn is_single_tenant(&self) -> bool {
        true
    }
}

/// Test verifier that accepts a fixed map of `bearer → verified token`.
///
/// The value is a [`Verified`] rather than a [`Subject`] so a test can give a
/// bearer a deadline, which is what makes mid-session expiry and the
/// `RefreshToken` exchange testable without a real IdP or a real clock.
#[derive(Debug, Default, Clone)]
pub struct StaticVerifier {
    /// Allowed bearers; a bearer not in the map is rejected.
    pub allowed: std::collections::HashMap<String, Verified>,
}

impl StaticVerifier {
    /// Accept `bearer` as `subject`, with no expiry deadline.
    #[must_use]
    pub fn with(mut self, bearer: impl Into<String>, subject: Subject) -> Self {
        self.allowed.insert(bearer.into(), Verified::new(subject));
        self
    }

    /// Accept `bearer` as `subject`, expiring at `at_ms`.
    #[must_use]
    pub fn with_expiring(
        mut self,
        bearer: impl Into<String>,
        subject: Subject,
        at_ms: u64,
    ) -> Self {
        self.allowed
            .insert(bearer.into(), Verified::new(subject).expiring_at(at_ms));
        self
    }
}

#[async_trait]
impl TokenVerifier for StaticVerifier {
    async fn verify(&self, bearer: &str) -> Result<Verified, AuthError> {
        self.allowed
            .get(bearer)
            .cloned()
            .ok_or_else(|| AuthError::Invalid("unknown bearer".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_verifier_accepts_anything() {
        let v = NullVerifier;
        let v = v.verify("anything").await.unwrap();
        assert_eq!(v.subject.subject, "self-host");
        assert_eq!(
            v.expires_at_ms, None,
            "self-host issues no credential, so nothing ages out"
        );
    }

    /// `ws_handshake.rs` connects with no `Authorization` header at all, which
    /// the upgrade path turns into `verify("")`. Self-host mode survives only
    /// because that case is accepted.
    #[tokio::test]
    async fn null_verifier_accepts_the_empty_bearer() {
        assert!(NullVerifier.verify("").await.is_ok());
    }

    #[tokio::test]
    async fn static_verifier_rejects_unknown() {
        let v = StaticVerifier::default();
        assert!(v.verify("nope").await.is_err());
    }

    #[tokio::test]
    async fn static_verifier_accepts_known() {
        let v = StaticVerifier::default().with("secret", Subject::new("iss", "alice"));
        let got = v.verify("secret").await.unwrap();
        assert_eq!(got.subject.subject, "alice");
        assert_eq!(got.expires_at_ms, None);
    }

    /// A deadline in the past is not treated as "no deadline". The two are
    /// distinct and the distinction is load-bearing: `None` means self-host,
    /// `Some(past)` means close the session.
    #[test]
    fn an_absent_deadline_is_not_an_elapsed_one() {
        let none = Verified::new(Subject::new("iss", "alice"));
        assert!(!none.is_expired_at(u64::MAX));

        let past = Verified::new(Subject::new("iss", "alice")).expiring_at(1_000);
        assert!(
            past.is_expired_at(1_000),
            "expiry is inclusive at the instant"
        );
        assert!(past.is_expired_at(1_001));
        assert!(!past.is_expired_at(999));
    }

    /// Two principals that differ only by where the issuer ends and the subject
    /// begins must not collapse onto one key — that would be an account
    /// takeover across federated issuers.
    #[test]
    fn principal_key_cannot_be_forged_by_a_hostile_sub() {
        let honest = Subject::new("https://idp.example", "alice");
        let attacker = Subject::new("https://idp.example/alice", "");
        assert_ne!(honest.principal_key(), attacker.principal_key());
    }
}
