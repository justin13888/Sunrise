//! The commands whose lowering does more than move a field: `BulkTasks` maps
//! each action to the core's own, `ResolveConflict` reads its value's bytes,
//! and `MoveTasks` hands over two anchors that mean nothing if swapped.
//!
//! What each does to a vault is pinned by `sunrise-core`'s engine tests. These
//! pin that a command from the client side reaches that behaviour as itself.
//! A real two-replica conflict is likewise the engine's to build: settling a
//! field that has none takes the same path through this seam.

use sunrise_core_bindings::command::{BulkTaskActionIn, BulkTaskItemIn};
use sunrise_core_bindings::dto::{TaskEdit, TimeValue};
use sunrise_core_bindings::task_order::{plan_task_move, TaskDrop};
use sunrise_core_bindings::{
    BindingError, CoreCommand, CoreQuery, CoreQueryResult, SunriseCore, TaskItem,
};
use sunrise_domain::TaskState;
use sunrise_id::{EntityKind, EntityRef};

mod support;
use support::{draft, open_core};

async fn create(core: &SunriseCore, title: &str) -> EntityRef {
    core.submit(CoreCommand::CreateTask {
        draft: draft(title),
    })
    .await
    .expect("create")
    .entity
}

async fn task_of(core: &SunriseCore, id: EntityRef) -> TaskItem {
    let CoreQueryResult::Task { task } = core
        .query(CoreQuery::EntityById { id })
        .await
        .expect("entity by id")
    else {
        panic!("a task id must return a task");
    };
    task
}

async fn inbox_ids(core: &SunriseCore) -> Vec<EntityRef> {
    let CoreQueryResult::Tasks { tasks } = core.query(CoreQuery::Inbox).await.expect("inbox")
    else {
        panic!("the inbox must return tasks");
    };
    tasks.into_iter().map(|t| t.id).collect()
}

fn item(id: EntityRef, action: BulkTaskActionIn) -> BulkTaskItemIn {
    BulkTaskItemIn { id, action }
}

fn nobody() -> EntityRef {
    EntityRef::new(EntityKind::Task, [9; 16])
}

