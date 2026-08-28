//! The OIDC login flow against an in-memory issuer.
//!
//! Modelled on `sunrise-server/tests/oidc_verifier.rs`'s `FakeIdp`: the issuer
//! is a `HttpClient` impl, so discovery and the token exchange are exercised
//! for real without a network or a browser. The one leg that cannot be faked is
//! the human at the issuer's login page — so the tests drive the *loopback
//! socket* directly, which is what the browser would do anyway.
//!
//! The interesting assertions are the refusals. A login flow that only works
//! when everything is well-behaved is not a login flow, it is a demo.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sunrise_auth::http::{CallFuture, Request};
use sunrise_auth::{HttpClient, LoginError, OidcClient, ProviderMetadata};
use tokio::io::AsyncWriteExt;

const ISSUER: &str = "https://idp.example";
const CLIENT_ID: &str = "sunrise-relay";
const DEVICE_ID: &str = "DEV-123";
const NOW: u64 = 1_700_000_000_000;

/// What the fake issuer recorded about the token request it served.
#[derive(Debug, Default, Clone)]
struct TokenRequest {
    params: Vec<(String, String)>,
}

impl TokenRequest {
    fn get(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// An in-memory OIDC provider.
#[derive(Debug)]
struct FakeIdp {
    /// The `issuer` value the discovery document claims. Usually [`ISSUER`];
    /// a test overrides it to stage a mix-up.
    claimed_issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    /// JSON the token endpoint returns, or `None` to return `400`.
    token_response: Mutex<Option<String>>,
    /// The last token request, for assertions.
    last_token_request: Mutex<TokenRequest>,
}

impl FakeIdp {
    fn new() -> Self {
        Self {
            claimed_issuer: ISSUER.to_string(),
            authorization_endpoint: format!("{ISSUER}/authorize"),
            token_endpoint: format!("{ISSUER}/token"),
            token_response: Mutex::new(Some(
                serde_json::json!({
                    "access_token": "the-access-token",
                    "token_type": "bearer",
                    "expires_in": 3600,
                    "refresh_token": "the-refresh-token",
                })
                .to_string(),
            )),
            last_token_request: Mutex::new(TokenRequest::default()),
        }
    }

    fn discovery_json(&self) -> String {
        serde_json::json!({
            "issuer": self.claimed_issuer,
            "authorization_endpoint": self.authorization_endpoint,
            "token_endpoint": self.token_endpoint,
            "response_types_supported": ["code"],
        })
        .to_string()
    }
}

impl HttpClient for FakeIdp {
    fn call(&self, request: Request) -> CallFuture {
        let path = request.uri().path().to_string();
        let discovery = self.discovery_json();
        let token_response = self.token_response.lock().unwrap().clone();
        if path.ends_with("/.well-known/openid-configuration") {
            return Box::pin(async move {
                Ok(http::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(discovery.into_bytes())
                    .unwrap())
            });
        }
        if path.ends_with("/token") {
            let body = String::from_utf8_lossy(request.body()).into_owned();
            let params = url::form_urlencoded::parse(body.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            *self.last_token_request.lock().unwrap() = TokenRequest { params };
            return Box::pin(async move {
                Ok(match token_response {
                    Some(json) => http::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(json.into_bytes())
                        .unwrap(),
                    None => http::Response::builder()
                        .status(400)
                        .header("content-type", "application/json")
                        .body(
                            br#"{"error":"invalid_grant","error_description":"code is spent"}"#
                                .to_vec(),
                        )
                        .unwrap(),
                })
            });
        }
        Box::pin(async move {
            Ok(http::Response::builder()
                .status(404)
                .body(Vec::new())
                .unwrap())
        })
    }
}

fn client(idp: Arc<FakeIdp>) -> OidcClient {
    OidcClient::new(ISSUER, CLIENT_ID, idp as Arc<dyn HttpClient>)
}

/// Pretend to be the browser: hit the loopback redirect the session is
/// listening on, and return whatever it writes back.
async fn drive_browser(redirect_uri: &str, query: &str) {
    let url = url::Url::parse(redirect_uri).unwrap();
    let addr = format!("127.0.0.1:{}", url.port().unwrap());
    let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
    socket
        .write_all(format!("GET /callback?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
        .await
        .unwrap();
    socket.flush().await.unwrap();
}

fn query_param(url: &str, key: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discovery_reads_the_endpoints_a_login_needs() {
    let meta = client(Arc::new(FakeIdp::new())).discover().await.unwrap();
    assert_eq!(
        meta,
        ProviderMetadata {
            issuer: ISSUER.into(),
            authorization_endpoint: format!("{ISSUER}/authorize"),
            token_endpoint: format!("{ISSUER}/token"),
        }
    );
}

/// The issuer-mix-up defence. A document served from one issuer's well-known
/// path that claims to *be* another issuer is how a login gets redirected at a
/// token endpoint the operator never chose.
#[tokio::test]
async fn a_document_claiming_another_issuer_is_refused() {
    let mut idp = FakeIdp::new();
    idp.claimed_issuer = "https://evil.example".into();
    let err = client(Arc::new(idp)).discover().await.unwrap_err();
    assert!(matches!(err, LoginError::Provider(_)), "{err:?}");
}

/// A plaintext issuer is one an on-path attacker can rewrite wholesale.
#[tokio::test]
async fn a_plaintext_issuer_is_refused() {
    let c = OidcClient::new(
        "http://idp.example",
        CLIENT_ID,
        Arc::new(FakeIdp::new()) as Arc<dyn HttpClient>,
    );
    let err = c.discover().await.unwrap_err();
    assert!(matches!(err, LoginError::Provider(_)), "{err:?}");
}

/// And an HTTPS issuer that advertises a plaintext endpoint is the same attack
/// one level down — the document is authentic, the endpoint it names is not
/// protected.
#[tokio::test]
async fn an_https_issuer_advertising_a_plaintext_endpoint_is_refused() {
    for (auth, token) in [
        ("http://idp.example/authorize", "https://idp.example/token"),
        ("https://idp.example/authorize", "http://idp.example/token"),
    ] {
        let mut idp = FakeIdp::new();
        idp.authorization_endpoint = auth.into();
        idp.token_endpoint = token.into();
        let err = client(Arc::new(idp)).discover().await.unwrap_err();
        assert!(
            matches!(err, LoginError::Provider(_)),
            "{auth} / {token}: {err:?}"
        );
    }
}

#[tokio::test]
async fn a_document_that_does_not_parse_is_malformed_not_a_login() {
    #[derive(Debug)]
    struct Garbage;
    impl HttpClient for Garbage {
        fn call(&self, _r: Request) -> CallFuture {
            Box::pin(async {
                Ok(http::Response::builder()
                    .status(200)
                    .body(b"not json".to_vec())
                    .unwrap())
            })
        }
    }
    let c = OidcClient::new(ISSUER, CLIENT_ID, Arc::new(Garbage) as Arc<dyn HttpClient>);
    assert!(matches!(
        c.discover().await.unwrap_err(),
        LoginError::Malformed(_)
    ));
}

// ---------------------------------------------------------------------------
// The authorization request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_authorize_url_carries_pkce_state_and_the_device_id() {
    let c = client(Arc::new(FakeIdp::new()));
    let meta = c.discover().await.unwrap();
    let session = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    let url = session.authorize_url();

    assert_eq!(query_param(url, "response_type").as_deref(), Some("code"));
    assert_eq!(query_param(url, "client_id").as_deref(), Some(CLIENT_ID));
    assert_eq!(
        query_param(url, "code_challenge_method").as_deref(),
        Some("S256"),
        "plain PKCE is not PKCE"
    );
    assert!(query_param(url, "code_challenge").is_some());
    assert!(query_param(url, "state").is_some());
    assert_eq!(
        query_param(url, "sunrise_device_id").as_deref(),
        Some(DEVICE_ID)
    );
    let scope = query_param(url, "scope").unwrap();
    assert!(scope.contains("openid"), "{scope}");
    assert!(
        scope.contains("offline_access"),
        "without it there is no refresh token and every renewal needs the browser: {scope}"
    );

    // RFC 8252 §7.3: loopback, on a kernel-chosen port the flow already holds.
    let redirect = query_param(url, "redirect_uri").unwrap();
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
    assert_eq!(redirect, session.redirect_uri());
}

/// Two logins must not share a `state` or a PKCE challenge; reusing either
/// would make one login's redirect acceptable to the other.
#[tokio::test]
async fn every_login_gets_fresh_randomness() {
    let c = client(Arc::new(FakeIdp::new()));
    let meta = c.discover().await.unwrap();
    let a = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    let b = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    assert_ne!(
        query_param(a.authorize_url(), "state"),
        query_param(b.authorize_url(), "state")
    );
    assert_ne!(
        query_param(a.authorize_url(), "code_challenge"),
        query_param(b.authorize_url(), "code_challenge")
    );
    assert_ne!(a.redirect_uri(), b.redirect_uri());
}

// ---------------------------------------------------------------------------
// The full flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_complete_login_yields_credentials_bound_by_pkce() {
    let idp = Arc::new(FakeIdp::new());
    let c = client(Arc::clone(&idp));
    let meta = c.discover().await.unwrap();
    let session = c.begin_login(&meta, DEVICE_ID).await.unwrap();

    let challenge = query_param(session.authorize_url(), "code_challenge").unwrap();
    let state = query_param(session.authorize_url(), "state").unwrap();
    let redirect_uri = session.redirect_uri().to_string();

    let browser = tokio::spawn({
        let redirect_uri = redirect_uri.clone();
        async move { drive_browser(&redirect_uri, &format!("code=the-code&state={state}")).await }
    });
    let capture = session
        .wait_for_redirect(Duration::from_secs(5))
        .await
        .expect("the redirect must be accepted");
    browser.await.unwrap();

    let creds = c.exchange(&capture, NOW).await.expect("code exchange");
    assert_eq!(creds.access_token, "the-access-token");
    assert_eq!(creds.refresh_token.as_deref(), Some("the-refresh-token"));
    assert_eq!(creds.expires_at_ms, NOW + 3_600_000);
    assert_eq!(creds.renew_at_ms, NOW + 2_700_000);

    // The exchange is what PKCE binds. Recompute the challenge from the
    // verifier the client actually sent and check it is the one the
    // authorization request committed to — a code stolen from the redirect is
    // useless without it.
    let sent = idp.last_token_request.lock().unwrap().clone();
    assert_eq!(sent.get("grant_type"), Some("authorization_code"));
    assert_eq!(sent.get("code"), Some("the-code"));
    assert_eq!(sent.get("client_id"), Some(CLIENT_ID));
    assert_eq!(sent.get("redirect_uri"), Some(redirect_uri.as_str()));
    let verifier = sent.get("code_verifier").expect("a PKCE verifier was sent");
    let recomputed = oauth2::PkceCodeChallenge::from_code_verifier_sha256(
        &oauth2::PkceCodeVerifier::new(verifier.to_string()),
    );
    assert_eq!(
        recomputed.as_str(),
        challenge,
        "the verifier must hash to the challenge the authorize URL committed to"
    );
    // A public client ships no secret; PKCE is what stands in for one.
    assert_eq!(sent.get("client_secret"), None);
}

/// The state check, through the real socket rather than the unit-tested parser.
#[tokio::test]
async fn a_redirect_with_a_forged_state_is_refused() {
    let c = client(Arc::new(FakeIdp::new()));
    let meta = c.discover().await.unwrap();
    let session = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    let redirect_uri = session.redirect_uri().to_string();

    let browser =
        tokio::spawn(async move { drive_browser(&redirect_uri, "code=stolen&state=forged").await });
    let err = session
        .wait_for_redirect(Duration::from_secs(5))
        .await
        .unwrap_err();
    browser.await.unwrap();
    assert!(matches!(err, LoginError::StateMismatch), "{err:?}");
}

/// A browser fetching `/favicon.ico` before the redirect must not abort the
/// login. Anything on the machine can reach the loopback port.
#[tokio::test]
async fn stray_loopback_traffic_does_not_abort_the_login() {
    let c = client(Arc::new(FakeIdp::new()));
    let meta = c.discover().await.unwrap();
    let session = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    let state = query_param(session.authorize_url(), "state").unwrap();
    let redirect_uri = session.redirect_uri().to_string();

    let browser = tokio::spawn(async move {
        drive_browser(&redirect_uri, "").await;
        drive_browser(&redirect_uri, "unrelated=1").await;
        drive_browser(&redirect_uri, &format!("code=the-code&state={state}")).await;
    });
    let capture = session.wait_for_redirect(Duration::from_secs(5)).await;
    browser.await.unwrap();
    assert!(capture.is_ok(), "{capture:?}");
}

#[tokio::test]
async fn an_issuer_error_in_the_redirect_is_reported() {
    let c = client(Arc::new(FakeIdp::new()));
    let meta = c.discover().await.unwrap();
    let session = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    let state = query_param(session.authorize_url(), "state").unwrap();
    let redirect_uri = session.redirect_uri().to_string();

    let browser = tokio::spawn(async move {
        drive_browser(
            &redirect_uri,
            &format!("error=access_denied&error_description=nope&state={state}"),
        )
        .await;
    });
    let err = session
        .wait_for_redirect(Duration::from_secs(5))
        .await
        .unwrap_err();
    browser.await.unwrap();
    match err {
        LoginError::Rejected(m) => assert!(m.contains("access_denied"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_login_nobody_completes_times_out() {
    let c = client(Arc::new(FakeIdp::new()));
    let meta = c.discover().await.unwrap();
    let session = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    let err = session
        .wait_for_redirect(Duration::from_millis(150))
        .await
        .unwrap_err();
    assert!(matches!(err, LoginError::TimedOut), "{err:?}");
}

#[tokio::test]
async fn an_issuer_that_declines_the_exchange_surfaces_as_rejected() {
    let idp = Arc::new(FakeIdp::new());
    *idp.token_response.lock().unwrap() = None;
    let c = client(Arc::clone(&idp));
    let meta = c.discover().await.unwrap();
    let session = c.begin_login(&meta, DEVICE_ID).await.unwrap();
    let state = query_param(session.authorize_url(), "state").unwrap();
    let redirect_uri = session.redirect_uri().to_string();

    let browser = tokio::spawn(async move {
        drive_browser(&redirect_uri, &format!("code=the-code&state={state}")).await;
    });
    let capture = session
        .wait_for_redirect(Duration::from_secs(5))
        .await
        .unwrap();
    browser.await.unwrap();

    let err = c.exchange(&capture, NOW).await.unwrap_err();
    assert!(matches!(err, LoginError::Rejected(_)), "{err:?}");
}

// ---------------------------------------------------------------------------
// Renewal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refresh_yields_a_new_access_token() {
    let idp = Arc::new(FakeIdp::new());
    let c = client(Arc::clone(&idp));
    let meta = c.discover().await.unwrap();
    let creds = c.refresh(&meta, "the-refresh-token", NOW).await.unwrap();
    assert_eq!(creds.access_token, "the-access-token");

    let sent = idp.last_token_request.lock().unwrap().clone();
    assert_eq!(sent.get("grant_type"), Some("refresh_token"));
    assert_eq!(sent.get("refresh_token"), Some("the-refresh-token"));
}

/// Issuers that do not rotate refresh tokens omit one from the response.
/// Dropping the token we already hold would make every such renewal the last
/// one this client could ever perform, and the failure would only show an hour
/// later.
#[tokio::test]
async fn a_refresh_keeps_the_old_token_when_the_issuer_omits_one() {
    let idp = Arc::new(FakeIdp::new());
    *idp.token_response.lock().unwrap() = Some(
        serde_json::json!({
            "access_token": "renewed",
            "token_type": "bearer",
            "expires_in": 3600,
        })
        .to_string(),
    );
    let c = client(Arc::clone(&idp));
    let meta = c.discover().await.unwrap();
    let creds = c.refresh(&meta, "still-valid", NOW).await.unwrap();
    assert_eq!(creds.access_token, "renewed");
    assert_eq!(creds.refresh_token.as_deref(), Some("still-valid"));
}

/// A rotating issuer's new token replaces the old one — the mirror of the case
/// above, and the reason it cannot simply always keep the old one.
#[tokio::test]
async fn a_rotated_refresh_token_replaces_the_old_one() {
    let idp = Arc::new(FakeIdp::new());
    *idp.token_response.lock().unwrap() = Some(
        serde_json::json!({
            "access_token": "renewed",
            "token_type": "bearer",
            "expires_in": 3600,
            "refresh_token": "rotated",
        })
        .to_string(),
    );
    let c = client(Arc::clone(&idp));
    let meta = c.discover().await.unwrap();
    let creds = c.refresh(&meta, "old", NOW).await.unwrap();
    assert_eq!(creds.refresh_token.as_deref(), Some("rotated"));
}

/// A revoked or expired refresh token means the browser again, not a retry.
#[tokio::test]
async fn a_revoked_refresh_token_is_rejected() {
    let idp = Arc::new(FakeIdp::new());
    *idp.token_response.lock().unwrap() = None;
    let c = client(Arc::clone(&idp));
    let meta = c.discover().await.unwrap();
    let err = c.refresh(&meta, "revoked", NOW).await.unwrap_err();
    assert!(matches!(err, LoginError::Rejected(_)), "{err:?}");
}
