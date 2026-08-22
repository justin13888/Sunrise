//! JWKS-backed OIDC access-token verifier.
//!
//! Implements step 1–3 of `docs/06-server/auth.md` §per-request auth:
//!
//! 1. Fetch the issuer's discovery document, then its JWKS, caching both for
//!    the lifetime the discovery document's `Cache-Control` allows.
//! 2. Verify the signature against the JWK named by the token's `kid`, then
//!    `iss`, `aud`, `exp` and `nbf`.
//! 3. Hand back a [`Subject`] keyed on `(iss, sub)` for the account lookup.
//!
//! # Why the claim checks are ours and not the library's
//!
//! `jsonwebtoken` can validate `exp`/`nbf` itself, but it reads the wall clock
//! through `SystemTime::now`. This workspace takes time from an injected
//! [`Clock`] so that expiry is a property a test can *state* rather than
//! wait for. So the library does what only it can do — the signature — and the
//! temporal and identity claims are checked here against `Clock`.
//!
//! # Algorithm confusion
//!
//! The verifying algorithm is pinned to the JWK's own `alg` when the issuer
//! publishes one, and otherwise to the token header's `alg` filtered through
//! [`is_supported_alg`], which admits asymmetric algorithms only. A JWKS
//! containing a symmetric (`oct`) key is refused outright. Without both guards
//! an attacker who can read the JWKS — it is public by construction — could
//! sign an `HS256` token using the RSA public key as the HMAC secret.

use std::sync::Arc;

use async_trait::async_trait;
use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use parking_lot::Mutex;
use serde::Deserialize;

use super::http::{CachePolicy, HttpFetch, HttpResponse};
use super::{AuthError, Subject, TokenVerifier};
use crate::state::Clock;

/// The URI-namespaced device-id claim from `docs/06-server/auth.md`
/// §device-binding (RFC 7519 §4.2).
pub const DEVICE_ID_CLAIM: &str = "https://sunrise.app/device_id";

/// Fallback JWKS lifetime when the issuer publishes no `Cache-Control`.
const DEFAULT_JWKS_TTL_SECS: u64 = 300;

/// Floor on how often an unknown `kid` may trigger a JWKS refetch.
///
/// Key rotation is real: an issuer can start signing with a `kid` we have never
/// seen, and refusing until the TTL lapses would lock every user out for
/// minutes. But refetching on *every* unknown `kid` hands an unauthenticated
/// caller a way to drive one outbound request per forged token, so the refetch
/// is rate-limited rather than unconditional.
const MIN_REFETCH_INTERVAL_SECS: u64 = 60;

/// Configuration for [`OidcVerifier`].
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// Expected `iss`, and the base the discovery document is fetched from.
    pub issuer: String,
    /// Expected `aud` — the server's configured OIDC client id.
    pub audience: String,
    /// Clock skew tolerated on `exp` and `nbf`, in seconds.
    pub leeway_secs: u64,
    /// JWKS cache lifetime used when the issuer sends no `Cache-Control`.
    pub default_ttl_secs: u64,
}

impl OidcConfig {
    /// Construct with the documented defaults.
    #[must_use]
    pub fn new(issuer: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            leeway_secs: 60,
            default_ttl_secs: DEFAULT_JWKS_TTL_SECS,
        }
    }
}

/// The OIDC discovery document, reduced to the two fields we use.
#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

/// A JWKS held in memory with the instant it stops being trusted.
#[derive(Debug)]
struct CachedJwks {
    keys: JwkSet,
    expires_at_ms: u64,
    /// Last time an unknown `kid` forced a refetch, for rate limiting.
    last_forced_refetch_ms: u64,
}

/// Verifier that validates OIDC access tokens against the issuer's JWKS.
#[derive(Debug)]
pub struct OidcVerifier {
    config: OidcConfig,
    http: Arc<dyn HttpFetch>,
    clock: Arc<dyn Clock>,
    cache: Mutex<Option<CachedJwks>>,
}

