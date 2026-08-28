//! The authorization-code + PKCE login flow, Matrix-client style.
//!
//! discovery → browser → loopback redirect → token exchange, per RFC 8252
//! ("OAuth 2.0 for Native Apps"). Sunrise is a public client: it ships no
//! client secret, because a secret embedded in a distributed binary is not a
//! secret. PKCE is what replaces it, and it is the reason a vetted crate does
//! this rather than hand-written code.
//!
//! # What is being defended against
//!
//! - **A stolen authorization code.** PKCE S256: the code is worthless without
//!   the verifier, which never leaves this process.
//! - **A response to an authorization request this process never made.** The
//!   `state` parameter, compared before the code is looked at. A mismatch is
//!   [`LoginError::StateMismatch`] and the code is discarded unexchanged.
//! - **An issuer mix-up.** The discovery document must claim the issuer that
//!   was configured; see [`crate::discovery`].
//! - **An on-path rewrite.** Every issuer endpoint must be `https://`, enforced
//!   in [`crate::http`] before a socket is opened.
//!
//! # Why the redirect is a loopback socket
//!
//! RFC 8252 §7.3. A native app cannot register a globally unique `https://`
//! redirect, and a custom URI scheme (`sunrise://`) can be claimed by any other
//! app on the machine — which is a code interception. A loopback listener on an
//! **ephemeral** port cannot be squatted: the port is chosen by the kernel at
//! `begin` time and the flow's own socket already holds it.
//!
//! Loopback is the one place plaintext `http://` is correct, and the RFC says
//! so: the bytes never leave the host.

use std::sync::Arc;
use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::credentials::Credentials;
use crate::discovery::{discover, ProviderMetadata};
use crate::error::LoginError;
use crate::http::{as_oauth2_client, HttpClient};

/// The claim the relay reads to bind a token to a device
/// (`docs/06-server/auth.md`, `auth::oidc::DEVICE_ID_CLAIM`).
pub const DEVICE_ID_CLAIM: &str = "https://sunrise.app/device_id";

/// The authorization-request parameter carrying the device id.
///
/// The issuer has to be configured to map this onto the
/// [`DEVICE_ID_CLAIM`] claim; nothing here can make it do so. Sending it is
/// still unconditionally right: an issuer that ignores it mints a token with no
/// device claim, which the relay accepts unless `require_device_sig` is on —
/// exactly the pre-existing behaviour.
pub const DEVICE_ID_PARAM: &str = "sunrise_device_id";

/// Scopes requested. `openid` is what makes this OIDC rather than bare OAuth 2;
/// `offline_access` is what asks for a refresh token, without which every
/// renewal would need the browser again.
const SCOPES: [&str; 2] = ["openid", "offline_access"];

/// How long the browser leg may take before the flow gives up.
pub const DEFAULT_REDIRECT_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest redirect request line accepted, in bytes.
///
/// A browser's `GET /callback?...` is a few hundred bytes. The cap is what
/// stops anything that can reach loopback from streaming the flow out of
/// memory while it waits.
const MAX_REDIRECT_REQUEST: usize = 8 * 1024;

/// A configured relying party.
#[derive(Debug, Clone)]
pub struct OidcClient {
    issuer: String,
    client_id: String,
    http: Arc<dyn HttpClient>,
}

