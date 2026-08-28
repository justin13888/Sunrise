//! The structured vocabulary, mirrored for the foreign side.
//!
//! # Why mirror rather than declare remotely
//!
//! `#[uniffi::remote(Record)]` would avoid the copies, but it still requires
//! every *field* type to be UniFFI-representable, and three of the domain's
//! shapes are not:
//!
//! * `BTreeSet<EntityRef>` — UniFFI has sequences, not sets. `Task` carries
//!   three of them.
//! * `Unknowns` — the forward-compatibility map. It exists so a field written
//!   by a newer schema survives a round trip through this build; it is not
//!   something a UI reads, and putting it on the wire would invite a client to
//!   treat it as data.
//! * `[u8; 16]` — op and device ids. UniFFI has no fixed-size array type at
//!   all.
//!
//! One mirror per shape is also what lets [`TaskItem`] be called that. A record
//! named `Task` generates a Swift `struct Task` that shadows
//! `_Concurrency.Task`, so every `Task { … }` in the app stops compiling. That
//! rename has to happen before any UI is written against it.
//!
//! # Drift
//!
//! Every `From` impl below **destructures its source exhaustively** — no `..`
//! rest patterns. Adding a field to a domain entity therefore fails this
//! crate's build, which is the only way a mirror layer stays honest.

use sunrise_core::queries::{
    ActionableTask, ContextRow, DeviceRow, FocusPlanRow, FocusSessionRow, StreamRow,
};
use sunrise_core::CommandResult;
use sunrise_domain::{
    ActivityEvent, ActivityKind, Calibration, Chunk, ConstraintSeverity, Context, DailyReview,
    DateRange, EffectiveTaskState, Energy, EnergyFit, EnergyFocus, FocusEnd, FocusKind,
    FocusSession, FocusStart, FocusStats, Frequency, Interruption, InterruptionReason,
    InterruptionTally, NoteBody, RRule, ReviewSnapshot, ReviewSnapshotStream, ReviewTotals,
    ReviewWindow, Routine, RoutineCatchupPolicy, RoutineDrift, RoutineRow, ScheduleConstraint,
    SessionPlan, Stream, StreamColor, StreamFocus, StreamReview, StreamReviewCadence, StreamTrend,
    SunriseTime, Task, TaskState, TaskTemplate, TimeOfDayRange, Trends, UnblockCascade, WeekBucket,
    Weekday, WeeklyReview,
};
use sunrise_id::EntityRef;

/// Monday-first, matching how the domain orders a weekday set.
const ALL_WEEKDAYS: [Weekday; 7] = [
    Weekday::Mo,
    Weekday::Tu,
    Weekday::We,
    Weekday::Th,
    Weekday::Fr,
    Weekday::Sa,
    Weekday::Su,
];

/// Format the 16-byte op/device ids UniFFI cannot carry as lowercase hex.
fn hex16(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(32);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::SunriseTime`]: "Tuesday morning", "09:00 New York" and
/// "this instant" are three different answers, and a client that flattens them
/// to one has thrown away what the user meant.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum TimeValue {
    /// A fixed point on the timeline, zone-independent.
    Instant {
        /// The instant.
        at: jiff::Timestamp,
    },
    /// A civil time pinned to a named IANA zone.
    Zoned {
        /// Wall-clock date and time in `tz`.
        civil: jiff::civil::DateTime,
        /// IANA zone name, e.g. `America/New_York`.
        tz: String,
    },
    /// A civil time with no zone: resolves against the reading device.
    Floating {
        /// Wall-clock date and time.
        civil: jiff::civil::DateTime,
    },
    /// A whole date with no time of day.
    AllDay {
        /// The date.
        date: jiff::civil::Date,
    },
}

impl From<&SunriseTime> for TimeValue {
    fn from(t: &SunriseTime) -> Self {
        match t {
            SunriseTime::Instant { at } => Self::Instant { at: *at },
            SunriseTime::Zoned { civil, tz } => Self::Zoned {
                civil: *civil,
                tz: tz.clone(),
            },
            SunriseTime::Floating { civil } => Self::Floating { civil: *civil },
            SunriseTime::AllDay { date } => Self::AllDay { date: *date },
        }
    }
}

impl From<TimeValue> for SunriseTime {
    fn from(t: TimeValue) -> Self {
        match t {
            TimeValue::Instant { at } => Self::Instant { at },
            TimeValue::Zoned { civil, tz } => Self::Zoned { civil, tz },
            TimeValue::Floating { civil } => Self::Floating { civil },
            TimeValue::AllDay { date } => Self::AllDay { date },
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduling constraints
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::TimeOfDayRange`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct TimeWindow {
    /// Inclusive start (local wall clock).
    pub start: jiff::civil::Time,
    /// Exclusive end (local wall clock).
    pub end: jiff::civil::Time,
}

/// See [`sunrise_domain::DateRange`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct DateWindow {
    /// Inclusive start.
    pub start: jiff::civil::Date,
    /// Inclusive end; open-ended when absent.
    pub end: Option<jiff::civil::Date>,
}

/// See [`sunrise_domain::ScheduleConstraint`]. `days_of_week` is a list rather
/// than the domain's bit set: UniFFI has sequences, not sets, and an empty list
/// means "every day" exactly as the empty set does.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Constraint {
    /// Time-of-day window.
    pub time_of_day: Option<TimeWindow>,
    /// Allowed weekdays; empty means all of them.
    pub days_of_week: Vec<Weekday>,
    /// Allowed date range.
    pub date_range: Option<DateWindow>,
    /// Whether violating this rejects the write or only demotes it.
    pub severity: ConstraintSeverity,
}

impl From<&ScheduleConstraint> for Constraint {
    fn from(c: &ScheduleConstraint) -> Self {
        let ScheduleConstraint {
            time_of_day,
            days_of_week,
            date_range,
            severity,
        } = c;
        Self {
            time_of_day: time_of_day.as_ref().map(|w| TimeWindow {
                start: w.start,
                end: w.end,
            }),
            days_of_week: ALL_WEEKDAYS
                .into_iter()
                .filter(|d| days_of_week.contains(*d))
                .collect(),
            date_range: date_range.as_ref().map(|r| DateWindow {
                start: r.start,
                end: r.end,
            }),
            severity: *severity,
        }
    }
}

