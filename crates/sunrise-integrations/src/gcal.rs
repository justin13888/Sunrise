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

// TODO(gcal): this subset is too narrow for a correct sync diff. Each missing
// field below is a bug the v0 prototype hit in practice:
//   - `updated`: the server-side change stamp. Without it a poll cycle cannot
//     distinguish a modified event from an unchanged one, so it either
//     republishes everything every cycle or nothing at all.
//   - `status`: Google reports deletions as `status: "cancelled"` rows within a
//     listing, not as absent ids.
//   - all-day events carry `start.date` / `end.date` (a bare civil date), not
//     `start.dateTime`. Decoding only `dateTime` silently drops every all-day
//     event.
//   - `recurring_event_id` / `original_start_time`: required to attach a single
//     modified instance back to its series.
//   - `html_link`: must be optional. One link-less event must not fail the
//     decode of the whole page.

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

// TODO(gcal): the flow stops at the authorization URL. Still missing:
//   - The code -> token exchange (`POST https://oauth2.googleapis.com/token`
//     carrying `code_verifier`) and the refresh call for an expired access
//     token.
//   - Storage rule: the *refresh* token is the durable credential. v0
//     deadlocked itself by reporting stored credentials as absent once the
//     access token neared expiry, which made refreshing impossible and forced a
//     re-login every hour. Expiry is a signal to refresh, never to forget.
//   - Persisting rotated credentials: Google may hand back a new refresh token
//     on refresh, and dropping it strands the account at the next expiry.

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

// TODO(gcal): `EventSyncer` covers list + insert only. Two-way sync also needs
// the following, each with a constraint that is not obvious from the API docs:
//   - `get_event(calendar_id, event_id)`.
//   - `patch_event(...)`: Google's `events.update` has *PUT* semantics — it
//     replaces the resource, so a partial body silently clears every field left
//     out of it. Updates must go through `events.patch`.
//   - `delete_event(...)`.
//   - `list_calendars()`, filtered to `accessRole` in {owner, writer}. A
//     read-only calendar must never be written to, and local edits against one
//     will fail at push time.
//   - `get_user_info()` for the account email / display name. Onboarding needs
//     it and it cannot be derived from the calendar scopes alone.

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

// TODO(gcal): no change-detection layer exists yet. When one lands, an id that
// is absent from a listing must NOT by itself be treated as a deletion — v0
// published phantom deletes from both of these:
//   - Window slide: the poll window's start advances each cycle, so an event
//     that merely ended falls out of the listing. Compare the remembered end
//     time against the new window start before emitting a delete.
//   - Page cap: if paging stops early on a page limit with a `nextPageToken`
//     still outstanding, the result set is incomplete and "missing" is
//     indistinguishable from "not fetched". Skip the deletion pass for that
//     cycle entirely.
// The first observation of a calendar must seed the snapshot silently, and
// snapshots for accounts that disconnect must be dropped so that a reconnect
// reseeds rather than replaying stale diffs.

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