impl OidcClient {
    /// A client for `issuer`, registered as `client_id`.
    #[must_use]
    pub fn new(
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        http: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            client_id: client_id.into(),
            http,
        }
    }

    /// Fetch and validate the issuer's metadata.
    ///
    /// # Errors
    /// See [`crate::discovery::discover`].
    pub async fn discover(&self) -> Result<ProviderMetadata, LoginError> {
        discover(&self.http, &self.issuer).await
    }

    /// Start a login: bind the loopback redirect, mint PKCE and `state`, and
    /// build the URL the browser must visit.
    ///
    /// The socket is bound **before** the URL is built, because the URL has to
    /// name the port. Holding it for the life of the session is also what makes
    /// the redirect URI unsquattable.
    ///
    /// `device_id` rides along as [`DEVICE_ID_PARAM`] so an issuer configured
    /// for Sunrise can mint the [`DEVICE_ID_CLAIM`] claim.
    ///
    /// # Errors
    /// [`LoginError::Redirect`] if loopback cannot be bound;
    /// [`LoginError::Provider`] if an advertised endpoint is not a valid URL.
    pub async fn begin_login(
        &self,
        metadata: &ProviderMetadata,
        device_id: &str,
    ) -> Result<LoginSession, LoginError> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| LoginError::Redirect(format!("bind loopback: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| LoginError::Redirect(format!("local addr: {e}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let client = self.oauth_client(metadata, &redirect_uri)?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut req = client
            .authorize_url(CsrfToken::new_random)
            .set_pkce_challenge(challenge);
        for scope in SCOPES {
            req = req.add_scope(Scope::new(scope.to_string()));
        }
        if !device_id.is_empty() {
            req = req.add_extra_param(DEVICE_ID_PARAM, device_id);
        }
        let (authorize_url, csrf) = req.url();

        Ok(LoginSession {
            listener,
            authorize_url: authorize_url.to_string(),
            redirect_uri,
            csrf,
            verifier,
            metadata: metadata.clone(),
        })
    }

    /// Exchange the code a completed [`LoginSession`] captured.
    ///
    /// # Errors
    /// [`LoginError::Rejected`] if the issuer declines the exchange;
    /// [`LoginError::Transport`] or [`LoginError::Malformed`] otherwise.
    pub async fn exchange(
        &self,
        redirect: &RedirectCapture,
        now_ms: u64,
    ) -> Result<Credentials, LoginError> {
        let client = self.oauth_client(&redirect.metadata, &redirect.redirect_uri)?;
        let http = as_oauth2_client(Arc::clone(&self.http));
        let token = client
            .exchange_code(AuthorizationCode::new(redirect.code.clone()))
            .set_pkce_verifier(PkceCodeVerifier::new(redirect.verifier.clone()))
            .request_async(&http)
            .await
            .map_err(|e| LoginError::Rejected(format!("code exchange: {e}")))?;
        Ok(Self::to_credentials(&token, now_ms))
    }

    /// Trade a refresh token for a fresh access token.
    ///
    /// # Errors
    /// [`LoginError::Rejected`] if the issuer declines — which includes a
    /// refresh token that has been revoked or has expired, and means the user
    /// must go through the browser again.
    pub async fn refresh(
        &self,
        metadata: &ProviderMetadata,
        refresh_token: &str,
        now_ms: u64,
    ) -> Result<Credentials, LoginError> {
        // No redirect URI: the refresh grant has no browser leg.
        let client = self.oauth_client_inner(metadata, None)?;
        let http = as_oauth2_client(Arc::clone(&self.http));
        let token = client
            .exchange_refresh_token(&oauth2::RefreshToken::new(refresh_token.to_string()))
            .request_async(&http)
            .await
            .map_err(|e| LoginError::Rejected(format!("refresh: {e}")))?;

        let mut creds = Self::to_credentials(&token, now_ms);
        // Issuers that do not rotate refresh tokens omit it from the response.
        // Dropping the one we already hold would turn a successful renewal into
        // the last one this client could ever perform.
        if creds.refresh_token.is_none() {
            creds.refresh_token = Some(refresh_token.to_string());
        }
        Ok(creds)
    }

    fn to_credentials(
        token: &oauth2::StandardTokenResponse<
            oauth2::EmptyExtraTokenFields,
            oauth2::basic::BasicTokenType,
        >,
        now_ms: u64,
    ) -> Credentials {
        Credentials::new(
            token.access_token().secret().clone(),
            token.refresh_token().map(|t| t.secret().clone()),
            token.expires_in().map(|d| d.as_secs()),
            now_ms,
        )
    }

    fn oauth_client(
        &self,
        metadata: &ProviderMetadata,
        redirect_uri: &str,
    ) -> Result<ConfiguredClient, LoginError> {
        self.oauth_client_inner(metadata, Some(redirect_uri))
    }

    fn oauth_client_inner(
        &self,
        metadata: &ProviderMetadata,
        redirect_uri: Option<&str>,
    ) -> Result<ConfiguredClient, LoginError> {
        let auth = AuthUrl::new(metadata.authorization_endpoint.clone())
            .map_err(|e| LoginError::Provider(format!("authorization_endpoint: {e}")))?;
        let token = TokenUrl::new(metadata.token_endpoint.clone())
            .map_err(|e| LoginError::Provider(format!("token_endpoint: {e}")))?;
        // No client secret: Sunrise is a public client (RFC 8252 §8.5). PKCE is
        // what stands in for one.
        let client = BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_auth_uri(auth)
            .set_token_uri(token);
        match redirect_uri {
            Some(uri) => Ok(client.set_redirect_uri(
                RedirectUrl::new(uri.to_string())
                    .map_err(|e| LoginError::Provider(format!("redirect_uri: {e}")))?,
            )),
            None => Ok(client.set_redirect_uri(
                // Never sent: `exchange_refresh_token` does not include a
                // redirect URI. A placeholder keeps one client type.
                RedirectUrl::new("http://127.0.0.1/unused".to_string())
                    .map_err(|e| LoginError::Provider(format!("redirect_uri: {e}")))?,
            )),
        }
    }
}

