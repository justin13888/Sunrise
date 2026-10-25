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
//! Both cores are this build and have told each other so, so their vault
//! requires `core.field_ops` and each write is a `Patch` naming the heads its
//! writer had (ADR-0065 §7). A delete concurrent with an edit then wins
//! whatever the clocks say, and the edit is kept inside the tombstone, so a
//! restore shows it (§3). The test asserts that, and that both replicas reach
//! the **same** answer.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, Core, SystemClock};
use sunrise_domain::{TaskDraft, TaskPatch};
use sunrise_e2e::{
    assert_task_converged, canonical_task_by_id, open_paired_core, open_synced_core, spawn_relay,
    wait_field_ops, wait_live, wait_pending_zero, wait_task_converges, wait_tasks_converge,
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
    let b = open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
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
/// Both replicas converge, and the delete wins whichever op the clocks order
/// last: `deleted` is a tombstone register (ADR-0065 §3). The rename is kept
/// inside the tombstone, and a restore brings the task back with it. Under the
/// entity-level LWW of the full-state ops this build writes into a vault an
/// older device shares (clarification D9), the later op's whole state would
/// win instead.
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
/// `TaskDelete` then came to carry full entity state (`DOC_SCHEMA_V = 4`) and
/// apply through the same path as `TaskUpdate`, so the whole row was replaced
/// and both replicas landed on the deleting replica's state. A delete is now a
/// `Patch` of `deleted` alone, and the convergence assertion is the one that
/// used to fail.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_delete_and_update_converge() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    wait_field_ops(&[&a, &b], TIMEOUT).await;
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
    // The delete wins whatever the stamps, and the rename is applied inside
    // the tombstone: two writes to different fields, both kept.
    assert!(ta.deleted, "a delete beats a concurrent edit: {ta:?}");
    assert_eq!(ta.title, "renamed", "the edit is kept inside the tombstone");

    // A restore descends from both and brings the task back with the edit.
    a.submit(Command::Restore(contested))
        .await
        .expect("restore");
    wait_pending_zero(&a, TIMEOUT).await;
    wait_task_converges(&a, &b, contested, TIMEOUT).await;
    let tb = canonical_task_by_id(&b, contested).await.unwrap();
    assert!(!tb.deleted, "restored on B");
    assert_eq!(tb.title, "renamed");

    a.shutdown().await;
    b.shutdown().await;
}
