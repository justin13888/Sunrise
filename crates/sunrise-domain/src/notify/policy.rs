//! Device policy: the lead-time hierarchy and quiet hours.

use jiff::civil;
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};

/// Default lead time for a Block reminder: 15 minutes before it starts, per
/// `docs/08-features/notifications.md` §Lead times.
pub const BLOCK_DEFAULT_LEAD_S: u32 = 15 * 60;

/// How far a queued notification may be pushed past its original time before
/// it is dropped instead: 4 hours, per the spec's quiet-hours rule.
pub const QUIET_HOURS_QUEUE_CAP_S: i64 = 4 * 60 * 60;

/// What a device does with a notification that lands inside quiet hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuietHoursPolicy {
    /// Fire at the next minute outside quiet hours, capped at
    /// [`QUIET_HOURS_QUEUE_CAP_S`]; still quiet at the cap means drop.
    #[default]
    Queue,
    /// Drop it outright.
    Drop,
}

/// A device-local quiet window. `start == end` is an empty window, not a
/// 24-hour one: a user who set both to the same time has configured nothing,
/// and silencing them permanently is the worse reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// Local wall-clock start.
    pub start: civil::Time,
    /// Local wall-clock end. May be earlier than `start`, which wraps midnight.
    pub end: civil::Time,
    /// What to do with a notification inside the window.
    #[serde(default)]
    pub policy: QuietHoursPolicy,
}

impl QuietHours {
    /// Is this wall-clock time inside the window?
    #[must_use]
    pub fn contains(&self, t: civil::Time) -> bool {
        if self.start == self.end {
            return false;
        }
        if self.start < self.end {
            t >= self.start && t < self.end
        } else {
            // Wraps midnight: 22:00..07:00 is "at or after 22:00, or before 07:00".
            t >= self.start || t < self.end
        }
    }
}

/// The per-device half of notification settings.
///
/// Deliberately **not** replicated state. The spec configures quiet hours per
/// device and makes exactly one device primary, so these travel in with the
/// query rather than living on an entity that would sync them to devices they
/// do not describe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReminderSettings {
    /// The device's global lead time in seconds — the floor of the hierarchy.
    pub default_lead_s: u32,
    /// Quiet window; `None` means never quiet.
    pub quiet_hours: Option<QuietHours>,
    /// Whether this device is the account's primary reminder device.
    ///
    /// A non-primary device plans **nothing**: the spec's dedup rule is "the
    /// primary handles reminders; other devices stay silent". The fallback
    /// when the primary goes quiet is a server-side decision (it is the only
    /// party that can see a device stop checking in), so a client that has
    /// been told to take over passes `true` here.
    pub is_primary_device: bool,
}

impl Default for ReminderSettings {
    fn default() -> Self {
        Self {
            // Spec default: fire at the scheduled time.
            default_lead_s: 0,
            quiet_hours: None,
            is_primary_device: true,
        }
    }
}

/// Resolve the lead-time hierarchy: per-entity, then per-Stream, then the
/// device's global default. First non-`None` wins
/// (`docs/08-features/notifications.md` §Lead-time hierarchy).
///
/// `Some(0)` is a value, not an absence — it means "fire at the scheduled
/// time", the documented default — so it stops the fallback like any other.
#[must_use]
pub const fn lead_time_s(own: Option<u32>, stream: Option<u32>, global: u32) -> u32 {
    match (own, stream) {
        (Some(v), _) | (None, Some(v)) => v,
        (None, None) => global,
    }
}