type ConfiguredClient = oauth2::Client<
    oauth2::basic::BasicErrorResponse,
    oauth2::basic::BasicTokenResponse,
    oauth2::basic::BasicTokenIntrospectionResponse,
    oauth2::StandardRevocableToken,
    oauth2::basic::BasicRevocationErrorResponse,
    oauth2::EndpointSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointNotSet,
    oauth2::EndpointSet,
>;

/// A login in flight: the URL to visit, and the socket the answer comes back on.
#[derive(Debug)]
pub struct LoginSession {
    listener: TcpListener,
    authorize_url: String,
    redirect_uri: String,
    csrf: CsrfToken,
    verifier: PkceCodeVerifier,
    metadata: ProviderMetadata,
}

/// A validated authorization code, ready to exchange.
#[derive(Clone)]
pub struct RedirectCapture {
    code: String,
    verifier: String,
    redirect_uri: String,
    metadata: ProviderMetadata,
}

impl std::fmt::Debug for RedirectCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedirectCapture")
            .field("code", &"<redacted>")
            .field("verifier", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("metadata", &self.metadata)
            .finish()
    }
}

impl LoginSession {
    /// The URL the user must open.
    #[must_use]
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// The loopback redirect URI this session is listening on.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Wait for the browser to come back, validate `state`, and return the code.
    ///
    /// Consumes the session: an authorization code is single-use, and so is the
    /// `state` that authenticates it.
    ///
    /// # Errors
    /// [`LoginError::StateMismatch`] if the redirect does not carry this
    /// request's `state`; [`LoginError::Rejected`] if the issuer sent an
    /// `error`; [`LoginError::TimedOut`] if the user never finished;
    /// [`LoginError::Redirect`] on a socket failure.
    pub async fn wait_for_redirect(self, timeout: Duration) -> Result<RedirectCapture, LoginError> {
        let code = tokio::time::timeout(timeout, self.accept_redirect())
            .await
            .map_err(|_| LoginError::TimedOut)??;
        Ok(RedirectCapture {
            code,
            verifier: self.verifier.secret().clone(),
            redirect_uri: self.redirect_uri,
            metadata: self.metadata,
        })
    }

    async fn accept_redirect(&self) -> Result<String, LoginError> {
        loop {
            let (mut socket, _peer) = self
                .listener
                .accept()
                .await
                .map_err(|e| LoginError::Redirect(format!("accept: {e}")))?;

            let Some(target) = read_request_target(&mut socket).await? else {
                continue;
            };
            match parse_redirect(&target, self.csrf.secret()) {
                // A browser fetching `/favicon.ico`, or a probe. Answer and
                // keep waiting rather than failing the login on noise that
                // anything on the machine can generate.
                Ok(None) => {
                    let _ = respond(&mut socket, "Waiting for the sign-in redirect…").await;
                }
                Ok(Some(code)) => {
                    let _ = respond(
                        &mut socket,
                        "Signed in. You can close this tab and return to Sunrise.",
                    )
                    .await;
                    return Ok(code);
                }
                Err(e) => {
                    let _ = respond(&mut socket, "Sign-in failed. Return to Sunrise.").await;
                    return Err(e);
                }
            }
        }
    }
}

/// Read the request line and return its target, e.g. `/callback?code=…`.
///
/// Reads only as far as the end of the request line: nothing after it is used,
/// and reading the rest would mean either trusting `Content-Length` or waiting
/// for a close.
async fn read_request_target(
    socket: &mut tokio::net::TcpStream,
) -> Result<Option<String>, LoginError> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = socket
            .read(&mut chunk)
            .await
            .map_err(|e| LoginError::Redirect(format!("read: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(eol) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..eol]).into_owned();
            // `GET /callback?... HTTP/1.1`
            let mut parts = line.split(' ');
            let (Some(_method), Some(target)) = (parts.next(), parts.next()) else {
                return Ok(None);
            };
            return Ok(Some(target.to_string()));
        }
        if buf.len() > MAX_REDIRECT_REQUEST {
            return Err(LoginError::Redirect(
                "redirect request line exceeded the size cap".into(),
            ));
        }
    }
}