// --- BulkTasks -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn each_bulk_action_is_the_command_of_the_same_name() {
    let (_dir, core) = open_core().await;
    let completed = create(&core, "complete").await;
    let deferred = create(&core, "defer").await;
    let edited = create(&core, "edit").await;
    let deleted = create(&core, "delete").await;

    let to =
        jiff::Timestamp::from_millisecond(i64::try_from(core.now_ms()).expect("ms") + 86_400_000)
            .expect("a time");
    core.submit(CoreCommand::BulkTasks {
        items: vec![
            item(completed, BulkTaskActionIn::Complete),
            item(
                deferred,
                BulkTaskActionIn::DeferTo {
                    to: TimeValue::Instant { at: to },
                },
            ),
            item(
                edited,
                BulkTaskActionIn::Update {
                    edit: TaskEdit {
                        title: Some("edited".into()),
                        ..TaskEdit::default()
                    },
                },
            ),
            item(deleted, BulkTaskActionIn::Delete),
        ],
    })
    .await
    .expect("bulk");

    assert_eq!(task_of(&core, completed).await.state, TaskState::Done);
    let task = task_of(&core, deferred).await;
    assert!(task.scheduled_at.is_some(), "the defer set the date");
    assert_eq!(task.deferred_count, 1, "and counted");
    assert_eq!(task_of(&core, edited).await.title, "edited");
    assert!(
        !inbox_ids(&core).await.contains(&deleted),
        "the delete happened"
    );

    core.submit(CoreCommand::BulkTasks {
        items: vec![item(deleted, BulkTaskActionIn::Restore)],
    })
    .await
    .expect("restore");
    assert!(
        inbox_ids(&core).await.contains(&deleted),
        "the restore brought it back"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_refused_item_refuses_the_whole_batch() {
    let (_dir, core) = open_core().await;
    let id = create(&core, "stays open").await;

    let refused = core
        .submit(CoreCommand::BulkTasks {
            items: vec![
                item(id, BulkTaskActionIn::Complete),
                item(nobody(), BulkTaskActionIn::Complete),
            ],
        })
        .await;
    assert!(refused.is_err(), "a task nobody holds refuses the batch");
    assert_eq!(
        task_of(&core, id).await.state,
        TaskState::Todo,
        "and the item ahead of it is not written"
    );

    let empty = core
        .submit(CoreCommand::BulkTasks { items: Vec::new() })
        .await;
    assert!(empty.is_err(), "an empty batch is refused");
    core.shutdown().await;
}

// --- ResolveConflict -------------------------------------------------------

/// A CBOR text of fewer than 24 bytes: its length in the head, then the bytes.
fn cbor_text(text: &str) -> Vec<u8> {
    let mut bytes = vec![0x60 + u8::try_from(text.len()).expect("short")];
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_settles_on_the_value_its_bytes_name() {
    let (_dir, core) = open_core().await;
    let id = create(&core, "before").await;

    let out = core
        .submit(CoreCommand::ResolveConflict {
            entity: id,
            field: "title".into(),
            value: cbor_text("settled"),
        })
        .await
        .expect("resolve");

    assert_eq!(out.entity, id);
    assert_eq!(task_of(&core, id).await.title, "settled");
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_value_that_is_not_canonical_is_refused_at_the_seam() {
    let (_dir, core) = open_core().await;
    let id = create(&core, "before").await;

    // The text "a" with its length in a wider head than it needs: readable
    // CBOR, and not the one encoding every replica would have to agree on.
    let refused = core
        .submit(CoreCommand::ResolveConflict {
            entity: id,
            field: "title".into(),
            value: vec![0x78, 0x01, b'a'],
        })
        .await;

    let Err(BindingError::Core(message)) = refused else {
        panic!("non-canonical bytes are a core error, got {refused:?}");
    };
    assert!(
        message.contains("not canonical CBOR"),
        "the seam's own refusal rather than the core's, got {message}"
    );
    assert_eq!(task_of(&core, id).await.title, "before");
    core.shutdown().await;
}

// --- MoveTasks -------------------------------------------------------------

/// The manual-order keys of `ids`, which must all be placed.
async fn keys(core: &SunriseCore, ids: &[EntityRef]) -> Vec<String> {
    let mut keys = Vec::with_capacity(ids.len());
    for id in ids {
        keys.push(
            task_of(core, *id)
                .await
                .sort_order
                .expect("a moved task has a key"),
        );
    }
    keys
}

/// Whether `ids` read in increasing manual order.
async fn in_order(core: &SunriseCore, ids: &[EntityRef]) -> bool {
    keys(core, ids).await.is_sorted_by(|a, b| a < b)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_lands_between_its_two_anchors() {
    let (_dir, core) = open_core().await;
    let a = create(&core, "a").await;
    let b = create(&core, "b").await;
    let c = create(&core, "c").await;

    core.submit(CoreCommand::MoveTasks {
        ids: vec![a, b, c],
        after: None,
        before: None,
    })
    .await
    .expect("place all three");
    assert!(in_order(&core, &[a, b, c]).await, "in the order given");

    core.submit(CoreCommand::MoveTasks {
        ids: vec![c],
        after: Some(a),
        before: Some(b),
    })
    .await
    .expect("between two anchors");
    assert!(
        in_order(&core, &[a, c, b]).await,
        "below after, above before"
    );

    let settled = keys(&core, &[a, c, b]).await;
    let swapped = core
        .submit(CoreCommand::MoveTasks {
            ids: vec![c],
            after: Some(b),
            before: Some(a),
        })
        .await;
    assert!(swapped.is_err(), "anchors out of manual order are refused");
    assert_eq!(
        keys(&core, &[a, c, b]).await,
        settled,
        "nothing was written"
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_planned_drag_is_a_move_the_core_accepts() {
    let (_dir, core) = open_core().await;
    let a = create(&core, "a").await;
    let b = create(&core, "b").await;
    core.submit(CoreCommand::MoveTasks {
        ids: vec![a, b],
        after: None,
        before: None,
    })
    .await
    .expect("place both");

    let list = vec![task_of(&core, a).await, task_of(&core, b).await];
    let planned = plan_task_move(list, vec![b], TaskDrop::Top).expect("the drop changes the list");
    core.submit(CoreCommand::MoveTasks {
        ids: planned.ids,
        after: planned.after,
        before: planned.before,
    })
    .await
    .expect("move");

    assert!(in_order(&core, &[b, a]).await, "the dragged row is on top");
    core.shutdown().await;
}
