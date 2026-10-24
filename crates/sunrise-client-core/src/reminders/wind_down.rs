//! The wind-down notification (`docs/08-features/notifications.md` #8,
//! `docs/02-domain/day-schedule.md` §Wind-down, #301): `lead_s` before the
//! resolved sleep time of the planner day the reader is in, or of the next
//! one when tonight's has already fired.
//!
//! The trigger is the core's, [`sunrise_domain::next_wind_down`], over the
//! vault's day schedule read in the reader's zone. What this module adds is
//! what every client would otherwise re-derive: whether this device fires it
//! at all, the identifier it is held under, and what to do with the one the
//! OS already holds.
//!
//! # Its own identifier, outside the reminder namespace
//!
//! A wind-down belongs to no entity and to no [`sunrise_domain::ReminderKind`],
//! so it is not one of [`super::plan`]'s notifications. It is held under
//! [`WIND_DOWN_IDENTIFIER`], which does not start with
//! [`super::IDENTIFIER_PREFIX`], so [`super::reconcile`] never cancels it and
//! [`reconcile_wind_down`] is the only thing that touches it. The identifier
//! is fixed: a zone change or a new sleep time moves the fire instant and
//! changes [`WindDownNotification::revision`], and the client replaces the
//! held request (#520's rule, for this kind too).
//!
//! # Who fires it
//!
//! The primary notification device, with `notifications.enabled` and
//! `notifications.wind_down.enabled` on. The latter is `vault_overridable`:
//! one device may turn it off for itself. It never fires for a date with no
//! sleep time, which is what makes it "on once a sleep time is set". Quiet
//! hours do not hold it: a sleep time inside them is one the user chose
//! (`notifications.md` §Quiet hours).

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_core::{Core, CoreError, Query, QueryResult};
use sunrise_domain::{next_wind_down, DaySchedule};

use super::{PendingNotification, ReminderPolicy};

/// The one identifier a wind-down is held under.
pub const WIND_DOWN_IDENTIFIER: &str = "sunrise.wind_down";

/// What the notification says under its title, in English. A client
/// rendering its own catalog builds the line from
/// [`WindDownNotification::sleep_at_ms`] instead.
pub const WIND_DOWN_TITLE: &str = "Time to wind down";

/// The next wind-down the OS should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindDownNotification {
    /// Always [`WIND_DOWN_IDENTIFIER`].
    pub identifier: String,
    /// Changes whenever the fire instant does, so a held request with another
    /// revision is replaced.
    pub revision: String,
    /// The planner day whose sleep time it counts back from.
    pub date: jiff::civil::Date,
    /// That sleep time, resolved in the reader's zone, epoch milliseconds.
    pub sleep_at_ms: i64,
    /// When it fires, epoch milliseconds: `sleep_at_ms` less the lead.
    pub fire_at_ms: i64,
}

/// What to do with the wind-down the OS holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindDownChange {
    /// None is held: schedule this.
    Add(WindDownNotification),
    /// One is held with another revision: schedule this, which replaces it.
    Replace(WindDownNotification),
    /// One is held and none is wanted: withdraw [`WIND_DOWN_IDENTIFIER`].
    Cancel,
    /// What is held is right, or nothing is held and nothing is wanted.
    Keep,
}

/// The next wind-down for this device, or `None` when it does not fire one:
/// notifications off, wind-down off, not the primary device, or no sleep time
/// tonight or tomorrow night.
#[must_use]
pub fn wind_down_notification(
    policy: &ReminderPolicy,
    schedule: &DaySchedule,
    now: Timestamp,
    tz: &TimeZone,
) -> Option<WindDownNotification> {
    if !(policy.enabled && policy.wind_down_enabled && policy.settings.is_primary_device) {
        return None;
    }
    let next = next_wind_down(schedule, now, tz, policy.wind_down_lead_s).ok()??;
    let fire_at_ms = next.fire_at.as_millisecond();
    Some(WindDownNotification {
        identifier: WIND_DOWN_IDENTIFIER.to_owned(),
        revision: format!("{fire_at_ms}.{}", next.sleep_at.as_millisecond()),
        date: next.date,
        sleep_at_ms: next.sleep_at.as_millisecond(),
        fire_at_ms,
    })
}

/// `desired` against what the OS holds.
#[must_use]
pub fn reconcile_wind_down(
    desired: Option<&WindDownNotification>,
    pending: &[PendingNotification],
) -> WindDownChange {
    let held = pending
        .iter()
        .find(|p| p.identifier == WIND_DOWN_IDENTIFIER);
    match (desired, held) {
        (None, None) => WindDownChange::Keep,
        (None, Some(_)) => WindDownChange::Cancel,
        (Some(d), None) => WindDownChange::Add(d.clone()),
        (Some(d), Some(h)) if h.revision.as_deref() == Some(d.revision.as_str()) => {
            WindDownChange::Keep
        }
        (Some(d), Some(_)) => WindDownChange::Replace(d.clone()),
    }
}