impl From<Constraint> for ScheduleConstraint {
    fn from(c: Constraint) -> Self {
        Self {
            time_of_day: c.time_of_day.map(|w| TimeOfDayRange {
                start: w.start,
                end: w.end,
            }),
            days_of_week: sunrise_domain::WeekdaySet::from_days(c.days_of_week),
            date_range: c.date_range.map(|r| DateRange {
                start: r.start,
                end: r.end,
            }),
            severity: c.severity,
        }
    }
}

// ---------------------------------------------------------------------------
// Recurrence
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::RRule`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct Recurrence {
    /// Base frequency.
    pub freq: Frequency,
    /// Every `interval` periods.
    pub interval: u32,
    /// `BYDAY`.
    pub by_day: Vec<Weekday>,
    /// `BYMONTHDAY`; negative counts back from the end of the month.
    pub by_month_day: Vec<i32>,
    /// `BYMONTH`.
    pub by_month: Vec<u32>,
    /// `BYSETPOS`.
    pub by_set_pos: Vec<i32>,
    /// `COUNT`.
    pub count: Option<u32>,
    /// `UNTIL`.
    pub until: Option<jiff::Timestamp>,
    /// Week start.
    pub wkst: Option<Weekday>,
}

impl From<&RRule> for Recurrence {
    fn from(r: &RRule) -> Self {
        let RRule {
            freq,
            interval,
            by_day,
            by_month_day,
            by_month,
            by_set_pos,
            count,
            until,
            wkst,
        } = r;
        Self {
            freq: *freq,
            interval: *interval,
            by_day: by_day.clone(),
            by_month_day: by_month_day.clone(),
            by_month: by_month.clone(),
            by_set_pos: by_set_pos.clone(),
            count: *count,
            until: *until,
            wkst: *wkst,
        }
    }
}

impl From<Recurrence> for RRule {
    fn from(r: Recurrence) -> Self {
        Self {
            freq: r.freq,
            interval: r.interval,
            by_day: r.by_day,
            by_month_day: r.by_month_day,
            by_month: r.by_month,
            by_set_pos: r.by_set_pos,
            count: r.count,
            until: r.until,
            wkst: r.wkst,
        }
    }
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

/// A task.
///
/// **Named `TaskItem`, not `Task`, on purpose.** See the module docs: a
/// generated Swift `Task` shadows `_Concurrency.Task`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TaskItem {
    /// Unique id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Title.
    pub title: String,
    /// Optional rich-text body.
    pub body: Option<NoteBody>,
    /// Owning Stream (the Inbox id if unassigned).
    pub stream_id: EntityRef,
    /// Contexts carried, sorted.
    pub contexts: Vec<EntityRef>,
    /// User-set state. `blocked` is derived and lives on
    /// [`ActionableTaskRow::effective_state`], never here.
    pub state: TaskState,
    /// Priority 1..=5.
    pub priority: Option<u8>,
    /// Energy facet.
    pub energy: Option<Energy>,
    /// Estimate, in seconds.
    pub estimated_duration_s: Option<u64>,
    /// When the user intends to do it.
    pub scheduled_at: Option<TimeValue>,
    /// Hard deadline.
    pub due_at: Option<TimeValue>,
    /// Scheduling constraints.
    pub scheduling_constraints: Vec<Constraint>,
    /// Set on transition to done; always an instant.
    pub completed_at: Option<TimeValue>,
    /// How many times this has been deferred.
    pub deferred_count: i64,
    /// Blocks scheduling this task.
    pub blocks: Vec<EntityRef>,
    /// Tasks this one depends on.
    pub blocked_by: Vec<EntityRef>,
    /// Informational assignee.
    pub assignee: Option<EntityRef>,
    /// The routine that generated it, if any.
    pub routine_id: Option<EntityRef>,
    /// Occurrence instant for routine-generated tasks.
    pub routine_occurrence: Option<jiff::Timestamp>,
    /// Archived.
    pub archived: bool,
    /// Tombstoned.
    pub deleted: bool,
}

impl From<&Task> for TaskItem {
    fn from(t: &Task) -> Self {
        let Task {
            id,
            created_at,
            updated_at,
            title,
            body,
            stream_id,
            contexts,
            state,
            priority,
            energy,
            estimated_duration_s,
            scheduled_at,
            due_at,
            scheduling_constraints,
            completed_at,
            deferred_count,
            blocks,
            blocked_by,
            assignee,
            routine_id,
            routine_occurrence,
            archived,
            deleted,
            // Deliberately not exported: see the module docs.
            unknown: _,
        } = t;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            title: title.clone(),
            body: body.clone(),
            stream_id: *stream_id,
            contexts: contexts.iter().copied().collect(),
            state: *state,
            priority: *priority,
            energy: *energy,
            estimated_duration_s: *estimated_duration_s,
            scheduled_at: scheduled_at.as_ref().map(TimeValue::from),
            due_at: due_at.as_ref().map(TimeValue::from),
            scheduling_constraints: scheduling_constraints
                .iter()
                .map(Constraint::from)
                .collect(),
            completed_at: completed_at.as_ref().map(TimeValue::from),
            deferred_count: *deferred_count,
            blocks: blocks.iter().copied().collect(),
            blocked_by: blocked_by.iter().copied().collect(),
            assignee: *assignee,
            routine_id: *routine_id,
            routine_occurrence: *routine_occurrence,
            archived: *archived,
            deleted: *deleted,
        }
    }
}

/// Fields the core fills for a new task; see [`sunrise_domain::TaskDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct TaskDraftIn {
    /// Title (trimmed and validated by the core).
    pub title: String,
    /// Optional body.
    pub body: Option<NoteBody>,
    /// Owning Stream; the Inbox when absent.
    pub stream_id: Option<EntityRef>,
    /// Contexts.
    pub contexts: Vec<EntityRef>,
    /// Priority 1..=5.
    pub priority: Option<u8>,
    /// Energy facet.
    pub energy: Option<Energy>,
    /// Estimate, in seconds.
    pub estimated_duration_s: Option<u64>,
    /// When to do it.
    pub scheduled_at: Option<TimeValue>,
    /// Deadline.
    pub due_at: Option<TimeValue>,
    /// Scheduling constraints.
    pub scheduling_constraints: Vec<Constraint>,
    /// Informational assignee.
    pub assignee: Option<EntityRef>,
}

