//! OIDC token validation interface.
//!
//! Production binds to a JWKS HTTP fetch + RS256 verify; v1 self-host
//! ships the `NullVerifier` (single-tenant: every request maps to the
//! same synthetic identity) and a `StaticJwksVerifier` test helper.
//! Adding a real JWKS-fetching impl is a follow-up that doesn't change
//! this trait surface.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Subject extracted from a verified token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subject {
    /// Stable account-id (synthetic in self-host mode; OIDC `sub` in prod).
    pub account_id: String,
    /// Email if present.
    pub email: Option<String>,
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
}

/// Self-host single-tenant verifier — every request maps to the same
/// synthetic subject. NEVER use in multi-tenant deployments.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullVerifier;

#[async_trait]
impl TokenVerifier for NullVerifier {
    async fn verify(&self, _bearer: &str) -> Result<Subject, AuthError> {
        Ok(Subject {
            account_id: "self-host".to_string(),
            email: None,
        })
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
        assert_eq!(s.account_id, "self-host");
    }

    #[tokio::test]
    async fn static_verifier_rejects_unknown() {
        let v = StaticVerifier::default();
        assert!(v.verify("nope").await.is_err());
    }

    #[tokio::test]
    async fn static_verifier_accepts_known() {
        let mut v = StaticVerifier::default();
        v.allowed.insert(
            "secret".into(),
            Subject {
                account_id: "alice".into(),
                email: Some("a@b".into()),
            },
        );
        let s = v.verify("secret").await.unwrap();
        assert_eq!(s.account_id, "alice");
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
