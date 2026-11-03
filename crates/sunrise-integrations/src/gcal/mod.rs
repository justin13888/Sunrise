//! Google Calendar OAuth + read-only event import.
//!
//! Scope is **read-only import** (GitHub issue #4): events flow from
//! Google into Sunrise, never the other way. Nothing here writes to Google, so
//! there is no `events.patch` / `events.delete` surface and no Google-vs-CRDT
//! write conflict to resolve.
//!
//! The event types Google returns live here; the OAuth flow and stored
//! credential live in `oauth`, and the host's transport interface and change
//! detection live in `poll`. Both are re-exported, so every item is named
//! `gcal::Item`.
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

use serde::{Deserialize, Serialize};

mod oauth;
mod poll;

pub use oauth::{
    Credentials, OAuthFlow, RefreshState, TokenRequest, TokenResponse, REFRESH_SKEW_MS,
    TOKEN_ENDPOINT,
};
pub use poll::{diff, Change, EventPage, EventSyncer, PollResult, Snapshot, SnapshotEntry};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
