//! Optional stream across the seam (ADR-0046, issue #332): a task with no
//! stream, the synthetic Inbox row the shipped sidebar reads, a stream delete
//! that re-homes, and triage. `flavor = "multi_thread"` for the reason
//! `seam.rs` gives.

use sunrise_core_bindings::dto::{StreamDraftIn, StreamListRow, TaskDraftIn, TaskItem};
use sunrise_core_bindings::vocab::inbox_stream_id;
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

async fn streams(core: &SunriseCore) -> Vec<StreamListRow> {
    match core.query(CoreQuery::StreamList).await.expect("streams") {
        CoreQueryResult::Streams { streams } => streams,
        other => panic!("expected streams, got {other:?}"),
    }
}

async fn inbox(core: &SunriseCore) -> Vec<EntityRef> {
    match core.query(CoreQuery::Inbox).await.expect("inbox") {
        CoreQueryResult::Tasks { tasks } => tasks.into_iter().map(|t| t.id).collect(),
        other => panic!("expected tasks, got {other:?}"),
    }
}

/// A task with no stream reads `home_stream: None` and keeps the legacy Inbox
/// id in `stream_id`; the stream list leads with the synthetic Inbox row,
/// counting the view; deleting a stream into another moves its task there,
/// and a plain delete leaves it with no stream; triage takes a task out of
/// the view and returning it puts it back.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_with_no_stream_crosses_the_seam_the_shipped_client_reads() {
    let (_dir, core) = open_core().await;
    let loose = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Loose"),
        })
        .await
        .expect("create")
        .entity;
    let t = task(&core, loose).await;
    assert_eq!(t.home_stream, None);
    assert_eq!(t.stream_id, inbox_stream_id());
    assert_eq!(t.triaged_at, None);

    let rows = streams(&core).await;
    assert_eq!(rows.len(), 1, "only the synthetic Inbox row: {rows:?}");
    assert_eq!(rows[0].id, inbox_stream_id());
    assert_eq!(rows[0].open_task_count, 1);

    let stream = |name: &str| StreamDraftIn {
        name: name.into(),
        description: None,
        color: None,
        parent_id: None,
        review_cadence: None,
        reminder_lead_s: None,
        icon: None,
        default_context: None,
    };
    let a = core
        .submit(CoreCommand::CreateStream { draft: stream("A") })
        .await
        .expect("create A")
        .entity;
    let b = core
        .submit(CoreCommand::CreateStream { draft: stream("B") })
        .await
        .expect("create B")
        .entity;
    let filed = core
        .submit(CoreCommand::CreateTask {
            draft: TaskDraftIn {
                stream_id: Some(a),
                ..draft("Filed")
            },
        })
        .await
        .expect("create filed")
        .entity;
    assert_eq!(task(&core, filed).await.home_stream, Some(a));
    let rows = streams(&core).await;
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0].id, inbox_stream_id(), "the Inbox row leads");

    core.submit(CoreCommand::DeleteStreamInto { id: a, into: b })
        .await
        .expect("delete A into B");
    assert_eq!(task(&core, filed).await.home_stream, Some(b));
    core.submit(CoreCommand::DeleteStream { id: b })
        .await
        .expect("delete B");
    let t = task(&core, filed).await;
    assert_eq!(t.home_stream, None);
    assert_eq!(t.stream_id, inbox_stream_id());
    let rows = streams(&core).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].open_task_count, 2);

    core.submit(CoreCommand::MarkTriaged { ids: vec![loose] })
        .await
        .expect("triage");
    assert!(task(&core, loose).await.triaged_at.is_some());
    assert_eq!(inbox(&core).await, vec![filed]);
    assert_eq!(streams(&core).await[0].open_task_count, 1);
    core.submit(CoreCommand::ReturnToInbox { ids: vec![loose] })
        .await
        .expect("untriage");
    assert_eq!(task(&core, loose).await.triaged_at, None);
    assert_eq!(inbox(&core).await.len(), 2);
    core.shutdown().await;
}
