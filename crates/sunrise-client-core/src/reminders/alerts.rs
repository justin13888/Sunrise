//! The on-event kinds: security events, sync stalled, time zone changed
//! (`docs/08-features/notifications.md` #10, #12–#15).
//!
//! Each is posted once and never reconciled: its identifier is under
//! [`super::ALERT_PREFIX`], which no reconcile cancels, and the
//! [`NotificationLedger`] remembers it was posted. A security event goes to
//! every device and passes focus and quiet hours; the stall notice is this
//! device's and follows both; the zone notice is the primary device's, off
//! by default, and follows both.

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_core::{SecurityEvent, SecurityEventKind};
use sunrise_domain::{place, FocusWindow, NotificationKind};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_sync::SyncState;

use super::catalog::{CatalogNotification, CatalogPolicy, Draft, ALERT_PREFIX};
use super::ledger::NotificationLedger;
use super::ReminderPolicy;

/// How long local ops may wait unacknowledged, with the network up, before
/// sync reads as stalled: 6 hours (#12).
pub const SYNC_STALL_AFTER_MS: u64 = 6 * 60 * 60 * 1000;

/// What the stall rule reads about sync on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncHealth {
    /// The session's state.
    pub state: SyncState,
    /// [`sunrise_core::Core::oldest_unacked_ms`].
    pub oldest_unacked_ms: Option<u64>,
    /// Whether the device has a network, as the OS says.
    pub network_available: bool,
}

/// Why sync reads as stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stall {
    /// The relay ended the session and will not take this device back on
    /// its own ([`SyncState::Stopped`]).
    Refused,
    /// Ops waited [`SYNC_STALL_AFTER_MS`] for the relay with the network up.
    Unacknowledged,
}

/// Whether `health` is a stall at `now_ms`, and which.
#[must_use]
pub fn stall(health: &SyncHealth, now_ms: u64) -> Option<Stall> {
    if health.state == SyncState::Stopped {
        return Some(Stall::Refused);
    }
    let waited = health
        .oldest_unacked_ms
        .is_some_and(|t| now_ms.saturating_sub(t) >= SYNC_STALL_AFTER_MS);
    (health.network_available && waited).then_some(Stall::Unacknowledged)
}

/// What an alert pass reads.
#[derive(Debug, Clone, Copy)]
pub struct AlertInputs<'a> {
    /// "Now".
    pub now: Timestamp,
    /// The reader's zone.
    pub tz: &'a TimeZone,
    /// Its IANA name, `None` while it is a stand-in.
    pub zone: Option<&'a str>,
    /// [`sunrise_core::Core::security_events`].
    pub events: &'a [SecurityEvent],
    /// Sync on this device.
    pub sync: Option<SyncHealth>,
    /// The focus sessions running.
    pub sessions: &'a [FocusWindow],
}

/// The identifier a security event is posted under.
#[must_use]
pub fn security_identifier(event: &SecurityEvent) -> String {
    let (kind, subject) = match event.kind {
        SecurityEventKind::DeviceJoined => (
            NotificationKind::DevicePaired,
            EntityRef::new(EntityKind::Device, event.subject).to_string(),
        ),
        SecurityEventKind::DeviceRevoked => (
            NotificationKind::DeviceRevoked,
            EntityRef::new(EntityKind::Device, event.subject).to_string(),
        ),
        SecurityEventKind::RecoveryCodeRetired => (
            NotificationKind::RecoveryCode,
            crate::ids::hex_id(&event.subject),
        ),
    };
    format!("{ALERT_PREFIX}{}.{subject}", kind.tag())
}

/// The fixed identifier of a kind with one alert at a time.
#[must_use]
pub fn alert_identifier(kind: NotificationKind) -> String {
    format!("{ALERT_PREFIX}{}", kind.tag())
}

