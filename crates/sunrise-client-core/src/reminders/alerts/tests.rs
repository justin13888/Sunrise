//! The on-event kinds, pure: each posted once, security on every device and
//! through focus and quiet hours, the stall raised once per stall, the zone
//! notice off by default and primary-only, a fresh ledger adopting.

use super::*;
use crate::reminders::{reminder_policy, CatalogPolicy};
use jiff::civil::time;
use sunrise_domain::{PrefScope, PrefSource, PrefValue, ResolvedPref};

fn ts(s: &str) -> Timestamp {
    s.parse().expect(s)
}

fn set(key: &'static str, value: PrefValue) -> ResolvedPref {
    ResolvedPref {
        key,
        scope: PrefScope::VaultOverridable,
        value: Some(value),
        source: PrefSource::Overlay,
        vault_set: false,
    }
}

fn joined(n: u8, at: &str) -> SecurityEvent {
    SecurityEvent {
        kind: SecurityEventKind::DeviceJoined,
        subject: [n; 16],
        at_ms: u64::try_from(ts(at).as_millisecond()).unwrap(),
        nickname: Some(format!("phone {n}")),
        this_device: false,
    }
}

struct Case {
    prefs: Vec<ResolvedPref>,
    primary: bool,
    now: Timestamp,
    zone: &'static str,
    events: Vec<SecurityEvent>,
    sync: Option<SyncHealth>,
    sessions: Vec<FocusWindow>,
}

impl Case {
    fn new() -> Self {
        Self {
            prefs: Vec::new(),
            primary: true,
            now: ts("2026-03-02T15:00Z"),
            zone: "America/New_York",
            events: Vec::new(),
            sync: None,
            sessions: Vec::new(),
        }
    }

    fn raise(&self, ledger: &mut NotificationLedger) -> Vec<CatalogNotification> {
        let mut policy = reminder_policy(
            &self.prefs,
            EntityRef::new(EntityKind::Device, [1; 16]),
            &[],
        );
        policy.settings.is_primary_device = self.primary;
        let catalog: CatalogPolicy = crate::reminders::catalog_policy(&self.prefs);
        let tz = TimeZone::get(self.zone).unwrap();
        raise_alerts(
            &policy,
            &catalog,
            &AlertInputs {
                now: self.now,
                tz: &tz,
                zone: Some(self.zone),
                events: &self.events,
                sync: self.sync,
                sessions: &self.sessions,
            },
            ledger,
        )
    }
}

/// A ledger that has seen one plan with nothing in it.
fn seen() -> NotificationLedger {
    let mut l = NotificationLedger::new();
    let _ = Case::new().raise(&mut l);
    l
}

#[test]
fn a_fresh_ledger_adopts_what_is_there_and_then_each_event_posts_once() {
    let mut c = Case::new();
    c.events = vec![joined(2, "2026-03-01T10:00Z")];
    let mut ledger = NotificationLedger::new();
    assert!(c.raise(&mut ledger).is_empty(), "adopted, not announced");
    assert!(ledger.initialized);

    c.events.push(joined(3, "2026-03-02T14:00Z"));
    let posted = c.raise(&mut ledger);
    assert_eq!(posted.len(), 1);
    let a = &posted[0];
    assert_eq!(a.kind, NotificationKind::DevicePaired);
    let dev = EntityRef::new(EntityKind::Device, [3; 16]);
    assert_eq!(a.identifier, format!("sunrise.alert.device_paired.{dev}"));
    assert_eq!(a.entity, Some(dev));
    assert_eq!(a.subject.as_deref(), Some("phone 3"));
    assert_eq!(a.fire_at_ms, c.now.as_millisecond());
    assert_eq!(a.level, sunrise_domain::InterruptionLevel::TimeSensitive);
    assert_eq!(
        a.actions,
        vec![sunrise_domain::NotificationAction::ReviewDevices]
    );
    assert!(c.raise(&mut ledger).is_empty(), "posted once");

    // Another device's revocation names it; this device's says so.
    c.events.push(SecurityEvent {
        kind: SecurityEventKind::DeviceRevoked,
        subject: [3; 16],
        at_ms: 3,
        nickname: Some("phone 3".into()),
        this_device: false,
    });
    let revoked = c.raise(&mut ledger);
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].body, "phone 3 was removed from your account.");
    assert_eq!(
        revoked[0].identifier,
        format!("sunrise.alert.device_revoked.{dev}")
    );
}

