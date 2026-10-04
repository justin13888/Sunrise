//! Task entity per `docs/02-domain/tasks.md`.

use crate::common::{Energy, NoteBody};
use crate::constraint::{validate_list as validate_constraint_list, ScheduleConstraint};
use crate::time::SunriseTime;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::{validate_title, ValidationError, MAX_TASK_TITLE_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// User-visible Task state. `done` and `cancelled` are NOT terminal: a task
/// can transition back to `todo`. `blocked` is **derived** at read time from
/// `blocked_by` and the blockers' states; it is NOT persisted.
///
/// An unrecognised state reads as [`TaskState::Todo`] (its
/// [`TaskState::effective`]) and is written back verbatim. It is an OPEN item:
/// reading it as `Done` would silently mark someone's work complete; reading
/// it as `Todo` at worst shows a task that a newer client considers handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    /// Pending; not yet started.
    Todo,
    /// Started but not yet complete.
    InProgress,
    /// Done. Not terminal — may transition back.
    Done,
    /// User explicitly cancelled. Not terminal.
    Cancelled,
    /// A state this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(TaskState, fallback = Todo, {
    Todo => "todo",
    InProgress => "in_progress",
    Done => "done",
    Cancelled => "cancelled",
});

impl TaskState {
    /// Allowed self-set transitions. (Note: `blocked` is derived, not
    /// persisted; it does not appear here.)
    ///
    /// An unknown current state transitions as its fallback, `todo`. An
    /// unknown *target* is allowed: no picker offers one, so it only arrives
    /// as a value restored verbatim — an undo putting back the state a newer
    /// client wrote — and this build has no rule to judge it by.
    #[must_use]
    pub fn can_transition_to(&self, next: &Self) -> bool {
        // todo ↔ in_progress, todo ↔ cancelled, in_progress ↔ done,
        // done → todo (resurrect), cancelled → todo. Same-state is a no-op.
        if self == next || next.is_unknown() {
            return true;
        }
        matches!(
            (self.effective(), next),
            (Self::Todo, Self::InProgress | Self::Cancelled | Self::Done)
                | (Self::InProgress, Self::Todo | Self::Done | Self::Cancelled)
                | (Self::Done | Self::Cancelled, Self::Todo | Self::InProgress)
        )
    }
}

/// Persisted Task.
///
/// How each field merges (ADR-0044, declared in `sunrise_id::for_each_entity!`):
/// scalars, optionals, `body` and `scheduling_constraints` are LWW registers;
/// `contexts` and `blocked_by` are add-wins OR-sets; `deferred_count` is a
/// PN-counter. `blocks` is derived from the bound blocks' `tasks` on read and
/// is not merged, and neither is `blocks_others`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Unique id (typed reference).
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-update time (CRDT-derived: max of contributing op times).
    pub updated_at: Timestamp,
    /// Title; non-empty after trim, ≤ 512 chars.
    pub title: String,
    /// Optional body (rich text).
    #[serde(default)]
    pub body: Option<NoteBody>,
    /// Owning Stream (Inbox if unassigned).
    pub stream_id: EntityRef,
    /// Tag set (OR-Set semantics on the wire).
    #[serde(default)]
    pub contexts: BTreeSet<EntityRef>,
    /// User-set state (`blocked` is derived at read time, never stored here).
    pub state: TaskState,
    /// Optional priority 1..=5.
    #[serde(default)]
    pub priority: Option<u8>,
    /// Optional energy facet.
    #[serde(default)]
    pub energy: Option<Energy>,
    /// Optional ISO-8601 duration in seconds (we store seconds rather than
    /// the ISO string to keep CBOR canonical).
    #[serde(default)]
    pub estimated_duration_s: Option<u64>,
    /// When the user intends to do it. See [`SunriseTime`] — "Tuesday
    /// morning" and "09:00 New York" are not the same kind of answer as
    /// "this instant", and an earlier schema stored all three as the last one.
    #[serde(default)]
    pub scheduled_at: Option<SunriseTime>,
    /// Hard deadline.
    #[serde(default)]
    pub due_at: Option<SunriseTime>,
    /// Scheduling constraints (value list; whole list is one LWW register).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduling_constraints: Vec<ScheduleConstraint>,
    /// Set on transition to Done. Always written as
    /// [`SunriseTime::Instant`] — a completion is a recorded fact about a
    /// moment, not a plan — but typed like its siblings so a reader has one
    /// shape to handle.
    #[serde(default)]
    pub completed_at: Option<SunriseTime>,
    /// PN-counter; system-incremented on defer.
    #[serde(default)]
    pub deferred_count: i64,
    /// Block ids scheduling this task: derived on read from every live block
    /// whose `tasks` names it, and not merged (ADR-0044 §3).
    #[serde(default)]
    pub blocks: BTreeSet<EntityRef>,
    /// Tasks this task depends on (OR-Set).
    #[serde(default)]
    pub blocked_by: BTreeSet<EntityRef>,
    /// Optional Person ref (informational only; no delegation).
    #[serde(default)]
    pub assignee: Option<EntityRef>,
    /// If generated by a routine.
    #[serde(default)]
    pub routine_id: Option<EntityRef>,
    /// Occurrence date for routine-generated tasks.
    #[serde(default)]
    pub routine_occurrence: Option<Timestamp>,
    /// How far ahead of `scheduled_at` this Task's reminder fires, in seconds.
    ///
    /// Top of the lead-time hierarchy in
    /// `docs/08-features/notifications.md`: per-task, then per-Stream, then
    /// the device's global default. `None` means "not set here", which is what
    /// makes the fallback a hierarchy rather than three independent settings —
    /// `Some(0)` is a real answer ("fire at the scheduled time") and must not
    /// be confused with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_lead_s: Option<u32>,
    /// Archived (out of default views).
    #[serde(default)]
    pub archived: bool,
    /// Deleted (tombstone until compaction).
    #[serde(default)]
    pub deleted: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Draft used by the UI when creating a Task. Only required fields appear;