fn security_draft(event: &SecurityEvent) -> Draft {
    let name = event
        .nickname
        .clone()
        .unwrap_or_else(|| "A device".to_owned());
    let (kind, title, body) = match event.kind {
        SecurityEventKind::DeviceJoined => (
            NotificationKind::DevicePaired,
            "New device on your account",
            format!("{name} joined. If this was not you, revoke it."),
        ),
        SecurityEventKind::DeviceRevoked if event.this_device => (
            NotificationKind::DeviceRevoked,
            "Device removed",
            "This device was removed from your account.".to_owned(),
        ),
        SecurityEventKind::DeviceRevoked => (
            NotificationKind::DeviceRevoked,
            "Device removed",
            format!("{name} was removed from your account."),
        ),
        SecurityEventKind::RecoveryCodeRetired => (
            NotificationKind::RecoveryCode,
            "Recovery code changed",
            "Your old recovery code no longer works.".to_owned(),
        ),
    };
    let mut d = Draft::new(security_identifier(event), kind, title, body);
    if kind == NotificationKind::RecoveryCode {
        d = d.linked("sunrise://settings/security");
    } else {
        d.entity = Some(EntityRef::new(EntityKind::Device, event.subject));
        d.subject.clone_from(&event.nickname);
        d = d.linked("sunrise://devices");
    }
    d
}

/// The alerts to post now, each once; updates `ledger`.
///
/// Nothing is posted with notifications off on this device: what is due
/// stays due, and posts once they are on again. A fresh ledger adopts the
/// security events already held without posting them (see
/// [`super::NotificationLedger`]), and the zone it first sees.
#[must_use]
pub fn raise_alerts(
    policy: &ReminderPolicy,
    catalog: &CatalogPolicy,
    inputs: &AlertInputs<'_>,
    ledger: &mut NotificationLedger,
) -> Vec<CatalogNotification> {
    let mut out = Vec::new();
    if !policy.enabled {
        return out;
    }
    let quiet = policy.settings.quiet_hours.as_ref();
    let now = inputs.now;
    let placed = |draft: Draft| -> Option<CatalogNotification> {
        let at = place(now, draft.kind, inputs.sessions, quiet, inputs.tz)?;
        Some(draft.at(at, now))
    };
    let fresh = !ledger.initialized;
    ledger.initialized = true;
    for event in inputs.events {
        if ledger.posted.insert(security_identifier(event)) && !fresh {
            out.extend(placed(security_draft(event)));
        }
    }

    let stalled = inputs
        .sync
        .and_then(|h| stall(&h, u64::try_from(now.as_millisecond()).unwrap_or(0)));
    match stalled {
        Some(why) if !ledger.stall_raised => {
            ledger.stall_raised = true;
            if catalog.is_on(NotificationKind::SyncStalled) {
                let (title, body) = match why {
                    Stall::Refused => (
                        "Sync stopped",
                        "The relay no longer accepts this device. Open Sync Status.",
                    ),
                    Stall::Unacknowledged => (
                        "Sync is stalled",
                        "Changes made here have not reached the relay for 6 hours.",
                    ),
                };
                let mut d = Draft::new(
                    alert_identifier(NotificationKind::SyncStalled),
                    NotificationKind::SyncStalled,
                    title,
                    body.to_owned(),
                );
                d = d.linked("sunrise://sync");
                out.extend(placed(d));
            }
        }
        Some(_) => {}
        // Cleared silently: the next stall is raised again.
        None => ledger.stall_raised = false,
    }

    if let Some(zone) = inputs.zone {
        let moved = ledger.last_zone.as_deref().is_some_and(|last| {
            TimeZone::get(last).map_or(true, |l| {
                !sunrise_domain::time::same_zone(&l, inputs.tz, now)
            })
        });
        if moved
            && policy.settings.is_primary_device
            && catalog.is_on(NotificationKind::TimezoneChanged)
        {
            let mut d = Draft::new(
                alert_identifier(NotificationKind::TimezoneChanged),
                NotificationKind::TimezoneChanged,
                "Time zone changed",
                format!("Times now read in {zone}."),
            );
            d.subject = Some(zone.to_owned());
            d = d.linked("sunrise://upcoming");
            out.extend(placed(d));
        }
        ledger.last_zone = Some(zone.to_owned());
    }
    out
}

#[cfg(test)]
mod tests;
