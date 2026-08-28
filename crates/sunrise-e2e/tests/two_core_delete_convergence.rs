//! Delete convergence: two real `Core`s through the real relay, asserting that
//! a *tombstone* replicates and that a concurrent delete-vs-update settles the
//! same way on both replicas.
//!
//! # Why this needed its own suite
//!
//! Every other convergence test here compares [`sunrise_e2e::canonical_tasks`],
//! which is built on the `StreamTasks` query — and that filters `deleted = 0`,
//! correctly, because it is what a UI wants. The side effect is that a deleted
//! task simply disappears from the comparison. "Both replicas deleted it" and
//! "one replica never heard of it" then look identical, and they are the two
//! outcomes a delete test exists to distinguish. So nothing here proved a
//! delete converged at all.
//!
//! These tests compare through [`sunrise_e2e::canonical_task_by_id`], which
//! reads the row tombstone and all.
//!
//! # Concurrent delete vs update
//!
//! A delete is not privileged. It is an op like any other and settles under the
//! same entity-level last-writer-wins rule as a title change (ADR-0014),
//! ordered by `(hlc, device_id, seq)`. The test below therefore does not assert
//! that *delete wins* — it asserts that both replicas reach the **same**
//! answer, whichever it is, because that is the actual invariant. Asserting a
//! fixed winner would be asserting the clock.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, SystemClock};
use sunrise_domain::{TaskDraft, TaskPatch};
use sunrise_e2e::{
    assert_task_converged, canonical_task_by_id, open_synced_core, spawn_relay, trust_each_other,
    wait_live, wait_pending_zero, wait_task_converges, wait_tasks_converge,
};
use sunrise_id::EntityRef;

const ROOT: [u8; 32] = [0x42; 32];
const TIMEOUT: Duration = Duration::from_secs(20);

async fn create_task(core: &Core, title: &str) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

async fn set_title(core: &Core, id: EntityRef, title: &str) {
    core.submit(Command::UpdateTask {
        id,
        patch: TaskPatch {
            title: Some(title.into()),
            ..Default::default()
        },
    })
    .await
    .expect("update task");
}

async fn delete(core: &Core, id: EntityRef) {
    core.submit(Command::DeleteTask(id))
        .await
        .expect("delete task");
}

/// A delete authored on A must reach B as a tombstone — not as a task that
/// merely stopped being listed.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_replicates_as_a_tombstone() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    let keep = create_task(&a, "keep").await;
    let doomed = create_task(&a, "doomed").await;
    wait_tasks_converge(&a, &b, 2, TIMEOUT).await;

    delete(&a, doomed).await;

    // The live view drops to one task on both sides...
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;
    // ...and, the part the live view cannot show, B holds a real tombstone
    // rather than simply never having heard of it.
    wait_task_converges(&a, &b, doomed, TIMEOUT).await;

    let tb = canonical_task_by_id(&b, doomed)
        .await
        .expect("B materialized the deleted task");
    assert!(tb.deleted, "B holds a tombstone, not an absence");
    assert_eq!(tb.title, "doomed", "the row survives its own deletion");

    // The untouched task is unaffected.
    assert_task_converged(&a, &b, keep).await;
    let ka = canonical_task_by_id(&a, keep).await.unwrap();
    assert!(!ka.deleted);

    wait_pending_zero(&a, TIMEOUT).await;
    wait_pending_zero(&b, TIMEOUT).await;
    a.shutdown().await;
    b.shutdown().await;
}

/// A delete on one replica racing an update on the other.
///
/// The assertion is convergence, not a winner: entity-level LWW orders the two
/// ops by `(hlc, device_id, seq)`, and which one lands last is a property of
/// the clock, not of the operation being a delete.
///
/// # Why this shape, and what it used to catch
///
/// This failed roughly one run in four. Both replicas agreed on `deleted` and
/// then disagreed on `title` **forever**:
///
/// ```text
/// a = CanonicalTask { title: "contested", deleted: true }
/// b = CanonicalTask { title: "renamed",   deleted: true }
/// ```
///
/// The cause was that `InnerOp::TaskDelete` carried only the task id, while
/// `TaskCreate`/`TaskUpdate` carried the entity's full state. Entity-level LWW
/// (ADR-0014) is defined as "the winning op's state replaces the entity", and
/// an id-only delete has no state to contribute. So the tombstone was set and
/// the row stamped with the winning LWW stamp while every other column stayed
/// at whatever that replica happened to hold — "contested" on the replica that
/// authored the delete, "renamed" on the one that authored the update. Both
/// then carried the *same* winning stamp, so neither would ever accept a
/// correction: the divergence was stable, not transient.
///
/// It was invisible in the UI, because a tombstoned task is filtered from every
/// read. It stopped being invisible on undelete, export, or any byte-identical
/// convergence check — which is exactly what this suite is.
///
/// `TaskDelete` now carries full entity state (`DOC_SCHEMA_V = 4`) and applies
/// through the same path as `TaskUpdate`, so the whole row is replaced and both
/// replicas land on the deleting replica's state. The assertions below are
/// unchanged from when they were failing.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_delete_and_update_converge() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    let contested = create_task(&a, "contested").await;
    wait_tasks_converge(&a, &b, 1, TIMEOUT).await;

    // Both replicas act on the same task before either has seen the other's op.
    // Issued back to back so the two ops are genuinely in flight together.
    tokio::join!(delete(&a, contested), set_title(&b, contested, "renamed"));

    wait_pending_zero(&a, TIMEOUT).await;
    wait_pending_zero(&b, TIMEOUT).await;
    wait_task_converges(&a, &b, contested, TIMEOUT).await;

    let ta = canonical_task_by_id(&a, contested).await.unwrap();
    let tb = canonical_task_by_id(&b, contested).await.unwrap();
    assert_eq!(
        ta, tb,
        "both replicas must land on the same answer, whichever op won"
    );
    // Whatever won, the outcome must be one of the two authored states — never
    // a blend of them, which is what an accidental field-level merge produces.
    let deleted_won = ta.deleted;
    let renamed_won = ta.title == "renamed";
    assert!(
        deleted_won || renamed_won,
        "converged on neither authored state: {ta:?}"
    );

    a.shutdown().await;
    b.shutdown().await;
}
