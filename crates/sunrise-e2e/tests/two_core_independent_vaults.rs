//! Two-Core scenario: each opens its own vault, both submit tasks
//! independently, both queries return the right number of rows.
//!
//! This is the smallest real cross-crate convergence proof until the
//! sync wire layer is connected through the relay (Phase 17 follow-up).
//! It validates that two `Core` instances can run simultaneously in the
//! same process without stepping on each other's vault locks.

#![allow(
    clippy::missing_panics_doc,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::manual_let_else,
    clippy::doc_markdown
)]

use parking_lot::Mutex;
use std::sync::Arc;
use sunrise_core::{Clock, Command, Core, CoreConfig, Query, QueryResult, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{TaskDraft, TaskState};

#[derive(Debug)]
struct FixedClock(Mutex<u64>);
impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        *self.0.lock()
    }
}

fn cfg(dir: &std::path::Path) -> CoreConfig {
    CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+e2e",
        Arc::new(FixedClock(Mutex::new(1_700_000_000_000))),
        Arc::new(SystemRng),
    )
}

fn unlock(seed: u8) -> Unlock {
    Unlock::DevicePaired {
        root: VaultRootKey::from_bytes([seed; 32]),
        paired: None,
    }
}

#[tokio::test]
async fn two_independent_cores_dont_clash() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = Core::open(cfg(dir_a.path()), unlock(1)).await.unwrap();
    let b = Core::open(cfg(dir_b.path()), unlock(2)).await.unwrap();

    a.submit(Command::CreateTask(TaskDraft {
        title: "alice's task".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    a.submit(Command::CreateTask(TaskDraft {
        title: "alice's other".into(),
        ..Default::default()
    }))
    .await
    .unwrap();
    b.submit(Command::CreateTask(TaskDraft {
        title: "bob's task".into(),
        ..Default::default()
    }))
    .await
    .unwrap();

    let inbox_a = match a.query(Query::Inbox).await.unwrap() {
        QueryResult::StreamTasks(v) => v,
        _ => panic!(),
    };
    let inbox_b = match b.query(Query::Inbox).await.unwrap() {
        QueryResult::StreamTasks(v) => v,
        _ => panic!(),
    };
    assert_eq!(inbox_a.len(), 2);
    assert_eq!(inbox_b.len(), 1);
    assert!(inbox_a.iter().all(|t| t.state == TaskState::Todo));
}
