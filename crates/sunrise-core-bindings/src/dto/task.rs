//! Task: the task record, its edit, its draft and their patch fold.

use sunrise_domain::{Energy, NoteBody, ScheduleConstraint, SunriseTime, Task, TaskState};
use sunrise_id::EntityRef;

use super::{Constraint, TimeValue};

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
    /// Owning Stream, or the legacy Inbox id
    /// ([`sunrise_domain::inbox_stream_ref`]) for a task that belongs to no
    /// stream. Kept for clients that predate ADR-0046; [`Self::home_stream`]
    /// says the same thing as an optional.
    pub stream_id: EntityRef,
    /// Contexts carried, sorted.
    pub contexts: Vec<EntityRef>,
    /// User-set state. `blocked` is derived and lives on
    /// [`ActionableTaskRow::effective_state`](super::ActionableTaskRow::effective_state),
    /// never here.
    pub state: TaskState,
    /// Priority 1..=5.
    pub priority: Option<u8>,
    /// Energy facet.
    pub energy: Option<Energy>,
    /// Estimate, in seconds.
    pub estimated_duration_s: Option<u64>,
    /// When the user intends to work on it: the task's `planned_at`
    /// (ADR-0047 §1), under the name it has always had here.
    pub scheduled_at: Option<TimeValue>,
    /// The hard deadline: the task's `hard_due_at` (ADR-0047 §1), under the
    /// name it has always had here.
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
    /// Per-task reminder lead time in seconds; absent falls back to the
    /// stream's, then to the device default.
    pub reminder_lead_s: Option<u32>,
    /// Archived.
    pub archived: bool,
    /// Tombstoned.
    pub deleted: bool,
    /// The task's manual-order key, or none when nobody placed it. Opaque:
    /// compare two with string order, which is their order, and move a task
    /// with `CoreCommand::MoveTasks`, never by writing one (#340).
    #[uniffi(default = None)]
    pub sort_order: Option<String>,
    /// The stream the task belongs to, or none (ADR-0046 §1).
    #[uniffi(default = None)]
    pub home_stream: Option<EntityRef>,
    /// When the task was triaged out of the Inbox, or none (ADR-0046 §3):
    /// `CoreCommand::MarkTriaged` sets it, `ReturnToInbox` clears it.
    #[uniffi(default = None)]
    pub triaged_at: Option<jiff::Timestamp>,
    /// The soft deadline (`target_at`, ADR-0047 §1), or none.
    #[uniffi(default = None)]
    pub target_at: Option<TimeValue>,
    /// Why the task was dropped, while it is cancelled (ADR-0047 §4); none
    /// otherwise, and none for a drop that gave no reason.
    #[uniffi(default = None)]
    pub cancel_reason: Option<String>,
    /// When the user last kept the task in the triage queue, or none.
    #[uniffi(default = None)]
    pub late_acknowledged_at: Option<jiff::Timestamp>,
    /// A routine occurrence generated planned where it breaks a `hard`
    /// scheduling constraint (#331): generated all the same, and flagged
    /// until it is re-planned.
    #[uniffi(default = false)]
    pub hard_constraint_violated: bool,
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
            planned_at,
            target_at,
            hard_due_at,
            scheduling_constraints,
            completed_at,
            deferred_count,
            blocks,
            blocked_by,
            assignee,
            routine_id,
            routine_occurrence,
            hard_constraint_violated,
            reminder_lead_s,
            sort_order,
            triaged_at,
            cancel_reason: _,
            late_acknowledged_at,
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
            stream_id: sunrise_domain::stream_key(*stream_id),
            contexts: contexts.iter().copied().collect(),
            state: state.clone(),
            priority: *priority,
            energy: energy.clone(),
            estimated_duration_s: *estimated_duration_s,
            scheduled_at: planned_at.as_ref().map(TimeValue::from),
            due_at: hard_due_at.as_ref().map(TimeValue::from),
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
            reminder_lead_s: *reminder_lead_s,
            archived: *archived,
            deleted: *deleted,
            sort_order: sort_order.clone(),
            home_stream: *stream_id,
            triaged_at: *triaged_at,
            target_at: target_at.as_ref().map(TimeValue::from),
            cancel_reason: t.drop_reason().map(str::to_owned),
            late_acknowledged_at: *late_acknowledged_at,
            hard_constraint_violated: *hard_constraint_violated,
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
    /// Owning Stream; no stream when absent or the legacy Inbox id.
    pub stream_id: Option<EntityRef>,
    /// Contexts.
    pub contexts: Vec<EntityRef>,
    /// Priority 1..=5.
    pub priority: Option<u8>,
    /// Energy facet.
    pub energy: Option<Energy>,
    /// Estimate, in seconds.
    pub estimated_duration_s: Option<u64>,
    /// When to do it: `planned_at`.
    pub scheduled_at: Option<TimeValue>,
    /// The hard deadline: `hard_due_at`.
    pub due_at: Option<TimeValue>,
    /// Scheduling constraints.
    pub scheduling_constraints: Vec<Constraint>,
    /// Informational assignee.
    pub assignee: Option<EntityRef>,
    /// Per-task reminder lead time in seconds.
    pub reminder_lead_s: Option<u32>,
    /// The soft deadline: `target_at`.
    #[uniffi(default = None)]
    pub target_at: Option<TimeValue>,
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
            planned_at: d.scheduled_at.map(SunriseTime::from),
            target_at: d.target_at.map(SunriseTime::from),
            hard_due_at: d.due_at.map(SunriseTime::from),
            scheduling_constraints: d
                .scheduling_constraints
                .into_iter()
                .map(ScheduleConstraint::from)
                .collect(),
            assignee: d.assignee,
            reminder_lead_s: d.reminder_lead_s,
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
///
/// Every field carries a UniFFI default, so the foreign side writes
/// `TaskEdit()` and sets the two fields it means. Without them a client
/// changing one field has to spell out all twenty-three, and the twenty-two it
/// does not care about are exactly where a `false` gets typed for a `clear_`
/// flag by accident.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct TaskEdit {
    /// New title.
    #[uniffi(default = None)]
    pub title: Option<String>,
    /// New body.
    #[uniffi(default = None)]
    pub set_body: Option<NoteBody>,
    /// Clear the body.
    #[uniffi(default = false)]
    pub clear_body: bool,
    /// Move to another Stream. The same move `PromoteToStream` makes: that
    /// command lowers to a stream-only `TaskPatch`, which is this field. The
    /// legacy Inbox id ([`crate::vocab::inbox_stream_id`]) moves it to no stream.
    #[uniffi(default = None)]
    pub stream_id: Option<EntityRef>,
    /// Replace the whole context set.
    #[uniffi(default = None)]
    pub contexts: Option<Vec<EntityRef>>,
    /// New state.
    #[uniffi(default = None)]
    pub state: Option<TaskState>,
    /// New priority.
    #[uniffi(default = None)]
    pub set_priority: Option<u8>,
    /// Clear the priority.
    #[uniffi(default = false)]
    pub clear_priority: bool,
    /// New energy facet.
    #[uniffi(default = None)]
    pub set_energy: Option<Energy>,
    /// Clear the energy facet.
    #[uniffi(default = false)]
    pub clear_energy: bool,
    /// New estimate, in seconds.
    #[uniffi(default = None)]
    pub set_estimated_duration_s: Option<u64>,
    /// Clear the estimate.
    #[uniffi(default = false)]
    pub clear_estimated_duration: bool,
    /// New plan (`planned_at`).
    #[uniffi(default = None)]
    pub set_scheduled_at: Option<TimeValue>,
    /// Clear the plan.
    #[uniffi(default = false)]
    pub clear_scheduled_at: bool,
    /// New hard deadline (`hard_due_at`).
    #[uniffi(default = None)]
    pub set_due_at: Option<TimeValue>,
    /// Clear the hard deadline.
    #[uniffi(default = false)]
    pub clear_due_at: bool,
    /// Replace the whole constraint list.
    #[uniffi(default = None)]
    pub scheduling_constraints: Option<Vec<Constraint>>,
    /// Replace the whole blocker set.
    #[uniffi(default = None)]
    pub blocked_by: Option<Vec<EntityRef>>,
    /// New assignee.
    #[uniffi(default = None)]
    pub set_assignee: Option<EntityRef>,
    /// Clear the assignee.
    #[uniffi(default = false)]
    pub clear_assignee: bool,
    /// New reminder lead time, in seconds.
    #[uniffi(default = None)]
    pub set_reminder_lead_s: Option<u32>,
    /// Clear it, falling back to the stream's.
    #[uniffi(default = false)]
    pub clear_reminder_lead_s: bool,
    /// Archive or unarchive.
    #[uniffi(default = None)]
    pub archived: Option<bool>,
    /// New soft deadline (`target_at`).
    #[uniffi(default = None)]
    pub set_target_at: Option<TimeValue>,
    /// Clear the soft deadline.
    #[uniffi(default = false)]
    pub clear_target_at: bool,
}

