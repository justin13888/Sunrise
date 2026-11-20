//! OIDC login across the seam: [`SunriseLogin`], which drives one sign-in or
//! renewal from the foreign side, and the [`LoginCredentials`] it yields.

use std::sync::{Arc, Mutex};

use crate::BindingError;
#[cfg(doc)]
use crate::SunriseCore;

impl From<sunrise_auth::LoginError> for BindingError {
    fn from(e: sunrise_auth::LoginError) -> Self {
        match e {
            sunrise_auth::LoginError::Rejected(_) => Self::LoginRefused(e.to_string()),
            other => Self::Login(other.to_string()),
        }
    }
}

/// Credentials a completed OIDC login yielded.
///
/// `refresh_token` is `None` when the issuer did not return one — some do not
/// for public clients — in which case renewal means logging in again.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct LoginCredentials {
    /// The bearer to hand to [`SunriseCore::set_sync_credential`].
    pub access_token: String,
    /// Used to obtain a new access token without user interaction.
    pub refresh_token: Option<String>,
    /// When `access_token` stops being accepted, epoch milliseconds.
    pub expires_at_ms: u64,
    /// When to renew — 75% of the token's lifetime, not its expiry, so a
    /// renewal has room to fail and be retried before anything breaks.
    pub renew_at_ms: u64,
}

// Hand-written, like `sunrise_auth::Credentials`' own: this record carries a
// live bearer AND a refresh token, and a derived `Debug` would put both in the
// first log line anyone writes while debugging the seam.
impl std::fmt::Debug for LoginCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginCredentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .field("renew_at_ms", &self.renew_at_ms)
            .finish()
    }
}

impl From<sunrise_auth::Credentials> for LoginCredentials {
    fn from(c: sunrise_auth::Credentials) -> Self {
        Self {
            access_token: c.access_token,
            refresh_token: c.refresh_token,
            expires_at_ms: c.expires_at_ms,
            renew_at_ms: c.renew_at_ms,
        }
    }
}

/// One OIDC login, driven from the foreign side.
///
/// Two calls, because a browser sits between them:
///
/// 1. [`SunriseLogin::begin`] returns the authorize URL to open, and parks a
///    loopback listener for the redirect.
/// 2. [`SunriseLogin::complete`] waits for that redirect and exchanges the code.
///
/// Storing the result is the caller's job, and on macOS it should be the OS
/// Keychain — reachable from Swift directly, which is why there is no Rust
/// keychain binding here. Hand the access token to
/// [`SunriseCore::set_sync_credential`]; a live session picks it up in band.
#[derive(uniffi::Object)]
pub struct SunriseLogin {
    client: sunrise_auth::OidcClient,
    /// The pending session between `begin` and `complete`.
    ///
    /// `wait_for_redirect` consumes the session, so it is taken out rather than
    /// borrowed — which also makes a second `complete` fail cleanly instead of
    /// waiting on a listener that is already gone.
    session: Mutex<Option<sunrise_auth::LoginSession>>,
}

