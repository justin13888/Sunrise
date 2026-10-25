//! The whole re-plan against a vault.

use super::*;
use crate::reminders::{identifier, primary_claim};
use std::sync::Arc;
use sunrise_core::{Command, CoreConfig, Unlock};
use sunrise_domain::{PrefTarget, PrefValue, ReminderKind, StreamDraft, SunriseTime, TaskDraft};

/// 2026-06-01T12:00:00Z.
const NOW: u64 = 1_780_315_200_000;

fn now() -> u64 {
    NOW
}

async fn vault(dir: &std::path::Path) -> Core {
    Core::open(
        CoreConfig::with_clock(
            dir.to_path_buf(),
            "test",
            Arc::new(crate::host::HostClock::new(now, "America/Toronto")),
            Arc::new(sunrise_core::SystemRng),
        ),
        Unlock::DevicePaired {
            root: sunrise_crypto::keys::VaultRootKey::from_bytes([4; 32]),
            paired: None,
        },
    )
    .await
    .unwrap()
}

async fn task_in(core: &Core, name: &str, minutes: i64) -> (EntityRef, EntityRef) {
    let stream = core
        .submit(Command::CreateStream(StreamDraft {
            name: name.into(),
            ..StreamDraft::default()
        }))
        .await
        .unwrap()
        .entity;
    let at =
        jiff::Timestamp::from_millisecond(i64::try_from(NOW).unwrap() + minutes * 60_000).unwrap();
    let task = core
        .submit(Command::CreateTask(TaskDraft {
            title: format!("{name} task"),
            stream_id: Some(stream),
            planned_at: Some(SunriseTime::instant(at)),
            ..TaskDraft::default()
        }))
        .await
        .unwrap()
        .entity;
    (stream, task)
}

fn request(pending: Vec<PendingNotification>) -> ScheduleRequest {
    ScheduleRequest {
        now_ms: NOW,
        authorized: true,
        focus_scope: None,
        pending,
        clock: ClockStyle::H24,
        ledger: NotificationLedger::new(),
        network_available: true,
    }
}

fn held(s: &Schedule) -> Vec<PendingNotification> {
    s.desired
        .iter()
        .map(|n| PendingNotification {
            identifier: n.identifier.clone(),
            revision: Some(n.revision.clone()),
        })
        .collect()
}

/// The whole loop against a vault: plan, hold, re-plan to nothing, mute a
/// stream under a Focus, and go quiet when the switch or the OS says so.
#[tokio::test]
async fn a_vaults_reminders_are_planned_muted_and_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let core = vault(dir.path()).await;
    let (work, _) = task_in(&core, "Work", 10).await;
    let (_, home_task) = task_in(&core, "Home", 20).await;

    let first = schedule(&core, request(Vec::new())).await.unwrap();
    assert_eq!(first.desired.len(), 2);
    assert_eq!(first.reconciliation.add.len(), 2);
    assert!(first.policy.claim_primary);
    assert!(!first.zone_unknown);

    let again = schedule(&core, request(held(&first))).await.unwrap();
    assert!(
        again.reconciliation.is_empty(),
        "{:?}",
        again.reconciliation
    );

    let focused = schedule(
        &core,
        ScheduleRequest {
            focus_scope: crate::focus::scope([work]),
            ..request(held(&first))
        },
    )
    .await
    .unwrap();
    assert_eq!(focused.desired.len(), 1);
    assert_eq!(
        focused.reconciliation.cancel,
        [identifier(home_task, ReminderKind::Task)],
        "a muted reminder is withdrawn, not merely not added"
    );

    let refused = schedule(
        &core,
        ScheduleRequest {
            authorized: false,
            ..request(held(&first))
        },
    )
    .await
    .unwrap();
    assert!(refused.desired.is_empty());
    assert_eq!(refused.reconciliation.cancel.len(), 2);

    core.submit(Command::SetPreference {
        key: "notifications.enabled".into(),
        value: PrefValue::Bool(false),
        target: PrefTarget::Device,
    })
    .await
    .unwrap();
    let off = schedule(&core, request(held(&first))).await.unwrap();
    assert!(off.desired.is_empty() && !off.policy.enabled);
    assert_eq!(off.reconciliation.cancel.len(), 2);
    core.shutdown().await;
}

/// Another device holding the register silences this one.
#[tokio::test]
async fn a_device_that_is_not_primary_plans_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let core = vault(dir.path()).await;
    task_in(&core, "Work", 10).await;
    let (key, value, target) = primary_claim(EntityRef::new(EntityKind::Device, [9; 16]));
    core.submit(Command::SetPreference {
        key: key.into(),
        value,
        target,
    })
    .await
    .unwrap();
    let s = schedule(&core, request(Vec::new())).await.unwrap();
    assert!(!s.policy.settings.is_primary_device);
    assert!(s.desired.is_empty());
    assert!(
        s.wind_down.is_none(),
        "a device that is not primary is silent"
    );
    core.shutdown().await;
}

