//! The HTTP surface the OIDC verifier fetches discovery + JWKS documents over.
//!
//! This is a trait rather than a direct `hyper` call for one reason: a verifier
//! that can only be exercised against a live IdP is a verifier that never gets
//! tested. [`HttpFetch`] lets a test hand the verifier a JWKS it minted itself
//! and count exactly how many times the verifier went to fetch it, which is the
//! only way to assert the caching contract in `docs/06-server/auth.md`.

use async_trait::async_trait;

use super::AuthError;

/// A fetched HTTP document.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body bytes.
    pub body: Vec<u8>,
    /// `Cache-Control` header verbatim, if any. The verifier reads `max-age`
    /// out of it to decide how long the document stays cached.
    pub cache_control: Option<String>,
}

impl HttpResponse {
    /// Build a `200 OK` response with no cache directives.
    #[must_use]
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            cache_control: None,
        }
    }

    /// Attach a `Cache-Control` header value.
    #[must_use]
    pub fn with_cache_control(mut self, value: impl Into<String>) -> Self {
        self.cache_control = Some(value.into());
        self
    }

    /// What the response's `Cache-Control` header asks for.
    ///
    /// `no-store` / `no-cache` win over `max-age`: an issuer that says "do not
    /// cache this" gets obeyed even when it also sends a lifetime. The
    /// three-way answer matters — "cache for zero seconds" and "said nothing"
    /// are different instructions, and collapsing them into `Option<u64>` would
    /// silently apply the default TTL to an issuer that explicitly refused
    /// caching.
    #[must_use]
    pub fn cache_policy(&self) -> CachePolicy {
        let Some(raw) = self.cache_control.as_deref() else {
            return CachePolicy::Unspecified;
        };
        let mut policy = CachePolicy::Unspecified;
        for directive in raw.split(',') {
            let lowered = directive.trim().to_ascii_lowercase();
            if lowered == "no-store" || lowered == "no-cache" {
                return CachePolicy::NoStore;
            }
            if let Some(v) = lowered.strip_prefix("max-age=") {
                if let Ok(secs) = v.trim().parse::<u64>() {
                    policy = CachePolicy::MaxAge(secs);
                }
            }
        }
        policy
    }
}

/// A response's caching instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    /// The issuer forbade caching.
    NoStore,
    /// The issuer named a lifetime, in seconds.
    MaxAge(u64),
    /// The issuer said nothing usable; the caller picks a default.
    Unspecified,
}

/// Injectable HTTP GET.
#[async_trait]
pub trait HttpFetch: Send + Sync + std::fmt::Debug {
    /// GET `url`, following no redirects.
    async fn get(&self, url: &str) -> Result<HttpResponse, AuthError>;
}

/// Production fetcher: hyper + rustls, Mozilla root bundle, HTTPS only.
///
/// HTTPS-only is deliberate. A JWKS fetched over plaintext is a JWKS an
/// on-path attacker can replace, and replacing it means minting arbitrary
/// tokens for every account on the server. `http://` is refused before a
/// connection is opened rather than being left to the connector.
#[derive(Clone)]
pub struct HttpsFetch {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        String,
    >,
    max_body_bytes: usize,
}

impl std::fmt::Debug for HttpsFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsFetch")
            .field("max_body_bytes", &self.max_body_bytes)
            .finish_non_exhaustive()
    }
}

/// 1 MiB. Real JWKS documents are a few kilobytes; the cap stops a hostile or
/// broken issuer from streaming the server out of memory.
const DEFAULT_MAX_DOC_BYTES: usize = 1024 * 1024;

impl HttpsFetch {
    /// Build a fetcher over the webpki (Mozilla) root store.
    #[must_use]
    pub fn new() -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(https);
        Self {
            client,
            max_body_bytes: DEFAULT_MAX_DOC_BYTES,
        }
    }
}

impl Default for HttpsFetch {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpFetch for HttpsFetch {
    async fn get(&self, url: &str) -> Result<HttpResponse, AuthError> {
        use http_body_util::BodyExt;

        if !url.starts_with("https://") {
            return Err(AuthError::Transport(format!(
                "refusing to fetch OIDC metadata over a non-HTTPS URL: {url}"
            )));
        }
        let req = hyper::Request::builder()
            .method(hyper::Method::GET)
            .uri(url)
            .header(hyper::header::ACCEPT, "application/json")
            .body(String::new())
            .map_err(|e| AuthError::Transport(format!("build request: {e}")))?;

        let res = self
            .client
            .request(req)
            .await
            .map_err(|e| AuthError::Transport(format!("GET {url}: {e}")))?;

        let status = res.status().as_u16();
        let cache_control = res
            .headers()
            .get(hyper::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let collected = res
            .into_body()
            .collect()
            .await
            .map_err(|e| AuthError::Transport(format!("read body of {url}: {e}")))?;
        let body = collected.to_bytes();
        if body.len() > self.max_body_bytes {
            return Err(AuthError::Transport(format!(
                "OIDC metadata at {url} exceeds {} bytes",
                self.max_body_bytes
            )));
        }

        Ok(HttpResponse {
            status,
            body: body.to_vec(),
            cache_control,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_age_is_read_out_of_cache_control() {
        let r = HttpResponse::ok("{}").with_cache_control("public, max-age=600");
        assert_eq!(r.cache_policy(), CachePolicy::MaxAge(600));
    }

    #[test]
    fn no_store_beats_max_age() {
        let r = HttpResponse::ok("{}").with_cache_control("max-age=600, no-store");
        assert_eq!(
            r.cache_policy(),
            CachePolicy::NoStore,
            "an issuer that says no-store must not be cached for ten minutes anyway"
        );
    }

    /// "Said nothing" must stay distinguishable from "said zero": only the
    /// former may be replaced by the configured default lifetime.
    #[test]
    fn absent_or_unparseable_cache_control_is_unspecified() {
        assert_eq!(
            HttpResponse::ok("{}").cache_policy(),
            CachePolicy::Unspecified
        );
        assert_eq!(
            HttpResponse::ok("{}")
                .with_cache_control("max-age=forever")
                .cache_policy(),
            CachePolicy::Unspecified
        );
        assert_eq!(
            HttpResponse::ok("{}")
                .with_cache_control("max-age=0")
                .cache_policy(),
            CachePolicy::MaxAge(0)
        );
    }

    #[tokio::test]
    async fn plaintext_urls_are_refused_before_dialling() {
        let f = HttpsFetch::new();
        let err = f
            .get("http://idp.example/.well-known/openid-configuration")
            .await
            .expect_err("a JWKS fetched over http:// is a JWKS an attacker can replace");
        assert!(matches!(err, AuthError::Transport(_)));
    }
}
