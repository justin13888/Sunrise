//! The OAuth flow and the stored credential: authorization-code + PKCE request
//! building, and the refresh rules a stored credential obeys.

use serde::{Deserialize, Serialize};

/// OAuth flow state. Authorization-code + PKCE, which is the correct shape for
/// an installed app: there is no client secret to protect, so the client id can
/// live in the binary.
#[derive(Debug, Clone)]
pub struct OAuthFlow {
    /// OAuth client id (a public string).
    pub client_id: String,
    /// Redirect URI the platform listens on. A loopback listener is the only
    /// workable choice for a desktop or terminal app.
    pub redirect_uri: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
}

/// Google's token endpoint.
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// A form-encoded request body the host should POST to [`TOKEN_ENDPOINT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRequest {
    /// Where to POST.
    pub url: &'static str,
    /// `application/x-www-form-urlencoded` body.
    pub body: String,
}

/// Google's token-endpoint response.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TokenResponse {
    /// Short-lived access token.
    pub access_token: String,
    /// Lifetime in seconds.
    pub expires_in: u64,
    /// Present on the initial exchange; **often absent on refresh**.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Space-separated granted scopes.
    #[serde(default)]
    pub scope: Option<String>,
}

impl OAuthFlow {
    /// Build the user-facing authorization URL.
    #[must_use]
    pub fn auth_url(&self, state: &str, code_challenge: &str) -> String {
        let scopes = self.scopes.join("%20");
        format!(
            "https://accounts.google.com/o/oauth2/v2/auth?\
             response_type=code&\
             client_id={}&\
             redirect_uri={}&\
             scope={scopes}&\
             code_challenge_method=S256&\
             code_challenge={code_challenge}&\
             state={state}&\
             access_type=offline&\
             prompt=consent",
            urlencode(&self.client_id),
            urlencode(&self.redirect_uri),
        )
    }

    /// Body for exchanging an authorization code for tokens.
    #[must_use]
    pub fn code_exchange_request(&self, code: &str, code_verifier: &str) -> TokenRequest {
        TokenRequest {
            url: TOKEN_ENDPOINT,
            body: format!(
                "grant_type=authorization_code&code={}&client_id={}&redirect_uri={}&code_verifier={}",
                urlencode(code),
                urlencode(&self.client_id),
                urlencode(&self.redirect_uri),
                urlencode(code_verifier),
            ),
        }
    }

    /// Body for refreshing an access token.
    #[must_use]
    pub fn refresh_request(&self, refresh_token: &str) -> TokenRequest {
        TokenRequest {
            url: TOKEN_ENDPOINT,
            body: format!(
                "grant_type=refresh_token&refresh_token={}&client_id={}",
                urlencode(refresh_token),
                urlencode(&self.client_id),
            ),
        }
    }
}

/// Stored Google credentials for one account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// Short-lived access token.
    pub access_token: String,
    /// The durable credential. Never discard this because the access token
    /// expired.
    pub refresh_token: String,
    /// Access-token expiry, unix ms.
    pub expires_at_ms: u64,
}

/// What a caller should do with a stored credential right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshState {
    /// Access token is good; use it.
    Usable,
    /// Access token is expired or close enough that it may expire in flight.
    /// Refresh first — the record is still perfectly valid.
    NeedsRefresh,
}

/// Refresh this long before nominal expiry, so a token cannot lapse between
/// the check and the request landing.
pub const REFRESH_SKEW_MS: u64 = 5 * 60 * 1000;

impl Credentials {
    /// Whether the access token can be used as-is.
    ///
    /// Note what this deliberately does **not** do: return "absent" or "invalid"
    /// near expiry. v0 nulled the whole record inside a 5-minute window, which
    /// removed the refresh token at exactly the moment it was needed and forced
    /// a re-login every hour. Expiry is a signal to refresh, not to forget.
    #[must_use]
    pub const fn refresh_state(&self, now_ms: u64) -> RefreshState {
        if self.expires_at_ms > now_ms.saturating_add(REFRESH_SKEW_MS) {
            RefreshState::Usable
        } else {
            RefreshState::NeedsRefresh
        }
    }

