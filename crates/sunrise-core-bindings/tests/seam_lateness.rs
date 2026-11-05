//! A task's three times and its lateness across the seam (ADR-0047, issue
//! #334): the soft deadline on the draft, the edit and the item, Today's
//! groups and each task's lateness as the core reads them, and the ordering
//! warnings an accepted write carries. `flavor = "multi_thread"` for the
//! reason `seam.rs` gives.

use sunrise_core_bindings::dto::{TaskDraftIn, TaskEdit, TaskItem, TimeValue};
use sunrise_core_bindings::lateness::{LatenessKind, TodayGroupItem};
use sunrise_core_bindings::{CoreCommand, CoreQuery, CoreQueryResult, SunriseCore};
use sunrise_id::EntityRef;

mod support;
use support::{draft, open_core};

async fn task(core: &SunriseCore, id: EntityRef) -> TaskItem {
    match core
        .query(CoreQuery::EntityById { id })
        .await
        .expect("read")
    {
        CoreQueryResult::Task { task } => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

async fn lateness(core: &SunriseCore, id: EntityRef) -> LatenessKind {
    match core
        .query(CoreQuery::Lateness { ids: vec![id] })
        .await
        .expect("lateness")
    {
        CoreQueryResult::Lateness { rows } => rows[0].lateness.state,
        other => panic!("expected Lateness, got {other:?}"),
    }
}

/// The instant an `Instant` value holds, as epoch ms.
fn ms_of(v: Option<&TimeValue>) -> Option<i64> {
    match v {
        Some(TimeValue::Instant { at }) => Some(at.as_millisecond()),
        _ => None,
    }
}

fn instant(ms: i64) -> TimeValue {
    TimeValue::Instant {
        at: jiff::Timestamp::from_millisecond(ms).unwrap(),
    }
}

/// A target an hour ago and a hard deadline in a month: late, softly, in
/// Today's Late group and in `CoreQuery.lateness`; the deadline set before
/// the target is accepted with its warning; clearing the target puts the
/// task back on track. `scheduledAt` and `dueAt` still carry the plan and
/// the hard deadline.
#[tokio::test(flavor = "multi_thread")]
async fn the_soft_deadline_and_lateness_cross_the_seam() {
    let (_dir, core) = open_core().await;
    let now = i64::try_from(core.now_ms()).unwrap();
    let hour = 3_600_000;
    let id = core
        .submit(CoreCommand::CreateTask {
            draft: TaskDraftIn {
                target_at: Some(instant(now - hour)),
                due_at: Some(instant(now + 30 * 24 * hour)),
                scheduled_at: Some(instant(now - 2 * hour)),
                ..draft("Send the report")
            },
        })
        .await
        .expect("create")
        .entity;
    let t = task(&core, id).await;
    assert_eq!(ms_of(t.target_at.as_ref()), Some(now - hour));
    assert_eq!(ms_of(t.due_at.as_ref()), Some(now + 30 * 24 * hour));
    assert_eq!(ms_of(t.scheduled_at.as_ref()), Some(now - 2 * hour));

    let rows = match core
        .query(CoreQuery::TodayView {
            now_ms: core.now_ms(),
            contexts: Vec::new(),
        })
        .await
        .expect("today")
    {
        CoreQueryResult::TodayView { rows } => rows,
        other => panic!("expected TodayView, got {other:?}"),
    };
    let [row] = rows.as_slice() else {
        panic!("one row: {rows:?}");
    };
    assert_eq!(row.task.id, id);
    assert_eq!(row.group, TodayGroupItem::Late);
    assert_eq!(row.lateness.state, LatenessKind::LateSoft);
    assert_eq!(
        row.lateness.since,
        Some(jiff::Timestamp::from_millisecond(now - hour).unwrap())
    );

    assert_eq!(lateness(&core, id).await, LatenessKind::LateSoft);

    // A hard deadline before the target: accepted, and said so.
    let outcome = core
        .submit(CoreCommand::UpdateTask {
            id,
            edit: TaskEdit {
                set_due_at: Some(instant(now - 3 * hour)),
                ..TaskEdit::default()
            },
        })
        .await
        .expect("an ordering never refuses");
    assert_eq!(
        outcome.ordering_warnings,
        ["target_after_hard_due", "planned_after_hard_due"]
    );
    assert_eq!(lateness(&core, id).await, LatenessKind::LateHard);

    // Clearing both deadlines leaves a task that is on track.
    core.submit(CoreCommand::UpdateTask {
        id,
        edit: TaskEdit {
            clear_target_at: true,
            clear_due_at: true,
            ..TaskEdit::default()
        },
    })
    .await
    .expect("clear");
    assert!(task(&core, id).await.target_at.is_none());
    assert_eq!(lateness(&core, id).await, LatenessKind::OnTrack);
}

async fn triage_queue(core: &SunriseCore) -> Vec<EntityRef> {
    match core.query(CoreQuery::Triage).await.expect("triage") {
        CoreQueryResult::Triage { rows } => rows.into_iter().map(|r| r.task.id).collect(),
        other => panic!("expected Triage, got {other:?}"),
    }
}

/// The triage queue and its actions across the seam (#335): a late task is
/// in the queue, a batch with a bad id is refused whole, a drop carries its
/// reason onto `TaskItem.cancelReason`, and a Keep takes a task out.
#[tokio::test(flavor = "multi_thread")]
async fn the_triage_queue_and_its_actions_cross_the_seam() {
    use sunrise_core_bindings::lateness::TriageActionIn;
    let (_dir, core) = open_core().await;
    let now = i64::try_from(core.now_ms()).unwrap();
    let late = |title: &str| TaskDraftIn {
        due_at: Some(instant(now - 3_600_000)),
        ..draft(title)
    };
    let a = core
        .submit(CoreCommand::CreateTask { draft: late("a") })
        .await
        .unwrap()
        .entity;
    let b = core
        .submit(CoreCommand::CreateTask { draft: late("b") })
        .await
        .unwrap()
        .entity;
    let mut queued = triage_queue(&core).await;
    queued.sort();
    let mut both = vec![a, b];
    both.sort();
    assert_eq!(queued, both);

    let unknown = EntityRef::new(sunrise_id::EntityKind::Task, [0x42; 16]);
    assert!(core
        .submit(CoreCommand::Triage {
            ids: vec![a, unknown],
            action: TriageActionIn::Keep,
        })
        .await
        .is_err());
    assert_eq!(triage_queue(&core).await.len(), 2, "nothing was kept");

    core.submit(CoreCommand::Triage {
        ids: vec![a],
        action: TriageActionIn::Drop {
            reason: Some("not mine".into()),
        },
    })
    .await
    .unwrap();
    assert_eq!(
        task(&core, a).await.cancel_reason.as_deref(),
        Some("not mine")
    );
    core.submit(CoreCommand::Triage {
        ids: vec![b],
        action: TriageActionIn::Keep,
    })
    .await
    .unwrap();
    assert!(task(&core, b).await.late_acknowledged_at.is_some());
    assert!(triage_queue(&core).await.is_empty());
}
