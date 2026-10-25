//! The scheduled catalog kinds, pure: fire times, the fold of the nudge into
//! the brief, the once-a-day rule, focus, quiet hours, toggles, the primary
//! rule, a DST day, and the reconcile.

use super::*;
use crate::reminders::reminder_policy;
use jiff::civil::time;
use sunrise_domain::{DayWindow, PrefScope, PrefSource, PrefValue, ResolvedPref};
use sunrise_id::EntityKind;

fn ts(s: &str) -> Timestamp {
    s.parse().expect(s)
}

fn ny() -> TimeZone {
    TimeZone::get("America/New_York").unwrap()
}

fn task(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Task, [n; 16])
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

fn off(key: &'static str) -> ResolvedPref {
    set(key, PrefValue::Bool(false))
}

/// Wake 07:00, sleep 23:00 every day.
fn day() -> DaySchedule {
    DaySchedule {
        weekday: [DayWindow::new(time(7, 0, 0, 0), time(23, 0, 0, 0)); 7],
        ..DaySchedule::default()
    }
}

struct Case {
    prefs: Vec<ResolvedPref>,
    now: Timestamp,
    schedule: DaySchedule,
    deadlines: Vec<DeadlineCandidate>,
    triage: Vec<EntityRef>,
    sessions: Vec<FocusWindow>,
    primary: bool,
    horizon_s: i64,
}

impl Case {
    /// 05:00 EST on Monday 2026-03-02.
    fn new() -> Self {
        Self {
            prefs: Vec::new(),
            now: ts("2026-03-02T10:00Z"),
            schedule: day(),
            deadlines: Vec::new(),
            triage: Vec::new(),
            sessions: Vec::new(),
            primary: true,
            horizon_s: 24 * 3600,
        }
    }

    fn plan(&self, ledger: &mut NotificationLedger) -> Vec<CatalogNotification> {
        let mut policy = reminder_policy(
            &self.prefs,
            EntityRef::new(EntityKind::Device, [1; 16]),
            &[],
        );
        policy.settings.is_primary_device = self.primary;
        let tz = ny();
        plan_catalog(
            &policy,
            &catalog_policy(&self.prefs),
            &CatalogInputs {
                now: self.now,
                horizon: self.now + jiff::SignedDuration::from_secs(self.horizon_s),
                tz: &tz,
                schedule: &self.schedule,
                deadlines: &self.deadlines,
                triage: &self.triage,
                sessions: &self.sessions,
                clock: ClockStyle::H24,
            },
            ledger,
        )
    }

    fn kinds(&self) -> Vec<(NotificationKind, i64)> {
        self.plan(&mut NotificationLedger::new())
            .iter()
            .map(|n| (n.kind, n.fire_at_ms))
            .collect()
    }
}

fn ms(s: &str) -> i64 {
    ts(s).as_millisecond()
}

#[test]
fn the_defaults_plan_the_morning_brief_and_the_evening_plan() {
    let c = Case::new();
    let plan = c.plan(&mut NotificationLedger::new());
    assert_eq!(
        plan.iter()
            .map(|n| (n.kind, n.fire_at_ms))
            .collect::<Vec<_>>(),
        vec![
            (NotificationKind::MorningBrief, ms("2026-03-02T12:15Z")),
            (NotificationKind::EveningPlan, ms("2026-03-03T02:00Z")),
        ],
        "07:15 and 21:00 EST"
    );
    let brief = &plan[0];
    assert_eq!(brief.identifier, "sunrise.notify.morning_brief");
    assert_eq!(brief.link, "sunrise://morning");
    assert_eq!(brief.body, MORNING_BODY);
    assert_eq!(brief.actions, vec![NotificationAction::PlanMyDay]);
    assert_eq!(plan[1].identifier, "sunrise.notify.evening_plan");
    assert!(plan.iter().all(|n| n.deferred_from_ms.is_none()));
}

#[test]
fn without_a_day_schedule_the_briefs_fire_at_eight_and_twenty() {
    let c = Case {
        schedule: DaySchedule::default(),
        ..Case::new()
    };
    assert_eq!(
        c.kinds(),
        vec![
            (NotificationKind::MorningBrief, ms("2026-03-02T13:00Z")),
            (NotificationKind::EveningPlan, ms("2026-03-03T01:00Z")),
        ]
    );
}