/// `Some(None)` when `clear`, `Some(Some(v))` when set, `None` when neither.
///
/// The nesting is the domain's `TaskPatch`/`StreamPatch`/`RoutinePatch` shape,
/// not a choice made here; this is the function that builds it.
#[allow(clippy::option_option)]
pub(super) fn patch_field<T>(set: Option<T>, clear: bool) -> Option<Option<T>> {
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
            // The legacy Inbox id moves the task to no stream (ADR-0046 §5).
            stream_id: e.stream_id.map(|s| sunrise_domain::home_stream(Some(s))),
            contexts: e.contexts,
            state: e.state,
            priority: patch_field(e.set_priority, e.clear_priority),
            energy: patch_field(e.set_energy, e.clear_energy),
            estimated_duration_s: patch_field(
                e.set_estimated_duration_s,
                e.clear_estimated_duration,
            ),
            planned_at: patch_field(
                e.set_scheduled_at.map(SunriseTime::from),
                e.clear_scheduled_at,
            ),
            target_at: patch_field(e.set_target_at.map(SunriseTime::from), e.clear_target_at),
            hard_due_at: patch_field(e.set_due_at.map(SunriseTime::from), e.clear_due_at),
            scheduling_constraints: e
                .scheduling_constraints
                .map(|l| l.into_iter().map(ScheduleConstraint::from).collect()),
            blocked_by: e.blocked_by,
            assignee: patch_field(e.set_assignee, e.clear_assignee),
            reminder_lead_s: patch_field(e.set_reminder_lead_s, e.clear_reminder_lead_s),
            archived: e.archived,
            // A client never writes a key: `CoreCommand::MoveTasks` does.
            sort_order: None,
            // Nor an acknowledgement: `CoreCommand::Triage` with Keep does.
            late_acknowledged_at: None,
        }
    }
}

