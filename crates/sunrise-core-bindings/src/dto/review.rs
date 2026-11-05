//! Stats and review: the daily, weekly and stream reviews and their trends.

use sunrise_domain::{
    DailyReview, ReviewSnapshot, ReviewSnapshotStream, ReviewTotals, ReviewWindow, RoutineDrift,
    StreamReview, StreamTrend, Trends, WeekBucket, WeeklyReview,
};
use sunrise_id::EntityRef;

use super::{FocusTotals, StreamFocusRow, TaskItem};

/// See [`sunrise_domain::WeekBucket`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct WeekCounts {
    /// Start of the week (epoch ms).
    pub week_start_ms: u64,
    /// Completions.
    pub completed: i64,
    /// Deferrals.
    pub deferred: i64,
    /// Creations.
    pub created: i64,
}

impl From<&WeekBucket> for WeekCounts {
    fn from(w: &WeekBucket) -> Self {
        let WeekBucket {
            week_start_ms,
            completed,
            deferred,
            created,
        } = w;
        Self {
            week_start_ms: *week_start_ms,
            completed: *completed,
            deferred: *deferred,
            created: *created,
        }
    }
}

/// See [`sunrise_domain::StreamTrend`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamTrendRow {
    /// The stream.
    pub stream: EntityRef,
    /// One bucket per week, oldest first.
    pub weeks: Vec<WeekCounts>,
}

impl From<&StreamTrend> for StreamTrendRow {
    fn from(t: &StreamTrend) -> Self {
        let StreamTrend { stream, weeks } = t;
        Self {
            stream: *stream,
            weeks: weeks.iter().map(WeekCounts::from).collect(),
        }
    }
}

/// See [`sunrise_domain::Trends`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct TrendReport {
    /// Week boundaries, oldest first.
    pub week_starts: Vec<u64>,
    /// Whole-vault buckets.
    pub overall: Vec<WeekCounts>,
    /// Per-stream buckets.
    pub per_stream: Vec<StreamTrendRow>,
}

impl From<&Trends> for TrendReport {
    fn from(t: &Trends) -> Self {
        let Trends {
            week_starts,
            overall,
            per_stream,
        } = t;
        Self {
            week_starts: week_starts.clone(),
            overall: overall.iter().map(WeekCounts::from).collect(),
            per_stream: per_stream.iter().map(StreamTrendRow::from).collect(),
        }
    }
}

/// See [`sunrise_domain::RoutineDrift`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct RoutineDriftRow {
    /// The routine.
    pub routine: EntityRef,
    /// Template title.
    pub title: String,
    /// Occurrences the rule produced in the window.
    pub expected: u32,
    /// Of which completed.
    pub completed: u32,
    /// Of which explicitly skipped.
    pub skipped: u32,
    /// Of which simply missed.
    pub missed: u32,
    /// Missed over expected.
    pub drift: f64,
    /// Whether drift crossed the reporting threshold.
    pub over_threshold: bool,
    /// Current streak.
    pub streak: i64,
    /// Last completion (epoch ms).
    pub last_completed_at_ms: Option<u64>,
    /// Paused.
    pub paused: bool,
}

impl From<&RoutineDrift> for RoutineDriftRow {
    fn from(d: &RoutineDrift) -> Self {
        let RoutineDrift {
            routine,
            title,
            expected,
            completed,
            skipped,
            missed,
            drift,
            over_threshold,
            streak,
            last_completed_at_ms,
            paused,
        } = d;
        Self {
            routine: *routine,
            title: title.clone(),
            expected: *expected,
            completed: *completed,
            skipped: *skipped,
            missed: *missed,
            drift: *drift,
            over_threshold: *over_threshold,
            streak: *streak,
            last_completed_at_ms: *last_completed_at_ms,
            paused: *paused,
        }
    }
}

/// See [`sunrise_domain::ReviewWindow`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct ReviewPeriod {
    /// Inclusive start (epoch ms).
    pub start_ms: u64,
    /// Exclusive end (epoch ms).
    pub end_ms: u64,
}

impl From<&ReviewWindow> for ReviewPeriod {
    fn from(w: &ReviewWindow) -> Self {
        let ReviewWindow { start_ms, end_ms } = w;
        Self {
            start_ms: *start_ms,
            end_ms: *end_ms,
        }
    }
}

