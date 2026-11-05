//! The notification catalog against a real vault through the seam (#347):
//! the reminders as before, the catalog kinds beside them, a ledger that
//! round-trips, and a toggle that silences its kind.

use sunrise_core_bindings::dto::TimeValue;
use sunrise_core_bindings::reminders::{ClockStyle, PendingReminder};
use sunrise_core_bindings::CoreCommand;
use sunrise_domain::NotificationKind;

mod support;
use support::{draft, open_core};

#[tokio::test(flavor = "multi_thread")]
async fn every_kind_is_planned_beside_the_reminders_and_reconciles_to_nothing() {
    let (_dir, core) = open_core().await;
    let now = core.now_ms();
    let at = |ms: i64| jiff::Timestamp::from_millisecond(i64::try_from(now).unwrap() + ms).unwrap();
    let mut d = draft("File taxes");
    d.scheduled_at = Some(TimeValue::Instant { at: at(600_000) });
    d.due_at = Some(TimeValue::Instant {
        at: at(4 * 3_600_000),
    });
    core.submit(CoreCommand::CreateTask { draft: d })
        .await
        .unwrap();

    let first = core
        .schedule_notifications(now, true, vec![], vec![], ClockStyle::H24, None, true)
        .await
        .unwrap();
    assert_eq!(first.reminders.desired.len(), 1);
    assert!(first
        .catalog
        .iter()
        .any(|n| n.kind == NotificationKind::Deadline));
    assert!(first.alerts.is_empty(), "a fresh ledger adopts");
    assert_eq!(first.catalog_policy.toggles.len(), 18);

    let held: Vec<PendingReminder> = first
        .reminders
        .desired
        .iter()
        .map(|n| (n.identifier.clone(), n.revision.clone()))
        .chain(
            first
                .catalog
                .iter()
                .map(|n| (n.identifier.clone(), n.revision.clone())),
        )
        .map(|(identifier, revision)| PendingReminder {
            identifier,
            revision: Some(revision),
        })
        .collect();
    let second = core
        .schedule_notifications(
            now,
            true,
            vec![],
            held,
            ClockStyle::H24,
            Some(first.ledger_json.clone()),
            true,
        )
        .await
        .unwrap();
    assert!(second.catalog_changes.add.is_empty() && second.catalog_changes.cancel.is_empty());
    assert!(second.reminders.changes.cancel.is_empty());
    assert_eq!(second.ledger_json, first.ledger_json);

    core.submit(CoreCommand::SetPreference {
        key: "notifications.deadline.enabled".into(),
        value: sunrise_core_bindings::dto::PreferenceValue::Bool { value: false },
        target: sunrise_core_bindings::dto::PreferenceTarget::Device,
    })
    .await
    .unwrap();
    let policy = core.notification_policy().await.unwrap();
    let deadline = policy
        .toggles
        .iter()
        .find(|t| t.kind == NotificationKind::Deadline)
        .unwrap();
    assert!(!deadline.enabled);
    core.shutdown().await;
}