/// Apply quiet hours to a fire time, in the reading device's zone.
///
/// Returns the instant to fire at, or `None` to drop. A queued notification
/// moves to the end of the window, and is dropped instead if that wait would
/// exceed [`QUIET_HOURS_QUEUE_CAP_S`].
///
/// The cap is on the wait **measured from this notification's own fire time**,
/// not on the length of the quiet window. A 22:00-07:00 window is nine hours
/// long -- well past a four-hour cap -- yet a notification landing at 06:00
/// still queues, because it only waits an hour. What the cap refuses is
/// holding something so long that firing it is no longer useful.
#[must_use]
pub fn apply_quiet_hours(at: &Zoned, quiet: Option<&QuietHours>) -> Option<Timestamp> {
    let Some(q) = quiet else {
        return Some(at.timestamp());
    };
    if !q.contains(at.time()) {
        return Some(at.timestamp());
    }
    if q.policy == QuietHoursPolicy::Drop {
        return None;
    }
    // The next occurrence of the window's end, at or after `at`.
    let mut end = at.with().time(q.end).build().ok()?;
    if end <= *at {
        end = end.tomorrow().ok()?.with().time(q.end).build().ok()?;
    }
    let delay = end.timestamp().as_second() - at.timestamp().as_second();
    if delay > QUIET_HOURS_QUEUE_CAP_S {
        return None;
    }
    Some(end.timestamp())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{at_local, quiet};
    use super::*;
    use jiff::tz::TimeZone;

    // ---- lead-time hierarchy ----

    #[test]
    fn the_first_non_null_level_wins() {
        assert_eq!(lead_time_s(Some(60), Some(300), 900), 60);
        assert_eq!(lead_time_s(None, Some(300), 900), 300);
        assert_eq!(lead_time_s(None, None, 900), 900);
    }

    /// `Some(0)` is the documented default ("fire at the scheduled time"), not
    /// an absence, so it must stop the fallback.
    #[test]
    fn zero_is_a_value_not_an_absence() {
        assert_eq!(lead_time_s(Some(0), Some(300), 900), 0);
        assert_eq!(lead_time_s(None, Some(0), 900), 0);
    }

    // ---- quiet hours ----

    #[test]
    fn a_wrapping_window_covers_both_sides_of_midnight() {
        let q = quiet(QuietHoursPolicy::Queue);
        assert!(q.contains(civil::time(23, 0, 0, 0)));
        assert!(q.contains(civil::time(3, 0, 0, 0)));
        assert!(!q.contains(civil::time(12, 0, 0, 0)));
        // Half-open: the end instant is already allowed.
        assert!(!q.contains(civil::time(7, 0, 0, 0)));
        assert!(q.contains(civil::time(22, 0, 0, 0)));
    }

    #[test]
    fn an_empty_window_silences_nothing() {
        let q = QuietHours {
            start: civil::time(22, 0, 0, 0),
            end: civil::time(22, 0, 0, 0),
            policy: QuietHoursPolicy::Drop,
        };
        assert!(!q.contains(civil::time(22, 0, 0, 0)));
        let at = at_local(22, 0);
        assert_eq!(apply_quiet_hours(&at, Some(&q)), Some(at.timestamp()));
    }

    #[test]
    fn a_notification_outside_the_window_is_untouched() {
        let at = at_local(12, 0);
        assert_eq!(
            apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Queue))),
            Some(at.timestamp())
        );
    }

    #[test]
    fn queue_moves_it_to_the_end_of_the_window() {
        // 06:15 is inside 22:00..07:00, so it queues to 07:00 — 45 minutes,
        // well inside the four-hour cap.
        let at = at_local(6, 15);
        let fired = apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Queue))).unwrap();
        assert_eq!(fired, at_local(7, 0).timestamp());
    }

    #[test]
    fn queue_drops_it_when_the_wait_exceeds_the_cap() {
        // 22:30 would have to wait 8.5 hours for 07:00.
        let at = at_local(22, 30);
        assert_eq!(
            apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Queue))),
            None
        );
    }

    /// The cap is on the **wait**, not on the window.
    ///
    /// Worth stating, because [`apply_quiet_hours`]'s own doc reads as if a
    /// long quiet window drops everything inside it — "if the window is longer
    /// than the cap". [`QUIET_HOURS_QUEUE_CAP_S`]'s doc is accurate; the
    /// function's is the one that misleads. It does not: the 22:00..07:00 window
    /// here is nine hours long, and a notification landing three hours before
    /// its end still fires. What is measured is `end - at`, from the
    /// notification's own time — so the same window queues a 06:15 and drops a
    /// 22:30.
    ///
    /// Pinned at the boundary in both directions, because `>` and `>=` are one
    /// character apart and a whole class of reminders sits on the line.
    #[test]
    fn the_queue_cap_is_measured_from_the_notification_and_is_inclusive() {
        let q = quiet(QuietHoursPolicy::Queue);

        // 03:00 → 07:00 is exactly four hours: the cap itself, which fires.
        let at_the_cap = at_local(3, 0);
        assert_eq!(
            at_local(7, 0).timestamp().as_second() - at_the_cap.timestamp().as_second(),
            QUIET_HOURS_QUEUE_CAP_S,
            "the fixture must sit exactly on the cap or it pins nothing"
        );
        assert_eq!(
            apply_quiet_hours(&at_the_cap, Some(&q)),
            Some(at_local(7, 0).timestamp()),
            "a wait of exactly the cap is not longer than the cap"
        );

        // One second earlier is one second over, and is dropped.
        let over_the_cap = civil::date(2026, 3, 4)
            .at(2, 59, 59, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        assert_eq!(
            at_local(7, 0).timestamp().as_second() - over_the_cap.timestamp().as_second(),
            QUIET_HOURS_QUEUE_CAP_S + 1
        );
        assert_eq!(
            apply_quiet_hours(&over_the_cap, Some(&q)),
            None,
            "one second past the cap is dropped, not fired late"
        );
    }

    #[test]
    fn drop_policy_drops_it_outright() {
        let at = at_local(6, 15);
        assert_eq!(
            apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Drop))),
            None
        );
    }
}