impl From<TaskDraftIn> for sunrise_domain::TaskDraft {
    fn from(d: TaskDraftIn) -> Self {
        Self {
            title: d.title,
            body: d.body,
            stream_id: d.stream_id,
            contexts: d.contexts,
            priority: d.priority,
            energy: d.energy,
            estimated_duration_s: d.estimated_duration_s,
            scheduled_at: d.scheduled_at.map(SunriseTime::from),
            due_at: d.due_at.map(SunriseTime::from),
            scheduling_constraints: d
                .scheduling_constraints
                .into_iter()
                .map(ScheduleConstraint::from)
                .collect(),
            assignee: d.assignee,
        }
    }
}

/// An edit to a task.
///
/// Every field is doubly optional in the domain (`None` leaves alone,
/// `Some(None)` clears), which UniFFI cannot express as a nested optional in
/// every target language. The two decisions are split here instead: `set_x`
/// carries the new value and `clear_x` asks for the field to be emptied.
/// Setting both is a contradiction, and **`clear` wins** — it is the one that
/// cannot be expressed any other way.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct TaskEdit {
    /// New title.
    pub title: Option<String>,
    /// New body.
    pub set_body: Option<NoteBody>,
    /// Clear the body.
    pub clear_body: bool,
    /// Move to another Stream. (A Stream move is `PromoteToStream`; this field
    /// exists because `TaskPatch` carries it.)
    pub stream_id: Option<EntityRef>,
    /// Replace the whole context set.
    pub contexts: Option<Vec<EntityRef>>,
    /// New state.
    pub state: Option<TaskState>,
    /// New priority.
    pub set_priority: Option<u8>,
    /// Clear the priority.
    pub clear_priority: bool,
    /// New energy facet.
    pub set_energy: Option<Energy>,
    /// Clear the energy facet.
    pub clear_energy: bool,
    /// New estimate, in seconds.
    pub set_estimated_duration_s: Option<u64>,
    /// Clear the estimate.
    pub clear_estimated_duration: bool,
    /// New scheduled time.
    pub set_scheduled_at: Option<TimeValue>,
    /// Clear the scheduled time.
    pub clear_scheduled_at: bool,
    /// New deadline.
    pub set_due_at: Option<TimeValue>,
    /// Clear the deadline.
    pub clear_due_at: bool,
    /// Replace the whole constraint list.
    pub scheduling_constraints: Option<Vec<Constraint>>,
    /// Replace the whole blocker set.
    pub blocked_by: Option<Vec<EntityRef>>,
    /// New assignee.
    pub set_assignee: Option<EntityRef>,
    /// Clear the assignee.
    pub clear_assignee: bool,
    /// Archive or unarchive.
    pub archived: Option<bool>,
}

/// `Some(None)` when `clear`, `Some(Some(v))` when set, `None` when neither.
///
/// The nesting is the domain's `TaskPatch`/`StreamPatch`/`RoutinePatch` shape,
/// not a choice made here; this is the function that builds it.
#[allow(clippy::option_option)]
fn patch_field<T>(set: Option<T>, clear: bool) -> Option<Option<T>> {
    if clear {
        Some(None)
    } else {
        set.map(Some)
    }
}

impl From<TaskEdit> for sunrise_domain::TaskPatch {
    fn from(e: TaskEdit) -> Self {
        Self {
            title: e.title,
            body: patch_field(e.set_body, e.clear_body),
            stream_id: e.stream_id,
            contexts: e.contexts,
            state: e.state,
            priority: patch_field(e.set_priority, e.clear_priority),
            energy: patch_field(e.set_energy, e.clear_energy),
            estimated_duration_s: patch_field(
                e.set_estimated_duration_s,
                e.clear_estimated_duration,
            ),
            scheduled_at: patch_field(
                e.set_scheduled_at.map(SunriseTime::from),
                e.clear_scheduled_at,
            ),
            due_at: patch_field(e.set_due_at.map(SunriseTime::from), e.clear_due_at),
            scheduling_constraints: e
                .scheduling_constraints
                .map(|l| l.into_iter().map(ScheduleConstraint::from).collect()),
            blocked_by: e.blocked_by,
            assignee: patch_field(e.set_assignee, e.clear_assignee),
            archived: e.archived,
        }
    }
}

// ---------------------------------------------------------------------------
// Stream / Context
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::Stream`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamItem {
    /// Stream id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<NoteBody>,
    /// Colour.
    pub color: StreamColor,
    /// Optional icon name.
    pub icon: Option<String>,
    /// Parent stream, for nesting.
    pub parent_id: Option<EntityRef>,
    /// Sibling ordering key.
    pub sort_order: String,
    /// Archived.
    pub archived: bool,
    /// Paused.
    pub paused: bool,
    /// Paused until this instant.
    pub paused_until: Option<jiff::Timestamp>,
    /// How often this stream expects a review.
    pub review_cadence: StreamReviewCadence,
    /// Context applied by default to tasks captured into this stream.
    pub default_context: Option<EntityRef>,
    /// Tombstoned.
    pub deleted: bool,
}

impl From<&Stream> for StreamItem {
    fn from(s: &Stream) -> Self {
        let Stream {
            id,
            created_at,
            updated_at,
            name,
            description,
            color,
            icon,
            parent_id,
            sort_order,
            archived,
            paused,
            paused_until,
            review_cadence,
            default_context,
            deleted,
            unknown: _,
        } = s;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            name: name.clone(),
            description: description.clone(),
            color: *color,
            icon: icon.clone(),
            parent_id: *parent_id,
            sort_order: sort_order.clone(),
            archived: *archived,
            paused: *paused,
            paused_until: *paused_until,
            review_cadence: *review_cadence,
            default_context: *default_context,
            deleted: *deleted,
        }
    }
}

/// See [`sunrise_domain::StreamDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamDraftIn {
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<NoteBody>,
    /// Colour; the core picks one when absent.
    pub color: Option<StreamColor>,
    /// Parent stream.
    pub parent_id: Option<EntityRef>,
    /// Review cadence.
    pub review_cadence: Option<StreamReviewCadence>,
}