impl OidcVerifier {
    /// Build a verifier over an injected HTTP surface and clock.
    #[must_use]
    pub fn new(config: OidcConfig, http: Arc<dyn HttpFetch>, clock: Arc<dyn Clock>) -> Self {
        Self {
            config,
            http,
            clock,
            cache: Mutex::new(None),
        }
    }

    /// Build the verifier a [`crate::ServerConfig`] describes, over the
    /// production HTTPS fetcher.
    ///
    /// `None` when no issuer is configured, which is the self-host case: the
    /// caller then leaves [`crate::NullVerifier`] in place and
    /// `ServerConfig::validate` holds it to loopback.
    #[must_use]
    pub fn from_server_config(
        cfg: &crate::config::ServerConfig,
        clock: Arc<dyn Clock>,
    ) -> Option<Self> {
        let issuer = cfg.oidc_issuer.clone()?;
        let audience = cfg.oidc_client_id.clone()?;
        Some(Self::new(
            OidcConfig {
                issuer,
                audience,
                leeway_secs: cfg.token_leeway_secs,
                default_ttl_secs: cfg.jwks_default_ttl_secs,
            },
            Arc::new(super::http::HttpsFetch::new()),
            clock,
        ))
    }

    /// The discovery URL for the configured issuer.
    #[must_use]
    pub fn discovery_url(&self) -> String {
        format!(
            "{}/.well-known/openid-configuration",
            self.config.issuer.trim_end_matches('/')
        )
    }

    /// Return a JWK for `kid`, fetching the JWKS if the cache is cold, stale,
    /// or does not know the key.
    async fn key_for(&self, kid: Option<&str>) -> Result<Jwk, AuthError> {
        let now_ms = self.clock.now_ms();

        // Fast path: a live cache that already holds the key.
        {
            let cache = self.cache.lock();
            if let Some(c) = cache.as_ref() {
                if now_ms < c.expires_at_ms {
                    if let Some(jwk) = find_key(&c.keys, kid) {
                        return Ok(jwk.clone());
                    }
                    // Live cache, unknown kid: rotation, or a forged token.
                    if now_ms.saturating_sub(c.last_forced_refetch_ms)
                        < MIN_REFETCH_INTERVAL_SECS * 1000
                    {
                        return Err(AuthError::Invalid(format!(
                            "no JWK matches kid {}",
                            kid.unwrap_or("<none>")
                        )));
                    }
                }
            }
        }

        let (keys, ttl_secs) = self.fetch_jwks().await?;
        let expires_at_ms = now_ms.saturating_add(ttl_secs.saturating_mul(1000));
        let found = find_key(&keys, kid).cloned();
        *self.cache.lock() = Some(CachedJwks {
            keys,
            expires_at_ms,
            last_forced_refetch_ms: now_ms,
        });

        found.ok_or_else(|| {
            AuthError::Invalid(format!("no JWK matches kid {}", kid.unwrap_or("<none>")))
        })
    }

    /// Fetch discovery + JWKS. Returns the key set and the lifetime to cache it
    /// for, which comes from the discovery document's `Cache-Control` per
    /// `docs/06-server/auth.md`, falling back to the JWKS response's own
    /// directive and then to the configured default.
    async fn fetch_jwks(&self) -> Result<(JwkSet, u64), AuthError> {
        let disco_res = self.http.get(&self.discovery_url()).await?;
        expect_ok(&disco_res, "discovery document")?;
        let disco: Discovery = serde_json::from_slice(&disco_res.body)
            .map_err(|e| AuthError::Transport(format!("discovery document is not JSON: {e}")))?;

        // A discovery document that names a different issuer than the one we
        // are configured for is either a misconfiguration or a redirect onto a
        // hostile IdP. Either way its `jwks_uri` must not be trusted.
        if disco.issuer.trim_end_matches('/') != self.config.issuer.trim_end_matches('/') {
            return Err(AuthError::Transport(format!(
                "discovery document declares issuer {:?}, expected {:?}",
                disco.issuer, self.config.issuer
            )));
        }

        let jwks_res = self.http.get(&disco.jwks_uri).await?;
        expect_ok(&jwks_res, "JWKS")?;
        let keys: JwkSet = serde_json::from_slice(&jwks_res.body)
            .map_err(|e| AuthError::Transport(format!("JWKS is not a valid key set: {e}")))?;

        let ttl = self.cache_ttl(disco_res.cache_policy(), jwks_res.cache_policy());
        Ok((keys, ttl))
    }

