//! Contexts converge across two real `Core`s through the real relay.
//!
//! Companion to `two_core_relay_convergence.rs`: same in-process relay, same
//! real WebSockets and wire protocol, but exercising the Context lifecycle —
//! create, rename, and the delete that must strip the Context from every Task
//! carrying it on *both* replicas (per `docs/02-domain/contexts-and-tags.md`),
//! without shipping one op per affected Task.

#![allow(
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::too_many_lines
)]

use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{Clock, Command, ContextRow, Core, Query, QueryResult, SystemClock};
use sunrise_domain::{ContextDraft, ContextPatch, TaskDraft};
use sunrise_e2e::{
    open_synced_core, spawn_relay, trust_each_other, wait_live, wait_tasks_converge,
};
use sunrise_id::EntityRef;

/// Shared paired-device vault root (matches the sibling convergence test).
const ROOT: [u8; 32] = [0x42; 32];

/// Generous CI-safe cap; healthy runs finish well inside a second per wait.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Poll interval used inside every timeout loop.
const POLL: Duration = Duration::from_millis(25);

/// Comparable projection of one context row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CanonicalContext {
    id: String,
    name: String,
    archived: bool,
    task_count: u64,
}

fn project(r: &ContextRow) -> CanonicalContext {
    CanonicalContext {
        id: r.id.to_str(),
        name: r.name.clone(),
        archived: r.archived,
        task_count: r.task_count,
    }
}

async fn canonical_contexts(core: &Core) -> Vec<CanonicalContext> {
    let rows = match core.query(Query::Contexts).await.expect("contexts") {
        QueryResult::Contexts(v) => v,
        other => panic!("expected Contexts, got {other:?}"),
    };
    let mut out: Vec<CanonicalContext> = rows.iter().map(project).collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Wait until both replicas expose identical context tables of exactly
/// `expected_len` rows, then return that converged table.
///
/// The length guard stops a premature match on the trivially-equal empty state
/// before any op has propagated.
async fn wait_contexts_converge(
    a: &Core,
    b: &Core,
    expected_len: usize,
    timeout: Duration,
) -> Vec<CanonicalContext> {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(timeout, async {
        loop {
            last_a = canonical_contexts(a).await;
            last_b = canonical_contexts(b).await;
            if last_a.len() == expected_len && last_a == last_b {
                return last_a.clone();
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(converged) = res else {
        panic!(
            "contexts did not converge to {expected_len} rows in time\n  A ({}): {last_a:#?}\n  B ({}): {last_b:#?}",
            last_a.len(),
            last_b.len()
        );
    };
    converged
}

/// Contexts carried by `task` on `core`, as sorted id strings.
async fn task_contexts(core: &Core, task: EntityRef) -> Vec<String> {
    match core.query(Query::EntityById(task)).await.expect("task") {
        QueryResult::Task(t) => {
            let mut v: Vec<String> = t.contexts.iter().map(EntityRef::to_str).collect();
            v.sort();
            v
        }
        other => panic!("expected Task, got {other:?}"),
    }
}

async fn create_context(core: &Core, name: &str) -> EntityRef {
    core.submit(Command::CreateContext(ContextDraft {
        name: name.into(),
        ..Default::default()
    }))
    .await
    .expect("create context")
    .entity
}

#[tokio::test(flavor = "multi_thread")]
async fn two_core_context_convergence() {
    let (addr, _relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let a = open_synced_core(dir_a.path(), ROOT, addr, clock.clone()).await;
    let b = open_synced_core(dir_b.path(), ROOT, addr, clock.clone()).await;
    trust_each_other(&a, &b).await;
    wait_live(&a, TIMEOUT).await;
    wait_live(&b, TIMEOUT).await;

    // --- 1. A creates two contexts; B learns both. ---
    let errands = create_context(&a, "errands").await;
    let home = create_context(&a, "home").await;
    let converged = wait_contexts_converge(&a, &b, 2, TIMEOUT).await;
    let names: Vec<&str> = converged.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"errands") && names.contains(&"home"),
        "both context names crossed the wire: {names:?}"
    );

    // --- 2. B (the replica that never created them) can tag tasks with them. ---
    let tagged = b
        .submit(Command::CreateTask(TaskDraft {
            title: "buy milk".into(),
            contexts: vec![errands, home],
            ..Default::default()
        }))
        .await
        .expect("create tagged task")
        .entity;
    let solo = b
        .submit(Command::CreateTask(TaskDraft {
            title: "post parcel".into(),
            contexts: vec![errands],
            ..Default::default()
        }))
        .await
        .expect("create task")
        .entity;
    wait_tasks_converge(&a, &b, 2, TIMEOUT).await;
    // `task_contexts` sorts, so the expectation must too. These two ids are
    // ULIDs minted microseconds apart, so they share a timestamp prefix and
    // their relative order is decided by the random suffix — comparing against
    // creation order made this assertion fail about half the time.
    let mut want = vec![errands.to_str(), home.to_str()];
    want.sort();
    assert_eq!(
        task_contexts(&a, tagged).await,
        want,
        "membership rode along with the task op to A"
    );

    // Both replicas agree on the usage counts.
    let converged = wait_contexts_converge(&a, &b, 2, TIMEOUT).await;
    let count_of = |rows: &[CanonicalContext], n: &str| {
        rows.iter().find(|c| c.name == n).expect("row").task_count
    };
    assert_eq!(count_of(&converged, "errands"), 2);
    assert_eq!(count_of(&converged, "home"), 1);

    // --- 3. B renames a context; A converges. ---
    b.submit(Command::UpdateContext {
        id: home,
        patch: ContextPatch {
            name: Some("home & garden".into()),
            ..Default::default()
        },
    })
    .await
    .expect("rename");
    let converged = wait_contexts_converge(&a, &b, 2, TIMEOUT).await;
    assert!(
        converged.iter().any(|c| c.name == "home & garden"),
        "B's rename reached A: {converged:#?}"
    );

    // --- 4. A deletes `errands`. It must vanish from B's list AND from every
    //        task on B that carried it — no per-task op is emitted. ---
    a.submit(Command::DeleteContext(errands))
        .await
        .expect("delete context");
    let converged = wait_contexts_converge(&a, &b, 1, TIMEOUT).await;
    assert_eq!(converged[0].name, "home & garden");

    for (label, core) in [("A", a.as_ref()), ("B", b.as_ref())] {
        assert_eq!(
            task_contexts(core, tagged).await,
            vec![home.to_str()],
            "{label}: the deleted context is stripped, the surviving one stays"
        );
        assert!(
            task_contexts(core, solo).await.is_empty(),
            "{label}: the only-errands task is left with no contexts"
        );
    }

    a.shutdown().await;
    b.shutdown().await;
}
