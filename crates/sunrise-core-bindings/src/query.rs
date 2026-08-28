//! The read surface: every [`sunrise_core::Query`] and its result.
//!
//! Same rule as [`crate::command`] — one variant per query, no aggregation.
//! Anything a client can ask the core, it can ask across the seam.

use sunrise_core::{Query, QueryResult};
use sunrise_domain::{Energy, ExportDataset, ExportFormat, SessionLength};
use sunrise_id::EntityRef;

use crate::dto::{
    ActionableTaskRow, ActivityRow, AttachmentItem, BlockGridRow, Cascade, ContextItem,
    ContextListRow, DailyReviewReport, DeviceListRow, FocusTotals, PlanRow, RoutineItem,
    SessionRow, Snapshot, StreamItem, StreamListRow, SyncSnapshot, TaskItem, TrendReport,
    WeeklyReviewReport,
};

/// A read query.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CoreQuery {
    /// Today: what is scheduled, due, or pulled in for today.
    Today {
        /// "Now" (epoch ms), from [`crate::SunriseCore::now_ms`].
        now_ms: u64,
        /// Narrow to these contexts; empty means all.
        contexts: Vec<EntityRef>,
    },
    /// The Inbox.
    Inbox,
    /// Every task in one stream.
    StreamTasks {
        /// The stream.
        stream: EntityRef,
    },
    /// Every live task carrying one context.
    ContextTasks {
        /// The context.
        context: EntityRef,
    },
    /// One entity by id.
    EntityById {
        /// The entity.
        id: EntityRef,
    },
    /// Paired devices.
    DeviceList,
    /// Sync health.
    SyncStatus,
    /// Streams, with open counts, Inbox first.
    StreamList,
    /// Contexts, with task counts, by name.
    Contexts,
    /// Live routines.
    Routines,
    /// Open tasks with their derived dependency state, actionable first.
    Actionable {
        /// Narrow to one stream.
        stream: Option<EntityRef>,
        /// Row cap.
        limit: u32,
    },
    /// The focus planner's ranked queue.
    FocusPlan {
        /// Narrow to one stream.
        stream: Option<EntityRef>,
        /// The session's energy budget; absent drops energy from the ranking.
        energy: Option<Energy>,
        /// How a session would be sized.
        length: SessionLength,
        /// Row cap.
        limit: u32,
    },
    /// Focus sessions for one task, newest first, running ones included.
    TaskFocusSessions {
        /// The task.
        task: EntityRef,
        /// Row cap.
        limit: u32,
    },
    /// Every session that has not ended yet.
    RunningFocusSessions,
    /// Focus totals and estimate calibration.
    FocusStats {
        /// Narrow to one stream.
        stream: Option<EntityRef>,
        /// Only fold sessions started at or after this instant (epoch ms).
        since_ms: Option<u64>,
        /// "Now" (epoch ms) — used only to price sessions still running.
        now_ms: u64,
    },
    /// What completing a task released.
    UnblockCascade {
        /// The completed task.
        task: EntityRef,
    },
    /// The weekly review, every step's list in one read.
    WeeklyReview {
        /// Week to review (epoch ms); the week containing `now_ms` when absent.
        week_start_ms: Option<u64>,
        /// "Now" (epoch ms).
        now_ms: u64,
    },
    /// The 60-second daily glance.
    DailyReview {
        /// Start of the glance window (epoch ms).
        since_ms: u64,
        /// "Now" (epoch ms).
        now_ms: u64,
    },
    /// Per-stream weekly trends.
    StreamTrends {
        /// How many weeks back.
        weeks: u32,
        /// "Now" (epoch ms).
        now_ms: u64,
    },
    /// One entity's activity feed, newest first.
    ActivityTimeline {
        /// Task or stream.
        entity: EntityRef,
        /// Row cap.
        limit: u32,
    },
    /// Saved review snapshots, newest window first.
    ReviewHistory {
        /// Row cap.
        limit: u32,
    },
    /// Render one stats dataset as JSON or CSV.
    ExportStats {
        /// Which dataset.
        dataset: ExportDataset,
        /// Which format.
        format: ExportFormat,
        /// Weeks of history for the trend dataset.
        weeks: u32,
        /// "Now" (epoch ms).
        now_ms: u64,
    },
    /// Live attachments on one task, oldest first. Metadata only: the bytes
    /// come from the blob store and open with each row's `blob_key`.
    TaskAttachments {
        /// The task.
        task: EntityRef,
    },
    /// The calendar grid for one civil day, in the device's zone. Overlap,
    /// not containment: a block running past midnight is on both days.
    DayBlocks {
        /// Any instant inside the day (epoch ms).
        day_ms: u64,
    },
    /// The calendar grid for one week, Monday-first, in the device's zone.
    WeekBlocks {
        /// Any instant inside the week (epoch ms).
        week_ms: u64,
    },
    /// Full-text search over tasks.
    Search {
        /// Raw query text.
        text: String,
        /// Row cap.
        limit: u32,
    },
}