/// the core fills timestamps, id, and defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskDraft {
    /// Title (will be trimmed and validated).
    pub title: String,
    /// Optional body.
    pub body: Option<NoteBody>,
    /// Owning Stream; defaults to Inbox if `None`.
    pub stream_id: Option<EntityRef>,
    /// Optional contexts.
    pub contexts: Vec<EntityRef>,
    /// Optional priority 1..=5.
    pub priority: Option<u8>,
    /// Optional energy facet.
    pub energy: Option<Energy>,
    /// Optional estimated duration in seconds.
    pub estimated_duration_s: Option<u64>,
    /// Optional scheduled_at.
    pub scheduled_at: Option<SunriseTime>,
    /// Optional due_at.
    pub due_at: Option<SunriseTime>,
    /// Optional scheduling constraints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduling_constraints: Vec<ScheduleConstraint>,
    /// Optional assignee.
    pub assignee: Option<EntityRef>,
    /// Optional per-task reminder lead time, in seconds.
    #[serde(default)]
    pub reminder_lead_s: Option<u32>,
}

/// Patch applied via `Command::UpdateTask`. `Some(None)` clears an optional
/// field; `None` leaves it unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskPatch {
    /// New title (if provided).
    pub title: Option<String>,
    /// Update body; outer `Option` chooses presence-of-update, inner is the
    /// new value (None to clear).
    pub body: Option<Option<NoteBody>>,
    /// Move to a new Stream.
    pub stream_id: Option<EntityRef>,
    /// Replace contexts (set semantics on apply).
    pub contexts: Option<Vec<EntityRef>>,
    /// New state.
    pub state: Option<TaskState>,
    /// New priority.
    pub priority: Option<Option<u8>>,
    /// New energy.
    pub energy: Option<Option<Energy>>,
    /// New duration.
    pub estimated_duration_s: Option<Option<u64>>,
    /// New scheduled_at.
    pub scheduled_at: Option<Option<SunriseTime>>,
    /// New due_at.
    pub due_at: Option<Option<SunriseTime>>,
    /// Replace the whole scheduling-constraints list (LWW). `None` leaves it
    /// unchanged; `Some(vec![])` clears it; `Some(list)` replaces it.
    pub scheduling_constraints: Option<Vec<ScheduleConstraint>>,
    /// New blocked_by set (replaces).
    pub blocked_by: Option<Vec<EntityRef>>,
    /// New assignee.
    pub assignee: Option<Option<EntityRef>>,
    /// New reminder lead time; `Some(None)` falls back to the Stream's.
    pub reminder_lead_s: Option<Option<u32>>,
    /// Archive or unarchive.
    pub archived: Option<bool>,
}

impl TaskDraft {
    /// Validate the draft against domain rules.
    ///
    /// Does NOT enforce the 1.25 MiB envelope cap (that's done in the core
    /// after CRDT encoding) or `blocked_by` cycles (done at submit time
    /// once the full graph is known).
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(&self.title, "task.title", MAX_TASK_TITLE_LEN)?;
        if let (Some(s), Some(d)) = (self.scheduled_at.as_ref(), self.due_at.as_ref()) {
            // Compared on the storage index key, which is the same key SQL
            // orders on — so "the deadline is before the plan" means the same
            // thing to the validator and to a `WHERE due_at_ms < ?` query,
            // whatever kinds the two values are. A kind this build cannot
            // place on the timeline is compared with nothing: its stand-in
            // key would invent a violation.
            if matches!((d.index_key(), s.index_key()), (Some(d), Some(s)) if d < s) {
                return Err(ValidationError::DueBeforeScheduled);
            }
        }
        if let Some(p) = self.priority {
            if !(1..=5).contains(&p) {
                return Err(ValidationError::Field {
                    field: "task.priority",
                    constraint: "range_1_5",
                });
            }
        }
        validate_constraint_list(&self.scheduling_constraints)?;
        Ok(())
    }
}