impl From<StreamDraftIn> for sunrise_domain::StreamDraft {
    fn from(d: StreamDraftIn) -> Self {
        Self {
            name: d.name,
            description: d.description,
            color: d.color,
            parent_id: d.parent_id,
            review_cadence: d.review_cadence,
        }
    }
}

/// An edit to a stream. Split-optional, like [`TaskEdit`].
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct StreamEdit {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub set_description: Option<NoteBody>,
    /// Clear the description.
    pub clear_description: bool,
    /// New colour.
    pub color: Option<StreamColor>,
    /// New parent.
    pub set_parent_id: Option<EntityRef>,
    /// Detach from any parent.
    pub clear_parent_id: bool,
    /// New review cadence.
    pub review_cadence: Option<StreamReviewCadence>,
    /// Archive or unarchive.
    pub archived: Option<bool>,
    /// Pause or resume.
    pub paused: Option<bool>,
    /// Pause until this instant.
    pub set_paused_until: Option<jiff::Timestamp>,
    /// Clear the pause deadline.
    pub clear_paused_until: bool,
}

impl From<StreamEdit> for sunrise_domain::StreamPatch {
    fn from(e: StreamEdit) -> Self {
        Self {
            name: e.name,
            description: patch_field(e.set_description, e.clear_description),
            color: e.color,
            parent_id: patch_field(e.set_parent_id, e.clear_parent_id),
            review_cadence: e.review_cadence,
            archived: e.archived,
            paused: e.paused,
            paused_until: patch_field(e.set_paused_until, e.clear_paused_until),
        }
    }
}

/// See [`sunrise_domain::Context`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ContextItem {
    /// Context id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Archived contexts stay on their tasks but leave the pickers.
    pub archived: bool,
    /// Tombstoned.
    pub deleted: bool,
}

impl From<&Context> for ContextItem {
    fn from(c: &Context) -> Self {
        let Context {
            id,
            created_at,
            updated_at,
            name,
            description,
            archived,
            deleted,
            unknown: _,
        } = c;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            name: name.clone(),
            description: description.clone(),
            archived: *archived,
            deleted: *deleted,
        }
    }
}

/// See [`sunrise_domain::ContextDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ContextDraftIn {
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
}

impl From<ContextDraftIn> for sunrise_domain::ContextDraft {
    fn from(d: ContextDraftIn) -> Self {
        Self {
            name: d.name,
            description: d.description,
        }
    }
}

/// An edit to a context. Split-optional, like [`TaskEdit`].
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct ContextEdit {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub set_description: Option<String>,
    /// Clear the description.
    pub clear_description: bool,
    /// Archive or unarchive.
    pub archived: Option<bool>,
}

impl From<ContextEdit> for sunrise_domain::ContextPatch {
    fn from(e: ContextEdit) -> Self {
        Self {
            name: e.name,
            description: patch_field(e.set_description, e.clear_description),
            archived: e.archived,
        }
    }
}

// ---------------------------------------------------------------------------
// Routine
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::TaskTemplate`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct Template {
    /// Title of each generated task.
    pub title: String,
    /// Stream each generated task lands in.
    pub stream_id: EntityRef,
    /// Contexts each generated task carries.
    pub contexts: Vec<EntityRef>,
    /// Energy facet.
    pub energy: Option<Energy>,
    /// Priority 1..=5.
    pub priority: Option<u8>,
    /// Estimate, in seconds.
    pub estimated_duration_s: Option<u64>,
    /// Body.
    pub body: Option<NoteBody>,
}

impl From<&TaskTemplate> for Template {
    fn from(t: &TaskTemplate) -> Self {
        let TaskTemplate {
            title,
            stream_id,
            contexts,
            energy,
            priority,
            estimated_duration_s,
            body,
        } = t;
        Self {
            title: title.clone(),
            stream_id: *stream_id,
            contexts: contexts.clone(),
            energy: *energy,
            priority: *priority,
            estimated_duration_s: *estimated_duration_s,
            body: body.clone(),
        }
    }
}

impl From<Template> for TaskTemplate {
    fn from(t: Template) -> Self {
        Self {
            title: t.title,
            stream_id: t.stream_id,
            contexts: t.contexts,
            energy: t.energy,
            priority: t.priority,
            estimated_duration_s: t.estimated_duration_s,
            body: t.body,
        }
    }
}

/// See [`sunrise_domain::Routine`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct RoutineItem {
    /// Routine id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// The task this generates.
    pub template: Template,
    /// The recurrence.
    pub rrule: Recurrence,
    /// IANA zone the recurrence is expanded in.
    pub timezone: String,
    /// Anchor instant.
    pub starts_at: jiff::Timestamp,
    /// Series end.
    pub ends_at: Option<jiff::Timestamp>,
    /// Scheduling constraints applied to generated tasks.
    pub scheduling_constraints: Vec<Constraint>,
    /// Occurrence keys the user skipped.
    pub skipped_keys: Vec<String>,
    /// What to do with occurrences missed while offline.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Current streak.
    pub streak_counter: i64,
    /// Last completion.
    pub last_completed_at: Option<jiff::Timestamp>,
    /// Grace window, in seconds.
    pub grace_window_s: Option<u64>,
    /// Whether a missed occurrence can be forgiven.
    pub forgiveness_enabled: bool,
    /// When the current streak began.
    pub streak_started_at: Option<jiff::Timestamp>,
    /// Forgivenesses used in the current window.
    pub forgivenesses_in_window: u32,
    /// Occurrence keys counted toward the streak.
    pub streak_keys: Vec<String>,
    /// Paused (generates nothing).
    pub paused: bool,
    /// Paused until this instant.
    pub paused_until: Option<jiff::Timestamp>,
    /// Archived.
    pub archived: bool,
    /// Tombstoned.
    pub deleted: bool,
}

impl From<&Routine> for RoutineItem {
    fn from(r: &Routine) -> Self {
        let Routine {
            id,
            created_at,
            updated_at,
            template,
            rrule,
            timezone,
            starts_at,
            ends_at,
            scheduling_constraints,
            // Dated for removal; `skipped_keys` is the live representation.
            skip_dates: _,
            skipped_keys,
            catchup_policy,
            streak_counter,
            last_completed_at,
            grace_window_s,
            forgiveness_enabled,
            streak_started_at,
            forgivenesses_in_window,
            streak_keys,
            paused,
            paused_until,
            archived,
            deleted,
            unknown: _,
        } = r;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            template: Template::from(template),
            rrule: Recurrence::from(rrule),
            timezone: timezone.clone(),
            starts_at: *starts_at,
            ends_at: *ends_at,
            scheduling_constraints: scheduling_constraints
                .iter()
                .map(Constraint::from)
                .collect(),
            skipped_keys: skipped_keys.clone(),
            catchup_policy: *catchup_policy,
            streak_counter: *streak_counter,
            last_completed_at: *last_completed_at,
            grace_window_s: *grace_window_s,
            forgiveness_enabled: *forgiveness_enabled,
            streak_started_at: *streak_started_at,
            forgivenesses_in_window: *forgivenesses_in_window,
            streak_keys: streak_keys.clone(),
            paused: *paused,
            paused_until: *paused_until,
            archived: *archived,
            deleted: *deleted,
        }
    }
}

/// See [`sunrise_domain::RoutineDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct RoutineDraftIn {
    /// The task to generate.
    pub template: Template,
    /// The recurrence.
    pub rrule: Recurrence,
    /// IANA zone to expand it in.
    pub timezone: String,
    /// Anchor instant.
    pub starts_at: jiff::Timestamp,
    /// Series end.
    pub ends_at: Option<jiff::Timestamp>,
    /// Scheduling constraints for generated tasks.
    pub scheduling_constraints: Vec<Constraint>,
    /// What to do with occurrences missed while offline.
    pub catchup_policy: RoutineCatchupPolicy,
}