impl CoreQuery {
    /// Lower into the core's own query type. Infallible, for the same reason
    /// [`crate::command::CoreCommand::into_core`] is.
    pub(crate) fn into_core(self) -> Query {
        match self {
            Self::Today { now_ms, contexts } => Query::Today { now_ms, contexts },
            Self::Inbox => Query::Inbox,
            Self::StreamTasks { stream } => Query::StreamTasks(stream),
            Self::ContextTasks { context } => Query::ContextTasks(context),
            Self::EntityById { id } => Query::EntityById(id),
            Self::DeviceList => Query::DeviceList,
            Self::SyncStatus => Query::SyncStatus,
            Self::StreamList => Query::StreamList,
            Self::Contexts => Query::Contexts,
            Self::Routines => Query::Routines,
            Self::Actionable { stream, limit } => Query::Actionable { stream, limit },
            Self::FocusPlan {
                stream,
                energy,
                length,
                limit,
            } => Query::FocusPlan {
                stream,
                energy,
                length,
                limit,
            },
            Self::TaskFocusSessions { task, limit } => Query::TaskFocusSessions { task, limit },
            Self::RunningFocusSessions => Query::RunningFocusSessions,
            Self::FocusStats {
                stream,
                since_ms,
                now_ms,
            } => Query::FocusStats {
                stream,
                since_ms,
                now_ms,
            },
            Self::UnblockCascade { task } => Query::UnblockCascade(task),
            Self::WeeklyReview {
                week_start_ms,
                now_ms,
            } => Query::WeeklyReview {
                week_start_ms,
                now_ms,
            },
            Self::DailyReview { since_ms, now_ms } => Query::DailyReview { since_ms, now_ms },
            Self::StreamTrends { weeks, now_ms } => Query::StreamTrends { weeks, now_ms },
            Self::ActivityTimeline { entity, limit } => Query::ActivityTimeline { entity, limit },
            Self::ReviewHistory { limit } => Query::ReviewHistory { limit },
            Self::ExportStats {
                dataset,
                format,
                weeks,
                now_ms,
            } => Query::ExportStats {
                dataset,
                format,
                weeks,
                now_ms,
            },
            Self::TaskAttachments { task } => Query::TaskAttachments(task),
            Self::DayBlocks { day_ms } => Query::DayBlocks { day_ms },
            Self::WeekBlocks { week_ms } => Query::WeekBlocks { week_ms },
            Self::Search { text, limit } => Query::Search { text, limit },
        }
    }
}

/// What a query returned.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CoreQueryResult {
    /// A task list.
    Tasks {
        /// The tasks.
        tasks: Vec<TaskItem>,
    },
    /// One stream.
    Stream {
        /// The stream.
        stream: StreamItem,
    },
    /// One task.
    Task {
        /// The task.
        task: TaskItem,
    },
    /// One routine.
    Routine {
        /// The routine.
        routine: RoutineItem,
    },
    /// One context.
    Context {
        /// The context.
        context: ContextItem,
    },
    /// Every live routine, projected for a list.
    Routines {
        /// The rows.
        routines: Vec<RoutineItem>,
    },
    /// Paired devices.
    Devices {
        /// The rows.
        devices: Vec<DeviceListRow>,
    },
    /// Sync health.
    SyncStatus {
        /// The snapshot.
        status: SyncSnapshot,
    },
    /// Streams with open counts.
    Streams {
        /// The rows.
        streams: Vec<StreamListRow>,
    },
    /// Contexts with task counts.
    Contexts {
        /// The rows.
        contexts: Vec<ContextListRow>,
    },
    /// Open tasks with derived dependency counts.
    Actionable {
        /// The rows.
        tasks: Vec<ActionableTaskRow>,
    },
    /// The focus planner's ranked queue.
    FocusPlan {
        /// The rows, best pick first.
        rows: Vec<PlanRow>,
    },
    /// Focus sessions.
    FocusSessions {
        /// The rows.
        sessions: Vec<SessionRow>,
    },
    /// Folded focus totals and calibration.
    FocusStats {
        /// The totals.
        stats: FocusTotals,
    },
    /// What a completion released.
    UnblockCascade {
        /// The cascade.
        cascade: Cascade,
    },
    /// The weekly review.
    WeeklyReview {
        /// The report.
        review: WeeklyReviewReport,
    },
    /// The daily glance.
    DailyReview {
        /// The report.
        review: DailyReviewReport,
    },
    /// Weekly trends.
    Trends {
        /// The report.
        trends: TrendReport,
    },
    /// An activity feed.
    Activity {
        /// The rows, newest first.
        events: Vec<ActivityRow>,
    },
    /// Saved review snapshots.
    ReviewSnapshots {
        /// The rows, newest window first.
        snapshots: Vec<Snapshot>,
    },
    /// The calendar grid's rows. `EntityById` on a `blk_` id returns one row
    /// here rather than in a variant of its own.
    Blocks {
        /// The rows, earliest start first.
        blocks: Vec<BlockGridRow>,
    },
    /// One task's attachment metadata.
    Attachments {
        /// The rows, oldest first.
        attachments: Vec<AttachmentItem>,
    },
    /// A rendered export document.
    Export {
        /// The document.
        body: String,
    },
}

