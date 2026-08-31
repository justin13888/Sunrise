//! Google Calendar OAuth + read-only event import.
//!
//! Scope for v1 is **read-only import** (GitHub issue #4): events flow from
//! Google into Sunrise, never the other way. Nothing here writes to Google, so
//! there is no `events.patch` / `events.delete` surface and no Google-vs-CRDT
//! write conflict to resolve.
//!
//! # Why there is no HTTP client here
//!
//! The OAuth flow runs **on-device** so refresh tokens never reach the relay
//! (`docs/06-server/overview.md` lists third-party integrations as an explicit
//! server non-responsibility). This module therefore *builds requests and
//! parses responses* and leaves the transport to the host, which keeps it
//! dependency-free, deterministic, and testable without a network.
//!
//! # The parts that are easy to get wrong
//!
//! Each of these cost the v0 prototype a real bug, and each has a test:
//!
//! - **The refresh token is the durable credential.** v0 treated stored
//!   credentials as absent once the *access* token neared expiry, which made
//!   refreshing impossible and forced a re-login every hour. Expiry is a
//!   signal to refresh, never to forget. See [`Credentials::refresh_state`].
//! - **Google rotates refresh tokens** and omits the field when it does not.
//!   Overwriting with `None` strands the account at the next expiry. See
//!   [`Credentials::apply_refresh`].
//! - **Deletions arrive as `status: "cancelled"` rows**, not as absent ids.
//! - **All-day events carry `start.date`**, a bare civil date, not
//!   `start.dateTime`. Decoding only `dateTime` silently drops every one.
//! - **An absent id is not a deletion.** The poll window slides forward, so an
//!   event that merely ended falls out of the listing; and a truncated page
//!   run cannot distinguish "missing" from "not fetched". See [`diff`].

use crate::IntegrationError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Google's status for an event within a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EventStatus {
    /// Normal.
    #[default]
    Confirmed,
    /// Tentatively scheduled.
    Tentative,
    /// **Deleted.** Google reports removals this way inside a listing rather
    /// than by omitting the id, which is why the diff must read it.
    Cancelled,
}

/// One end of an event's time range.
///
/// Google sends either `dateTime` (a timed event) or `date` (an all-day event,
/// a bare civil date). Modelling both is not optional: an implementation that
/// reads only `dateTime` drops every all-day event without erroring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventTime {
    /// RFC 3339 instant, for a timed event.
    #[serde(rename = "dateTime", skip_serializing_if = "Option::is_none", default)]
    pub date_time: Option<String>,
    /// `YYYY-MM-DD`, for an all-day event.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub date: Option<String>,
    /// IANA zone, when Google supplies one.
    #[serde(rename = "timeZone", skip_serializing_if = "Option::is_none", default)]
    pub time_zone: Option<String>,
}

impl EventTime {
    /// The comparable instant-or-date string, whichever form Google used.
    ///
    /// Both forms sort correctly against each other lexicographically for the
    /// purpose the diff needs (is this before the window start?), because an
    /// RFC 3339 instant and a `YYYY-MM-DD` date share a prefix ordering.
    #[must_use]
    pub fn marker(&self) -> Option<&str> {
        self.date_time.as_deref().or(self.date.as_deref())
    }

    /// Whether this is an all-day event.
    #[must_use]
    pub const fn is_all_day(&self) -> bool {
        self.date_time.is_none() && self.date.is_some()
    }
}

/// One Google Calendar event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GCalEvent {
    /// Google id.
    pub id: String,
    /// Optional human title.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    /// Start.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub start: Option<EventTime>,
    /// End.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end: Option<EventTime>,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,
    /// Server-side change stamp. Without it a poll cannot tell a modified
    /// event from an unchanged one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub updated: Option<String>,
    /// Confirmed / tentative / cancelled.
    #[serde(default)]
    pub status: EventStatus,
    /// Set on a single modified instance of a recurring series.
    #[serde(
        rename = "recurringEventId",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub recurring_event_id: Option<String>,
    /// The instance's original slot in its series.
    #[serde(
        rename = "originalStartTime",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub original_start_time: Option<EventTime>,
    /// Web link. **Optional**: some events have none, and treating it as
    /// required fails the decode of an entire page over one row.
    #[serde(rename = "htmlLink", skip_serializing_if = "Option::is_none", default)]
    pub html_link: Option<String>,
}

/// Access role Google reports for a calendar in the user's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccessRole {
    /// Can see free/busy only.
    FreeBusyReader,
    /// Can read events.
    Reader,
    /// Can modify events.
    Writer,
    /// Owns the calendar.
    Owner,
}

