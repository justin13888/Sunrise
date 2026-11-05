//! Canonical projections: comparable task and stream tables (table-dump
//! equality) for cross-replica convergence assertions.

use sunrise_core::{Core, Query, QueryResult};
use sunrise_domain::{SunriseTime, Task, TaskState};
use sunrise_id::EntityRef;

/// Canonical, comparable projection of a [`Task`] for cross-replica equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalTask {
    /// Stable task id (prefixed ULID string).
    pub id: String,
    /// Title.
    pub title: String,
    /// User-visible state (`todo` / `in_progress` / `done` / `cancelled`).
    pub state: String,
    /// Owning stream's raw 16-byte id.
    pub stream_id: [u8; 16],
    /// `scheduled_at` in epoch ms, if set.
    pub scheduled_at_ms: Option<i64>,
    /// `due_at` in epoch ms, if set.
    pub due_at_ms: Option<i64>,
    /// Tombstone flag.
    pub deleted: bool,
    /// Optional priority.
    pub priority: Option<u8>,
    /// Scheduling constraints, rendered canonically (stable derived `Debug`).
    pub constraints: Vec<String>,
}

/// Canonical, comparable projection of a stream row for cross-replica equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalStream {
    /// Stream id (prefixed ULID string).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Palette color (lowercase wire form).
    pub color: String,
    /// Archived flag.
    pub archived: bool,
}

fn state_str(s: &TaskState) -> String {
    s.as_str().to_string()
}

fn project_task(t: &Task) -> CanonicalTask {
    CanonicalTask {
        id: t.id.to_str(),
        title: t.title.clone(),
        state: state_str(&t.state),
        // No stream is the legacy Inbox id, which a baseline build reads.
        stream_id: *sunrise_domain::stream_key(t.stream_id).bytes(),
        scheduled_at_ms: t.planned_at.as_ref().map(SunriseTime::index_ms),
        due_at_ms: t.hard_due_at.as_ref().map(SunriseTime::index_ms),
        deleted: t.deleted,
        priority: t.priority,
        constraints: t
            .scheduling_constraints
            .iter()
            .map(|c| format!("{c:?}"))
            .collect(),
    }
}

/// Gather every non-deleted task across the Inbox and all live streams.
async fn all_tasks(core: &Core) -> Vec<Task> {
    let streams = match core.query(Query::StreamList).await.expect("stream list") {
        QueryResult::Streams(v) => v,
        other => panic!("expected Streams, got {other:?}"),
    };
    let mut out = Vec::new();
    // Every stream's tasks, and the tasks of no stream, which the legacy
    // Inbox id names (ADR-0046): the Inbox is a view, not a stream row.
    let homes = streams
        .into_iter()
        .map(|row| row.id)
        .chain(std::iter::once(sunrise_domain::inbox_stream_ref()));
    for home in homes {
        match core
            .query(Query::StreamTasks(home))
            .await
            .expect("stream tasks")
        {
            QueryResult::StreamTasks(ts) => out.extend(ts),
            other => panic!("expected StreamTasks, got {other:?}"),
        }
    }
    out
}

/// Canonical task table for `core`, sorted by id — the equality unit for
/// convergence assertions.
pub async fn canonical_tasks(core: &Core) -> Vec<CanonicalTask> {
    let mut rows: Vec<CanonicalTask> = all_tasks(core).await.iter().map(project_task).collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// Canonical projection of one task **including tombstones**.
///
/// [`canonical_tasks`] is built on `StreamTasks`, which filters `deleted = 0`
/// because that is what a UI wants. The consequence for convergence testing is
/// that a deleted task simply vanishes from the comparison, so "both replicas
/// agree" and "both replicas deleted it" become indistinguishable from "one
/// replica never heard about it at all" — the two outcomes a delete test most
/// needs to tell apart.
///
/// `EntityById` reads the row regardless of its tombstone flag, which makes the
/// delete itself comparable rather than invisible.
///
/// Returns `None` only when the replica has never materialized the task.
pub async fn canonical_task_by_id(core: &Core, id: EntityRef) -> Option<CanonicalTask> {
    match core.query(Query::EntityById(id)).await {
        Ok(QueryResult::Task(t)) => Some(project_task(&t)),
        Ok(other) => panic!("expected Task, got {other:?}"),
        // A replica that has not yet applied the create reports not-found.
        Err(_) => None,
    }
}

/// Assert both replicas agree on a task *including* whether it is deleted.
///
/// # Panics
/// If either replica lacks the task, or the two projections differ.
pub async fn assert_task_converged(a: &Core, b: &Core, id: EntityRef) {
    let ta = canonical_task_by_id(a, id).await;
    let tb = canonical_task_by_id(b, id).await;
    assert!(ta.is_some(), "replica A never materialized {id}");
    assert!(tb.is_some(), "replica B never materialized {id}");
    assert_eq!(ta, tb, "replicas disagree about {id}");
}

/// Canonical stream table for `core`, sorted by id. The Inbox is a view and
/// has no row (ADR-0046).
pub async fn canonical_streams(core: &Core) -> Vec<CanonicalStream> {
    let streams = match core.query(Query::StreamList).await.expect("stream list") {
        QueryResult::Streams(v) => v,
        other => panic!("expected Streams, got {other:?}"),
    };
    let mut rows: Vec<CanonicalStream> = streams
        .into_iter()
        .map(|r| CanonicalStream {
            id: r.id.to_str(),
            name: r.name,
            color: r.color.as_str().to_string(),
            archived: r.archived,
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}
