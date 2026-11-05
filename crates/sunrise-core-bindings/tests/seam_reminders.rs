//! Reminder scheduling, the briefs and the daily glance, against a real vault
//! through the seam.

use sunrise_core_bindings::dto::TimeValue;
use sunrise_core_bindings::reminders::{ClockStyle, PendingReminder};
use sunrise_core_bindings::CoreCommand;

mod support;
use support::{draft, open_core};

#[tokio::test(flavor = "multi_thread")]
async fn a_schedule_reconciles_to_nothing_the_second_time() {
    let (_dir, core) = open_core().await;
    let now = core.now_ms();
    let mut d = draft("Renew passport");
    d.scheduled_at = Some(TimeValue::Instant {
        at: jiff::Timestamp::from_millisecond(i64::try_from(now).unwrap() + 600_000).unwrap(),
    });
    core.submit(CoreCommand::CreateTask { draft: d })
        .await
        .unwrap();

    let first = core
        .schedule_reminders(now, true, vec![], vec![], ClockStyle::H24)
        .await
        .unwrap();
    assert_eq!(first.desired.len(), 1);
    assert_eq!(first.changes.add.len(), 1);
    assert!(first.policy.enabled && first.policy.claim_primary);

    let held = first
        .desired
        .iter()
        .map(|n| PendingReminder {
            identifier: n.identifier.clone(),
            revision: Some(n.revision.clone()),
        })
        .collect();
    let second = core
        .schedule_reminders(now, true, vec![], held, ClockStyle::H24)
        .await
        .unwrap();
    assert!(second.changes.add.is_empty());
    assert!(second.changes.replace.is_empty());
    assert!(second.changes.cancel.is_empty());
    assert_eq!(second.changes.kept.len(), 1);

    let plan = core
        .reminder_plan(now, now + 86_400_000, first.policy.settings.clone())
        .unwrap();
    assert_eq!(plan.reminders.len(), 1);
    assert!(plan.unscheduled.is_empty());

    core.claim_primary_reminder_device().await.unwrap();
    let policy = core.reminder_policy().await.unwrap();
    assert!(policy.settings.is_primary_device && !policy.claim_primary);
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_briefs_and_the_glance_answer_in_the_readers_zone() {
    let (_dir, core) = open_core().await;
    let id = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Ship the thing"),
        })
        .await
        .unwrap()
        .entity;
    core.submit(CoreCommand::CompleteTask { id }).await.unwrap();
    let now = core.now_ms();

    let morning = core.morning_brief(now).await.unwrap();
    let ids: Vec<_> = morning.sections.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["yesterday", "today", "triage", "due"]);
    assert_eq!(
        morning.sections[1].tasks.len(),
        1,
        "finished today, not yesterday"
    );
    assert!(morning.headline.starts_with("1 completed since yesterday"));

    let evening = core.evening_brief(now).await.unwrap();
    assert_eq!(evening.sections.len(), 3);

    let glance = core.daily_review(now).await.unwrap();
    // Yesterday's midnight: at least a (23-hour, on a DST day) day back.
    assert!(now - glance.window.start_ms >= 23 * 3_600_000, "{glance:?}");
    assert!(now - glance.window.start_ms <= 49 * 3_600_000, "{glance:?}");

    let name = core.export_filename(
        sunrise_domain::ExportDataset::Trends,
        sunrise_domain::ExportFormat::Csv,
        now,
    );
    assert!(name.starts_with("sunrise-trends-"), "{name}");
    assert_eq!(std::path::Path::new(&name).extension().unwrap(), "csv");
    core.shutdown().await;
}