impl Task {
    /// Re-check cross-field invariants on a materialized Task.
    ///
    /// Enforced after applying a patch (a patch that sets only `due_at` earlier
    /// than an existing `scheduled_at` would otherwise silently break the
    /// deadline invariant): `scheduled_at ≤ due_at` when both are present, and
    /// the scheduling-constraint list is valid.
    pub fn validate_invariants(&self) -> Result<(), ValidationError> {
        if let (Some(s), Some(d)) = (self.scheduled_at.as_ref(), self.due_at.as_ref()) {
            // Compared on the storage index key, which is the same key SQL
            // orders on — so "the deadline is before the plan" means the same
            // thing to the validator and to a `WHERE due_at_ms < ?` query,
            // whatever kinds the two values are. A kind this build cannot
            // place on the timeline is compared with nothing: its stand-in
            // key would invent a violation.
            if matches!((d.index_key(), s.index_key()), (Some(d), Some(s)) if d < s) {
                return Err(ValidationError::DueBeforeScheduled);
            }
        }
        validate_constraint_list(&self.scheduling_constraints)?;
        Ok(())
    }
}

impl TaskPatch {
    /// Validate the patch's individual fields.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(t) = &self.title {
            let _ = validate_title(t, "task.title", MAX_TASK_TITLE_LEN)?;
        }
        if let Some(Some(p)) = self.priority {
            if !(1..=5).contains(&p) {
                return Err(ValidationError::Field {
                    field: "task.priority",
                    constraint: "range_1_5",
                });
            }
        }
        if let Some(list) = &self.scheduling_constraints {
            validate_constraint_list(list)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::{EntityKind, EntityRef};

    fn ref_t() -> EntityRef {
        EntityRef::new(EntityKind::Stream, [1u8; 16])
    }

    #[test]
    fn task_state_transitions() {
        assert!(TaskState::Todo.can_transition_to(&TaskState::InProgress));
        assert!(TaskState::InProgress.can_transition_to(&TaskState::Done));
        assert!(TaskState::Done.can_transition_to(&TaskState::Todo));
        assert!(TaskState::Cancelled.can_transition_to(&TaskState::Todo));
    }

    #[test]
    fn an_unknown_state_transitions_as_todo_and_can_be_restored() {
        let unknown = TaskState::from_raw("archived");
        // Out of it: as `todo` would.
        assert!(unknown.can_transition_to(&TaskState::InProgress));
        assert!(unknown.can_transition_to(&TaskState::Done));
        // Back into it, as an undo restoring it verbatim does.
        assert!(TaskState::Done.can_transition_to(&unknown));
        assert!(unknown.can_transition_to(&unknown.clone()));
    }

    #[test]
    fn draft_validates_title() {
        let mut d = TaskDraft {
            title: "  do the thing  ".into(),
            stream_id: Some(ref_t()),
            ..Default::default()
        };
        d.validate().unwrap();
        d.title = String::new();
        assert_eq!(d.validate(), Err(ValidationError::InvalidTitle));
    }

    #[test]
    fn draft_rejects_due_before_scheduled() {
        let now = Timestamp::from_millisecond(1_700_000_000_000).unwrap();
        let later = now + jiff::SignedDuration::from_hours(1);
        let d = TaskDraft {
            title: "x".into(),
            scheduled_at: Some(later.into()),
            due_at: Some(now.into()),
            ..Default::default()
        };
        assert_eq!(d.validate(), Err(ValidationError::DueBeforeScheduled));
    }

    /// A time kind this build cannot place is compared with nothing, on
    /// either side.
    #[test]
    fn an_unplaceable_time_kind_never_violates_due_after_scheduled() {
        let now = Timestamp::from_millisecond(1_700_000_000_000).unwrap();
        let unknown = crate::SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: crate::Unknowns::new(),
        };
        for (scheduled_at, due_at) in [
            (Some(unknown.clone()), Some(now.into())),
            (Some(now.into()), Some(unknown)),
        ] {
            let d = TaskDraft {
                title: "x".into(),
                scheduled_at,
                due_at,
                ..Default::default()
            };
            d.validate().unwrap();
        }
    }

    #[test]
    fn draft_rejects_priority_out_of_range() {
        let d = TaskDraft {
            title: "x".into(),
            priority: Some(6),
            ..Default::default()
        };
        assert!(matches!(
            d.validate(),
            Err(ValidationError::Field {
                constraint: "range_1_5",
                ..
            })
        ));
    }
}