#[cfg(test)]
mod task_edit_tests {
    use sunrise_domain::TaskPatch;
    use sunrise_id::{EntityKind, EntityRef};

    use super::{patch_field, TaskEdit};

    /// The three answers a doubly optional field can carry, and `clear`
    /// winning over a value set beside it.
    #[test]
    fn clear_wins_over_a_value_set_beside_it() {
        assert_eq!(patch_field(Some(3u8), true), Some(None));
        assert_eq!(patch_field(None::<u8>, true), Some(None));
        assert_eq!(patch_field(Some(3u8), false), Some(Some(3)));
        assert_eq!(patch_field(None::<u8>, false), None);
    }

    /// The rule holds through the edit a client hands in, not only in the
    /// helper.
    #[test]
    fn a_task_edit_setting_and_clearing_a_field_clears_it() {
        let assignee = EntityRef::new(EntityKind::Device, [4; 16]);
        let patch = TaskPatch::from(TaskEdit {
            set_priority: Some(3),
            clear_priority: true,
            set_estimated_duration_s: Some(600),
            clear_estimated_duration: true,
            set_assignee: Some(assignee),
            clear_assignee: true,
            set_reminder_lead_s: Some(300),
            clear_reminder_lead_s: true,
            ..TaskEdit::default()
        });
        assert_eq!(patch.priority, Some(None));
        assert_eq!(patch.estimated_duration_s, Some(None));
        assert_eq!(patch.assignee, Some(None));
        assert_eq!(patch.reminder_lead_s, Some(None));

        let set = TaskPatch::from(TaskEdit {
            set_priority: Some(3),
            ..TaskEdit::default()
        });
        assert_eq!(set.priority, Some(Some(3)));
        assert_eq!(set.estimated_duration_s, None, "an untouched field");
    }
}