/// #301's trigger against a vault: once a sleep time is set the plan
/// carries a wind-down an hour before it, and a zone change re-plans it to
/// the same wall clock in the new zone under the same identifier.
#[tokio::test]
async fn a_sleep_time_plans_a_wind_down_that_follows_the_zone() {
    let dir = tempfile::tempdir().unwrap();
    let core = vault(dir.path()).await;
    let none = schedule(&core, request(Vec::new())).await.unwrap();
    assert!(none.wind_down.is_none(), "no sleep time, no wind-down");
    assert_eq!(none.wind_down_change, WindDownChange::Keep);

    core.submit(Command::SetPreference {
        key: "day_schedule.weekday.MO".into(),
        value: PrefValue::DayWindow {
            wake: jiff::civil::time(7, 0, 0, 0),
            sleep: jiff::civil::time(23, 0, 0, 0),
        },
        target: PrefTarget::Vault,
    })
    .await
    .unwrap();
    // 2026-06-01 is a Monday: 23:00 EDT in Toronto is 03:00Z the next day.
    let toronto = schedule(&core, request(Vec::new())).await.unwrap();
    let w = toronto.wind_down.clone().unwrap();
    assert_eq!(w.fire_at_ms, 1_780_365_600_000, "22:00 EDT");
    assert!(matches!(toronto.wind_down_change, WindDownChange::Add(_)));
    assert!(
        toronto
            .reconciliation
            .add
            .iter()
            .all(|n| n.identifier != w.identifier),
        "the reminder reconcile never names it"
    );

    let held = vec![PendingNotification {
        identifier: w.identifier.clone(),
        revision: Some(w.revision.clone()),
    }];
    let again = schedule(&core, request(held.clone())).await.unwrap();
    assert_eq!(again.wind_down_change, WindDownChange::Keep);
    assert!(
        again.reconciliation.cancel.is_empty(),
        "a held wind-down is not one of the reminders to withdraw"
    );

    core.on_time_zone_changed("America/Vancouver").unwrap();
    let moved = schedule(&core, request(held)).await.unwrap();
    let m = moved.wind_down.clone().unwrap();
    assert_eq!(m.identifier, w.identifier);
    assert_eq!(m.fire_at_ms, 1_780_376_400_000, "22:00 PDT");
    assert!(matches!(moved.wind_down_change, WindDownChange::Replace(_)));

    core.submit(Command::SetPreference {
        key: "notifications.wind_down.enabled".into(),
        value: PrefValue::Bool(false),
        target: PrefTarget::Device,
    })
    .await
    .unwrap();
    let off = schedule(
        &core,
        request(vec![PendingNotification {
            identifier: m.identifier.clone(),
            revision: Some(m.revision),
        }]),
    )
    .await
    .unwrap();
    assert!(off.wind_down.is_none());
    assert_eq!(off.wind_down_change, WindDownChange::Cancel);
    core.shutdown().await;
}

