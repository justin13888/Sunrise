//! The editors' forms and the defer/schedule targets, lowered onto
//! [`sunrise_client_core::editor`] and [`sunrise_client_core::schedule`].
//!
//! Each editor is a pair: `*_editor_form` seeds what the sheet shows from the
//! stored record, and `*_editor_edit` turns what the user left on screen into
//! the split-optional edit that carries **only what changed**, with every time
//! in the kind it already had (#515, ADR-0066 §4). A form left alone is an
//! empty edit. The defer and schedule functions return the [`TimeValue`] a
//! `CoreCommand::DeferTaskTo` or an edit carries, counted in civil days in the
//! caller's zone.
//!
//! `tz` is an IANA zone name. Unlike [`crate::vocab`], an unknown one is an
//! error ([`BindingError::BadTime`]), never UTC: these write, and a write in
//! the wrong zone is stored (ADR-0066 §3).

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_client_core::civil::{self, CivilError, TimeKind};
use sunrise_client_core::editor::{self, EditorError};
use sunrise_client_core::schedule;
use sunrise_domain::{
    BlockPatch, Energy, RRule, RoutineCatchupPolicy, RoutinePatch, SnoozeSpan, SunriseTime,
    TaskPatch, TaskTemplate,
};
use sunrise_id::EntityRef;

use crate::dto::{
    BlockEdit, BlockItem, Recurrence, RoutineEdit, RoutineItem, TaskEdit, TaskItem, Template,
    TimeValue,
};
use crate::BindingError;

/// See [`sunrise_client_core::civil::TimeKind`]: which of the four kinds of
/// time a picker asks for.
#[uniffi::remote(Enum)]
pub enum TimeKind {
    Instant,
    Zoned,
    Floating,
    AllDay,
}

/// A moment a picker produced, and the kind asked for (`None` keeps the
/// stored kind). See [`sunrise_client_core::civil::PickedTime`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct PickedTime {
    /// Epoch milliseconds.
    pub ms: i64,
    /// The kind asked for, or `None` to keep the stored one.
    #[uniffi(default = None)]
    pub kind: Option<TimeKind>,
}

impl From<PickedTime> for civil::PickedTime {
    fn from(p: PickedTime) -> Self {
        Self {
            ms: p.ms,
            kind: p.kind,
        }
    }
}

impl From<civil::PickedTime> for PickedTime {
    fn from(p: civil::PickedTime) -> Self {
        Self {
            ms: p.ms,
            kind: p.kind,
        }
    }
}

/// What the task editor shows. See [`sunrise_client_core::editor::TaskForm`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct TaskEditorForm {
    /// Title.
    pub title: String,
    /// Priority 1..=5, or none.
    pub priority: Option<u8>,
    /// Energy facet, or any.
    pub energy: Option<Energy>,
    /// Estimate in seconds; none or zero is no estimate.
    pub estimated_duration_s: Option<u64>,
    /// The hard-deadline picker, or none when the deadline is switched off.
    pub due_at: Option<PickedTime>,
    /// The soft-deadline picker (`target_at`, ADR-0047 §1), or none when it
    /// is switched off. [`task_editor_form`] seeds it from the task; a client
    /// posts back the form it was given, so one that does not show the
    /// target leaves it as it was.
    #[uniffi(default = None)]
    pub target_at: Option<PickedTime>,
}

/// What the block editor shows. See [`sunrise_client_core::editor::BlockForm`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct BlockEditorForm {
    /// The stored title (blank clears it), not the resolved one a row shows.
    pub title: String,
    /// Follow the single bound task's title.
    pub title_track_task: bool,
    /// Start, epoch milliseconds.
    pub starts_at_ms: i64,
    /// End, epoch milliseconds.
    pub ends_at_ms: i64,
    /// The kind picker.
    pub kind: TimeKind,
}

/// What the routine editor shows. See
/// [`sunrise_client_core::editor::RoutineForm`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct RoutineEditorForm {
    /// Title of each generated task.
    pub title: String,
    /// Stream each generated task lands in.
    pub stream_id: EntityRef,
    /// Priority 1..=5, or none.
    pub priority: Option<u8>,
    /// Energy facet, or any.
    pub energy: Option<Energy>,
    /// Estimate in seconds; none or zero is no estimate.
    pub estimated_duration_s: Option<u64>,
    /// The recurrence.
    pub rrule: Recurrence,
    /// IANA zone.
    pub timezone: String,
    /// Catch-up policy.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Series end as an instant, resolved in `timezone`; kept for clients
    /// built before #506. An unchanged one keeps the stored end's kind.
    pub ends_at: Option<Timestamp>,
    /// Series end with its kind (`floating` or `all_day`). Wins over
    /// `ends_at` when set.
    #[uniffi(default = None)]
    pub ends_at_time: Option<TimeValue>,
}