impl From<RoutineDraftIn> for sunrise_domain::RoutineDraft {
    fn from(d: RoutineDraftIn) -> Self {
        Self {
            template: d.template.into(),
            rrule: d.rrule.into(),
            timezone: d.timezone,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            scheduling_constraints: d
                .scheduling_constraints
                .into_iter()
                .map(ScheduleConstraint::from)
                .collect(),
            catchup_policy: d.catchup_policy,
        }
    }
}

/// An edit to a routine. Split-optional, like [`TaskEdit`].
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct RoutineEdit {
    /// Replace the whole template.
    pub template: Option<Template>,
    /// Replace the recurrence.
    pub rrule: Option<Recurrence>,
    /// New IANA zone.
    pub timezone: Option<String>,
    /// New anchor.
    pub starts_at: Option<jiff::Timestamp>,
    /// New series end.
    pub set_ends_at: Option<jiff::Timestamp>,
    /// Remove the series end.
    pub clear_ends_at: bool,
    /// Replace the constraint list.
    pub scheduling_constraints: Option<Vec<Constraint>>,
    /// New catch-up policy.
    pub catchup_policy: Option<RoutineCatchupPolicy>,
    /// New grace window, in seconds.
    pub set_grace_window_s: Option<u64>,
    /// Clear the grace window.
    pub clear_grace_window: bool,
    /// Enable or disable forgiveness.
    pub forgiveness_enabled: Option<bool>,
    /// Pause or resume.
    pub paused: Option<bool>,
    /// Pause until this instant.
    pub set_paused_until: Option<jiff::Timestamp>,
    /// Clear the pause deadline.
    pub clear_paused_until: bool,
    /// Archive or unarchive.
    pub archived: Option<bool>,
}

impl From<RoutineEdit> for sunrise_domain::RoutinePatch {
    fn from(e: RoutineEdit) -> Self {
        Self {
            template: e.template.map(TaskTemplate::from),
            rrule: e.rrule.map(RRule::from),
            timezone: e.timezone,
            starts_at: e.starts_at,
            ends_at: patch_field(e.set_ends_at, e.clear_ends_at),
            scheduling_constraints: e
                .scheduling_constraints
                .map(|l| l.into_iter().map(ScheduleConstraint::from).collect()),
            catchup_policy: e.catchup_policy,
            grace_window_s: patch_field(e.set_grace_window_s, e.clear_grace_window),
            forgiveness_enabled: e.forgiveness_enabled,
            paused: e.paused,
            paused_until: patch_field(e.set_paused_until, e.clear_paused_until),
            archived: e.archived,
        }
    }
}

/// See [`sunrise_domain::RoutineRow`] — a routine projected for a list, with
/// its next occurrence resolved and its cadence in prose.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RoutineListRow {
    /// Routine id.
    pub id: EntityRef,
    /// Template title.
    pub title: String,
    /// Human-readable cadence, e.g. `every 2 weeks on Mo, We`.
    pub cadence: String,
    /// Next occurrence inside the routine's materialization horizon.
    pub next: Option<jiff::Timestamp>,
    /// Paused.
    pub paused: bool,
    /// The full template, so an edit does not drop what it did not touch.
    pub template: Template,
    /// The parsed recurrence, for the same reason.
    pub rrule: Recurrence,
    /// Current streak.
    pub streak: i64,
}

impl From<&RoutineRow> for RoutineListRow {
    fn from(r: &RoutineRow) -> Self {
        let RoutineRow {
            id,
            title,
            rrule,
            next,
            paused,
            template,
            rule,
            streak,
        } = r;
        Self {
            id: *id,
            title: title.clone(),
            cadence: rrule.clone(),
            next: *next,
            paused: *paused,
            template: Template::from(template),
            rrule: Recurrence::from(rule),
            streak: *streak,
        }
    }
}

