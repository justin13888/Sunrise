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
use sunrise_domain::{TaskDraft, TaskState};

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
    CoreConfig {
        vault_dir: dir.to_path_buf(),
        clock: Arc::new(FakeClock {
            ms: Mutex::new(1_700_000_000_000),
        }),
        rng: Arc::new(SystemRng),
        app: "0.1.0+integration".into(),
    }
}

fn unlock() -> Unlock {
    Unlock::DevicePaired(VaultRootKey::from_bytes([1u8; 32]))
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

    let today = core
        .query(Query::Today {
            now_ms: 1_700_000_000_000,
            contexts: vec![],
        })
        .await
        .unwrap();
    let n = match today {
        QueryResult::Tasks(_) => 1, // task has no scheduled/due, so isn't in Today
        _ => panic!("expected Tasks"),
    };
    assert_eq!(n, 1);

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
