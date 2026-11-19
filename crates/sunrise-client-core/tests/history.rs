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
use sunrise_domain::{PlaceDraft, PlacePatch, TaskDraft, TaskState};
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

/// The live place `id`'s name and label, or `None` once it is gone.
async fn place(core: &Core, id: EntityRef) -> Option<(String, Option<String>)> {
    match core.query(Query::EntityById(id)).await {
        Ok(QueryResult::Places(p)) => p
            .into_iter()
            .find(|p| !p.deleted)
            .map(|p| (p.name, p.address_label)),
        _ => None,
    }
}

/// A place is undoable like every other entity a user makes: the create by
/// a delete, an edit by writing back what it touched (a label it set, by
/// clearing it), and a delete by a restore.
#[tokio::test]
async fn place_commands_are_undone_and_redone() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();

    let (made, refused) = history
        .submit(
            &core,
            Command::CreatePlace(PlaceDraft {
                name: "Office".into(),
                lat_e7: 437_000_000,
                lon_e7: -794_000_000,
                radius_m: 150,
                address_label: None,
            }),
            "create".into(),
        )
        .await
        .unwrap();
    assert_eq!(refused, None, "a place's create is undoable");
    let id = made.entity;
    history.undo(&core).await.unwrap();
    assert_eq!(place(&core, id).await, None, "undo deleted it");
    history.redo(&core).await.unwrap();
    let QueryResult::Places(live) = core.query(Query::Places).await.unwrap() else {
        panic!("Places answers Places");
    };
    let id = live.first().expect("redo made it").id;

    let (_, refused) = history
        .submit(
            &core,
            Command::UpdatePlace {
                id,
                patch: PlacePatch {
                    name: Some("Home".into()),
                    address_label: Some(Some("1 Main St".into())),
                    ..PlacePatch::default()
                },
            },
            "rename".into(),
        )
        .await
        .unwrap();
    assert_eq!(refused, None, "a place's edit is undoable");
    assert_eq!(
        place(&core, id).await,
        Some(("Home".into(), Some("1 Main St".into())))
    );
    history.undo(&core).await.unwrap();
    assert_eq!(
        place(&core, id).await,
        Some(("Office".into(), None)),
        "the old name back, and the label it never had cleared"
    );
    history.redo(&core).await.unwrap();
    assert_eq!(
        place(&core, id).await,
        Some(("Home".into(), Some("1 Main St".into())))
    );

    let (_, refused) = history
        .submit(&core, Command::DeletePlace(id), "delete".into())
        .await
        .unwrap();
    assert_eq!(refused, None, "a place's delete is undoable");
    assert_eq!(place(&core, id).await, None);
    history.undo(&core).await.unwrap();
    assert_eq!(
        place(&core, id).await,
        Some(("Home".into(), Some("1 Main St".into()))),
        "restored as it was"
    );
    core.shutdown().await;
}

async fn in_inbox(core: &Core, id: EntityRef) -> bool {
    let QueryResult::StreamTasks(inbox) = core.query(Query::Inbox).await.unwrap() else {
        panic!("Inbox answers StreamTasks");
    };
    inbox.iter().any(|t| t.id == id)
}

/// A triage takes a task out of the Inbox and its undo puts it back; redo
/// takes it out again. The rows the inverse is read from have to be read for
/// it, or the step is refused and nothing goes on the stack.
#[tokio::test]
async fn a_triage_is_undone_and_redone() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();
    let task = core.submit(draft("Call mum")).await.unwrap().entity;
    assert!(in_inbox(&core, task).await);

    let (_, refused) = history
        .submit(
            &core,
            Command::MarkTriaged { ids: vec![task] },
            "triage".into(),
        )
        .await
        .unwrap();
    assert_eq!(refused, None, "a triage is undoable");
    assert!(!in_inbox(&core, task).await);

    assert_eq!(
        history.undo(&core).await.unwrap().as_deref(),
        Some("triage")
    );
    assert!(in_inbox(&core, task).await, "undo returned it to the Inbox");
    assert_eq!(
        history.redo(&core).await.unwrap().as_deref(),
        Some("triage")
    );
    assert!(!in_inbox(&core, task).await, "redo triaged it again");
    core.shutdown().await;
}

/// The other direction: returning a task to the Inbox is undone by triaging
/// it again.
#[tokio::test]
async fn a_return_to_the_inbox_is_undone_and_redone() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();
    let task = core.submit(draft("Call mum")).await.unwrap().entity;
    core.submit(Command::MarkTriaged { ids: vec![task] })
        .await
        .unwrap();

    let (_, refused) = history
        .submit(
            &core,
            Command::ReturnToInbox { ids: vec![task] },
            "untriage".into(),
        )
        .await
        .unwrap();
    assert_eq!(refused, None, "an untriage is undoable");
    assert!(in_inbox(&core, task).await);

    history.undo(&core).await.unwrap();
    assert!(!in_inbox(&core, task).await, "undo triaged it again");
    history.redo(&core).await.unwrap();
    assert!(in_inbox(&core, task).await, "redo returned it again");
    core.shutdown().await;
}

/// A triage step survives the written history a client whose session spans
/// processes keeps: written, read back into a fresh history, and undone there.
#[tokio::test]
async fn a_triage_step_is_undone_from_a_written_history() {
    use sunrise_client_core::history::persist;

    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();
    let task = core.submit(draft("Call mum")).await.unwrap().entity;
    history
        .submit(
            &core,
            Command::MarkTriaged { ids: vec![task] },
            "triage".into(),
        )
        .await
        .unwrap();

    let (json, dropped) = persist::to_json(&history.snapshot()).unwrap();
    assert_eq!(dropped, 0, "the step is written, not left out");
    let later = UndoHistory::from_stacks(persist::from_json(&json).unwrap());
    assert_eq!(later.labels().undo.as_deref(), Some("triage"));
    later.undo(&core).await.unwrap();
    assert!(in_inbox(&core, task).await);
    core.shutdown().await;
}

/// A triage that changes nothing (the task is already triaged) has nothing
/// to undo, and says so rather than recording a step that does nothing.
#[tokio::test]
async fn a_triage_that_changes_nothing_is_refused_for_undo() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let history = UndoHistory::new();
    let task = core.submit(draft("Call mum")).await.unwrap().entity;
    core.submit(Command::MarkTriaged { ids: vec![task] })
        .await
        .unwrap();

    let (_, refused) = history
        .submit(
            &core,
            Command::MarkTriaged { ids: vec![task] },
            "triage".into(),
        )
        .await
        .unwrap();
    assert_eq!(refused, Some(NotUndoable::Unsupported));
    assert_eq!(history.labels(), UndoLabels::default());
    core.shutdown().await;
}