    /// Reconcile the discovery document's caching instruction with the JWKS
    /// response's own. The discovery document leads, per
    /// `docs/06-server/auth.md`, but a `no-store` from either end wins: caching
    /// a key set the issuer asked us not to keep is the failure that leaves a
    /// compromised key live after the issuer pulled it.
    fn cache_ttl(&self, discovery: CachePolicy, jwks: CachePolicy) -> u64 {
        if discovery == CachePolicy::NoStore || jwks == CachePolicy::NoStore {
            return 0;
        }
        match (discovery, jwks) {
            (CachePolicy::MaxAge(secs), _)
            | (CachePolicy::Unspecified, CachePolicy::MaxAge(secs)) => secs,
            _ => self.config.default_ttl_secs,
        }
    }
}

fn expect_ok(res: &HttpResponse, what: &str) -> Result<(), AuthError> {
    if res.status == 200 {
        Ok(())
    } else {
        Err(AuthError::Transport(format!(
            "{what} fetch returned HTTP {}",
            res.status
        )))
    }
}

/// Find the JWK a token's `kid` names.
///
/// A token with no `kid` is only resolvable when the issuer publishes exactly
/// one key; picking arbitrarily from a multi-key set would let an attacker
/// choose which key their token is checked against by omitting the header.
fn find_key<'a>(set: &'a JwkSet, kid: Option<&str>) -> Option<&'a Jwk> {
    match kid {
        Some(kid) => set.find(kid),
        None if set.keys.len() == 1 => set.keys.first(),
        None => None,
    }
}

/// Whether an algorithm is one we will verify a bearer token with.
///
/// Asymmetric only: `HS*` would treat the (public) JWKS material as a shared
/// secret, and `none` is not representable here at all.
#[must_use]
pub fn is_supported_alg(alg: Algorithm) -> bool {
    matches!(
        alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::PS256
            | Algorithm::PS384
            | Algorithm::PS512
            | Algorithm::ES256
            | Algorithm::ES384
            | Algorithm::EdDSA
    )
}

/// `aud` is either a string or an array of strings (RFC 7519 §4.1.3).
#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
    #[default]
    Absent,
}

impl Audience {
    fn contains(&self, want: &str) -> bool {
        match self {
            Self::One(a) => a == want,
            Self::Many(v) => v.iter().any(|a| a == want),
            Self::Absent => false,
        }
    }
}

/// The claims we read off a verified token.
#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    #[serde(default)]
    aud: Audience,
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default, rename = "https://sunrise.app/device_id")]
    device_id: Option<String>,
}