/// The task editor's form for `task`, seeded from what is stored.
#[uniffi::export]
pub fn task_editor_form(task: TaskItem, tz: String) -> Result<TaskEditorForm, BindingError> {
    let form = editor::task_form(&task_fields(&task), &zone(&tz)?);
    Ok(TaskEditorForm {
        title: form.title,
        priority: form.priority,
        energy: form.energy,
        estimated_duration_s: form.estimated_duration_s,
        due_at: form.hard_due_at.map(PickedTime::from),
        target_at: form.target_at.map(PickedTime::from),
    })
}

/// The edit the task editor's `form` makes to `task`: only what changed, and
/// a deadline in the kind it already had. The body is not the form's; a
/// caller with a note editor adds its two fields to the result.
#[uniffi::export]
pub fn task_editor_edit(
    task: TaskItem,
    form: TaskEditorForm,
    tz: String,
) -> Result<TaskEdit, BindingError> {
    let form = editor::TaskForm {
        title: form.title,
        priority: form.priority,
        energy: form.energy,
        estimated_duration_s: form.estimated_duration_s,
        target_at: form.target_at.map(civil::PickedTime::from),
        hard_due_at: form.due_at.map(civil::PickedTime::from),
    };
    let patch =
        editor::task_patch(&task_fields(&task), &form, &zone(&tz)?).map_err(editor_error)?;
    Ok(task_edit(patch))
}

/// The block editor's form for `block`, seeded from what is stored.
#[uniffi::export]
pub fn block_editor_form(block: BlockItem, tz: String) -> Result<BlockEditorForm, BindingError> {
    let form = editor::block_form(&block.to_domain(), &zone(&tz)?);
    Ok(BlockEditorForm {
        title: form.title,
        title_track_task: form.title_track_task,
        starts_at_ms: form.starts_at_ms,
        ends_at_ms: form.ends_at_ms,
        kind: form.kind,
    })
}

/// The edit the block editor's `form` makes to `block`. A moved bound keeps
/// its kind and zone; a changed kind rewrites both bounds.
#[uniffi::export]
pub fn block_editor_edit(
    block: BlockItem,
    form: BlockEditorForm,
    tz: String,
) -> Result<BlockEdit, BindingError> {
    let form = editor::BlockForm {
        title: form.title,
        title_track_task: form.title_track_task,
        starts_at_ms: form.starts_at_ms,
        ends_at_ms: form.ends_at_ms,
        kind: form.kind,
    };
    editor::block_patch(&block.to_domain(), &form, &zone(&tz)?)
        .map(block_edit)
        .map_err(editor_error)
}

/// The routine editor's form for `routine`, seeded from what is stored.
#[uniffi::export]
#[must_use]
pub fn routine_editor_form(routine: RoutineItem) -> RoutineEditorForm {
    let form = editor::routine_form(&routine_fields(routine));
    RoutineEditorForm {
        title: form.title,
        stream_id: sunrise_domain::stream_key(form.stream_id),
        priority: form.priority,
        energy: form.energy,
        estimated_duration_s: form.estimated_duration_s,
        rrule: Recurrence::from(&form.rrule),
        ends_at: form
            .ends_at
            .as_ref()
            .map(|e| e.instant_or_last(&sunrise_client_core::vocab::zone_or_utc(&form.timezone))),
        ends_at_time: form.ends_at.as_ref().map(TimeValue::from),
        timezone: form.timezone,
        catchup_policy: form.catchup_policy,
    }
}

