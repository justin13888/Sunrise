//! The catalog against its page, and the shared placement rules.

use super::*;
use crate::day_schedule::DayWindow;
use crate::notify::QuietHours;
use jiff::civil::{date, time};

fn ts(s: &str) -> Timestamp {
    s.parse().expect(s)
}

fn tz(name: &str) -> TimeZone {
    TimeZone::get(name).expect(name)
}

/// The rows of `notifications.md` §Catalog, as cells.
fn catalog_rows() -> Vec<Vec<String>> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/08-features/notifications.md"
    );
    let page = std::fs::read_to_string(path).expect("notifications.md");
    let section = page
        .split("## Catalog")
        .nth(1)
        .and_then(|s| s.split("\n## ").next())
        .expect("§Catalog");
    section
        .lines()
        .filter(|l| l.starts_with("| ") && l[2..].starts_with(|c: char| c.is_ascii_digit()))
        .map(|l| l.split('|').skip(1).map(|c| c.trim().to_owned()).collect())
        .collect()
}

/// Every kind is the documented row of its number: its default, its key,
/// its delivery and its level.
#[test]
fn the_catalog_is_the_documented_one() {
    let rows = catalog_rows();
    assert_eq!(rows.len(), NotificationKind::ALL.len());
    for (i, (kind, row)) in NotificationKind::ALL.iter().zip(&rows).enumerate() {
        assert_eq!(row[0], (i + 1).to_string(), "{kind:?}");
        let (default, keys, delivery, level) = (&row[4], &row[5], &row[6], &row[7]);
        match kind.enabled_key() {
            None => {
                assert_eq!(default, "**always**", "{kind:?}");
                assert_eq!(keys, "—", "{kind:?}");
            }
            Some(key) => {
                assert!(keys.contains(&format!("`{key}`")), "{kind:?}: {keys}");
                assert_eq!(
                    !default.contains("**off**"),
                    kind.default_enabled(),
                    "{kind:?}"
                );
            }
        }
        let documented = if delivery.starts_with("on event") {
            Delivery::OnEvent
        } else {
            Delivery::Scheduled
        };
        assert_eq!(documented, kind.delivery(), "{kind:?}");
        let documented = if level.starts_with("time-sensitive") {
            InterruptionLevel::TimeSensitive
        } else if level.starts_with("passive") {
            InterruptionLevel::Passive
        } else {
            InterruptionLevel::Active
        };
        assert_eq!(documented, kind.level(), "{kind:?}");
    }
}

/// Each toggle is a key of the table, with the catalog's default, so
/// [`NotificationKind::default_enabled`] cannot drift from what an unset key
/// resolves to.
#[test]
fn every_toggle_is_a_known_key_with_the_catalog_default() {
    for kind in NotificationKind::ALL {
        assert_eq!(NotificationKind::from_tag(kind.tag()), Some(kind));
        let Some(key) = kind.enabled_key() else {
            assert!(kind.is_security());
            continue;
        };
        assert_eq!(key, format!("notifications.{}.enabled", kind.tag()));
        let spec = crate::pref_spec(key).expect(key);
        let resolved =
            crate::resolve_preference(spec, None, None, crate::DeviceClass::Desktop).value;
        assert_eq!(
            resolved,
            Some(crate::PrefValue::Bool(kind.default_enabled())),
            "{key}"
        );
    }
    assert_eq!(NotificationKind::from_tag("nope"), None);
}

#[test]
fn security_events_reach_every_device_and_nothing_holds_them() {
    for kind in NotificationKind::ALL {
        let security = matches!(
            kind,
            NotificationKind::DevicePaired
                | NotificationKind::DeviceRevoked
                | NotificationKind::RecoveryCode
        );
        assert_eq!(kind.is_security(), security, "{kind:?}");
        if security {
            assert_eq!(kind.audience(), NotificationAudience::EveryDevice);
            assert!(kind.passes_focus() && !kind.held_by_quiet_hours());
            assert_eq!(kind.level(), InterruptionLevel::TimeSensitive);
        }
        assert!(!kind.actions().is_empty(), "{kind:?}");
    }
    assert_eq!(
        NotificationKind::SyncStalled.audience(),
        NotificationAudience::ThisDevice
    );
    assert_eq!(
        NotificationKind::Deadline.audience(),
        NotificationAudience::Primary
    );
    assert!(NotificationKind::FocusEnd.passes_focus());
    assert!(!NotificationKind::WindDown.held_by_quiet_hours());
    assert!(!NotificationKind::MorningBrief.passes_focus());
    assert!(NotificationKind::MorningBrief.held_by_quiet_hours());
    assert_eq!(
        NotificationKind::of_reminder(ReminderKind::Routine),
        NotificationKind::RoutineDue
    );
    assert_eq!(
        NotificationKind::of_reminder(ReminderKind::Task),
        NotificationKind::TaskReminder
    );
    assert_eq!(
        NotificationKind::of_reminder(ReminderKind::Block),
        NotificationKind::BlockStart
    );
}

