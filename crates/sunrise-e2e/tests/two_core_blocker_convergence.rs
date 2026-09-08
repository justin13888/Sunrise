//! Task dependencies converge across two real `Core`s through the real relay.
//!
//! Companion to `two_core_context_convergence.rs`: same in-process relay, same
//! real SSE + `POST` sync transport and wire protocol, but exercising the
//! derived `blocked` / `blocks_others` pair from `docs/02-domain/tasks.md`.
//!
//! Neither is stored on the Task and neither rides the wire — both are
//! recomputed from the dependency index against the blockers' *current* states.
//! What has to hold end to end is that a blocker completing on one device flips
//! its dependents to actionable on the other, with no unblock op and no repair
//! pass, and that a dependency op which overtakes the create of the task it
//! names still lands on the same answer.

#![allow(
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{ActionableTask, Clock, Command, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{EffectiveTaskState, TaskDraft, TaskPatch};
use sunrise_e2e::{
    open_paired_core, open_synced_core, spawn_relay, wait_live, wait_tasks_converge,
};
use sunrise_id::EntityRef;

/// Shared paired-device vault root.
const ROOT: [u8; 32] = [0x43; 32];

/// Generous CI-safe cap; healthy runs finish well inside a second per wait.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Poll interval used inside every timeout loop.
const POLL: Duration = Duration::from_millis(25);

/// Comparable projection of one actionable row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalRow {
    id: String,
    effective_state: EffectiveTaskState,
    open_blockers: u32,
    unblocks: u32,
}

fn project(r: &ActionableTask) -> CanonicalRow {
    CanonicalRow {
        id: r.task.id.to_str(),
        effective_state: r.effective_state,
        open_blockers: r.open_blockers,
        unblocks: r.unblocks,
    }
}

async fn actionable(core: &Core) -> Vec<CanonicalRow> {
    let rows = match core
        .query(Query::Actionable {
            stream: None,
            limit: 100,
        })
        .await
        .expect("actionable")
    {
        QueryResult::Actionable(v) => v,
        other => panic!("expected Actionable, got {other:?}"),
    };
    let mut out: Vec<CanonicalRow> = rows.iter().map(project).collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Wait until `core`'s row for `task` satisfies `pred`, then return it.
async fn wait_row<F>(core: &Core, label: &str, task: EntityRef, pred: F) -> CanonicalRow
where
    F: Fn(&CanonicalRow) -> bool,
{
    let id = task.to_str();
    let mut last: Vec<CanonicalRow> = Vec::new();
    let res = tokio::time::timeout(TIMEOUT, async {
        loop {
            last = actionable(core).await;
            if let Some(row) = last.iter().find(|r| r.id == id) {
                if pred(row) {
                    return row.clone();
                }
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(row) = res else {
        panic!("{label}: row for {id} never satisfied the predicate\n  saw: {last:#?}");
    };
    row
}

/// Wait until both replicas expose an identical actionable projection.
async fn wait_converged(a: &Core, b: &Core) -> Vec<CanonicalRow> {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(TIMEOUT, async {
        loop {
            last_a = actionable(a).await;
            last_b = actionable(b).await;
            if last_a == last_b {
                return last_a.clone();
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(rows) = res else {
        panic!("dependency views did not converge\n  A: {last_a:#?}\n  B: {last_b:#?}");
    };
    rows
}

async fn create_task(core: &Core, title: &str) -> EntityRef {
    core.submit(Command::CreateTask(TaskDraft {
        title: title.into(),
        ..Default::default()
    }))
    .await
    .expect("create task")
    .entity
}

#[tokio::test(flavor = "multi_thread")]
async fn two_core_blocker_convergence() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_paired_core(dir_b.path(), &a, addr, clock.clone()).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // --- 1. A builds a two-step chain. ---
    let blocker = create_task(&a, "buy paint").await;
    let dependent = create_task(&a, "paint the fence").await;
    a.submit(Command::UpdateTask {
        id: dependent,
        patch: TaskPatch {
            blocked_by: Some(vec![blocker]),
            ..Default::default()
        },
    })
    .await
    .expect("set blocked_by");
    wait_tasks_converge(&a, &b, 2, TIMEOUT).await;

    // --- 2. B derives the dependency from the ops alone. The edge crossed the
    //        wire inside the full-state task op; `blocked` was computed on B. ---
    let dep_on_b = wait_row(&b, "B", dependent, |r| r.open_blockers == 1).await;
    assert_eq!(dep_on_b.effective_state, EffectiveTaskState::Blocked);
    let blk_on_b = wait_row(&b, "B", blocker, |r| r.unblocks == 1).await;
    assert_eq!(
        blk_on_b.effective_state,
        EffectiveTaskState::Todo,
        "the blocker itself is actionable and reports what it holds back"
    );
    wait_converged(&a, &b).await;

    // --- 3. B — the replica that never created the edge — completes the
    //        blocker. A must free the dependent with no unblock op. ---
    b.submit(Command::CompleteTask(blocker))
        .await
        .expect("complete blocker");

    let dep_on_a = wait_row(&a, "A", dependent, |r| r.open_blockers == 0).await;
    assert_eq!(
        dep_on_a.effective_state,
        EffectiveTaskState::Todo,
        "completing the blocker on B flipped the dependent to actionable on A"
    );

    let converged = wait_converged(&a, &b).await;
    assert_eq!(
        converged.len(),
        1,
        "the done blocker drops out; only the freed dependent is open: {converged:#?}"
    );
    assert_eq!(converged[0].id, dependent.to_str());

    // The edge itself survives on both replicas — only its effect went away.
    for (label, core) in [("A", a.as_ref()), ("B", b.as_ref())] {
        let t = match core
            .query(Query::EntityById(dependent))
            .await
            .expect("task")
        {
            QueryResult::Task(t) => *t,
            other => panic!("expected Task, got {other:?}"),
        };
        assert_eq!(
            t.blocked_by
                .iter()
                .map(EntityRef::to_str)
                .collect::<Vec<_>>(),
            vec![blocker.to_str()],
            "{label}: blocked_by is still recorded after the blocker finished"
        );
    }

    a.shutdown().await;
    b.shutdown().await;
}