/// The edit the routine editor's `form` makes to `routine`. The template is
/// sent only when a field of it changed, and keeps the fields the form does
/// not show.
#[uniffi::export]
pub fn routine_editor_edit(
    routine: RoutineItem,
    form: RoutineEditorForm,
) -> Result<RoutineEdit, BindingError> {
    let stored = routine_fields(routine);
    let zone = sunrise_client_core::vocab::zone_or_utc(&stored.timezone);
    let ends_at = match (form.ends_at_time, form.ends_at) {
        (Some(t), _) => Some(SunriseTime::from(t)),
        // A legacy client hands back the instant it was given: unchanged,
        // so the stored end keeps its kind.
        (None, Some(at))
            if stored
                .ends_at
                .as_ref()
                .is_some_and(|e| e.instant_or_last(&zone) == at) =>
        {
            stored.ends_at.clone()
        }
        (None, other) => other.map(SunriseTime::from),
    };
    let form = editor::RoutineForm {
        title: form.title,
        // The legacy Inbox id means no stream (ADR-0046 §5).
        stream_id: sunrise_domain::home_stream(Some(form.stream_id)),
        priority: form.priority,
        energy: form.energy,
        estimated_duration_s: form.estimated_duration_s,
        rrule: RRule::from(form.rrule),
        timezone: form.timezone,
        catchup_policy: form.catchup_policy,
        ends_at,
    };
    editor::routine_patch(&stored, &form)
        .map(routine_edit)
        .map_err(editor_error)
}

/// Where a task scheduled at `scheduled_at` lands when deferred to `days`
/// days after today: the date moves, the time and kind stay. See
/// [`sunrise_client_core::schedule::deferred_to`]. Hand it to
/// `CoreCommand::DeferTaskTo`.
#[uniffi::export]
pub fn defer_time_value(
    scheduled_at: Option<TimeValue>,
    days: i32,
    now_ms: u64,
    tz: String,
) -> Result<TimeValue, BindingError> {
    let plan = scheduled_at.map(SunriseTime::from);
    schedule::deferred_to(plan.as_ref(), days, now(now_ms)?, &zone(&tz)?)
        .map(|v| TimeValue::from(&v))
        .map_err(civil_error)
}

/// Where a snooze of `span` puts a task scheduled at `scheduled_at`: an hour
/// is an instant an hour on, tomorrow and next week keep the plan's kind.
/// See [`sunrise_client_core::schedule::snoozed_to`].
#[uniffi::export]
pub fn snooze_time_value(
    scheduled_at: Option<TimeValue>,
    span: SnoozeSpan,
    now_ms: u64,
    tz: String,
) -> Result<TimeValue, BindingError> {
    let plan = scheduled_at.map(SunriseTime::from);
    schedule::snoozed_to(plan.as_ref(), span, now(now_ms)?, &zone(&tz)?)
        .map(|v| TimeValue::from(&v))
        .map_err(civil_error)
}

/// A plan picked in a "Schedule…" sheet: the moment `ms` as `kind`, floating
/// when `kind` is none. See [`sunrise_client_core::schedule::scheduled_at`].
#[uniffi::export]
pub fn scheduled_time_value(
    ms: i64,
    kind: Option<TimeKind>,
    tz: String,
) -> Result<TimeValue, BindingError> {
    schedule::scheduled_at(ms, kind, &zone(&tz)?)
        .map(|v| TimeValue::from(&v))
        .map_err(civil_error)
}

/// The day `days` after today, all day: "tomorrow" (`1`) or "next week"
/// (`7`). See [`sunrise_client_core::schedule::scheduled_on_day`].
#[uniffi::export]
pub fn scheduled_day_value(days: i32, now_ms: u64, tz: String) -> Result<TimeValue, BindingError> {
    schedule::scheduled_on_day(days, now(now_ms)?, &zone(&tz)?)
        .map(|v| TimeValue::from(&v))
        .map_err(civil_error)
}

/// The plan a capture into Today gets when the line named no date: today,
/// all day. See [`sunrise_client_core::schedule::today_capture_plan`].
#[uniffi::export]
pub fn today_capture_scheduled_at(now_ms: u64, tz: String) -> Result<TimeValue, BindingError> {
    Ok(TimeValue::from(&schedule::today_capture_plan(
        now(now_ms)?,
        &zone(&tz)?,
    )))
}

/// The reader's zone, refusing a name the tzdb does not know.
pub(crate) fn zone(tz: &str) -> Result<TimeZone, BindingError> {
    civil::reader_zone(tz).map_err(civil_error)
}

/// `now_ms` as a timestamp.
fn now(now_ms: u64) -> Result<Timestamp, BindingError> {
    let ms = i64::try_from(now_ms).map_err(|_| BindingError::BadTime {
        value: now_ms.to_string(),
        cause: "past the end of the timestamp range".into(),
    })?;
    civil::instant(ms).map_err(civil_error)
}