impl AccessRole {
    /// Whether this role can read event details.
    ///
    /// `freeBusyReader` cannot: its events come back without summaries, so
    /// importing them would produce a calendar of untitled blocks.
    #[must_use]
    pub const fn can_read_details(self) -> bool {
        matches!(self, Self::Reader | Self::Writer | Self::Owner)
    }
}

/// One entry from the user's calendar list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEntry {
    /// Calendar id.
    pub id: String,
    /// Display name.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,
    /// The caller's role on this calendar.
    #[serde(rename = "accessRole")]
    pub access_role: AccessRole,
    /// Whether the user has this calendar selected in the Google UI.
    #[serde(default)]
    pub selected: bool,
}

// ---------------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Transport interface
// ---------------------------------------------------------------------------

/// One page of a listing.
#[derive(Debug, Clone, Default)]
pub struct EventPage {
    /// Events on this page.
    pub items: Vec<GCalEvent>,
    /// Continuation token, if Google has more.
    pub next_page_token: Option<String>,
}

/// Per-platform HTTP-bound read interface.
///
/// Read-only by design: v1 imports from Google and never writes back, so there
/// is deliberately no insert/patch/delete here.
#[async_trait]
pub trait EventSyncer: Send + Sync + std::fmt::Debug {
    /// The user's calendars.
    async fn list_calendars(&self) -> Result<Vec<CalendarEntry>, IntegrationError>;

    /// One page of events in a window.
    async fn list_events(
        &self,
        calendar_id: &str,
        window_start_rfc3339: &str,
        window_end_rfc3339: &str,
        page_token: Option<&str>,
    ) -> Result<EventPage, IntegrationError>;

    /// A single event.
    async fn get_event(
        &self,
        calendar_id: &str,
        event_id: &str,
    ) -> Result<GCalEvent, IntegrationError>;
}

// ---------------------------------------------------------------------------
// Change detection
// ---------------------------------------------------------------------------

/// What the previous poll saw for one event: its change stamp, and where it
/// ended so a window slide can be told apart from a deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// Last seen `updated` stamp.
    pub updated: String,
    /// Last seen end marker (instant or civil date).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end: Option<String>,
}

/// Per-calendar snapshot from the previous poll.
pub type Snapshot = BTreeMap<String, SnapshotEntry>;

/// A change the importer should apply locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// New to us.
    Created(Box<GCalEvent>),
    /// Changed since we last saw it.
    Updated(Box<GCalEvent>),
    /// Genuinely gone.
    Deleted(String),
}

/// One page-run's worth of listing results.
#[derive(Debug, Clone)]
pub struct PollResult {
    /// Every event fetched across all pages.
    pub events: Vec<GCalEvent>,
    /// True when paging stopped on a cap while Google still had more.
    pub truncated: bool,
}

