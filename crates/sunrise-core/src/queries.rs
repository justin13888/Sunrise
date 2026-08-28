//! Read queries.

use serde::{Deserialize, Serialize};
use sunrise_domain::{
    ActivityEvent, Block, Context, DailyReview, EffectiveTaskState, Energy, EnergyFit,
    ExportDataset, ExportFormat, FocusSession, FocusStats, ReviewSnapshot, Routine, SessionPlan,
    Stream, StreamColor, Task, Trends, UnblockCascade, WeeklyReview,
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
    /// All live tasks carrying one Context.
    ///
    /// The Context counterpart of [`Query::StreamTasks`]. Streams partition
    /// tasks and Contexts cut across them
    /// (`docs/02-domain/contexts-and-tags.md`), so "everything tagged
    /// `@errands`" is a first-class listing, not a filter over one Stream —
    /// and without it a Context is only ever reachable as a capture token,
    /// never as a place to stand.
    ContextTasks(EntityRef),
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
    /// **Weekly review** (`docs/08-features/reviews-and-stats.md` §Weekly
    /// review): every step's list, in one read.
    ///
    /// Assembled by the pure `sunrise_domain::build_weekly_review` from four
    /// folds the engine feeds it — the activity feed, the trend fold, the focus
    /// fold and the per-Routine drift measure — so the review can never
    /// disagree with the timeline or the stats screens about what happened.
    WeeklyReview {
        /// Start of the week to review; `None` reviews the week containing
        /// `now_ms`.
        week_start_ms: Option<u64>,
        /// "Now" (ms since epoch), from the injected clock. Fixes the week
        /// grid and prices any still-running focus session.
        now_ms: u64,
    },
    /// **Daily review** (spec §Daily review, off by default): the 60-second
    /// glance — recent captures, today's plan, and which of it is blocked.
    DailyReview {
        /// Start of the glance window (typically yesterday evening).
        since_ms: u64,
        /// "Now" (ms since epoch).
        now_ms: u64,
    },
    /// **Per-Stream trends** (spec §Stats): completed / deferred / created per
    /// week for the last `weeks` civil weeks, whole-vault and per Stream.
    ///
    /// Folded from the op log rather than from `tasks.completed_at`, because
    /// the spec's stability rule ("a re-open decrements the week of the
    /// *original* completion") needs an instant the materialized register no
    /// longer holds.
    StreamTrends {
        /// How many weeks back to report; clamped to a sane maximum.
        weeks: u32,
        /// "Now" (ms since epoch).
        now_ms: u64,
    },
    /// **Activity timeline** for one Task or Stream (spec §Activity timeline):
    /// user-visible ops only, newest first.
    ActivityTimeline {
        /// Task or Stream whose feed to read.
        entity: EntityRef,
        /// Maximum number of rows.
        limit: u32,
    },
    /// Saved review snapshots, newest window first — the spec's "queryable in
    /// History".
    ReviewHistory {
        /// Maximum number of rows.
        limit: u32,
    },
    /// **Export** one stats dataset as JSON or CSV (spec §Export).
    ExportStats {
        /// Which dataset.
        dataset: ExportDataset,
        /// Serialization format.
        format: ExportFormat,
        /// Weeks of history for the trend dataset.
        weeks: u32,
        /// "Now" (ms since epoch).
        now_ms: u64,
    },
    /// **Calendar grid, one day**: every live Block overlapping the civil day
    /// that contains `day_ms`, in the device's zone.
    ///
    /// Overlap, not containment — a block that started yesterday evening and
    /// runs past midnight belongs on today's grid.
    DayBlocks {
        /// Any instant inside the day to show (ms since epoch).
        day_ms: u64,
    },
    /// **Calendar grid, one week**: every live Block overlapping the seven
    /// civil days beginning at the Monday of the week containing `week_ms`.
    ///
    /// Monday-first, matching `WeekGrid` and every other weekly fold. The
    /// window is built from civil dates rather than by adding 7 x 86_400_000,
    /// so a week containing a DST transition is still exactly seven days.
    WeekBlocks {
        /// Any instant inside the week to show (ms since epoch).
        week_ms: u64,
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
    /// `FocusPlan` returns the ranked planner queue, best pick first.
    FocusPlan(Vec<FocusPlanRow>),
    /// `TaskFocusSessions` / `RunningFocusSessions` return session views.
    FocusSessions(Vec<FocusSessionRow>),
    /// `FocusStats` returns the folded calibration + totals.
    FocusStats(Box<FocusStats>),
    /// `UnblockCascade` returns what a completion released.
    UnblockCascade(Box<UnblockCascade>),
    /// `WeeklyReview` returns the assembled five-step review.
    WeeklyReview(Box<WeeklyReview>),
    /// `DailyReview` returns the 60-second glance.
    DailyReview(Box<DailyReview>),
    /// `StreamTrends` returns the folded weekly trends.
    Trends(Box<Trends>),
    /// `ActivityTimeline` returns one entity's feed, newest first.
    Activity(Vec<ActivityEvent>),
    /// `ReviewHistory` returns saved snapshots, newest window first.
    ReviewSnapshots(Vec<ReviewSnapshot>),
    /// `DayBlocks` / `WeekBlocks` return the calendar grid's rows. So does
    /// `EntityById` on a `blk_` id, as a single-element list — a Block is only
    /// ever useful with its resolved title and bound task titles attached, and
    /// a second one-Block variant would be the same row under another name.
    Blocks(Vec<BlockRow>),
    /// `ExportStats` returns the rendered document.
    Export(String),
}

/// One row of [`Query::DayBlocks`] / [`Query::WeekBlocks`].
///
/// Carries the bound Tasks' titles alongside the Block so a grid can label a
/// block without a query per bound task — and because resolving the Block's
/// own title needs them anyway (`docs/02-domain/time-blocks.md` §Block title).
#[derive(Debug, Clone, Serialize)]
pub struct BlockRow {
    /// The Block.
    pub block: Block,
    /// The title to show, after the shadow-copy / `title_track_task` rules.
    pub title: Option<String>,
    /// Titles of the bound live Tasks this replica has materialized, in
    /// `block.tasks` order. Shorter than `block.tasks` when a binding names a
    /// Task whose op has not arrived yet — a valid state, not a repair case.
    pub task_titles: Vec<String>,
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
    /// Whether the stream is paused (always false for Inbox).
    ///
    /// Projected alongside `archived` because a client cannot offer
    /// pause/resume without knowing which one the key means, and reading it
    /// per row through `EntityById` would cost a query per sidebar row.
    pub paused: bool,
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
