//! Read queries.

use serde::{Deserialize, Serialize};
use sunrise_domain::{
    Context, EffectiveTaskState, Energy, EnergyFit, FocusSession, FocusStats, Routine, SessionPlan,
    Stream, StreamColor, Task, UnblockCascade,
};
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
    /// **Focus Planner**: the ranked queue of what to work on next
    /// (`docs/08-features/focus-mode.md` §Focus Planner).
    ///
    /// Three stacked criteria, all derived and none stored:
    /// 1. **actionable only** — anything with an open blocker never appears,
    ///    so the planner is never a dead end;
    /// 2. **energy-matched** against the session's declared budget, so deep
    ///    work lands in deep-work windows; and
    /// 3. **ranked by leverage** — how much open work finishing it releases —
    ///    then `due_at`, `priority`, `scheduled_at`.
    ///
    /// Shares the dependency walk with [`Query::Actionable`]; the ranking
    /// itself is the pure `sunrise_domain::focus::rank_focus_plan`.
    FocusPlan {
        /// Restrict to one Stream; `None` spans every stream.
        stream: Option<EntityRef>,
        /// The session's declared energy budget. `None` means "no signal" and
        /// energy drops out of the ranking entirely.
        energy: Option<Energy>,
        /// How the proposed session would be sized, used to fill each row's
        /// `suggested` plan (chunk N-of-M included).
        length: sunrise_domain::SessionLength,
        /// Maximum number of rows.
        limit: u32,
    },
    /// Focus sessions for one Task, newest first — including any that are
    /// **still running** (a `start` with no `end`).
    TaskFocusSessions {
        /// Target task.
        task: EntityRef,
        /// Maximum number of rows.
        limit: u32,
    },
    /// Every focus session that has not ended yet, across all tasks.
    ///
    /// A client resuming after a crash reads this to find the session it left
    /// running; a dangling start is a valid state, not a repair case.
    RunningFocusSessions,
    /// **Estimate calibration** and focus totals: a pure fold over the
    /// immutable session log (`docs/08-features/focus-mode.md` §Estimate
    /// calibration), bucketed per Stream and per energy.
    FocusStats {
        /// Restrict to one Stream; `None` spans every stream.
        stream: Option<EntityRef>,
        /// Only fold sessions started at or after this instant (ms since
        /// epoch); `None` folds the whole log.
        since_ms: Option<u64>,
        /// "Now" (ms since epoch), from the injected clock — the only time
        /// that enters the fold, and only to price sessions still running.
        now_ms: u64,
    },
    /// **Unblock cascade**: what completing `task` released
    /// (`docs/08-features/focus-mode.md` §Unblock cascade).
    ///
    /// Recomputes the graph frontier around the task against the blockers'
    /// *current* states, so it is correct whether the completion happened here
    /// or merged in from another device. Informational; it keeps no score.
    UnblockCascade(EntityRef),
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
    /// `FocusPlan` returns the ranked planner queue, best pick first.
    FocusPlan(Vec<FocusPlanRow>),
    /// `TaskFocusSessions` / `RunningFocusSessions` return session views.
    FocusSessions(Vec<FocusSessionRow>),
    /// `FocusStats` returns the folded calibration + totals.
    FocusStats(Box<FocusStats>),
    /// `UnblockCascade` returns what a completion released.
    UnblockCascade(Box<UnblockCascade>),
}

/// One row of [`Query::FocusPlan`]: a proposal, with the two facts that put it
/// where it is and the session the core would open for it.
#[derive(Debug, Clone, Serialize)]
pub struct FocusPlanRow {
    /// The proposed task. Its `blocked_by` set is populated from the index.
    pub task: Task,
    /// How many open tasks finishing this one releases — the leverage signal.
    pub unblocks: u32,
    /// How this task's energy facet scored against the session budget.
    pub energy_fit: EnergyFit,
    /// The session the core would open: planned length and `chunk N of M`.
    pub suggested: SessionPlan,
    /// Work sessions this task has already had, which is what makes the
    /// suggested chunk read "3 of 4" rather than always "1 of 4".
    pub prior_sessions: u32,
}

/// One row of a focus-session query: the assembled
/// [`sunrise_domain::FocusSession`] view plus the two derived numbers a caller
/// would otherwise have to recompute against the clock.
#[derive(Debug, Clone, Serialize)]
pub struct FocusSessionRow {
    /// Start, optional end, and the union of interruptions.
    pub session: FocusSession,
    /// `true` while the session has no `end` op — a valid state.
    pub running: bool,
    /// Focused time: frozen once ended, derived from the clock while running.
    pub focused_ms: u64,
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
