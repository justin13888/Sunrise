#![allow(
    clippy::manual_let_else,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::doc_markdown
)]

//! End-to-end integration test covering the public Core API.
//!
//! Exercises open → submit (CreateTask) → query (Today) → submit
//! (CompleteTask) → query (Today, expecting empty) → close. This is
//! the smallest meaningful proof that the engine pipeline is wired
//! through `Core` end-to-end.

use parking_lot::Mutex;
use std::sync::Arc;
use sunrise_core::{
    Clock, Command, CommandResult, Core, CoreConfig, Query, QueryResult, SystemRng, Unlock,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{TaskDraft, TaskPatch, TaskState};

#[derive(Debug)]
struct FakeClock {
    ms: Mutex<u64>,
}
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        *self.ms.lock()
    }
}

fn cfg(dir: &std::path::Path) -> CoreConfig {
    CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+integration",
        Arc::new(FakeClock {
            ms: Mutex::new(1_700_000_000_000),
        }),
        Arc::new(SystemRng),
    )
}

fn unlock() -> Unlock {
    Unlock::DevicePaired {
        root: VaultRootKey::from_bytes([1u8; 32]),
        paired: None,
    }
}

/// Every Task `Query::Today` answers with at `now_ms`.
async fn today(core: &Core, now_ms: u64) -> Vec<sunrise_domain::Task> {
    match core
        .query(Query::Today {
            now_ms,
            contexts: vec![],
        })
        .await
        .unwrap()
    {
        QueryResult::Tasks(v) => v,
        other => panic!("expected Tasks, got {other:?}"),
    }
}

#[tokio::test]
async fn create_then_complete_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();

    let CommandResult { entity, state, .. } = core
        .submit(Command::CreateTask(TaskDraft {
            title: "ship the engine".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(state, Some(TaskState::Todo));

    // This asserted `1 == 1`: the bound was the literal the match arm
    // produced, and the returned vector was discarded, so `Today` could have
    // answered with nothing or with ten thousand rows and the test still
    // passed. The trailing comment said what the author expected — an
    // unscheduled task is not in Today — so that is what is asserted, and then
    // the other half, that giving it a due date inside the window puts it
    // there. Without the second half "Today always answers empty" passes too.
    assert!(
        today(&core, 1_700_000_000_000).await.is_empty(),
        "the task has neither a scheduled nor a due time, so Today holds nothing"
    );

    core.submit(Command::UpdateTask {
        id: entity,
        patch: TaskPatch {
            due_at: Some(Some(
                jiff::Timestamp::from_millisecond(1_700_000_000_000 + 3_600_000)
                    .unwrap()
                    .into(),
            )),
            ..Default::default()
        },
    })
    .await
    .unwrap();

    let in_today = today(&core, 1_700_000_000_000).await;
    assert_eq!(
        in_today
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        vec!["ship the engine"],
        "due in an hour puts it in Today, and nothing else is there"
    );

    let inbox = core.query(Query::Inbox).await.unwrap();
    let inbox_n = match inbox {
        QueryResult::StreamTasks(v) => v.len(),
        _ => panic!(),
    };
    assert_eq!(inbox_n, 1);

    core.submit(Command::CompleteTask(entity)).await.unwrap();

    let inbox = core.query(Query::Inbox).await.unwrap();
    let inbox_after = match inbox {
        QueryResult::StreamTasks(v) => v,
        _ => panic!(),
    };
    assert_eq!(inbox_after.len(), 1);
    assert_eq!(inbox_after[0].state, TaskState::Done);

    core.close().await.unwrap();
}

#[tokio::test]
async fn changes_event_emitted_on_create() {
    let dir = tempfile::tempdir().unwrap();
    let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
    let mut rx = core.changes();
    let _ = core
        .submit(Command::CreateTask(TaskDraft {
            title: "x".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    let evt = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(evt, sunrise_core::DomainEvent::Created(_)));
}