#[test]
fn offsets_move_the_briefs_and_a_toggle_silences_its_kind() {
    let c = Case {
        prefs: vec![
            set(
                "notifications.morning_brief.offset_s",
                PrefValue::Int(-1800),
            ),
            set("notifications.evening_plan.offset_s", PrefValue::Int(600)),
        ],
        ..Case::new()
    };
    assert_eq!(
        c.kinds(),
        vec![
            (NotificationKind::MorningBrief, ms("2026-03-02T11:30Z")),
            (NotificationKind::EveningPlan, ms("2026-03-03T03:50Z")),
        ]
    );
    for (key, left) in [
        (
            "notifications.morning_brief.enabled",
            NotificationKind::EveningPlan,
        ),
        (
            "notifications.evening_plan.enabled",
            NotificationKind::MorningBrief,
        ),
    ] {
        let c = Case {
            prefs: vec![off(key)],
            ..Case::new()
        };
        assert_eq!(
            c.kinds().iter().map(|k| k.0).collect::<Vec<_>>(),
            vec![left],
            "{key}"
        );
    }
}

#[test]
fn a_device_that_is_not_primary_or_has_notifications_off_plans_none() {
    let mut c = Case {
        primary: false,
        ..Case::new()
    };
    assert!(c.kinds().is_empty());
    c.primary = true;
    c.prefs = vec![off("notifications.enabled")];
    assert!(c.kinds().is_empty());
}

#[test]
fn a_hard_deadline_fires_its_lead_before_or_at_the_brief_of_its_date() {
    let timed = DeadlineCandidate {
        task: task(1),
        title: "File taxes".into(),
        hard_due_at: SunriseTime::instant(ts("2026-03-02T22:00Z")),
    };
    let dated = DeadlineCandidate {
        task: task(2),
        title: "Renew passport".into(),
        hard_due_at: SunriseTime::AllDay {
            date: jiff::civil::date(2026, 3, 2),
        },
    };
    let gone = DeadlineCandidate {
        task: task(3),
        title: "Past".into(),
        hard_due_at: SunriseTime::instant(ts("2026-03-02T09:00Z")),
    };
    let c = Case {
        deadlines: vec![timed, dated, gone],
        prefs: vec![off("notifications.evening_plan.enabled")],
        ..Case::new()
    };
    let plan = c.plan(&mut NotificationLedger::new());
    let deadlines: Vec<_> = plan
        .iter()
        .filter(|n| n.kind == NotificationKind::Deadline)
        .collect();
    assert_eq!(deadlines.len(), 2, "one already past is not planned");
    assert_eq!(deadlines[0].entity, Some(task(2)));
    assert_eq!(deadlines[0].fire_at_ms, ms("2026-03-02T12:15Z"));
    assert_eq!(deadlines[0].body, "Due today");
    assert_eq!(deadlines[1].entity, Some(task(1)));
    assert_eq!(
        deadlines[1].fire_at_ms,
        ms("2026-03-02T21:00Z"),
        "an hour before"
    );
    assert_eq!(deadlines[1].body, "Due 17:00");
    assert_eq!(
        deadlines[1].identifier,
        format!("sunrise.notify.deadline.{}", task(1))
    );
    assert_eq!(deadlines[1].link, format!("sunrise://task/{}", task(1)));
    assert_eq!(
        deadlines[1].actions,
        vec![
            NotificationAction::MarkDone,
            NotificationAction::Defer,
            NotificationAction::SnoozeOneHour
        ]
    );

    let lead = Case {
        prefs: vec![set("notifications.deadline.lead_s", PrefValue::Uint(600))],
        ..c
    };
    let plan = lead.plan(&mut NotificationLedger::new());
    assert!(plan
        .iter()
        .any(|n| n.entity == Some(task(1)) && n.fire_at_ms == ms("2026-03-02T21:50Z")));
    let silenced = Case {
        prefs: vec![off("notifications.deadline.enabled")],
        ..lead
    };
    assert!(silenced
        .kinds()
        .iter()
        .all(|(k, _)| *k != NotificationKind::Deadline));
}