/// A [`CivilError`] as the seam's error.
pub(crate) fn civil_error(e: CivilError) -> BindingError {
    let value = match &e {
        CivilError::UnknownZone { name } => name.clone(),
        CivilError::OutOfRange { ms } => ms.to_string(),
        CivilError::Unresolvable(_) => String::new(),
    };
    BindingError::BadTime {
        value,
        cause: e.to_string(),
    }
}

/// An [`EditorError`] as the seam's error.
pub(crate) fn editor_error(e: EditorError) -> BindingError {
    match e {
        EditorError::Time(t) => civil_error(t),
        other => BindingError::Core(other.to_string()),
    }
}

/// The stored fields the task editor compares against.
fn task_fields(task: &TaskItem) -> editor::TaskFields {
    editor::TaskFields {
        title: task.title.clone(),
        priority: task.priority,
        energy: task.energy.clone(),
        estimated_duration_s: task.estimated_duration_s,
        target_at: task.target_at.clone().map(SunriseTime::from),
        hard_due_at: task.due_at.clone().map(SunriseTime::from),
    }
}

/// The stored fields the routine editor compares against.
fn routine_fields(routine: RoutineItem) -> editor::RoutineFields {
    editor::RoutineFields {
        template: TaskTemplate::from(routine.template),
        rrule: RRule::from(routine.rrule),
        timezone: routine.timezone,
        catchup_policy: routine.catchup_policy,
        ends_at: routine
            .ends_at_time
            .map(SunriseTime::from)
            .or_else(|| routine.ends_at.map(Into::into)),
    }
}

/// A `Some(None)` / `Some(Some(v))` / `None` patch field as the seam's
/// `set_x` / `clear_x` pair.
#[allow(clippy::option_option)]
fn split<T>(field: Option<Option<T>>) -> (Option<T>, bool) {
    match field {
        Some(Some(v)) => (Some(v), false),
        Some(None) => (None, true),
        None => (None, false),
    }
}

/// The task editor's patch as a [`TaskEdit`]. The editor sets only its own
/// six fields; the rest stay at their defaults.
fn task_edit(patch: TaskPatch) -> TaskEdit {
    let (set_priority, clear_priority) = split(patch.priority);
    let (set_energy, clear_energy) = split(patch.energy);
    let (set_estimated_duration_s, clear_estimated_duration) = split(patch.estimated_duration_s);
    let (set_due_at, clear_due_at) = split(patch.hard_due_at);
    let (set_target_at, clear_target_at) = split(patch.target_at);
    TaskEdit {
        title: patch.title,
        set_priority,
        clear_priority,
        set_energy,
        clear_energy,
        set_estimated_duration_s,
        clear_estimated_duration,
        set_due_at: set_due_at.as_ref().map(TimeValue::from),
        clear_due_at,
        set_target_at: set_target_at.as_ref().map(TimeValue::from),
        clear_target_at,
        ..TaskEdit::default()
    }
}

/// A block patch as a [`BlockEdit`].
pub(crate) fn block_edit(patch: BlockPatch) -> BlockEdit {
    let (set_title, clear_title) = split(patch.title);
    BlockEdit {
        starts_at: patch.starts_at.as_ref().map(TimeValue::from),
        ends_at: patch.ends_at.as_ref().map(TimeValue::from),
        set_title,
        clear_title,
        title_track_task: patch.title_track_task,
        stream_id: patch.stream_id.map(sunrise_domain::stream_key),
    }
}

/// The routine editor's patch as a [`RoutineEdit`].
fn routine_edit(patch: RoutinePatch) -> RoutineEdit {
    let (set_ends_at, clear_ends_at) = split(patch.ends_at);
    RoutineEdit {
        template: patch.template.as_ref().map(Template::from),
        rrule: patch.rrule.as_ref().map(Recurrence::from),
        timezone: patch.timezone,
        catchup_policy: patch.catchup_policy,
        set_ends_at: set_ends_at
            .as_ref()
            .map(|e| e.instant_or_last(&jiff::tz::TimeZone::UTC)),
        set_ends_at_time: set_ends_at.as_ref().map(TimeValue::from),
        clear_ends_at,
        ..RoutineEdit::default()
    }
}

#[cfg(test)]
mod tests;
