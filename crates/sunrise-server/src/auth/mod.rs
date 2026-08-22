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
//! - [`device_sig`] — `header_sig_v1` request signing (`X-Sunrise-Device-Sig`).
//! - [`request`] — the per-request pipeline handlers call: bearer → subject →
//!   account row → device binding.
//! - [`NullVerifier`] — self-host single-tenant escape hatch.

pub mod device_sig;
pub mod http;
pub mod oidc;
pub mod request;

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
    /// Verify a bearer token; on success, return the subject.
    async fn verify(&self, bearer: &str) -> Result<Subject, AuthError>;

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
    async fn verify(&self, _bearer: &str) -> Result<Subject, AuthError> {
        Ok(Subject::new(SELF_HOST_ISSUER, "self-host"))
    }

    fn is_single_tenant(&self) -> bool {
        true
    }
}

/// Test verifier that accepts a fixed map of `bearer → subject`.
#[derive(Debug, Default, Clone)]
pub struct StaticVerifier {
    /// Allowed bearers; a bearer not in the map is rejected.
    pub allowed: std::collections::HashMap<String, Subject>,
}

#[async_trait]
impl TokenVerifier for StaticVerifier {
    async fn verify(&self, bearer: &str) -> Result<Subject, AuthError> {
        self.allowed
            .get(bearer)
            .cloned()
            .ok_or_else(|| AuthError::Invalid("unknown bearer".into()))
    }
}

/// Extract the bearer token from an `Authorization: Bearer <token>` header.
#[must_use]
pub fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let h = headers.get(axum::http::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    s.strip_prefix("Bearer ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_verifier_accepts_anything() {
        let v = NullVerifier;
        let s = v.verify("anything").await.unwrap();
        assert_eq!(s.subject, "self-host");
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
        let mut v = StaticVerifier::default();
        v.allowed
            .insert("secret".into(), Subject::new("iss", "alice"));
        let s = v.verify("secret").await.unwrap();
        assert_eq!(s.subject, "alice");
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

    #[test]
    fn extract_bearer_works() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer xyz".parse().unwrap(),
        );
        assert_eq!(extract_bearer(&h), Some("xyz"));
        h.clear();
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Basic abc".parse().unwrap(),
        );
        assert_eq!(extract_bearer(&h), None);
    }
}
