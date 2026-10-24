//! The undo history against a real vault, with no seam in between.
//!
//! What the `UniFFI` seam's undo tests assert through Swift-shaped records,
//! asserted here on the shared implementation itself, so a second client
//! inherits the guarantee rather than re-proving it.

use std::sync::Arc;

use sunrise_client_core::history::{UndoHistory, UndoLabels};
use sunrise_client_core::NotUndoable;
use sunrise_core::{Clock, Command, Core, CoreConfig, Query, QueryResult, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{TaskDraft, TaskState};
use sunrise_id::EntityRef;

/// 2026-01-01T00:00:00Z in UTC, so nothing here reads the host clock or zone.
#[derive(Debug)]
struct Fixed;

impl Clock for Fixed {
    fn now_ms(&self) -> u64 {
        1_767_225_600_000
    }
    fn timezone(&self) -> String {
        "UTC".to_owned()
    }
}

async fn open(dir: &std::path::Path) -> Core {
    let cfg = CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+test",
        Arc::new(Fixed),
        Arc::new(SystemRng),
    );
    Core::open(
        cfg,
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes([9; 32]),
            paired: None,
        },
    )
    .await
    .expect("open a scratch vault")
}

fn draft(title: &str) -> Command {
    Command::CreateTask(TaskDraft {
        title: title.to_owned(),
        ..TaskDraft::default()
    })
}

async fn task_state(core: &Core, id: EntityRef) -> Option<TaskState> {
    match core.query(Query::EntityById(id)).await {
        Ok(QueryResult::Task(t)) if !t.deleted => Some(t.state),
        _ => None,
    }
}

#[tokio::test]
async fn undo_reopens_a_completion_and_redo_finishes_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();
    let task = core.submit(draft("Renew passport")).await.unwrap().entity;

    let (_, refused) = history
        .submit(&core, Command::CompleteTask(task), "complete".into())
        .await
        .unwrap();
    assert_eq!(refused, None);
    assert_eq!(task_state(&core, task).await, Some(TaskState::Done));
    assert_eq!(history.labels().undo.as_deref(), Some("complete"));

    assert_eq!(
        history.undo(&core).await.unwrap().as_deref(),
        Some("complete")
    );
    assert_eq!(task_state(&core, task).await, Some(TaskState::Todo));
    assert_eq!(
        history.labels(),
        UndoLabels {
            undo: None,
            redo: Some("complete".into())
        }
    );

    assert_eq!(
        history.redo(&core).await.unwrap().as_deref(),
        Some("complete")
    );
    assert_eq!(task_state(&core, task).await, Some(TaskState::Done));
    core.shutdown().await;
}

/// The second undo is the load-bearing half: a redone create is a different
/// entity, and the undo that goes back on the stack must name it.
#[tokio::test]
async fn a_redone_create_is_undone_by_the_id_the_redo_minted() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();

    let (made, refused) = history
        .submit(&core, draft("Draft"), "create".into())
        .await
        .unwrap();
    assert_eq!(refused, None, "a create is reversible");
    history.undo(&core).await.unwrap();
    assert_eq!(
        task_state(&core, made.entity).await,
        None,
        "undo deleted it"
    );

    history.redo(&core).await.unwrap();
    let QueryResult::StreamTasks(inbox) = core.query(Query::Inbox).await.unwrap() else {
        panic!("Inbox answers StreamTasks");
    };
    let remade = inbox
        .iter()
        .find(|t| t.title == "Draft")
        .expect("redo made it")
        .id;
    assert_ne!(remade, made.entity);

    history.undo(&core).await.unwrap();
    assert_eq!(
        task_state(&core, remade).await,
        None,
        "the second undo deleted the remake"
    );
    core.shutdown().await;
}

/// Undo of a delete is a restore (ADR-0065 §3): the task comes back, and
/// redo deletes it again.
#[tokio::test]
async fn undo_of_a_delete_is_a_restore() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();
    let task = core.submit(draft("Gone")).await.unwrap().entity;

    let (_, refused) = history
        .submit(&core, Command::DeleteTask(task), "delete".into())
        .await
        .unwrap();
    assert_eq!(refused, None::<NotUndoable>, "a delete is undoable");
    assert_eq!(task_state(&core, task).await, None);
    assert_eq!(
        history.undo(&core).await.unwrap().as_deref(),
        Some("delete")
    );
    assert_eq!(
        task_state(&core, task).await,
        Some(TaskState::Todo),
        "restored"
    );
    assert_eq!(
        history.redo(&core).await.unwrap().as_deref(),
        Some("delete")
    );
    assert_eq!(task_state(&core, task).await, None, "and deleted again");
    core.shutdown().await;
}