// Hand-written: `OidcClient` holds the client id and the HTTP stack, and the
// pending session holds a PKCE verifier. Only whether a login is in flight is
// safe to print.
impl std::fmt::Debug for SunriseLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pending = self.session.lock().map(|g| g.is_some()).unwrap_or(false);
        f.debug_struct("SunriseLogin")
            .field("login_in_progress", &pending)
            .finish_non_exhaustive()
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseLogin {
    /// A login against `issuer` as `client_id`.
    #[uniffi::constructor]
    #[must_use]
    pub fn new(issuer: String, client_id: String) -> Arc<Self> {
        let http = Arc::new(sunrise_auth::HttpsClient::new()) as Arc<dyn sunrise_auth::HttpClient>;
        Arc::new(Self {
            client: sunrise_auth::OidcClient::new(issuer, client_id, http),
            session: Mutex::new(None),
        })
    }

    /// Discover the provider and start a login. Returns the URL to open.
    ///
    /// `device_id` binds the token to this device: the issuer stamps it into a
    /// `device_id` claim and the relay refuses a token whose claim names a
    /// different device, so a token lifted off this machine is useless
    /// elsewhere. Get it from [`SunriseCore::device_id`].
    pub async fn begin(&self, device_id: String) -> Result<String, BindingError> {
        let metadata = self.client.discover().await?;
        let session = self.client.begin_login(&metadata, &device_id).await?;
        let url = session.authorize_url().to_string();
        if let Ok(mut g) = self.session.lock() {
            *g = Some(session);
        }
        Ok(url)
    }

    /// Start a login that makes the user authenticate again now, whatever
    /// session the browser holds, and return the URL to open.
    ///
    /// The OIDC step-up the recovery blob route requires
    /// (`max_age=0` and `prompt=login`): it refuses a token whose `auth_time`
    /// is older than a few minutes, and an ordinary [`Self::begin`] against a
    /// browser with a live session returns exactly that. Finish it with
    /// [`Self::complete`] and hand the access token to
    /// [`SunriseCore::recover_account`].
    ///
    /// `device_id` is empty for a recovery: the device it would name does not
    /// exist until the blob has been opened.
    pub async fn begin_step_up(&self, device_id: String) -> Result<String, BindingError> {
        let metadata = self.client.discover().await?;
        let session = self
            .client
            .begin_step_up_login(&metadata, &device_id)
            .await?;
        let url = session.authorize_url().to_string();
        if let Ok(mut g) = self.session.lock() {
            *g = Some(session);
        }
        Ok(url)
    }

    /// Wait for the browser redirect, then exchange the code for tokens.
    ///
    /// Fails if [`SunriseLogin::begin`] has not run, or has already been
    /// completed.
    pub async fn complete(
        &self,
        timeout_ms: u64,
        now_ms: u64,
    ) -> Result<LoginCredentials, BindingError> {
        // Taken, not borrowed, and the guard is dropped before the await:
        // `wait_for_redirect` consumes the session, and holding a std guard
        // across an await point would not be `Send`.
        let session = self
            .session
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .ok_or_else(|| BindingError::Login("no login is in progress".into()))?;
        let capture = session
            .wait_for_redirect(std::time::Duration::from_millis(timeout_ms))
            .await?;
        Ok(self.client.exchange(&capture, now_ms).await?.into())
    }

    /// Exchange a refresh token for a fresh access token, without user
    /// interaction. Drive it from [`LoginCredentials::renew_at_ms`].
    ///
    /// # Errors
    /// [`BindingError::LoginRefused`] when the issuer refused the refresh
    /// token, which it will do again: drop it. [`BindingError::Login`] for
    /// everything else, an unreachable issuer included, which says nothing
    /// about the token.
    pub async fn refresh(
        &self,
        refresh_token: String,
        now_ms: u64,
    ) -> Result<LoginCredentials, BindingError> {
        let metadata = self.client.discover().await?;
        Ok(self
            .client
            .refresh(&metadata, &refresh_token, now_ms)
            .await?
            .into())
    }
}

#[cfg(test)]
mod tests {
    use crate::BindingError;
    use sunrise_auth::LoginError;

    /// The one variant a client drops a refresh token on crosses the seam as
    /// its own, and nothing else does: an unreachable issuer arriving as a
    /// refusal is what would turn every offline launch into a browser sign-in.
    #[test]
    fn only_an_issuer_refusal_crosses_as_login_refused() {
        let refused = BindingError::from(LoginError::Rejected("refresh: invalid_grant".into()));
        assert!(
            matches!(&refused, BindingError::LoginRefused(m) if m.contains("invalid_grant")),
            "{refused:?}"
        );

        for other in [
            LoginError::Transport("connection refused".into()),
            LoginError::Malformed("not json".into()),
            LoginError::Provider("no token_endpoint".into()),
            LoginError::TimedOut,
        ] {
            let crossed = BindingError::from(other);
            assert!(matches!(crossed, BindingError::Login(_)), "{crossed:?}");
        }
    }

    /// The split changes what a client branches on, not what it shows.
    #[test]
    fn a_refusal_reads_the_same_as_any_other_login_failure() {
        let refused = BindingError::from(LoginError::Rejected("refresh: invalid_grant".into()));
        assert_eq!(
            refused.to_string(),
            "login: issuer declined: refresh: invalid_grant"
        );
    }
}