/// With the brief on, the queue is folded into it; with it off, the nudge
/// fires alone, once a day, and only for a queue that changed.
#[test]
fn the_triage_nudge_folds_into_the_brief_or_fires_once_for_a_changed_queue() {
    let c = Case {
        triage: vec![task(1), task(2), task(3)],
        ..Case::new()
    };
    let plan = c.plan(&mut NotificationLedger::new());
    assert_eq!(plan[0].kind, NotificationKind::MorningBrief);
    assert_eq!(plan[0].body, "3 tasks need a decision");
    assert_eq!(plan[0].count, Some(3));
    assert!(plan[0].actions.contains(&NotificationAction::OpenTriage));
    assert!(plan.iter().all(|n| n.kind != NotificationKind::TriageNudge));

    let mut alone = Case {
        prefs: vec![
            off("notifications.morning_brief.enabled"),
            off("notifications.evening_plan.enabled"),
        ],
        ..c
    };
    let mut ledger = NotificationLedger::new();
    let nudge = alone.plan(&mut ledger);
    assert_eq!(nudge.len(), 1);
    assert_eq!(
        (nudge[0].kind, nudge[0].fire_at_ms, nudge[0].link.as_str()),
        (
            NotificationKind::TriageNudge,
            ms("2026-03-02T12:15Z"),
            "sunrise://triage"
        )
    );
    assert_eq!(nudge[0].level, InterruptionLevel::Passive);
    assert_eq!(
        alone.plan(&mut ledger.clone()),
        nudge,
        "a re-plan before it fires is the same request"
    );

    // It fired. Tomorrow, an untouched queue does not nag; a changed one
    // does, once.
    alone.now = ts("2026-03-02T13:00Z");
    assert!(alone.plan(&mut ledger).is_empty());
    assert_eq!(
        ledger.nudged.as_ref().map(|n| n.date),
        Some(jiff::civil::date(2026, 3, 2))
    );
    alone.triage.push(task(4));
    let changed = alone.plan(&mut ledger);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].fire_at_ms, ms("2026-03-03T12:15Z"));
    assert_eq!(changed[0].body, "4 tasks need a decision");

    // An empty queue nudges nothing, and its toggle silences it. The nudge
    // planned for tomorrow is forgotten, so it never counts as fired.
    assert!(ledger.nudge_planned.is_some());
    alone.triage.clear();
    assert!(alone.plan(&mut ledger).is_empty());
    assert_eq!(ledger.nudge_planned, None);
    assert!(alone.plan(&mut NotificationLedger::new()).is_empty());
    alone.triage.push(task(9));
    alone.prefs.push(off("notifications.triage_nudge.enabled"));
    assert!(alone.plan(&mut NotificationLedger::new()).is_empty());
}

#[test]
fn quiet_hours_and_focus_hold_a_brief_and_say_when_it_was_due() {
    let quiet = set(
        "notifications.quiet_hours.window",
        PrefValue::TimeWindow {
            start: time(22, 0, 0, 0),
            end: time(7, 30, 0, 0),
        },
    );
    let c = Case {
        prefs: vec![quiet.clone()],
        ..Case::new()
    };
    let plan = c.plan(&mut NotificationLedger::new());
    assert_eq!(
        plan[0].fire_at_ms,
        ms("2026-03-02T12:30Z"),
        "07:30, the end"
    );
    assert_eq!(plan[0].deferred_from_ms, Some(ms("2026-03-02T12:15Z")));

    let focus = Case {
        sessions: vec![FocusWindow {
            start: ts("2026-03-02T12:00Z"),
            end: Some(ts("2026-03-02T12:25Z")),
        }],
        ..Case::new()
    };
    let plan = focus.plan(&mut NotificationLedger::new());
    assert_eq!(plan[0].fire_at_ms, ms("2026-03-02T12:25:30Z"));
    let drop = Case {
        prefs: vec![
            quiet,
            set(
                "notifications.quiet_hours.policy",
                PrefValue::Text("drop".into()),
            ),
        ],
        ..focus
    };
    assert!(drop
        .kinds()
        .iter()
        .all(|(k, _)| *k != NotificationKind::MorningBrief));
}

/// A zone change moves a brief to the same wall clock under the same
/// identifier and a new revision, so the reconcile replaces it.
#[test]
fn a_zone_change_replaces_a_brief_and_an_unchanged_plan_keeps_it() {
    let c = Case::new();
    let plan = c.plan(&mut NotificationLedger::new());
    let held: Vec<PendingNotification> = plan
        .iter()
        .map(|n| PendingNotification {
            identifier: n.identifier.clone(),
            revision: Some(n.revision.clone()),
        })
        .chain([
            PendingNotification {
                identifier: "sunrise.notify.deadline.gone".into(),
                revision: Some("x".into()),
            },
            PendingNotification {
                identifier: "sunrise.reminder.task.x".into(),
                revision: None,
            },
        ])
        .collect();
    let again = reconcile_catalog(&plan, &held);
    assert!(again.add.is_empty() && again.replace.is_empty());
    assert!(!again.is_empty(), "a cancel alone is something to do");
    assert_eq!(
        again.cancel,
        vec!["sunrise.notify.deadline.gone".to_owned()]
    );
    assert_eq!(again.kept.len(), 2);

    let la = TimeZone::get("America/Los_Angeles").unwrap();
    let mut policy = reminder_policy(&[], EntityRef::new(EntityKind::Device, [1; 16]), &[]);
    policy.settings.is_primary_device = true;
    let moved = plan_catalog(
        &policy,
        &catalog_policy(&[]),
        &CatalogInputs {
            now: c.now,
            horizon: c.now + jiff::SignedDuration::from_hours(24),
            tz: &la,
            schedule: &c.schedule,
            deadlines: &[],
            triage: &[],
            sessions: &[],
            clock: ClockStyle::H24,
        },
        &mut NotificationLedger::new(),
    );
    assert_eq!(moved[0].identifier, plan[0].identifier);
    assert_eq!(moved[0].fire_at_ms, ms("2026-03-02T15:15Z"), "07:15 PST");
    let changes = reconcile_catalog(&moved, &held);
    assert_eq!(changes.replace.len(), 2);
    assert!(!changes.is_empty());
}

