//! Google Calendar OAuth + event sync.
//!
//! v1 ships an interface-only crate: the OAuth flow happens on-device
//! (so refresh tokens never reach the relay), and the HTTP client is
//! injected by the host. This module provides:
//!
//! - [`OAuthFlow`]: PKCE-style authorization-URL + code-exchange helpers.
//! - [`EventSyncer`]: trait for the per-platform HTTP client to implement.
//! - Mapping between Google Calendar `Event` JSON and `sunrise_domain::Block`.

use crate::IntegrationError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// One Google Calendar event (subset).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GCalEvent {
    /// Google id.
    pub id: String,
    /// Optional human title.
    pub summary: Option<String>,
    /// `start.dateTime` in RFC 3339.
    pub start: Option<String>,
    /// `end.dateTime` in RFC 3339.
    pub end: Option<String>,
    /// Optional description.
    pub description: Option<String>,
}

/// OAuth flow state. v1 uses authorization-code + PKCE. Tests substitute
/// a recorder; production binds to the platform's secure token store.
#[derive(Debug, Clone)]
pub struct OAuthFlow {
    /// OAuth client id (a public string; safe to commit to the binary
    /// since PKCE removes the need for a client secret on installed apps).
    pub client_id: String,
    /// Redirect URI the platform listens on.
    pub redirect_uri: String,
    /// Requested scopes.
    pub scopes: Vec<String>,
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

/// Per-platform HTTP-bound syncer interface.
#[async_trait]
pub trait EventSyncer: Send + Sync + std::fmt::Debug {
    /// List events in a calendar window.
    async fn list_events(
        &self,
        calendar_id: &str,
        window_start_rfc3339: &str,
        window_end_rfc3339: &str,
    ) -> Result<Vec<GCalEvent>, IntegrationError>;

    /// Insert one event.
    async fn insert_event(
        &self,
        calendar_id: &str,
        event: &GCalEvent,
    ) -> Result<GCalEvent, IntegrationError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_includes_required_params() {
        let f = OAuthFlow {
            client_id: "abc.apps.googleusercontent.com".into(),
            redirect_uri: "http://127.0.0.1:9123/callback".into(),
            scopes: vec!["https://www.googleapis.com/auth/calendar".into()],
        };
        let url = f.auth_url("state-xyz", "ch-abc");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=abc.apps.googleusercontent.com"));
        assert!(url.contains("code_challenge=ch-abc"));
        assert!(url.contains("state=state-xyz"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A9123%2Fcallback"));
    }

    #[test]
    fn event_serializes() {
        let e = GCalEvent {
            id: "evt1".into(),
            summary: Some("Hi".into()),
            start: Some("2026-03-01T14:00:00Z".into()),
            end: None,
            description: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: GCalEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