    /// Apply a token-endpoint response.
    ///
    /// Google returns a new refresh token only when it rotates one, and omits
    /// the field otherwise. Overwriting with `None` would strand the account at
    /// the next expiry, so an absent field keeps the existing token.
    pub fn apply_refresh(&mut self, res: &TokenResponse, now_ms: u64) {
        self.access_token.clone_from(&res.access_token);
        if let Some(new_refresh) = res.refresh_token.as_ref() {
            self.refresh_token.clone_from(new_refresh);
        }
        self.expires_at_ms = now_ms.saturating_add(res.expires_in.saturating_mul(1000));
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow() -> OAuthFlow {
        OAuthFlow {
            client_id: "abc.apps.googleusercontent.com".into(),
            redirect_uri: "http://127.0.0.1:9123/callback".into(),
            scopes: vec!["https://www.googleapis.com/auth/calendar.readonly".into()],
        }
    }

    #[test]
    fn auth_url_includes_required_params() {
        let url = flow().auth_url("state-xyz", "ch-abc");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=abc.apps.googleusercontent.com"));
        assert!(url.contains("code_challenge=ch-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-xyz"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9123%2Fcallback"));
        // Without these two, Google returns no refresh token at all and the
        // integration silently becomes one-hour-only.
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
    }

    #[test]
    fn code_exchange_carries_the_verifier() {
        let r = flow().code_exchange_request("the-code", "the-verifier");
        assert_eq!(r.url, TOKEN_ENDPOINT);
        assert!(r.body.contains("grant_type=authorization_code"));
        assert!(r.body.contains("code=the-code"));
        assert!(
            r.body.contains("code_verifier=the-verifier"),
            "PKCE without the verifier is just an authorization code"
        );
        assert!(
            !r.body.contains("client_secret"),
            "an installed app has no secret to send"
        );
    }

    /// The v0 auth deadlock, as a test.
    #[test]
    fn near_expiry_means_refresh_not_forget() {
        let c = Credentials {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at_ms: 1_000_000,
        };
        assert_eq!(c.refresh_state(0), RefreshState::Usable);
        // Inside the skew window.
        assert_eq!(
            c.refresh_state(1_000_000 - REFRESH_SKEW_MS + 1),
            RefreshState::NeedsRefresh
        );
        assert_eq!(c.refresh_state(2_000_000), RefreshState::NeedsRefresh);
        // The record survives either way — that is the whole point.
        assert_eq!(c.refresh_token, "rt");
    }

    #[test]
    fn refresh_without_a_new_token_keeps_the_old_one() {
        let mut c = Credentials {
            access_token: "old".into(),
            refresh_token: "durable".into(),
            expires_at_ms: 0,
        };
        c.apply_refresh(
            &TokenResponse {
                access_token: "new".into(),
                expires_in: 3600,
                refresh_token: None,
                scope: None,
            },
            10_000,
        );
        assert_eq!(c.access_token, "new");
        assert_eq!(
            c.refresh_token, "durable",
            "an omitted refresh_token must not erase the durable credential"
        );
        assert_eq!(c.expires_at_ms, 10_000 + 3_600_000);
    }

    #[test]
    fn refresh_with_a_rotated_token_takes_it() {
        let mut c = Credentials {
            access_token: "old".into(),
            refresh_token: "old-rt".into(),
            expires_at_ms: 0,
        };
        c.apply_refresh(
            &TokenResponse {
                access_token: "new".into(),
                expires_in: 60,
                refresh_token: Some("new-rt".into()),
                scope: None,
            },
            0,
        );
        assert_eq!(c.refresh_token, "new-rt");
    }
}