impl CoreQueryResult {
    /// Lift from the core's own result type.
    ///
    /// Total over `QueryResult`, and deliberately so: an unhandled variant
    /// would have to panic, and a panic in an exported method reaches Swift as
    /// a trapped `rustPanic` — an app crash rather than a thrown error.
    pub(crate) fn from_core(r: QueryResult) -> Self {
        match r {
            // `Tasks` and `StreamTasks` differ only in which query produced
            // them; a client that asked knows which it asked.
            QueryResult::Tasks(t) | QueryResult::StreamTasks(t) => Self::Tasks {
                tasks: t.iter().map(TaskItem::from).collect(),
            },
            QueryResult::Stream(s) => Self::Stream {
                stream: StreamItem::from(s.as_ref()),
            },
            QueryResult::Task(t) => Self::Task {
                task: TaskItem::from(t.as_ref()),
            },
            QueryResult::Routine(r) => Self::Routine {
                routine: RoutineItem::from(r.as_ref()),
            },
            QueryResult::Context(c) => Self::Context {
                context: ContextItem::from(c.as_ref()),
            },
            QueryResult::Routines(rs) => Self::Routines {
                routines: rs.iter().map(RoutineItem::from).collect(),
            },
            QueryResult::Devices(d) => Self::Devices {
                devices: d.iter().map(DeviceListRow::from).collect(),
            },
            QueryResult::SyncStatus(s) => Self::SyncStatus {
                status: SyncSnapshot::from(&s),
            },
            QueryResult::Streams(s) => Self::Streams {
                streams: s.iter().map(StreamListRow::from).collect(),
            },
            QueryResult::Contexts(c) => Self::Contexts {
                contexts: c.iter().map(ContextListRow::from).collect(),
            },
            QueryResult::Actionable(a) => Self::Actionable {
                tasks: a.iter().map(ActionableTaskRow::from).collect(),
            },
            QueryResult::FocusPlan(p) => Self::FocusPlan {
                rows: p.iter().map(PlanRow::from).collect(),
            },
            QueryResult::FocusSessions(s) => Self::FocusSessions {
                sessions: s.iter().map(SessionRow::from).collect(),
            },
            QueryResult::FocusStats(s) => Self::FocusStats {
                stats: FocusTotals::from(s.as_ref()),
            },
            QueryResult::UnblockCascade(c) => Self::UnblockCascade {
                cascade: Cascade::from(c.as_ref()),
            },
            QueryResult::WeeklyReview(w) => Self::WeeklyReview {
                review: WeeklyReviewReport::from(w.as_ref()),
            },
            QueryResult::DailyReview(d) => Self::DailyReview {
                review: DailyReviewReport::from(d.as_ref()),
            },
            QueryResult::Trends(t) => Self::Trends {
                trends: TrendReport::from(t.as_ref()),
            },
            QueryResult::Activity(a) => Self::Activity {
                events: a.iter().map(ActivityRow::from).collect(),
            },
            QueryResult::ReviewSnapshots(s) => Self::ReviewSnapshots {
                snapshots: s.iter().map(Snapshot::from).collect(),
            },
            QueryResult::Blocks(b) => Self::Blocks {
                blocks: b.iter().map(BlockGridRow::from).collect(),
            },
            QueryResult::Attachments(a) => Self::Attachments {
                attachments: a.iter().map(AttachmentItem::from).collect(),
            },
            QueryResult::Export(body) => Self::Export { body },
        }
    }
}