/// Diff a poll against the previous snapshot.
///
/// Returns the changes to apply and the snapshot to persist.
///
/// The subtle half is deletion. An id absent from the listing is **not**
/// evidence of a deletion, for two independent reasons, and v0 published
/// phantom deletes from both:
///
/// 1. **Window slide.** The poll window's start advances every cycle, so an
///    event that merely ended drops out. If its remembered end is at or before
///    the new window start, it left the window rather than the calendar.
/// 2. **Truncation.** If paging stopped on a cap with more pages outstanding,
///    the result set is incomplete and "missing" is indistinguishable from "not
///    fetched". The deletion pass is skipped entirely for that cycle.
///
/// A genuine deletion arrives as `status: "cancelled"` and is reported
/// regardless of either rule.
///
/// The first observation of a calendar (`previous` empty) seeds silently: with
/// no prior state, every event would otherwise look new.
#[must_use]
pub fn diff(
    previous: &Snapshot,
    poll: &PollResult,
    window_start_rfc3339: &str,
) -> (Vec<Change>, Snapshot) {
    let mut changes = Vec::new();
    let mut next: Snapshot = Snapshot::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();

    for ev in &poll.events {
        if ev.id.is_empty() {
            continue;
        }
        seen.insert(ev.id.as_str());

        if ev.status == EventStatus::Cancelled {
            // An explicit tombstone. Report it only if we knew the event —
            // otherwise it is a deletion of something we never imported.
            if previous.contains_key(&ev.id) {
                changes.push(Change::Deleted(ev.id.clone()));
            }
            continue; // deliberately not carried into the next snapshot
        }

        let updated = ev.updated.clone().unwrap_or_default();
        let end = ev.end.as_ref().and_then(|e| e.marker()).map(str::to_string);
        next.insert(
            ev.id.clone(),
            SnapshotEntry {
                updated: updated.clone(),
                end,
            },
        );

        if previous.is_empty() {
            continue; // seeding
        }
        match previous.get(&ev.id) {
            None => changes.push(Change::Created(Box::new(ev.clone()))),
            Some(prev) if prev.updated != updated => {
                changes.push(Change::Updated(Box::new(ev.clone())));
            }
            Some(_) => {}
        }
    }

    if previous.is_empty() {
        return (changes, next);
    }

    // With an incomplete listing, absence proves nothing.
    if poll.truncated {
        // Carry forward what we did not re-observe, so a later complete cycle
        // can still diff against it.
        for (id, entry) in previous {
            if !seen.contains(id.as_str()) {
                next.insert(id.clone(), entry.clone());
            }
        }
        return (changes, next);
    }

    for (id, entry) in previous {
        if seen.contains(id.as_str()) {
            continue;
        }
        // Ended at or before the new window start: it slid out, it was not
        // deleted. Drop it from the snapshot without reporting a change.
        if entry
            .end
            .as_deref()
            .is_some_and(|e| e <= window_start_rfc3339)
        {
            continue;
        }
        changes.push(Change::Deleted(id.clone()));
    }

    (changes, next)
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

    #[test]
    fn all_day_events_decode() {
        let json = r#"{"id":"e1","summary":"Holiday","start":{"date":"2026-08-22"},
                       "end":{"date":"2026-08-23"},"updated":"2026-08-01T00:00:00Z"}"#;
        let ev: GCalEvent = serde_json::from_str(json).expect("all-day event must decode");
        assert!(ev.start.as_ref().unwrap().is_all_day());
        assert_eq!(ev.start.unwrap().marker(), Some("2026-08-22"));
    }

    #[test]
    fn a_link_less_event_does_not_fail_the_page() {
        let json = r#"{"id":"e1","summary":"No link"}"#;
        let ev: GCalEvent = serde_json::from_str(json).expect("htmlLink must be optional");
        assert!(ev.html_link.is_none());
        assert_eq!(ev.status, EventStatus::Confirmed, "status defaults");
    }

    #[test]
    fn series_instance_fields_decode() {
        let json = r#"{"id":"e1_20260822","recurringEventId":"e1",
                       "originalStartTime":{"dateTime":"2026-08-22T09:00:00Z"}}"#;
        let ev: GCalEvent = serde_json::from_str(json).unwrap();
        assert_eq!(ev.recurring_event_id.as_deref(), Some("e1"));
        assert!(ev.original_start_time.is_some());
    }

    #[test]
    fn free_busy_reader_calendars_are_not_readable() {
        assert!(!AccessRole::FreeBusyReader.can_read_details());
        for r in [AccessRole::Reader, AccessRole::Writer, AccessRole::Owner] {
            assert!(r.can_read_details());
        }
    }

    fn ev(id: &str, updated: &str, end: &str) -> GCalEvent {
        GCalEvent {
            id: id.into(),
            updated: Some(updated.into()),
            end: Some(EventTime {
                date_time: Some(end.into()),
                date: None,
                time_zone: None,
            }),
            ..Default::default()
        }
    }

    fn poll(events: Vec<GCalEvent>, truncated: bool) -> PollResult {
        PollResult { events, truncated }
    }

    #[test]
    fn first_observation_seeds_silently() {
        let (changes, snap) = diff(
            &Snapshot::new(),
            &poll(vec![ev("a", "u1", "2026-09-01T00:00:00Z")], false),
            "2026-08-22T00:00:00Z",
        );
        assert!(changes.is_empty(), "seeding must not report every event");
        assert_eq!(snap.len(), 1);
    }

    #[test]
    fn created_and_updated_are_detected() {
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(vec![ev("a", "u1", "2026-09-01T00:00:00Z")], false),
            "2026-08-22T00:00:00Z",
        );
        let (changes, _) = diff(
            &base,
            &poll(
                vec![
                    ev("a", "u2", "2026-09-01T00:00:00Z"),
                    ev("b", "u1", "2026-09-02T00:00:00Z"),
                ],
                false,
            ),
            "2026-08-22T00:00:00Z",
        );
        assert!(changes
            .iter()
            .any(|c| matches!(c, Change::Updated(e) if e.id == "a")));
        assert!(changes
            .iter()
            .any(|c| matches!(c, Change::Created(e) if e.id == "b")));
    }

    #[test]
    fn unchanged_events_produce_nothing() {
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(vec![ev("a", "u1", "2026-09-01T00:00:00Z")], false),
            "2026-08-22T00:00:00Z",
        );
        let (changes, _) = diff(
            &base,
            &poll(vec![ev("a", "u1", "2026-09-01T00:00:00Z")], false),
            "2026-08-22T00:00:00Z",
        );
        assert!(changes.is_empty());
    }

    /// Phantom-delete #1: the event ended, so the sliding window no longer
    /// covers it. It is gone from the listing but not from the calendar.
    #[test]
    fn an_event_that_slid_out_of_the_window_is_not_a_deletion() {
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(vec![ev("past", "u1", "2026-08-21T10:00:00Z")], false),
            "2026-08-21T00:00:00Z",
        );
        let (changes, snap) = diff(&base, &poll(vec![], false), "2026-08-22T00:00:00Z");
        assert!(
            changes.is_empty(),
            "an event whose end precedes the new window start slid out; got {changes:?}"
        );
        assert!(!snap.contains_key("past"), "and it leaves the snapshot");
    }

    /// Phantom-delete #2: paging hit a cap, so absence proves nothing.
    #[test]
    fn a_truncated_listing_suppresses_the_deletion_pass() {
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(
                vec![
                    ev("a", "u1", "2026-09-01T00:00:00Z"),
                    ev("b", "u1", "2026-09-02T00:00:00Z"),
                ],
                false,
            ),
            "2026-08-22T00:00:00Z",
        );
        let (changes, snap) = diff(
            &base,
            &poll(vec![ev("a", "u1", "2026-09-01T00:00:00Z")], true),
            "2026-08-22T00:00:00Z",
        );
        assert!(
            !changes.iter().any(|c| matches!(c, Change::Deleted(_))),
            "a truncated page run cannot distinguish missing from not-fetched"
        );
        assert!(
            snap.contains_key("b"),
            "unobserved entries must carry forward so a later complete cycle can diff them"
        );
    }

    /// A future-dated event that vanishes from a complete listing really is
    /// gone — the suppression rules must not swallow real deletions.
    #[test]
    fn a_real_deletion_is_still_reported() {
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(vec![ev("a", "u1", "2026-12-01T00:00:00Z")], false),
            "2026-08-22T00:00:00Z",
        );
        let (changes, _) = diff(&base, &poll(vec![], false), "2026-08-22T00:00:00Z");
        assert_eq!(changes, vec![Change::Deleted("a".into())]);
    }

    /// Google's actual deletion signal.
    #[test]
    fn a_cancelled_row_is_a_deletion() {
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(vec![ev("a", "u1", "2026-12-01T00:00:00Z")], false),
            "2026-08-22T00:00:00Z",
        );
        let mut cancelled = ev("a", "u2", "2026-12-01T00:00:00Z");
        cancelled.status = EventStatus::Cancelled;
        let (changes, snap) = diff(&base, &poll(vec![cancelled], false), "2026-08-22T00:00:00Z");
        assert_eq!(changes, vec![Change::Deleted("a".into())]);
        assert!(
            !snap.contains_key("a"),
            "a tombstone must leave the snapshot"
        );
    }

    /// A cancellation for something we never imported is not a change.
    #[test]
    fn a_cancelled_row_we_never_saw_is_ignored() {
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(vec![ev("a", "u1", "2026-12-01T00:00:00Z")], false),
            "2026-08-22T00:00:00Z",
        );
        let mut cancelled = ev("unknown", "u1", "2026-12-01T00:00:00Z");
        cancelled.status = EventStatus::Cancelled;
        let (changes, _) = diff(
            &base,
            &poll(
                vec![ev("a", "u1", "2026-12-01T00:00:00Z"), cancelled],
                false,
            ),
            "2026-08-22T00:00:00Z",
        );
        assert!(changes.is_empty(), "got {changes:?}");
    }

    /// All-day events have a bare date as their end marker; the slide rule must
    /// still work on them rather than treating them as never-expiring.
    #[test]
    fn all_day_events_participate_in_the_slide_rule() {
        let mut all_day = GCalEvent {
            id: "holiday".into(),
            updated: Some("u1".into()),
            ..Default::default()
        };
        all_day.end = Some(EventTime {
            date_time: None,
            date: Some("2026-08-21".into()),
            time_zone: None,
        });
        let (_, base) = diff(
            &Snapshot::new(),
            &poll(vec![all_day], false),
            "2026-08-20T00:00:00Z",
        );
        assert_eq!(
            base.get("holiday").unwrap().end.as_deref(),
            Some("2026-08-21")
        );
        let (changes, _) = diff(&base, &poll(vec![], false), "2026-08-22T00:00:00Z");
        assert!(
            changes.is_empty(),
            "an all-day event that already ended slid out; got {changes:?}"
        );
    }
}