#[test]
fn a_timed_deadline_fires_its_lead_before_and_a_dated_one_at_the_brief() {
    let ny = tz("America/New_York");
    let schedule = DaySchedule {
        weekday: [DayWindow::new(time(6, 30, 0, 0), time(23, 0, 0, 0)); 7],
        ..DaySchedule::default()
    };
    let timed = SunriseTime::instant(ts("2026-03-02T22:00Z"));
    assert_eq!(
        deadline_fire_at(&schedule, &timed, &ny, 3600, 900),
        Some(ts("2026-03-02T21:00Z"))
    );
    let dated = SunriseTime::AllDay {
        date: date(2026, 3, 2),
    };
    assert_eq!(
        deadline_fire_at(&schedule, &dated, &ny, 3600, 900),
        Some(ts("2026-03-02T11:45Z")),
        "06:45 EST: wake plus the brief's offset"
    );
    assert_eq!(
        deadline_fire_at(&DaySchedule::default(), &dated, &ny, 3600, 900),
        Some(ts("2026-03-02T13:00Z")),
        "08:00 with no schedule"
    );
}

fn session(start: &str, end: Option<&str>) -> FocusWindow {
    FocusWindow {
        start: ts(start),
        end: end.map(ts),
    }
}

/// Only its own end and security events pass a focus session; the rest wait
/// until 30 s after it, under the quiet-hours rule.
#[test]
fn a_focus_session_holds_everything_but_its_end_and_security() {
    use NotificationKind as K;
    let s = [session("2026-03-02T14:00Z", Some("2026-03-02T15:00Z"))];
    let during = ts("2026-03-02T14:30Z");
    let released = ts("2026-03-02T15:00:30Z");
    let hold = |kind, policy| focus_hold(during, kind, &s, policy);
    assert_eq!(
        hold(K::TaskReminder, QuietHoursPolicy::Queue),
        Some(released)
    );
    assert_eq!(
        hold(K::MorningBrief, QuietHoursPolicy::Queue),
        Some(released)
    );
    assert_eq!(hold(K::TaskReminder, QuietHoursPolicy::Drop), None);
    assert_eq!(hold(K::FocusEnd, QuietHoursPolicy::Drop), Some(during));
    assert_eq!(hold(K::DeviceRevoked, QuietHoursPolicy::Drop), Some(during));
    // Before it, after its release, and in the 30 s tail.
    let at = |t: &str| focus_hold(ts(t), K::TaskReminder, &s, QuietHoursPolicy::Queue);
    assert_eq!(at("2026-03-02T13:59:59Z"), Some(ts("2026-03-02T13:59:59Z")));
    assert_eq!(at("2026-03-02T14:00Z"), Some(released));
    assert_eq!(at("2026-03-02T15:00:10Z"), Some(released));
    assert_eq!(
        focus_hold(released, K::TaskReminder, &s, QuietHoursPolicy::Drop),
        Some(released),
        "the release is past the session, so drop does not drop it"
    );
    assert_eq!(
        at("2026-03-02T15:00:30Z"),
        Some(released),
        "the release itself"
    );
    // A hold longer than the queue cap drops.
    let long = [session("2026-03-02T09:00Z", Some("2026-03-02T15:00Z"))];
    assert_eq!(
        focus_hold(
            ts("2026-03-02T10:00Z"),
            K::TaskReminder,
            &long,
            QuietHoursPolicy::Queue
        ),
        None
    );
    assert_eq!(
        focus_hold(
            ts("2026-03-02T11:00:30Z"),
            K::TaskReminder,
            &long,
            QuietHoursPolicy::Queue
        ),
        Some(ts("2026-03-02T15:00:30Z")),
        "exactly the cap still queues"
    );
    // An open-ended session has no end to wait for.
    let open = [session("2026-03-02T14:00Z", None)];
    assert_eq!(
        focus_hold(during, K::TaskReminder, &open, QuietHoursPolicy::Queue),
        None
    );
    assert_eq!(
        focus_hold(during, K::RecoveryCode, &open, QuietHoursPolicy::Queue),
        Some(during)
    );
}

#[test]
fn placement_is_focus_then_quiet_hours_and_security_ignores_both() {
    let utc = TimeZone::UTC;
    let quiet = QuietHours {
        start: time(22, 0, 0, 0),
        end: time(7, 0, 0, 0),
        policy: QuietHoursPolicy::Queue,
    };
    let late = ts("2026-03-02T06:00Z");
    assert_eq!(
        place(
            late,
            NotificationKind::MorningBrief,
            &[],
            Some(&quiet),
            &utc
        ),
        Some(ts("2026-03-02T07:00Z"))
    );
    assert_eq!(
        place(
            late,
            NotificationKind::DevicePaired,
            &[],
            Some(&quiet),
            &utc
        ),
        Some(late)
    );
    assert_eq!(
        place(late, NotificationKind::WindDown, &[], Some(&quiet), &utc),
        Some(late)
    );
    // Released from a session into quiet hours, it then waits for their end.
    let s = [session("2026-03-02T05:00Z", Some("2026-03-02T05:59:30Z"))];
    assert_eq!(
        place(
            ts("2026-03-02T05:30Z"),
            NotificationKind::Deadline,
            &s,
            Some(&quiet),
            &utc
        ),
        Some(ts("2026-03-02T07:00Z"))
    );
    let drop = QuietHours {
        policy: QuietHoursPolicy::Drop,
        ..quiet
    };
    assert_eq!(
        place(
            ts("2026-03-02T05:30Z"),
            NotificationKind::Deadline,
            &s,
            Some(&drop),
            &utc
        ),
        None,
        "drop drops a focus hold too"
    );
    assert_eq!(
        place(
            ts("2026-03-02T12:00Z"),
            NotificationKind::Deadline,
            &[],
            None,
            &utc
        ),
        Some(ts("2026-03-02T12:00Z"))
    );
}
