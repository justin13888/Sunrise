//! Sunrise domain entities and validation.
//!
//! Implements `docs/02-domain/`. Each entity lives in its own module; the
//! module-level docs cite the relevant spec file. Validation rules return
//! [`sunrise_error::ErrorCode`] values so the core can surface them to the
//! UI without translation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Domain entities use a lot of CBOR-mapped optional fields; relax the
// stylistic lints while the layer stabilizes (revisit Phase 17).
#![allow(
    clippy::doc_markdown,
    clippy::manual_let_else,
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::missing_const_for_fn,
    clippy::struct_excessive_bools
)]

pub mod activity;
pub mod annotate;
pub mod attachment;
pub mod block;
pub mod capture;
pub mod common;
pub mod constraint;
pub mod context;
pub mod deps;
pub mod epoch_ms;
pub mod export;
pub mod focus;
pub mod inbox;
pub mod note;
pub mod notify;
pub mod person;
pub mod phrase;
pub mod planning;
pub mod recur;
pub mod review;
pub mod routine;
pub mod routine_gen;
pub mod rrule;
pub mod schema;
pub mod stats;
pub mod streak;
pub mod stream;
pub mod task;
pub mod time;
pub mod unknown;
pub mod validation;

pub use activity::{
    changed_task_fields, fold_activity, for_entities as activity_for_entities,
    for_entity as activity_for_entity, ActivityEvent, ActivityKind, OpPayload, OpRecord,
    TRACKED_TASK_FIELDS,
};
pub use annotate::{parse as parse_annotate, EditError, TaskEdit};
pub use attachment::{Attachment, AttachmentDraft};
pub use block::{Block, BlockDraft, BlockPatch};
pub use capture::now_ts;
pub use common::{Energy, NoteBody};
pub use constraint::{
    validate_list as validate_constraint_list, violations_by_severity, ConstraintError,
    ConstraintSeverity, DateRange, ScheduleConstraint, TimeOfDayRange, WeekdaySet, MAX_CONSTRAINTS,
};
pub use context::{
    Context, ContextDraft, ContextFacet, ContextPatch, ENERGY_PREFIX, MAX_CONTEXT_DESCRIPTION_LEN,
    WAITING_ON_PREFIX,
};
pub use deps::{
    blocker_is_open, effective_state, is_actionable, DependencyGraph, EffectiveTaskState,
};
pub use export::{
    activity_table, focus_table, streaks_table, trends_table, Cell, ExportDataset, ExportFormat,
    Table,
};
pub use focus::{
    break_after, chunk_count, energy_fit, fold_focus_stats, plan_session, rank_focus_plan,
    unblock_cascade, Calibration, Chunk, EnergyFit, EnergyFocus, FocusEnd, FocusKind, FocusSession,
    FocusStart, FocusStats, Interruption, InterruptionReason, InterruptionTally, PlanCandidate,
    PlanRanked, Segment, SessionLength, SessionPlan, SessionRecord, StreamFocus, UnblockCascade,
    CYCLES_BEFORE_LONG_BREAK, LONG_BREAK_MS, POMODORO_MS, SHORT_BREAK_MS,
};
pub use inbox::{inbox_stream_ref, INBOX_STREAM_BYTES, INBOX_STREAM_ID};
pub use note::Note;
pub use notify::{
    apply_quiet_hours, build_end_of_day_plan, build_morning_summary, lead_time_s, plan_reminders,
    EndOfDayPlan, MorningSummary, QuietHours, QuietHoursPolicy, ReminderCandidate, ReminderIntent,
    ReminderKind, ReminderSettings, BLOCK_DEFAULT_LEAD_S, QUIET_HOURS_QUEUE_CAP_S,
};
pub use person::Person;
pub use phrase::{
    activity_phrase, constraint_summary, energy_budget_label, energy_fit_label, fmt_duration_ms,
    length_label, plan_reason, relative_day, short_duration,
};
pub use planning::{is_overdue, today_section, TodaySection};
pub use recur::parse_recurrence;
pub use review::{
    build_daily_review, build_weekly_review, DailyReview, ReviewSnapshot, ReviewSnapshotDraft,
    ReviewSnapshotStream, ReviewStream, ReviewTotals, ReviewWindow, StreakRow, StreamReview,
    WeeklyReview, WeeklyReviewInput,
};
pub use routine::{
    materialization_horizon_days, Routine, RoutineCatchupPolicy, RoutineDraft, RoutinePatch,
    RoutineReviewCadence, TaskTemplate,
};
pub use routine_gen::{
    expand, occurrence_key_at, occurrence_task_id, routine_rows, ExpandError, Occurrence,
    RoutineRow,
};
pub use rrule::{rrule_summary, Frequency, RRule, RRuleParseError, Weekday};
pub use schema::DOC_SCHEMA_V;
pub use stats::{
    days_between, fold_trends, routine_drift, RoutineDrift, StatsError, StreamTrend, Trends,
    WeekBucket, WeekGrid, DEFAULT_DRIFT_THRESHOLD, DRIFT_WINDOW_WEEKS, TREND_WEEKS,
};
pub use streak::{
    StreakOutcome, DEFAULT_GRACE_WINDOW_S, FORGIVENESS_ALLOWANCE, FORGIVENESS_WINDOW_S,
    MAX_GRACE_WINDOW_S,
};
pub use stream::{Stream, StreamColor, StreamDraft, StreamPatch, StreamReviewCadence};
pub use task::{Task, TaskDraft, TaskPatch, TaskState};
pub use time::SunriseTime;
pub use unknown::{CborValue, Unknowns};
pub use validation::{
    ValidationError, MAX_ATTACHMENT_BYTES, MAX_BLOCK_TITLE_LEN, MAX_CONTEXT_NAME_LEN,
    MAX_FILENAME_LEN, MAX_MIME_TYPE_LEN, MAX_TASK_ENVELOPE_BYTES, MAX_TASK_TITLE_LEN,
};