/// The vault's day schedule, as `Query::DaySchedule` reads it.
///
/// # Errors
///
/// A failed read of the vault.
pub async fn read_day_schedule(core: &Core, now_ms: u64) -> Result<DaySchedule, CoreError> {
    match core.query(Query::DaySchedule { at_ms: now_ms }).await? {
        QueryResult::DaySchedule(day) => Ok(day.schedule),
        _ => Err(super::schedule::unexpected("day schedule")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reminders::reminder_policy;
    use jiff::civil::time;
    use sunrise_domain::{DayWindow, PrefScope, PrefSource, PrefValue, ResolvedPref};
    use sunrise_id::{EntityKind, EntityRef};

    fn sleeps_at_one() -> DaySchedule {
        DaySchedule {
            weekday: [DayWindow::new(time(7, 0, 0, 0), time(1, 0, 0, 0)); 7],
            ..DaySchedule::default()
        }
    }

    fn policy(prefs: &[ResolvedPref]) -> ReminderPolicy {
        reminder_policy(prefs, EntityRef::new(EntityKind::Device, [1; 16]), &[])
    }

    fn off(key: &'static str) -> ResolvedPref {
        ResolvedPref {
            key,
            scope: PrefScope::VaultOverridable,
            value: Some(PrefValue::Bool(false)),
            source: PrefSource::Overlay,
            vault_set: false,
        }
    }

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn it_fires_the_lead_before_tonights_sleep_and_only_where_it_should() {
        let ny = TimeZone::get("America/New_York").unwrap();
        // 22:00 EST on Monday 2026-03-02; sleep is 01:00 Tuesday, 06:00Z.
        let now = at("2026-03-03T03:00Z");
        let n = wind_down_notification(&policy(&[]), &sleeps_at_one(), now, &ny).unwrap();
        assert_eq!(n.identifier, WIND_DOWN_IDENTIFIER);
        assert_eq!(n.date, jiff::civil::date(2026, 3, 2));
        assert_eq!(n.sleep_at_ms, at("2026-03-03T06:00Z").as_millisecond());
        assert_eq!(n.fire_at_ms, at("2026-03-03T05:00Z").as_millisecond());

        assert_eq!(
            wind_down_notification(&policy(&[]), &DaySchedule::default(), now, &ny),
            None,
            "no sleep time, no wind-down"
        );
        for key in ["notifications.wind_down.enabled", "notifications.enabled"] {
            assert_eq!(
                wind_down_notification(&policy(&[off(key)]), &sleeps_at_one(), now, &ny),
                None,
                "{key} off"
            );
        }
        let mut not_primary = policy(&[]);
        not_primary.settings.is_primary_device = false;
        assert_eq!(
            wind_down_notification(&not_primary, &sleeps_at_one(), now, &ny),
            None
        );
    }

    /// A zone change moves the fire instant to the sleep time's wall clock in
    /// the new zone, under the same identifier and a new revision.
    #[test]
    fn a_zone_change_replaces_the_held_wind_down() {
        let now = at("2026-03-03T03:00Z");
        let ny = TimeZone::get("America/New_York").unwrap();
        let la = TimeZone::get("America/Los_Angeles").unwrap();
        let p = policy(&[]);
        let in_ny = wind_down_notification(&p, &sleeps_at_one(), now, &ny).unwrap();
        let in_la = wind_down_notification(&p, &sleeps_at_one(), now, &la).unwrap();
        assert_eq!(in_la.fire_at_ms, at("2026-03-03T08:00Z").as_millisecond());
        assert_eq!(in_ny.identifier, in_la.identifier);
        assert_ne!(in_ny.revision, in_la.revision);

        let held = [PendingNotification {
            identifier: in_ny.identifier.clone(),
            revision: Some(in_ny.revision.clone()),
        }];
        assert_eq!(
            reconcile_wind_down(Some(&in_ny), &held),
            WindDownChange::Keep
        );
        assert_eq!(
            reconcile_wind_down(Some(&in_la), &held),
            WindDownChange::Replace(in_la.clone())
        );
        assert_eq!(reconcile_wind_down(None, &held), WindDownChange::Cancel);
        assert_eq!(
            reconcile_wind_down(Some(&in_la), &[]),
            WindDownChange::Add(in_la)
        );
        assert_eq!(reconcile_wind_down(None, &[]), WindDownChange::Keep);
    }
}