#[async_trait]
impl TokenVerifier for OidcVerifier {
    async fn verify(&self, bearer: &str) -> Result<Subject, AuthError> {
        if bearer.trim().is_empty() {
            return Err(AuthError::Missing);
        }
        let header =
            decode_header(bearer).map_err(|e| AuthError::Invalid(format!("header: {e}")))?;
        if !is_supported_alg(header.alg) {
            return Err(AuthError::Invalid(format!(
                "unsupported token algorithm {:?}",
                header.alg
            )));
        }

        let jwk = self.key_for(header.kid.as_deref()).await?;
        if matches!(jwk.algorithm, AlgorithmParameters::OctetKey(_)) {
            return Err(AuthError::Invalid(
                "JWKS contains a symmetric key; refusing to verify a bearer token with it".into(),
            ));
        }
        // Pin the algorithm to the key's own `alg` where the issuer declares
        // one, so the token header cannot select it.
        let alg = match jwk.common.key_algorithm.and_then(key_alg_to_alg) {
            Some(declared) => {
                if declared != header.alg {
                    return Err(AuthError::Invalid(format!(
                        "token alg {:?} does not match the JWK's declared alg {declared:?}",
                        header.alg
                    )));
                }
                declared
            }
            None => header.alg,
        };

        let key = DecodingKey::from_jwk(&jwk)
            .map_err(|e| AuthError::Invalid(format!("unusable JWK: {e}")))?;

        // The library verifies the signature; every claim below is checked here
        // against the injected clock.
        let mut validation = Validation::new(alg);
        validation.required_spec_claims.clear();
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.validate_aud = false;
        let data = decode::<Claims>(bearer, &key, &validation)
            .map_err(|e| AuthError::Invalid(format!("signature: {e}")))?;
        let claims = data.claims;

        if claims.iss.trim_end_matches('/') != self.config.issuer.trim_end_matches('/') {
            return Err(AuthError::Invalid(format!(
                "issuer {:?} is not this server's issuer",
                claims.iss
            )));
        }
        if !claims.aud.contains(&self.config.audience) {
            return Err(AuthError::Invalid(
                "token audience does not include this server's client id".into(),
            ));
        }
        if claims.sub.is_empty() {
            return Err(AuthError::Invalid("token has an empty sub".into()));
        }

        let now_secs = i64::try_from(self.clock.now_ms() / 1000).unwrap_or(i64::MAX);
        let leeway = i64::try_from(self.config.leeway_secs).unwrap_or(0);
        // `exp` is mandatory: a token with no expiry is a permanent credential.
        let exp = claims
            .exp
            .ok_or_else(|| AuthError::Invalid("token has no exp".into()))?;
        if now_secs - leeway >= exp {
            return Err(AuthError::Expired);
        }
        if let Some(nbf) = claims.nbf {
            if now_secs + leeway < nbf {
                return Err(AuthError::Invalid("token is not valid yet (nbf)".into()));
            }
        }

        Ok(Subject {
            issuer: claims.iss,
            subject: claims.sub,
            email: claims.email,
            device_id: claims.device_id,
        })
    }
}

/// Map a JWK `alg` value onto the signing algorithms we accept.
fn key_alg_to_alg(k: jsonwebtoken::jwk::KeyAlgorithm) -> Option<Algorithm> {
    use jsonwebtoken::jwk::KeyAlgorithm as K;
    Some(match k {
        K::RS256 => Algorithm::RS256,
        K::RS384 => Algorithm::RS384,
        K::RS512 => Algorithm::RS512,
        K::PS256 => Algorithm::PS256,
        K::PS384 => Algorithm::PS384,
        K::PS512 => Algorithm::PS512,
        K::ES256 => Algorithm::ES256,
        K::ES384 => Algorithm::ES384,
        K::EdDSA => Algorithm::EdDSA,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric_algorithms_are_not_verifiable() {
        for alg in [Algorithm::HS256, Algorithm::HS384, Algorithm::HS512] {
            assert!(
                !is_supported_alg(alg),
                "{alg:?} would treat the public JWKS as an HMAC secret"
            );
        }
        assert!(is_supported_alg(Algorithm::RS256));
        assert!(is_supported_alg(Algorithm::ES256));
    }

    #[test]
    fn audience_matches_string_and_array_forms() {
        let one: Audience = serde_json::from_str(r#""client-a""#).unwrap();
        assert!(one.contains("client-a"));
        assert!(!one.contains("client-b"));
        let many: Audience = serde_json::from_str(r#"["client-a","client-b"]"#).unwrap();
        assert!(many.contains("client-b"));
        assert!(!many.contains("client-c"));
        assert!(!Audience::Absent.contains("client-a"));
    }

    /// A token with no `kid` may only resolve when the issuer publishes exactly
    /// one key — otherwise the client picks its own verification key.
    #[test]
    fn kidless_tokens_do_not_resolve_against_a_multi_key_set() {
        let set: JwkSet = serde_json::from_str(
            r#"{"keys":[
                {"kty":"RSA","kid":"a","n":"AQAB","e":"AQAB"},
                {"kty":"RSA","kid":"b","n":"AQAB","e":"AQAB"}
            ]}"#,
        )
        .unwrap();
        assert!(find_key(&set, None).is_none());
        assert!(find_key(&set, Some("a")).is_some());
        assert!(find_key(&set, Some("zzz")).is_none());
    }
}
