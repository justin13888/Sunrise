//! Conflict convergence: two real `Core`s through the real relay, writing
//! concurrent `Patch`es of one field, report the same conflict, and one
//! resolution clears it on both (ADR-0065 §3–§4).
//!
//! Both cores are this build and have told each other so, so the vault
//! requires `core.field_ops` at its first entity write, and every command is
//! a `Patch` naming the heads its writer had applied (§7). Two retitles made
//! before either has seen the other's are then concurrent versions of
//! `title`, a tracked field: one wins on both replicas, and the other is kept
//! and reported. A third field edited concurrently survives beside them.

#![allow(clippy::missing_panics_doc)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::conflicts::EntityConflict;
use sunrise_core::{Clock, Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{TaskDraft, TaskPatch};
use sunrise_e2e::{
    canonical_task_by_id, open_paired_core, open_synced_core, spawn_relay, wait_field_ops,
    wait_live, wait_pending_zero, wait_task_converges, wait_tasks_converge,
};
use sunrise_id::EntityRef;

const ROOT: [u8; 32] = [0x47; 32];
const TIMEOUT: Duration = Duration::from_secs(20);

async fn conflicts(core: &Core) -> Vec<EntityConflict> {
    match core
        .query(Query::Conflicts {
            entity: None,
            include_quiet: false,
        })
        .await
        .expect("conflicts")
    {
        QueryResult::Conflicts(c) => c,
        other => panic!("expected Conflicts, got {other:?}"),
    }
}

async fn edit(core: &Core, id: EntityRef, patch: TaskPatch) {
    core.submit(Command::UpdateTask { id, patch })
        .await
        .expect("update task");
}

/// Wait until both replicas report the same conflicts, and return them.
async fn wait_conflicts_converge(a: &Core, b: &Core) -> Vec<EntityConflict> {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let (ca, cb) = (conflicts(a).await, conflicts(b).await);
        if ca == cb || tokio::time::Instant::now() >= deadline {
            assert_eq!(ca, cb, "both replicas report the same conflicts");
            return ca;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_retitles_are_one_conflict_on_both_and_one_resolve_clears_it() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;
    wait_field_ops(&[&a, &b], TIMEOUT).await;

    // Both retitle before either has seen the other's write, and B also sets
    // the priority. The two writes race the relay; should one reach the other
    // replica before it writes, they are ordered and nothing is in conflict,
    // so the race is run again on a fresh task, a few times at most.
    let retitle = |title: &str| TaskPatch {
        title: Some(title.into()),
        ..Default::default()
    };
    let mut found = None;
    for attempt in 0..3usize {
        let task = a
            .submit(Command::CreateTask(TaskDraft {
                title: format!("Renew passport {attempt}"),
                ..Default::default()
            }))
            .await
            .expect("create task")
            .entity;
        wait_tasks_converge(&a, &b, attempt + 1, TIMEOUT).await;
        tokio::join!(
            edit(&a, task, retitle("Renew passport at the embassy")),
            async {
                edit(&b, task, retitle("Renew passport by post")).await;
                edit(
                    &b,
                    task,
                    TaskPatch {
                        priority: Some(Some(1)),
                        ..Default::default()
                    },
                )
                .await;
            }
        );
        wait_pending_zero(&a, TIMEOUT).await;
        wait_pending_zero(&b, TIMEOUT).await;
        wait_task_converges(&a, &b, task, TIMEOUT).await;
        let t = canonical_task_by_id(&a, task).await.unwrap();
        assert_eq!(t.priority, Some(1), "the edit of another field survives");
        let listed = wait_conflicts_converge(&a, &b).await;
        let titles: Vec<&EntityConflict> = listed
            .iter()
            .filter(|c| c.entity == task && c.field == "title")
            .collect();
        if titles.is_empty() {
            continue;
        }
        assert_eq!(titles.len(), 1, "{listed:?}");
        assert_eq!(titles[0].losers.len(), 1, "{listed:?}");
        let loser = titles[0].losers[0]
            .value
            .clone()
            .expect("the loser's title");
        found = Some((task, loser));
        break;
    }
    let (task, loser) = found.expect("one of three concurrent retitles made a conflict");

    // A settles the field on the loser's title.
    a.submit(Command::ResolveConflict {
        entity: task,
        field: "title".into(),
        value: loser.clone(),
    })
    .await
    .expect("resolve");
    wait_pending_zero(&a, TIMEOUT).await;
    wait_task_converges(&a, &b, task, TIMEOUT).await;
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !(conflicts(&a).await.is_empty() && conflicts(&b).await.is_empty()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resolution cleared the set on both"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let want = loser.get().as_text().map(str::to_owned);
    for core in [&a, &b] {
        let t = canonical_task_by_id(core, task).await.unwrap();
        assert_eq!(Some(t.title), want, "the loser is the title on both");
        assert_eq!(t.priority, Some(1));
    }

    a.shutdown().await;
    b.shutdown().await;
}
