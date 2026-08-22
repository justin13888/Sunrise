//! Read queries.

use serde::{Deserialize, Serialize};
use sunrise_domain::{Context, EffectiveTaskState, Routine, Stream, StreamColor, Task};
use sunrise_id::EntityRef;

/// Read query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Query {
    /// Today view: scheduled blocks, due-today tasks, manually-pulled tasks.
    Today {
        /// "Now" (ms since epoch).
        now_ms: u64,
        /// Filter to these contexts (empty means all).
        contexts: Vec<EntityRef>,
    },
    /// Inbox.
    Inbox,
    /// All tasks in a single Stream.
    StreamTasks(EntityRef),
    /// One entity by id.
    EntityById(EntityRef),
    /// Connected device list.
    DeviceList,
    /// Snapshot of sync status.
    SyncStatus,
    /// All streams (plus the synthetic Inbox row), with open-task counts.
    StreamList,
    /// All live (non-deleted) contexts, with the count of tasks carrying each.
    Contexts,
    /// All live (non-deleted) routines.
    Routines,
    /// Open tasks with their derived dependency state, ordered actionable-first
    /// and then by how much finishing each one would unblock.
    ///
    /// This is the read path for the derived `blocked` / `blocks_others` pair
    /// from `docs/02-domain/tasks.md`: neither is stored on the Task, both are
    /// recomputed here from the dependency index against the blockers' *current*
    /// states, so a blocker completing anywhere (locally or via a merge) flips
    /// its dependents without any repair pass.
    Actionable {
        /// Restrict to one Stream; `None` spans every stream.
        stream: Option<EntityRef>,
        /// Maximum number of rows.
        limit: u32,
    },
    /// Full-text search over tasks.
    Search {
        /// Raw user query text (sanitized before hitting FTS5).
        text: String,
        /// Maximum number of results to return.
        limit: u32,
    },
}

/// Query result. (Not `Deserialize`: some inner types use Cow/static
/// references in their `serde` impls. Query results are one-direction
/// across the FFI seam.)
#[derive(Debug, Clone, Serialize)]
pub enum QueryResult {
    /// `Today` returns a flat list of tasks (UI groups them).
    Tasks(Vec<Task>),
    /// `Inbox` and `StreamTasks` return tasks.
    StreamTasks(Vec<Task>),
    /// `EntityById` may return a stream.
    Stream(Box<Stream>),
    /// `EntityById` may return a task.
    Task(Box<Task>),
    /// `EntityById` may return a routine.
    Routine(Box<Routine>),
    /// `EntityById` may return a context.
    Context(Box<Context>),
    /// `Routines` returns all live routines.
    Routines(Vec<Routine>),
    /// Device list rows: (device_id, nickname, platform, is_revoked).
    Devices(Vec<DeviceRow>),
    /// Sync status snapshot.
    SyncStatus(crate::events::SyncStatus),
    /// `StreamList` returns stream rows (Inbox first).
    Streams(Vec<StreamRow>),
    /// `Contexts` returns context rows, ordered by name.
    Contexts(Vec<ContextRow>),
    /// `Actionable` returns open tasks plus their derived dependency counts.
    Actionable(Vec<ActionableTask>),
}

/// One row of [`Query::Actionable`]: a Task with the two derived dependency
/// facts that never live on the entity itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionableTask {
    /// The task. Its `blocked_by` set is populated from the dependency index.
    pub task: Task,
    /// User-set state widened with the derived `blocked` case.
    pub effective_state: EffectiveTaskState,
    /// How many of `task.blocked_by` are still open. Blockers this replica has
    /// not materialized yet count as open, so an op that arrives before the
    /// task it references still reads as blocked rather than as actionable.
    pub open_blockers: u32,
    /// How many open tasks are waiting on this one — the derived
    /// `blocks_others` cardinality, and the ranking signal a Focus Planner
    /// wants ("what does finishing this release?").
    pub unblocks: u32,
}

/// One row of [`Query::StreamList`]. The synthetic Inbox row uses
/// [`sunrise_domain::inbox_stream_ref`] as its `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRow {
    /// Stream id (the all-zero id for the Inbox row).
    pub id: EntityRef,
    /// Display name ("Inbox" for the synthetic row).
    pub name: String,
    /// Stream color.
    pub color: StreamColor,
    /// Count of open tasks (todo / in-progress, not deleted).
    pub open_task_count: u64,
    /// Whether the stream is archived (always false for Inbox).
    pub archived: bool,
}

/// One row of [`Query::Contexts`].
///
/// Mirrors [`StreamRow`]: identity plus the one count a picker actually needs,
/// so listing contexts never costs a per-row follow-up query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRow {
    /// Context id.
    pub id: EntityRef,
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Archived contexts stay on their tasks but drop out of pickers and
    /// `@name` capture resolution.
    pub archived: bool,
    /// Number of live (non-deleted) tasks carrying this context.
    pub task_count: u64,
}

/// One row of [`Query::DeviceList`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRow {
    /// Device id.
    pub device_id: [u8; 16],
    /// Human-readable nickname.
    pub nickname: String,
    /// Platform string.
    pub platform: String,
    /// Revoked.
    pub revoked: bool,
}
