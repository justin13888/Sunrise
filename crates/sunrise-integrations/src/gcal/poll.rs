//! The poll half: the host's read-only transport interface, and change
//! detection that diffs one poll against the previous snapshot.

use super::{CalendarEntry, EventStatus, GCalEvent};
use crate::IntegrationError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
/// Read-only by design: this imports from Google and never writes back, so there
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
    use super::super::EventTime;
    use super::*;

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
