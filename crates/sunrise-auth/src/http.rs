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
use std::time::Duration;

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
/// can come from a self-hoster's config. It bounds what is *read*: the body is
/// taken a frame at a time and abandoned as soon as it passes the cap, so an
/// issuer that never stops sending is refused after about this many bytes.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 30 s for one whole exchange: connecting, the TLS handshake, the response
/// head and every byte of the body.
///
/// A discovery document or a token response arrives in well under a second
/// from a working issuer, and 30 s still leaves a slow self-hosted one on a
/// poor link room to answer. What it stops is an issuer that accepts the
/// connection and then says nothing, or trickles the body a byte at a time,
/// which would otherwise hold `sunrise login` open for as long as the issuer
/// liked. One deadline over the whole exchange rather than one per phase,
/// because a per-read timeout is what a trickle defeats.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
            if request.uri().scheme_str() != Some("https") {
                return Err(LoginError::Provider(format!(
                    "refusing to talk to an OIDC endpoint over a non-HTTPS URL: {}",
                    request.uri()
                )));
            }
            exchange(&client, request, REQUEST_TIMEOUT, MAX_BODY_BYTES).await
        })
    }
}

/// Send `request` on `client` and read the response, the whole of it within
/// `timeout` and its body no longer than `cap`.
///
/// Generic over the connector so a test can drive it over plain TCP against a
/// local server; [`HttpsClient::call`] is the only production caller, and it
/// refuses a non-HTTPS URL before this runs.
async fn exchange<C>(
    client: &hyper_util::client::legacy::Client<C, String>,
    request: Request,
    timeout: Duration,
    cap: usize,
) -> Result<Response, LoginError>
where
    C: hyper_util::client::legacy::connect::Connect + Clone + Send + Sync + 'static,
{
    let uri = request.uri().to_string();
    let (parts, body) = request.into_parts();
    let body = String::from_utf8(body)
        .map_err(|_| LoginError::Transport("request body is not UTF-8".into()))?;
    let req = hyper::Request::from_parts(parts, body);

    let whole = async {
        let res = client
            .request(req)
            .await
            .map_err(|e| LoginError::Transport(format!("{uri}: {e}")))?;
        let (parts, body) = res.into_parts();
        let bytes = read_capped(body, cap, &uri).await?;
        Ok(http::Response::from_parts(parts, bytes))
    };
    tokio::time::timeout(timeout, whole).await.map_err(|_| {
        LoginError::Transport(format!("{uri}: no complete response within {timeout:?}"))
    })?
}

/// Read `body` to its end, a frame at a time, and stop as soon as it would
/// pass `cap` bytes: at most `cap` bytes are ever held, and nothing after the
/// frame that crossed it is read.
async fn read_capped<B>(mut body: B, cap: usize, uri: &str) -> Result<Vec<u8>, LoginError>
where
    B: hyper::body::Body<Data = hyper::body::Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    use http_body_util::BodyExt;

    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| LoginError::Transport(format!("read body of {uri}: {e}")))?;
        let Ok(data) = frame.into_data() else {
            continue; // Trailers carry nothing the flow reads.
        };
        if data.len() > cap - out.len() {
            return Err(LoginError::Transport(format!(
                "response from {uri} exceeds {cap} bytes"
            )));
        }
        out.extend_from_slice(&data);
    }
    Ok(out)
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
mod tests;