// ---------------------------------------------------------------------------
// Focus
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::Chunk`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct ChunkMarker {
    /// 1-based index of this sitting.
    pub index: u32,
    /// Total sittings the estimate implies.
    pub total: u32,
}

impl From<&Chunk> for ChunkMarker {
    fn from(c: &Chunk) -> Self {
        let Chunk { index, total } = c;
        Self {
            index: *index,
            total: *total,
        }
    }
}

/// See [`sunrise_domain::SessionPlan`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct PlannedSession {
    /// Planned length; absent means "until done".
    pub planned_ms: Option<u64>,
    /// Chunk marker when the estimate exceeds one sitting.
    pub chunk: Option<ChunkMarker>,
}

impl From<&SessionPlan> for PlannedSession {
    fn from(p: &SessionPlan) -> Self {
        let SessionPlan { planned_ms, chunk } = p;
        Self {
            planned_ms: *planned_ms,
            chunk: chunk.as_ref().map(ChunkMarker::from),
        }
    }
}

/// See [`sunrise_domain::Interruption`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct InterruptionRow {
    /// The session interrupted.
    pub session_id: EntityRef,
    /// When.
    pub at: jiff::Timestamp,
    /// One-tap reason.
    pub reason: InterruptionReason,
}

impl From<&Interruption> for InterruptionRow {
    fn from(i: &Interruption) -> Self {
        let Interruption {
            session_id,
            at,
            reason,
        } = i;
        Self {
            session_id: *session_id,
            at: *at,
            reason: *reason,
        }
    }
}

/// See [`sunrise_domain::FocusStart`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionStart {
    /// Session id.
    pub id: EntityRef,
    /// Task focused on.
    pub task_id: EntityRef,
    /// Owning stream.
    pub stream_id: EntityRef,
    /// When it opened.
    pub started_at: jiff::Timestamp,
    /// Planned length; absent means "until done".
    pub planned_ms: Option<u64>,
    /// Declared energy budget.
    pub energy: Option<Energy>,
    /// Work or break.
    pub kind: FocusKind,
    /// Chunk marker.
    pub chunk: Option<ChunkMarker>,
}

impl From<&FocusStart> for SessionStart {
    fn from(s: &FocusStart) -> Self {
        let FocusStart {
            id,
            task_id,
            stream_id,
            started_at,
            planned_ms,
            energy,
            kind,
            chunk,
            unknown: _,
        } = s;
        Self {
            id: *id,
            task_id: *task_id,
            stream_id: *stream_id,
            started_at: *started_at,
            planned_ms: *planned_ms,
            energy: *energy,
            kind: *kind,
            chunk: chunk.as_ref().map(ChunkMarker::from),
        }
    }
}

/// See [`sunrise_domain::FocusEnd`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionEnd {
    /// Session closed.
    pub session_id: EntityRef,
    /// When.
    pub ended_at: jiff::Timestamp,
    /// Frozen focused time.
    pub actual_focused_ms: u64,
    /// Interruptions recorded against the session.
    pub interruptions: Vec<InterruptionRow>,
    /// Whether the task was completed in this session.
    pub completed_task: bool,
}

impl From<&FocusEnd> for SessionEnd {
    fn from(e: &FocusEnd) -> Self {
        let FocusEnd {
            session_id,
            ended_at,
            actual_focused_ms,
            interruptions,
            completed_task,
            unknown: _,
        } = e;
        Self {
            session_id: *session_id,
            ended_at: *ended_at,
            actual_focused_ms: *actual_focused_ms,
            interruptions: interruptions.iter().map(InterruptionRow::from).collect(),
            completed_task: *completed_task,
        }
    }
}

/// One focus session, with the two numbers derived from the clock rather than
/// stored. See [`sunrise_core::queries::FocusSessionRow`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionRow {
    /// How it opened.
    pub start: SessionStart,
    /// How it closed, if it has.
    pub end: Option<SessionEnd>,
    /// Interruptions, from both ops.
    pub interruptions: Vec<InterruptionRow>,
    /// Still running (a `start` with no `end` — a valid state).
    pub running: bool,
    /// Focused time: frozen once ended, derived while running.
    pub focused_ms: u64,
}

impl From<&FocusSessionRow> for SessionRow {
    fn from(r: &FocusSessionRow) -> Self {
        let FocusSessionRow {
            session,
            running,
            focused_ms,
        } = r;
        let FocusSession {
            start,
            end,
            interruptions,
        } = session;
        Self {
            start: SessionStart::from(start),
            end: end.as_ref().map(SessionEnd::from),
            interruptions: interruptions.iter().map(InterruptionRow::from).collect(),
            running: *running,
            focused_ms: *focused_ms,
        }
    }
}

/// See [`sunrise_domain::Calibration`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct CalibrationRow {
    /// Actual over estimated.
    pub factor: f64,
    /// How many sessions the factor folds.
    pub samples: u32,
    /// Total estimated time, in ms.
    pub estimated_ms: u64,
    /// Total actual focused time, in ms.
    pub actual_ms: u64,
}

impl From<&Calibration> for CalibrationRow {
    fn from(c: &Calibration) -> Self {
        let Calibration {
            factor,
            samples,
            estimated_ms,
            actual_ms,
        } = c;
        Self {
            factor: *factor,
            samples: *samples,
            estimated_ms: *estimated_ms,
            actual_ms: *actual_ms,
        }
    }
}

/// See [`sunrise_domain::StreamFocus`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamFocusRow {
    /// The stream.
    pub stream: EntityRef,
    /// Sessions folded.
    pub sessions: u32,
    /// Focused time, in ms.
    pub focused_ms: u64,
    /// Estimate calibration, when there is enough data.
    pub calibration: Option<CalibrationRow>,
}

impl From<&StreamFocus> for StreamFocusRow {
    fn from(s: &StreamFocus) -> Self {
        let StreamFocus {
            stream,
            sessions,
            focused_ms,
            calibration,
        } = s;
        Self {
            stream: *stream,
            sessions: *sessions,
            focused_ms: *focused_ms,
            calibration: calibration.as_ref().map(CalibrationRow::from),
        }
    }
}

/// See [`sunrise_domain::EnergyFocus`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct EnergyFocusRow {
    /// The energy facet; absent is the "no signal" bucket.
    pub energy: Option<Energy>,
    /// Sessions folded.
    pub sessions: u32,
    /// Focused time, in ms.
    pub focused_ms: u64,
    /// Estimate calibration.
    pub calibration: Option<CalibrationRow>,
}

impl From<&EnergyFocus> for EnergyFocusRow {
    fn from(e: &EnergyFocus) -> Self {
        let EnergyFocus {
            energy,
            sessions,
            focused_ms,
            calibration,
        } = e;
        Self {
            energy: *energy,
            sessions: *sessions,
            focused_ms: *focused_ms,
            calibration: calibration.as_ref().map(CalibrationRow::from),
        }
    }
}

/// See [`sunrise_domain::InterruptionTally`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct InterruptionTallyRow {
    /// The reason.
    pub reason: InterruptionReason,
    /// How many times.
    pub count: u32,
}

impl From<&InterruptionTally> for InterruptionTallyRow {
    fn from(t: &InterruptionTally) -> Self {
        let InterruptionTally { reason, count } = t;
        Self {
            reason: *reason,
            count: *count,
        }
    }
}

/// See [`sunrise_domain::FocusStats`] — totals and a calibration factor. No
/// score, no streak, no quota.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FocusTotals {
    /// Sessions folded.
    pub sessions: u32,
    /// Of which work (rather than break) sessions.
    pub work_sessions: u32,
    /// Sessions still running.
    pub running: u32,
    /// Focused time, in ms.
    pub total_focused_ms: u64,
    /// Interruptions logged.
    pub interruptions: u32,
    /// Per-stream breakdown.
    pub per_stream: Vec<StreamFocusRow>,
    /// Per-energy breakdown.
    pub per_energy: Vec<EnergyFocusRow>,
    /// Whole-vault calibration.
    pub overall: Option<CalibrationRow>,
    /// Most common interruption reasons.
    pub top_interruptions: Vec<InterruptionTallyRow>,
}

impl From<&FocusStats> for FocusTotals {
    fn from(s: &FocusStats) -> Self {
        let FocusStats {
            sessions,
            work_sessions,
            running,
            total_focused_ms,
            interruptions,
            per_stream,
            per_energy,
            overall,
            top_interruptions,
        } = s;
        Self {
            sessions: *sessions,
            work_sessions: *work_sessions,
            running: *running,
            total_focused_ms: *total_focused_ms,
            interruptions: *interruptions,
            per_stream: per_stream.iter().map(StreamFocusRow::from).collect(),
            per_energy: per_energy.iter().map(EnergyFocusRow::from).collect(),
            overall: overall.as_ref().map(CalibrationRow::from),
            top_interruptions: top_interruptions
                .iter()
                .map(InterruptionTallyRow::from)
                .collect(),
        }
    }
}

/// See [`sunrise_domain::UnblockCascade`] — what completing a task released.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Cascade {
    /// The task that was completed.
    pub completed: EntityRef,
    /// Tasks it released.
    pub released: Vec<EntityRef>,
    /// Tasks still waiting on something else.
    pub still_blocked: Vec<EntityRef>,
}

impl From<&UnblockCascade> for Cascade {
    fn from(c: &UnblockCascade) -> Self {
        let UnblockCascade {
            completed,
            released,
            still_blocked,
        } = c;
        Self {
            completed: *completed,
            released: released.clone(),
            still_blocked: still_blocked.clone(),
        }
    }
}

/// One row of the focus planner. See [`sunrise_core::queries::FocusPlanRow`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlanRow {
    /// The proposed task.
    pub task: TaskItem,
    /// Open tasks finishing this one would release — the leverage signal.
    pub unblocks: u32,
    /// How its energy facet scored against the session budget.
    pub energy_fit: EnergyFit,
    /// The session the core would open.
    pub suggested: PlannedSession,
    /// Work sessions this task has already had.
    pub prior_sessions: u32,
    /// Why it sits where it does, in one line.
    pub reason: String,
}

impl From<&FocusPlanRow> for PlanRow {
    fn from(r: &FocusPlanRow) -> Self {
        let FocusPlanRow {
            task,
            unblocks,
            energy_fit,
            suggested,
            prior_sessions,
        } = r;
        Self {
            task: TaskItem::from(task),
            unblocks: *unblocks,
            energy_fit: *energy_fit,
            suggested: PlannedSession::from(suggested),
            prior_sessions: *prior_sessions,
            reason: sunrise_domain::plan_reason(*unblocks, *energy_fit, suggested),
        }
    }
}

/// One row of the actionable list: a task plus the two dependency facts that
/// are recomputed rather than stored.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionableTaskRow {
    /// The task.
    pub task: TaskItem,
    /// User state widened with the derived `Blocked` case.
    pub effective_state: EffectiveTaskState,
    /// How many blockers are still open.
    pub open_blockers: u32,
    /// How many open tasks are waiting on this one.
    pub unblocks: u32,
}

impl From<&ActionableTask> for ActionableTaskRow {
    fn from(a: &ActionableTask) -> Self {
        let ActionableTask {
            task,
            effective_state,
            open_blockers,
            unblocks,
        } = a;
        Self {
            task: TaskItem::from(task),
            effective_state: *effective_state,
            open_blockers: *open_blockers,
            unblocks: *unblocks,
        }
    }
}

// ---------------------------------------------------------------------------
// Stats & review
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------

/// What happened, in one event. See [`sunrise_domain::ActivityKind`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ActivityDetail {
    /// Task created.
    TaskCreated,
    /// Task completed.
    TaskCompleted,
    /// Task reopened.
    TaskReopened,
    /// Task cancelled.
    TaskCancelled,
    /// Task deferred.
    TaskDeferred {
        /// Deferral count after the write.
        count: i64,
    },
    /// Task moved between streams.
    TaskMoved {
        /// Stream it left.
        from: EntityRef,
        /// Stream it joined.
        to: EntityRef,
    },
    /// Task edited.
    TaskUpdated {
        /// How many tracked fields changed.
        fields: u32,
    },
    /// Task tombstoned.
    TaskDeleted,
    /// Stream created.
    StreamCreated,
    /// Stream tombstoned.
    StreamDeleted,
    /// Focus session opened.
    FocusStarted {
        /// The session.
        session: EntityRef,
        /// Planned length.
        planned_ms: Option<u64>,
    },
    /// Focus session closed.
    FocusEnded {
        /// The session.
        session: EntityRef,
        /// Frozen focused time.
        focused_ms: u64,
        /// Whether the task was completed in it.
        completed_task: bool,
    },
}

impl From<&ActivityKind> for ActivityDetail {
    fn from(k: &ActivityKind) -> Self {
        match k {
            ActivityKind::TaskCreated => Self::TaskCreated,
            ActivityKind::TaskCompleted => Self::TaskCompleted,
            ActivityKind::TaskReopened => Self::TaskReopened,
            ActivityKind::TaskCancelled => Self::TaskCancelled,
            ActivityKind::TaskDeferred { count } => Self::TaskDeferred { count: *count },
            ActivityKind::TaskMoved { from, to } => Self::TaskMoved {
                from: *from,
                to: *to,
            },
            ActivityKind::TaskUpdated { fields } => Self::TaskUpdated { fields: *fields },
            ActivityKind::TaskDeleted => Self::TaskDeleted,
            ActivityKind::StreamCreated => Self::StreamCreated,
            ActivityKind::StreamDeleted => Self::StreamDeleted,
            ActivityKind::FocusStarted {
                session,
                planned_ms,
            } => Self::FocusStarted {
                session: *session,
                planned_ms: *planned_ms,
            },
            ActivityKind::FocusEnded {
                session,
                focused_ms,
                completed_task,
            } => Self::FocusEnded {
                session: *session,
                focused_ms: *focused_ms,
                completed_task: *completed_task,
            },
        }
    }
}

/// One row of an activity timeline.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ActivityRow {
    /// When (epoch ms).
    pub at_ms: u64,
    /// Op id, lowercase hex — UniFFI has no fixed-size array type.
    pub op_id: String,
    /// Authoring device id, lowercase hex.
    pub device: String,
    /// The entity the event is about.
    pub entity: EntityRef,
    /// The entity's title at the time.
    pub label: String,
    /// What happened.
    pub detail: ActivityDetail,
    /// What happened, in words — the shared phrasing, so every client says it
    /// the same way.
    pub phrase: String,
}

impl From<&ActivityEvent> for ActivityRow {
    fn from(e: &ActivityEvent) -> Self {
        let ActivityEvent {
            at_ms,
            op_id,
            device,
            entity,
            label,
            kind,
        } = e;
        Self {
            at_ms: *at_ms,
            op_id: hex16(op_id),
            device: hex16(device),
            entity: *entity,
            label: label.clone(),
            detail: ActivityDetail::from(kind),
            phrase: sunrise_domain::activity_phrase(kind),
        }
    }
}

// ---------------------------------------------------------------------------
// List rows and status
// ---------------------------------------------------------------------------

/// One row of the stream list; the Inbox comes first and is synthetic.
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamListRow {
    /// Stream id.
    pub id: EntityRef,
    /// Display name.
    pub name: String,
    /// Colour.
    pub color: StreamColor,
    /// Open tasks (todo / in-progress, not deleted).
    pub open_task_count: u64,
    /// Archived.
    pub archived: bool,
    /// Paused.
    pub paused: bool,
}

impl From<&StreamRow> for StreamListRow {
    fn from(s: &StreamRow) -> Self {
        let StreamRow {
            id,
            name,
            color,
            open_task_count,
            archived,
            paused,
        } = s;
        Self {
            id: *id,
            name: name.clone(),
            color: *color,
            open_task_count: *open_task_count,
            archived: *archived,
            paused: *paused,
        }
    }
}

/// One row of the context list.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ContextListRow {
    /// Context id.
    pub id: EntityRef,
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Archived.
    pub archived: bool,
    /// Live tasks carrying it.
    pub task_count: u64,
}

impl From<&ContextRow> for ContextListRow {
    fn from(c: &ContextRow) -> Self {
        let ContextRow {
            id,
            name,
            description,
            archived,
            task_count,
        } = c;
        Self {
            id: *id,
            name: name.clone(),
            description: description.clone(),
            archived: *archived,
            task_count: *task_count,
        }
    }
}

/// One paired device.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceListRow {
    /// Device id, lowercase hex.
    pub device_id: String,
    /// Human-readable nickname.
    pub nickname: String,
    /// Platform string.
    pub platform: String,
    /// Revoked.
    pub revoked: bool,
}

impl From<&DeviceRow> for DeviceListRow {
    fn from(d: &DeviceRow) -> Self {
        let DeviceRow {
            device_id,
            nickname,
            platform,
            revoked,
        } = d;
        Self {
            device_id: hex16(device_id),
            nickname: nickname.clone(),
            platform: platform.clone(),
            revoked: *revoked,
        }
    }
}

/// A snapshot of sync health.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncSnapshot {
    /// Connection state.
    pub state: sunrise_sync::SyncState,
    /// Ops waiting to be sent.
    pub outbox_pending: u32,
    /// Trusted peer devices.
    pub peer_devices: u32,
    /// Last successful sync (epoch ms).
    pub last_sync_ms: Option<u64>,
}

impl From<&sunrise_core::SyncStatus> for SyncSnapshot {
    fn from(s: &sunrise_core::SyncStatus) -> Self {
        let sunrise_core::SyncStatus {
            state,
            outbox_pending,
            peer_devices,
            last_sync_ms,
        } = s;
        Self {
            state: *state,
            outbox_pending: *outbox_pending,
            peer_devices: *peer_devices,
            last_sync_ms: *last_sync_ms,
        }
    }
}

/// What a command did.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CommandOutcome {
    /// The entity the command touched.
    pub entity: EntityRef,
    /// New task state, where the command set one.
    pub state: Option<TaskState>,
    /// Op id, lowercase hex.
    pub op_id: String,
    /// Sequence number the core assigned.
    pub seq: u64,
    /// `soft` scheduling constraints the accepted time violates.
    ///
    /// A soft violation never blocks the write (a hard one is rejected
    /// outright), but it must not vanish silently either: it demotes the item
    /// in planning views, and the client is expected to say *which* window was
    /// missed. Empty for every command that schedules nothing.
    pub soft_violations: Vec<Constraint>,
}

impl From<&CommandResult> for CommandOutcome {
    fn from(r: &CommandResult) -> Self {
        let CommandResult {
            entity,
            state,
            op_id,
            seq,
            soft_violations,
        } = r;
        Self {
            entity: *entity,
            state: *state,
            op_id: hex16(op_id),
            seq: *seq,
            soft_violations: soft_violations.iter().map(Constraint::from).collect(),
        }
    }
}
