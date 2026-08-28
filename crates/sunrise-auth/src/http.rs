//! The HTTP surface the login flow needs, and the seam that makes it testable.
//!
//! `oauth2` is brought in with `default-features = false`, so it carries no
//! HTTP client of its own; it hands out an [`oauth2::HttpRequest`] and expects
//! one back. That is exactly the shape this module fills — and it is why the
//! whole flow can be exercised against an in-memory issuer, the way
//! `sunrise-server`'s `FakeIdp` exercises the verifier.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::error::LoginError;

/// One HTTP exchange. `http::Request`/`Response` because that is the currency
/// `oauth2` speaks.
pub type Request = http::Request<Vec<u8>>;
/// The response half.
pub type Response = http::Response<Vec<u8>>;

/// A future carrying an HTTP exchange to completion.
///
/// `'static` rather than borrowed from `&self`, which costs an `Arc` clone per
/// call and buys the only shape `oauth2`'s `AsyncHttpClient` blanket impl
/// accepts: it requires the closure's return type to be one concrete type, and
/// a future borrowing the client is a different type for every borrow.
pub type CallFuture = Pin<Box<dyn Future<Output = Result<Response, LoginError>> + Send + 'static>>;

/// An HTTP client the login flow can drive.
///
/// Object-safe on purpose: the production client and a test issuer are held
/// behind the same `Arc<dyn HttpClient>`.
pub trait HttpClient: Send + Sync + std::fmt::Debug {
    /// Perform one request.
    fn call(&self, request: Request) -> CallFuture;
}

/// 1 MiB, matching the server's JWKS cap.
///
/// Discovery documents and token responses are a few kilobytes. The cap is
/// what stops a hostile or broken issuer from streaming the client out of
/// memory — the client is the more exposed side here, because the issuer URL
/// can come from a self-hoster's config.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Production client: hyper + rustls over the Mozilla root bundle, HTTPS only.
///
/// HTTPS-only is enforced here rather than left to the connector, and it is
/// load-bearing twice over: a discovery document fetched in plaintext is one an
/// on-path attacker can rewrite to point the authorization and token endpoints
/// wherever they like, and a token endpoint reached in plaintext hands them the
/// bearer directly.
#[derive(Clone)]
pub struct HttpsClient {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        String,
    >,
}

impl std::fmt::Debug for HttpsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsClient").finish_non_exhaustive()
    }
}

impl HttpsClient {
    /// Build a client over the webpki (Mozilla) root store.
    #[must_use]
    pub fn new() -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        Self {
            client: hyper_util::client::legacy::Client::builder(
                hyper_util::rt::TokioExecutor::new(),
            )
            .build(https),
        }
    }
}

impl Default for HttpsClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for HttpsClient {
    fn call(&self, request: Request) -> CallFuture {
        let client = self.client.clone();
        Box::pin(async move {
            use http_body_util::BodyExt;

            let uri = request.uri().to_string();
            if request.uri().scheme_str() != Some("https") {
                return Err(LoginError::Provider(format!(
                    "refusing to talk to an OIDC endpoint over a non-HTTPS URL: {uri}"
                )));
            }

            let (parts, body) = request.into_parts();
            let body = String::from_utf8(body)
                .map_err(|_| LoginError::Transport("request body is not UTF-8".into()))?;
            let req = hyper::Request::from_parts(parts, body);

            let res = client
                .request(req)
                .await
                .map_err(|e| LoginError::Transport(format!("{uri}: {e}")))?;

            let (parts, body) = res.into_parts();
            let collected = body
                .collect()
                .await
                .map_err(|e| LoginError::Transport(format!("read body of {uri}: {e}")))?;
            let bytes = collected.to_bytes();
            if bytes.len() > MAX_BODY_BYTES {
                return Err(LoginError::Transport(format!(
                    "response from {uri} exceeds {MAX_BODY_BYTES} bytes"
                )));
            }
            Ok(http::Response::from_parts(parts, bytes.to_vec()))
        })
    }
}

/// Adapt an [`HttpClient`] into the closure shape `oauth2` accepts.
///
/// `oauth2` blanket-implements its `AsyncHttpClient` for any
/// `Fn(HttpRequest) -> Future`, so no wrapper type is needed — just a closure
/// that borrows the client.
pub(crate) fn as_oauth2_client(http: Arc<dyn HttpClient>) -> impl Fn(Request) -> CallFuture {
    move |req| http.call(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plaintext_endpoints_are_refused_before_the_socket_opens() {
        let c = HttpsClient::new();
        let req = http::Request::builder()
            .uri("http://idp.example/.well-known/openid-configuration")
            .body(Vec::new())
            .unwrap();
        let err = c.call(req).await.expect_err("http:// must be refused");
        assert!(matches!(err, LoginError::Provider(_)), "{err:?}");
    }
}