/// Validate one redirect target.
///
/// `Ok(None)` means "not the redirect" — a favicon fetch or a stray probe.
/// `Err` means the redirect was for us and was bad, which ends the login.
///
/// **`state` is compared before `code` is read.** That ordering is the point of
/// the parameter: a redirect carrying someone else's authorization code must be
/// discarded without ever being exchanged.
fn parse_redirect(target: &str, expected_state: &str) -> Result<Option<String>, LoginError> {
    let url = url::Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|e| LoginError::Redirect(format!("unparseable redirect target: {e}")))?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => error_description = Some(v.into_owned()),
            _ => {}
        }
    }
    if code.is_none() && error.is_none() {
        return Ok(None);
    }
    // Constant-time is not required: `expected_state` is a fresh 128-bit random
    // value per login, so there is no secret to walk out one byte at a time
    // within its lifetime.
    if state.as_deref() != Some(expected_state) {
        return Err(LoginError::StateMismatch);
    }
    if let Some(e) = error {
        return Err(LoginError::Rejected(match error_description {
            Some(d) => format!("{e}: {d}"),
            None => e,
        }));
    }
    code.map(Some)
        .ok_or_else(|| LoginError::Malformed("redirect carried neither a code nor an error".into()))
}

/// Answer the browser with a minimal page.
async fn respond(socket: &mut tokio::net::TcpStream, message: &str) -> std::io::Result<()> {
    // The message is one of this module's own constants, never user or issuer
    // input, so there is nothing here to escape.
    let body = format!("<!doctype html><meta charset=\"utf-8\"><title>Sunrise</title><p>{message}");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.flush().await
}

/// Open `url` in the user's default browser.
///
/// # Errors
/// [`LoginError::Browser`] if the platform's opener could not be spawned. The
/// caller should fall back to printing the URL — a login that cannot launch a
/// browser is still completable by hand, and refusing to continue would strand
/// anyone on a headless box or over SSH.
pub fn open_in_browser(url: &str) -> Result<(), LoginError> {
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    };
    std::process::Command::new(program)
        .args(args)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| LoginError::Browser(format!("{program}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE: &str = "the-expected-state";

    #[test]
    fn a_matching_redirect_yields_its_code() {
        let got = parse_redirect(&format!("/callback?code=abc123&state={STATE}"), STATE).unwrap();
        assert_eq!(got.as_deref(), Some("abc123"));
    }

    /// The whole reason `state` exists. A redirect carrying a valid-looking
    /// code but the wrong state is a response to an authorization request this
    /// process never made — the code must be discarded, not exchanged.
    #[test]
    fn a_code_with_the_wrong_state_is_refused() {
        let err = parse_redirect("/callback?code=abc123&state=attacker", STATE).unwrap_err();
        assert!(matches!(err, LoginError::StateMismatch), "{err:?}");
    }

    /// And a code with *no* state at all, which is the same attack without the
    /// effort.
    #[test]
    fn a_code_with_no_state_is_refused() {
        let err = parse_redirect("/callback?code=abc123", STATE).unwrap_err();
        assert!(matches!(err, LoginError::StateMismatch), "{err:?}");
    }

    /// An `error` redirect is checked for `state` too, before it is believed.
    /// Otherwise anything that can reach loopback can abort a login in flight.
    #[test]
    fn an_error_with_the_wrong_state_is_a_state_mismatch_not_a_rejection() {
        let err =
            parse_redirect("/callback?error=access_denied&state=attacker", STATE).unwrap_err();
        assert!(matches!(err, LoginError::StateMismatch), "{err:?}");
    }

    #[test]
    fn a_declined_login_surfaces_the_issuers_reason() {
        let err = parse_redirect(
            &format!("/callback?error=access_denied&error_description=User+said+no&state={STATE}"),
            STATE,
        )
        .unwrap_err();
        match err {
            LoginError::Rejected(m) => {
                assert!(
                    m.contains("access_denied") && m.contains("User said no"),
                    "{m}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A browser fetching `/favicon.ico` must not fail the login. Anything on
    /// the machine can hit the loopback port; only a request that actually
    /// looks like the redirect is treated as one.
    #[test]
    fn unrelated_requests_are_ignored_rather_than_fatal() {
        assert_eq!(parse_redirect("/favicon.ico", STATE).unwrap(), None);
        assert_eq!(parse_redirect("/", STATE).unwrap(), None);
        assert_eq!(parse_redirect("/callback", STATE).unwrap(), None);
    }

    /// Percent-encoded values are decoded before comparison — otherwise a
    /// legitimate redirect whose state was encoded by the browser would read as
    /// an attack.
    #[test]
    fn percent_encoded_parameters_are_decoded() {
        let got = parse_redirect("/callback?code=a%2Fb&state=with%20space", "with space").unwrap();
        assert_eq!(got.as_deref(), Some("a/b"));
    }
}