#[test]
fn the_dst_morning_brief_lands_at_the_wall_clock() {
    // 2026-03-08 springs forward in New York: 07:15 EDT is 11:15Z.
    let c = Case {
        now: ts("2026-03-08T06:00Z"),
        ..Case::new()
    };
    assert_eq!(
        c.kinds()[0],
        (NotificationKind::MorningBrief, ms("2026-03-08T11:15Z"))
    );
}

#[test]
fn the_policy_reads_each_toggle_and_the_entries_list_every_kind() {
    let p = catalog_policy(&[off("notifications.deadline.enabled")]);
    assert!(!p.is_on(NotificationKind::Deadline));
    assert!(p.is_on(NotificationKind::MorningBrief));
    assert!(
        !p.is_on(NotificationKind::TimezoneChanged),
        "off by default"
    );
    assert!(p.is_on(NotificationKind::DevicePaired), "always");
    assert_eq!(
        (p.deadline_lead_s, p.morning_offset_s, p.evening_offset_s),
        (3600, 900, 7200)
    );
    let entries = catalog_entries(&p);
    assert_eq!(entries.len(), NotificationKind::ALL.len());
    assert!(!entries[1].enabled && entries[0].enabled);
    assert_eq!(
        triage_fingerprint(&[task(1), task(2)]),
        triage_fingerprint(&[task(2), task(1)])
    );
    assert_ne!(
        triage_fingerprint(&[task(1)]),
        triage_fingerprint(&[task(1), task(2)])
    );
    assert_eq!(
        catalog_identifier(NotificationKind::TriageNudge, None),
        "sunrise.notify.triage_nudge"
    );
    // FNV-1a over the bytes and a terminating zero, the same on every build.
    assert_eq!(fnv(0xcbf2_9ce4_8422_2325, "a"), 0x089b_e207_b544_f1e4);
}

/// The window is closed at both ends: a deadline due now and one due at the
/// horizon are planned, one a second past it is not; a brief past a short
/// horizon is not planned either.
#[test]
fn the_window_holds_its_ends_and_nothing_past_them() {
    let due = |n: u8, at: &str| DeadlineCandidate {
        task: task(n),
        title: format!("d{n}"),
        hard_due_at: SunriseTime::instant(ts(at)),
    };
    // With a 0 s lead, a timed deadline fires at its own instant.
    let c = Case {
        prefs: vec![
            set("notifications.deadline.lead_s", PrefValue::Uint(0)),
            off("notifications.morning_brief.enabled"),
            off("notifications.evening_plan.enabled"),
        ],
        horizon_s: 3600,
        deadlines: vec![
            due(1, "2026-03-02T10:00Z"),
            due(2, "2026-03-02T11:00Z"),
            due(3, "2026-03-02T11:00:01Z"),
        ],
        ..Case::new()
    };
    let planned: Vec<_> = c
        .plan(&mut NotificationLedger::new())
        .iter()
        .filter_map(|n| n.entity)
        .collect();
    assert_eq!(planned, vec![task(1), task(2)]);

    // 07:15 EST is 2 h 15 min after 05:00: past a 2 h horizon, at a 2 h 15
    // min one.
    let short = Case {
        horizon_s: 2 * 3600,
        ..Case::new()
    };
    assert!(short.kinds().is_empty());
    let exact = Case {
        horizon_s: 2 * 3600 + 15 * 60,
        ..Case::new()
    };
    assert_eq!(
        exact.kinds(),
        vec![(NotificationKind::MorningBrief, ms("2026-03-02T12:15Z"))]
    );
}