/// Every device posts a security event, whatever the primary register, the
/// focus session, the quiet window or its policy says.
#[test]
fn security_events_pass_the_primary_rule_focus_and_quiet_hours() {
    let mut c = Case {
        primary: false,
        prefs: vec![
            set(
                "notifications.quiet_hours.window",
                PrefValue::TimeWindow {
                    start: time(9, 0, 0, 0),
                    end: time(12, 0, 0, 0),
                },
            ),
            set(
                "notifications.quiet_hours.policy",
                PrefValue::Text("drop".into()),
            ),
        ],
        sessions: vec![FocusWindow {
            start: ts("2026-03-02T14:30Z"),
            end: None,
        }],
        ..Case::new()
    };
    let mut ledger = seen();
    let revoked = SecurityEvent {
        kind: SecurityEventKind::DeviceRevoked,
        subject: [1; 16],
        at_ms: 1,
        nickname: Some("this mac".into()),
        this_device: true,
    };
    let retired = SecurityEvent {
        kind: SecurityEventKind::RecoveryCodeRetired,
        subject: [0xab; 16],
        at_ms: 2,
        nickname: None,
        this_device: false,
    };
    c.events = vec![revoked, retired];
    let posted = c.raise(&mut ledger);
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0].body, "This device was removed from your account.");
    assert_eq!(posted[1].kind, NotificationKind::RecoveryCode);
    assert_eq!(
        posted[1].identifier,
        format!("sunrise.alert.recovery_code.{}", "ab".repeat(16))
    );
    assert!(posted
        .iter()
        .all(|a| a.fire_at_ms == c.now.as_millisecond()));

    // Only the master switch on this device holds them, and they stay due.
    let mut l = seen();
    let mut quiet = Case::new();
    quiet.events = vec![joined(4, "2026-03-02T14:00Z")];
    quiet.prefs = vec![set("notifications.enabled", PrefValue::Bool(false))];
    assert!(quiet.raise(&mut l).is_empty());
    quiet.prefs.clear();
    assert_eq!(quiet.raise(&mut l).len(), 1);
}

#[test]
fn a_stall_is_raised_once_and_cleared_silently() {
    let now_ms = 1_772_463_600_000u64; // 2026-03-02T15:00Z
    let waiting = |since_h: u64, network| SyncHealth {
        state: SyncState::Disconnected,
        oldest_unacked_ms: Some(now_ms - since_h * 3_600_000),
        network_available: network,
    };
    assert_eq!(stall(&waiting(5, true), now_ms), None);
    assert_eq!(
        stall(&waiting(6, true), now_ms),
        Some(Stall::Unacknowledged)
    );
    assert_eq!(
        stall(&waiting(9, false), now_ms),
        None,
        "no network, no stall"
    );
    let refused = SyncHealth {
        state: SyncState::Stopped,
        oldest_unacked_ms: None,
        network_available: false,
    };
    assert_eq!(stall(&refused, now_ms), Some(Stall::Refused));
    let caught_up = SyncHealth {
        state: SyncState::Live,
        oldest_unacked_ms: None,
        network_available: true,
    };
    assert_eq!(stall(&caught_up, now_ms), None);

    let mut c = Case {
        primary: false,
        sync: Some(waiting(7, true)),
        ..Case::new()
    };
    let mut ledger = seen();
    let raised = c.raise(&mut ledger);
    assert_eq!(raised.len(), 1, "this device's, primary or not");
    assert_eq!(raised[0].identifier, "sunrise.alert.sync_stalled");
    assert_eq!(raised[0].title, "Sync is stalled");
    assert!(c.raise(&mut ledger).is_empty(), "once per stall");
    c.sync = Some(caught_up);
    assert!(c.raise(&mut ledger).is_empty());
    assert!(!ledger.stall_raised, "cleared silently");
    c.sync = Some(refused);
    assert_eq!(c.raise(&mut ledger)[0].title, "Sync stopped");

    let mut off = Case {
        sync: Some(refused),
        prefs: vec![set(
            "notifications.sync_stalled.enabled",
            PrefValue::Bool(false),
        )],
        ..Case::new()
    };
    let mut ledger = seen();
    assert!(off.raise(&mut ledger).is_empty());
    off.prefs.clear();
    assert!(
        off.raise(&mut ledger).is_empty(),
        "this stall was already seen"
    );
}

#[test]
fn the_zone_notice_is_off_by_default_primary_only_and_names_the_new_zone() {
    let on = set(
        "notifications.timezone_changed.enabled",
        PrefValue::Bool(true),
    );
    let mut ledger = seen();
    assert_eq!(ledger.last_zone.as_deref(), Some("America/New_York"));
    let moved = Case {
        zone: "Europe/Paris",
        ..Case::new()
    };
    assert!(
        moved.raise(&mut ledger.clone()).is_empty(),
        "off by default"
    );
    let mut c = Case {
        prefs: vec![on.clone()],
        ..moved
    };
    let posted = c.raise(&mut ledger.clone());
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].identifier, "sunrise.alert.timezone_changed");
    assert_eq!(posted[0].body, "Times now read in Europe/Paris.");
    assert_eq!(posted[0].subject.as_deref(), Some("Europe/Paris"));
    c.primary = false;
    assert!(
        c.raise(&mut ledger.clone()).is_empty(),
        "the primary device's"
    );
    c.primary = true;
    let _ = c.raise(&mut ledger);
    assert!(
        c.raise(&mut ledger).is_empty(),
        "the same zone again is no change"
    );
    // An alias of the zone it was in is not a change.
    let mut l = NotificationLedger::new();
    l.initialized = true;
    l.last_zone = Some("US/Eastern".into());
    let alias = Case {
        prefs: vec![on],
        ..Case::new()
    };
    assert!(alias.raise(&mut l).is_empty());
}