/// See [`sunrise_domain::ReviewTotals`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct ReviewCounts {
    /// Completed in the window.
    pub completed: u32,
    /// Deferred in the window.
    pub deferred: u32,
    /// Cancelled in the window.
    pub dropped: u32,
    /// Created in the window.
    pub created: u32,
    /// Reopened in the window.
    pub reopened: u32,
}

impl From<&ReviewTotals> for ReviewCounts {
    fn from(t: &ReviewTotals) -> Self {
        let ReviewTotals {
            completed,
            deferred,
            dropped,
            created,
            reopened,
            unknown: _,
        } = t;
        Self {
            completed: *completed,
            deferred: *deferred,
            dropped: *dropped,
            created: *created,
            reopened: *reopened,
        }
    }
}

impl From<ReviewCounts> for ReviewTotals {
    fn from(c: ReviewCounts) -> Self {
        Self {
            completed: c.completed,
            deferred: c.deferred,
            dropped: c.dropped,
            created: c.created,
            reopened: c.reopened,
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

/// See [`sunrise_domain::StreakRow`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreakEntry {
    /// The routine.
    pub routine: EntityRef,
    /// Template title.
    pub title: String,
    /// Current streak.
    pub streak: i64,
    /// Last completion (epoch ms).
    pub last_completed_at_ms: Option<u64>,
}

impl From<&sunrise_domain::StreakRow> for StreakEntry {
    fn from(s: &sunrise_domain::StreakRow) -> Self {
        let sunrise_domain::StreakRow {
            routine,
            title,
            streak,
            last_completed_at_ms,
            unknown: _,
        } = s;
        Self {
            routine: *routine,
            title: title.clone(),
            streak: *streak,
            last_completed_at_ms: *last_completed_at_ms,
        }
    }
}

impl From<StreakEntry> for sunrise_domain::StreakRow {
    fn from(s: StreakEntry) -> Self {
        Self {
            routine: s.routine,
            title: s.title,
            streak: s.streak,
            last_completed_at_ms: s.last_completed_at_ms,
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

/// One stream's slice of the weekly review.
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamReviewRow {
    /// The stream.
    pub stream: EntityRef,
    /// Its name at review time.
    pub name: String,
    /// Completed in the window.
    pub completed: Vec<TaskItem>,
    /// Deferred in the window.
    pub deferred: Vec<TaskItem>,
    /// Created in the window and never touched again.
    pub created_untouched: Vec<TaskItem>,
    /// Focus totals for the stream.
    pub focus: Option<StreamFocusRow>,
    /// Weekly trend.
    pub trend: Vec<WeekCounts>,
}

impl From<&StreamReview> for StreamReviewRow {
    fn from(r: &StreamReview) -> Self {
        let StreamReview {
            stream,
            name,
            completed,
            deferred,
            created_untouched,
            focus,
            trend,
        } = r;
        Self {
            stream: *stream,
            name: name.clone(),
            completed: completed.iter().map(TaskItem::from).collect(),
            deferred: deferred.iter().map(TaskItem::from).collect(),
            created_untouched: created_untouched.iter().map(TaskItem::from).collect(),
            focus: focus.as_ref().map(StreamFocusRow::from),
            trend: trend.iter().map(WeekCounts::from).collect(),
        }
    }
}

/// The assembled weekly review — every step's list, in one read.
#[derive(Debug, Clone, uniffi::Record)]
pub struct WeeklyReviewReport {
    /// The week reviewed.
    pub window: ReviewPeriod,
    /// Per-stream slices.
    pub streams: Vec<StreamReviewRow>,
    /// Inbox items awaiting triage.
    pub inbox: Vec<TaskItem>,
    /// Routines drifting past the threshold.
    pub drifting_routines: Vec<RoutineDriftRow>,
    /// Routine streaks.
    pub streaks: Vec<StreakEntry>,
    /// Tasks whose date slipped past.
    pub slipped: Vec<TaskItem>,
    /// Whole-window counts.
    pub totals: ReviewCounts,
    /// Focus totals.
    pub focus: FocusTotals,
    /// Weekly trends.
    pub trends: TrendReport,
}

impl From<&WeeklyReview> for WeeklyReviewReport {
    fn from(r: &WeeklyReview) -> Self {
        let WeeklyReview {
            window,
            streams,
            inbox,
            drifting_routines,
            streaks,
            slipped,
            totals,
            focus,
            trends,
        } = r;
        Self {
            window: ReviewPeriod::from(window),
            streams: streams.iter().map(StreamReviewRow::from).collect(),
            inbox: inbox.iter().map(TaskItem::from).collect(),
            drifting_routines: drifting_routines
                .iter()
                .map(RoutineDriftRow::from)
                .collect(),
            streaks: streaks.iter().map(StreakEntry::from).collect(),
            slipped: slipped.iter().map(TaskItem::from).collect(),
            totals: ReviewCounts::from(totals),
            focus: FocusTotals::from(focus),
            trends: TrendReport::from(trends),
        }
    }
}

/// The 60-second daily glance.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DailyReviewReport {
    /// The window glanced at.
    pub window: ReviewPeriod,
    /// Recent captures.
    pub inbox: Vec<TaskItem>,
    /// Today's plan.
    pub today: Vec<TaskItem>,
    /// Which of it is blocked.
    pub blocked: Vec<TaskItem>,
}

impl From<&DailyReview> for DailyReviewReport {
    fn from(r: &DailyReview) -> Self {
        let DailyReview {
            window,
            inbox,
            today,
            blocked,
        } = r;
        Self {
            window: ReviewPeriod::from(window),
            inbox: inbox.iter().map(TaskItem::from).collect(),
            today: today.iter().map(TaskItem::from).collect(),
            blocked: blocked.iter().map(TaskItem::from).collect(),
        }
    }
}

/// One stream's counts inside a saved snapshot.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SnapshotStream {
    /// The stream.
    pub stream: EntityRef,
    /// Its name at snapshot time.
    pub name: String,
    /// Completed.
    pub completed: u32,
    /// Deferred.
    pub deferred: u32,
    /// Created.
    pub created: u32,
}

impl From<&ReviewSnapshotStream> for SnapshotStream {
    fn from(s: &ReviewSnapshotStream) -> Self {
        let ReviewSnapshotStream {
            stream,
            name,
            completed,
            deferred,
            created,
            unknown: _,
        } = s;
        Self {
            stream: *stream,
            name: name.clone(),
            completed: *completed,
            deferred: *deferred,
            created: *created,
        }
    }
}

impl From<SnapshotStream> for ReviewSnapshotStream {
    fn from(s: SnapshotStream) -> Self {
        Self {
            stream: s.stream,
            name: s.name,
            completed: s.completed,
            deferred: s.deferred,
            created: s.created,
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

/// A saved review snapshot — the one fact about a review that cannot be
/// re-derived from the op log.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Snapshot {
    /// Snapshot id.
    pub id: EntityRef,
    /// When it was saved.
    pub created_at: jiff::Timestamp,
    /// Window start.
    pub window_start: jiff::Timestamp,
    /// Window end.
    pub window_end: jiff::Timestamp,
    /// The counts the screen showed.
    pub totals: ReviewCounts,
    /// Per-stream counts.
    pub streams: Vec<SnapshotStream>,
    /// Routine streaks at the time.
    pub streaks: Vec<StreakEntry>,
    /// Optional note.
    pub note: Option<String>,
}

impl From<&ReviewSnapshot> for Snapshot {
    fn from(s: &ReviewSnapshot) -> Self {
        let ReviewSnapshot {
            id,
            created_at,
            window_start,
            window_end,
            totals,
            streams,
            streaks,
            note,
            unknown: _,
        } = s;
        Self {
            id: *id,
            created_at: *created_at,
            window_start: *window_start,
            window_end: *window_end,
            totals: ReviewCounts::from(totals),
            streams: streams.iter().map(SnapshotStream::from).collect(),
            streaks: streaks.iter().map(StreakEntry::from).collect(),
            note: note.clone(),
        }
    }
}

/// What a client sends to save a review snapshot.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SnapshotDraftIn {
    /// Window start (epoch ms).
    pub window_start_ms: u64,
    /// Window end (epoch ms).
    pub window_end_ms: u64,
    /// The counts the screen showed.
    pub totals: ReviewCounts,
    /// Per-stream counts.
    pub streams: Vec<SnapshotStream>,
    /// Routine streaks at the time.
    pub streaks: Vec<StreakEntry>,
    /// Optional note.
    pub note: Option<String>,
}

impl From<SnapshotDraftIn> for sunrise_domain::ReviewSnapshotDraft {
    fn from(d: SnapshotDraftIn) -> Self {
        Self {
            window_start_ms: d.window_start_ms,
            window_end_ms: d.window_end_ms,
            totals: d.totals.into(),
            streams: d.streams.into_iter().map(Into::into).collect(),
            streaks: d.streaks.into_iter().map(Into::into).collect(),
            note: d.note,
        }
    }
}