/// The catalog against a vault: the briefs and a deadline beside the
/// reminders, a focus session holding both, a re-plan with what is held
/// changing nothing, and the ledger carried across.
#[tokio::test]
async fn the_catalog_is_planned_beside_the_reminders_and_focus_holds_both() {
    let dir = tempfile::tempdir().unwrap();
    let core = vault(dir.path()).await;
    let (_, task) = task_in(&core, "Work", 10).await;
    let due =
        jiff::Timestamp::from_millisecond(i64::try_from(NOW).unwrap() + 3 * 3_600_000).unwrap();
    let taxes = core
        .submit(Command::CreateTask(TaskDraft {
            title: "File taxes".into(),
            hard_due_at: Some(SunriseTime::instant(due)),
            ..TaskDraft::default()
        }))
        .await
        .unwrap()
        .entity;

    let first = schedule(&core, request(Vec::new())).await.unwrap();
    let kinds: Vec<_> = first.catalog.iter().map(|n| n.kind).collect();
    assert_eq!(
        kinds,
        [
            NotificationKind::MorningBrief,
            NotificationKind::Deadline,
            NotificationKind::EveningPlan
        ],
        "08:00 and 20:00 Toronto with no day schedule, the deadline an hour before"
    );
    assert_eq!(first.catalog[0].fire_at_ms, i64::try_from(NOW).unwrap());
    assert_eq!(first.catalog[1].entity, Some(taxes));
    assert_eq!(first.catalog_changes.add.len(), 3);
    assert!(first.alerts.is_empty() && first.ledger.initialized);
    assert_eq!(first.ledger.last_zone.as_deref(), Some("America/Toronto"));

    let mut pending = held(&first);
    pending.extend(first.catalog.iter().map(|n| PendingNotification {
        identifier: n.identifier.clone(),
        revision: Some(n.revision.clone()),
    }));
    let again = schedule(
        &core,
        ScheduleRequest {
            ledger: first.ledger.clone(),
            ..request(pending.clone())
        },
    )
    .await
    .unwrap();
    assert!(
        again.catalog_changes.is_empty(),
        "{:?}",
        again.catalog_changes
    );
    assert!(
        again.reconciliation.is_empty(),
        "neither reconcile cancels the other's"
    );

    // A 25-minute session from now holds the task reminder and the brief
    // until 30 s after it; the deadline, after it, is untouched.
    core.submit(Command::StartFocus(sunrise_core::FocusStartDraft {
        task_id: task,
        kind: sunrise_domain::FocusKind::Work,
        length: sunrise_domain::SessionLength::OnePomodoro,
        energy: None,
    }))
    .await
    .unwrap();
    let released = i64::try_from(NOW).unwrap() + 25 * 60_000 + 30_000;
    let focused = schedule(&core, request(pending)).await.unwrap();
    let reminder = &focused.desired[0];
    assert_eq!(reminder.fire_at_ms, released);
    assert!(
        reminder.body.starts_with(crate::reminders::FOCUS_NOTE),
        "{}",
        reminder.body
    );
    assert_eq!(focused.reconciliation.replace.len(), 1);
    assert_eq!(focused.catalog[0].kind, NotificationKind::MorningBrief);
    assert_eq!(focused.catalog[0].fire_at_ms, released);
    assert_eq!(focused.catalog[1].fire_at_ms, first.catalog[1].fire_at_ms);
    assert_eq!(focused.catalog_changes.replace.len(), 1);

    core.submit(Command::SetPreference {
        key: "notifications.deadline.enabled".into(),
        value: PrefValue::Bool(false),
        target: PrefTarget::Device,
    })
    .await
    .unwrap();
    let off = schedule(&core, request(Vec::new())).await.unwrap();
    assert!(off
        .catalog
        .iter()
        .all(|n| n.kind != NotificationKind::Deadline));
    assert!(!off.catalog_policy.is_on(NotificationKind::Deadline));
    core.shutdown().await;
}

/// The cap is over reminders and catalog notifications together, the
/// earliest kept, ties on the identifier.
#[test]
fn the_cap_keeps_the_earliest_of_both_lists() {
    let reminder = |ms: i64, id: &str| PlannedNotification {
        identifier: id.to_owned(),
        revision: String::new(),
        title: String::new(),
        body: String::new(),
        category: crate::reminders::ReminderCategory::Task,
        kind: ReminderKind::Task,
        entity: EntityRef::new(EntityKind::Task, [1; 16]),
        fire_at_ms: ms,
        deferred_from_ms: None,
    };
    let brief = |ms: i64, id: &str| CatalogNotification {
        identifier: id.to_owned(),
        revision: String::new(),
        kind: NotificationKind::MorningBrief,
        title: String::new(),
        body: String::new(),
        entity: None,
        subject: None,
        count: None,
        link: String::new(),
        actions: Vec::new(),
        level: sunrise_domain::InterruptionLevel::Active,
        fire_at_ms: ms,
        deferred_from_ms: None,
    };
    let (r, c) = cap(
        vec![reminder(1, "a"), reminder(3, "c"), reminder(4, "d")],
        vec![brief(2, "b"), brief(3, "b2")],
        3,
    );
    assert_eq!(
        r.iter().map(|n| n.identifier.as_str()).collect::<Vec<_>>(),
        ["a"]
    );
    assert_eq!(
        c.iter().map(|n| n.identifier.as_str()).collect::<Vec<_>>(),
        ["b", "b2"],
        "b2 sorts before c at the same instant"
    );
    let (r, c) = cap(vec![reminder(1, "a")], vec![brief(2, "b")], 3);
    assert_eq!((r.len(), c.len()), (1, 1));
}

/// The vault's triage queue reaches the morning brief: a task past its hard
/// deadline is counted in the brief's line.
#[tokio::test]
async fn the_triage_queue_is_folded_into_the_morning_brief() {
    let dir = tempfile::tempdir().unwrap();
    let core = vault(dir.path()).await;
    let past = jiff::Timestamp::from_millisecond(i64::try_from(NOW).unwrap() - 86_400_000).unwrap();
    core.submit(Command::CreateTask(TaskDraft {
        title: "Renew passport".into(),
        hard_due_at: Some(SunriseTime::instant(past)),
        ..TaskDraft::default()
    }))
    .await
    .unwrap();
    let s = schedule(&core, request(Vec::new())).await.unwrap();
    let brief = s
        .catalog
        .iter()
        .find(|n| n.kind == NotificationKind::MorningBrief)
        .unwrap();
    assert_eq!(brief.count, Some(1));
    assert_eq!(brief.body, "1 task needs a decision");
    core.shutdown().await;
}
