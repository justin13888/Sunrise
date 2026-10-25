//! Task entity per `docs/02-domain/tasks.md`.

use crate::common::{Energy, NoteBody};
use crate::constraint::{validate_list as validate_constraint_list, ScheduleConstraint};
use crate::time::SunriseTime;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::{validate_title, ValidationError, MAX_TASK_TITLE_LEN};
use jiff::tz::TimeZone;
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
    /// Owning Stream, or `None` for a task that belongs to no stream
    /// (ADR-0046 §1), whose ops are sealed under the private key domain.
    ///
    /// A projected task carries its *effective* stream: a tombstoned stream
    /// it names reads as wherever that stream's deletion re-homed its
    /// dependents ([`crate::effective_stream`]). On the wire `None` is the
    /// legacy Inbox reference ([`crate::inbox::wire`]), which every build
    /// reads as "no stream".
    #[serde(default, with = "crate::inbox::wire")]
    pub stream_id: Option<EntityRef>,
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
    /// When the user intends to work on it (ADR-0047 §1): a plan, not a
    /// promise. The planner and defer own it, and a slipped plan is never
    /// lateness. See [`SunriseTime`] — "Tuesday morning" and "09:00 New
    /// York" are not the same kind of answer as "this instant".
    ///
    /// On the wire it keeps the key `scheduled_at`, which every build reads
    /// as this field's meaning (ADR-0047 clarification C1).
    #[serde(default, rename = "scheduled_at")]
    pub planned_at: Option<SunriseTime>,
    /// The soft deadline: when the user wants it done (ADR-0047 §1). Missing
    /// it makes an open task [`crate::Lateness::LateSoft`].
    ///
    /// Absent on the wire when unset, so a task with no target encodes as it
    /// did before `DOC_SCHEMA_V` 20, and an older build keeps a set one as an
    /// unknown field and writes it back unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_at: Option<SunriseTime>,
    /// The hard deadline: when it must be done (ADR-0047 §1). Missing it
    /// makes an open task [`crate::Lateness::LateHard`].
    ///
    /// On the wire it keeps the key `due_at` (ADR-0047 clarification C1).
    #[serde(default, rename = "due_at")]
    pub hard_due_at: Option<SunriseTime>,
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
    /// Where the task sits in a hand-arranged list: a fractional-index key
    /// in the encoding of [`crate::sort_order`], or `None` for a task nobody
    /// has placed (`docs/02-domain/tasks.md`, issue #340).
    ///
    /// One order per task, within its stream. A list that honours manual
    /// order shows the placed tasks first, by key and then by id (two devices
    /// inserting at one gap write the same key, and the id breaks the tie the
    /// same way on every replica), and then the unplaced ones in the list's
    /// own order ([`crate::task_order::sort_manually`]). It is one register
    /// (quiet), so a reorder rewrites this task's key and no sibling's.
    ///
    /// Written by `Command::MoveTasks`, which computes the key, and by the
    /// undo of one, which puts back the key it replaced. Absent on the wire
    /// when unset, so a task nobody placed encodes as it did before
    /// `DOC_SCHEMA_V` 16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<String>,
    /// When the user acknowledged a task that has no stream and no date, so
    /// it leaves the Inbox ([`Task::in_inbox`], ADR-0046 §3): set to now by
    /// `Command::MarkTriaged`, cleared by `Command::ReturnToInbox`. Never
    /// inferred, and nothing else writes it. One register (quiet). Absent on
    /// the wire when unset, so a task nobody triaged encodes as it did
    /// before `DOC_SCHEMA_V` 19.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triaged_at: Option<Timestamp>,
    /// Why the task was dropped (ADR-0047 §4), `text<512>`: written by
    /// `Command::Triage` with `Drop`, cleared by any transition out of
    /// `cancelled`. Meaningful only while the task is cancelled: a reader
    /// ignores it otherwise ([`Task::drop_reason`]), since a concurrent edit
    /// can leave the two registers disagreeing. One register (quiet). Absent
    /// on the wire when unset, so an older build keeps a set one as an
    /// unknown field and writes it back unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
    /// When the user last kept the task in the triage queue (ADR-0047 §3):
    /// set to now by `Command::Triage` with `Keep`. The task leaves the
    /// queue while this is at or after the moment its current lateness
    /// began, and comes back by itself at the next threshold. One register
    /// (quiet), absent on the wire when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_acknowledged_at: Option<Timestamp>,
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
    /// Owning Stream; `None` for a task that belongs to no stream. The
    /// legacy Inbox reference means `None` too.
    pub stream_id: Option<EntityRef>,
    /// Optional contexts.
    pub contexts: Vec<EntityRef>,
    /// Optional priority 1..=5.
    pub priority: Option<u8>,
    /// Optional energy facet.
    pub energy: Option<Energy>,
    /// Optional estimated duration in seconds.
    pub estimated_duration_s: Option<u64>,
    /// Optional plan (`planned_at`).
    #[serde(default, rename = "scheduled_at")]
    pub planned_at: Option<SunriseTime>,
    /// Optional soft deadline.
    #[serde(default)]
    pub target_at: Option<SunriseTime>,
    /// Optional hard deadline.
    #[serde(default, rename = "due_at")]
    pub hard_due_at: Option<SunriseTime>,
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
    /// Move to another Stream, or (`Some(None)`) to no stream. The legacy
    /// Inbox reference means no stream too.
    pub stream_id: Option<Option<EntityRef>>,
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
    /// New plan.
    #[serde(default, rename = "scheduled_at")]
    pub planned_at: Option<Option<SunriseTime>>,
    /// New soft deadline.
    #[serde(default)]
    pub target_at: Option<Option<SunriseTime>>,
    /// New hard deadline.
    #[serde(default, rename = "due_at")]
    pub hard_due_at: Option<Option<SunriseTime>>,
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
    /// Put back a manual-order key, or (`Some(None)`) unplace the task.
    ///
    /// Not how a task is moved: `Command::MoveTasks` computes keys, and a
    /// client never does. This is what the undo of a move writes, the key the
    /// move replaced, read off the row before it. Validated with
    /// [`crate::sort_order::validate`].
    #[serde(default)]
    pub sort_order: Option<Option<String>>,
    /// Put back a triage acknowledgement, or (`Some(None)`) clear it.
    ///
    /// Not how a task is kept: `Command::Triage` with `Keep` stamps it with
    /// the core's clock. This is what the undo of a Keep writes, the value
    /// the Keep replaced.
    #[serde(default)]
    pub late_acknowledged_at: Option<Option<Timestamp>>,
}

impl TaskDraft {
    /// Validate the draft against domain rules.
    ///
    /// Does NOT enforce the 1.25 MiB envelope cap (that's done in the core
    /// after CRDT encoding) or `blocked_by` cycles (done at submit time
    /// once the full graph is known).
    ///
    /// The order of the three times is not checked here: an ordering is a
    /// warning, never a rejection ([`Task::ordering_warnings`], ADR-0047 §1).
    /// `_tz` is kept for the callers that pass the reader's zone.
    pub fn validate(&self, _tz: &TimeZone) -> Result<(), ValidationError> {
        let _ = validate_title(&self.title, "task.title", MAX_TASK_TITLE_LEN)?;
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

/// An ordering of a task's three times that ADR-0047 §1 *expects* and does
/// not enforce: reported beside an accepted write, and computed by readers
/// for display, never a reason to refuse one.
///
/// Concurrent per-field edits on two devices can produce any combination of
/// the three registers, and a rule merge cannot enforce must not be one a
/// command pretends to; and "I plan to do it after the deadline, knowingly"
/// is a real decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderingWarning {
    /// The soft deadline is missed after the hard one: `target_at` is later
    /// than `hard_due_at`.
    TargetAfterHardDue,
    /// The plan starts at or after the moment the hard deadline is missed.
    PlannedAfterHardDue,
}

impl OrderingWarning {
    /// The stable spelling, as the CLI and the bindings print it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TargetAfterHardDue => "target_after_hard_due",
            Self::PlannedAfterHardDue => "planned_after_hard_due",
        }
    }
}

impl Task {
    /// The task a create of `draft` makes, with id `id`, created and touched
    /// at `at`: `todo`, never deferred, placed nowhere, and the draft's title
    /// trimmed. Not validated; [`TaskDraft::validate`] is the create's gate.
    #[must_use]
    pub fn from_draft(id: EntityRef, at: Timestamp, draft: TaskDraft) -> Self {
        Self {
            id,
            created_at: at,
            updated_at: at,
            title: draft.title.trim().to_string(),
            body: draft.body,
            stream_id: draft.stream_id,
            contexts: draft.contexts.into_iter().collect(),
            state: TaskState::Todo,
            priority: draft.priority,
            energy: draft.energy,
            estimated_duration_s: draft.estimated_duration_s,
            planned_at: draft.planned_at,
            target_at: draft.target_at,
            hard_due_at: draft.hard_due_at,
            scheduling_constraints: draft.scheduling_constraints,
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: draft.assignee,
            routine_id: None,
            routine_occurrence: None,
            reminder_lead_s: draft.reminder_lead_s,
            sort_order: None,
            triaged_at: None,
            cancel_reason: None,
            late_acknowledged_at: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    /// Whether the task is in the Inbox (ADR-0046 §3): open, live, and
    /// untriaged, which is to have no stream, no plan, no deadline and no
    /// [`Self::triaged_at`].
    ///
    /// A pure predicate over merged fields, so giving a task a stream or a
    /// date takes it out of the Inbox without any command clearing a flag,
    /// and a peer that predates `triaged_at` and plans a task still moves it
    /// out on every replica.
    #[must_use]
    pub fn in_inbox(&self) -> bool {
        self.stream_id.is_none()
            && self.planned_at.is_none()
            && !self.has_deadline()
            && self.triaged_at.is_none()
            && self.is_open()
    }

    /// Open: `todo` or `in_progress` (an unknown state reads as `todo`),
    /// not archived and not deleted. The tasks lateness is evaluated for
    /// (ADR-0047 §2).
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(
            self.state.effective(),
            TaskState::Todo | TaskState::InProgress
        ) && !self.archived
            && !self.deleted
    }

    /// The drop reason as a reader shows it: [`Self::cancel_reason`] while
    /// the task is cancelled, and nothing otherwise (ADR-0047 §4).
    #[must_use]
    pub fn drop_reason(&self) -> Option<&str> {
        (self.state == TaskState::Cancelled)
            .then_some(self.cancel_reason.as_deref())
            .flatten()
    }

    /// Whether the task carries a deadline of either kind.
    #[must_use]
    pub const fn has_deadline(&self) -> bool {
        self.target_at.is_some() || self.hard_due_at.is_some()
    }

    /// Re-check the structural invariants of a materialized Task: the
    /// scheduling-constraint list is valid.
    ///
    /// The order of the three times is not one of them: it is reported by
    /// [`Self::ordering_warnings`] and never rejects (ADR-0047 §1). `_tz` is
    /// the reader's zone, kept for the callers that pass it.
    pub fn validate_invariants(&self, _tz: &TimeZone) -> Result<(), ValidationError> {
        validate_constraint_list(&self.scheduling_constraints)?;
        Ok(())
    }

    /// The orderings ADR-0047 §1 expects and this task breaks, for a reader
    /// in `tz` whose days are `schedule`'s planner days: `target_at ≤
    /// hard_due_at`, and `planned_at ≤ hard_due_at`.
    ///
    /// Each pair resolves in the reader's zone (`docs/10-cross-cutting/time.md`
    /// §2 rule 1), never on the storage index. Both deadlines compare as
    /// deadlines, at the instant they are missed ([`crate::deadline_at`]: the
    /// END of an all-day date's planner day), and the plan at its start
    /// ([`crate::starts_at`]), so a task planned for 09:00 Friday and due
    /// "Friday" is consistent. A kind this build cannot place is compared
    /// with nothing: its stand-in would invent a warning.
    #[must_use]
    pub fn ordering_warnings(
        &self,
        tz: &TimeZone,
        schedule: &crate::DaySchedule,
    ) -> Vec<OrderingWarning> {
        let mut out = Vec::new();
        let Some(hard) = self
            .hard_due_at
            .as_ref()
            .and_then(|h| crate::deadline_at(schedule, h, tz))
        else {
            return out;
        };
        if self
            .target_at
            .as_ref()
            .and_then(|t| crate::deadline_at(schedule, t, tz))
            .is_some_and(|t| t > hard)
        {
            out.push(OrderingWarning::TargetAfterHardDue);
        }
        if self
            .planned_at
            .as_ref()
            .and_then(|p| crate::starts_at(schedule, p, tz))
            .is_some_and(|p| p >= hard)
        {
            out.push(OrderingWarning::PlannedAfterHardDue);
        }
        out
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
        if let Some(Some(k)) = &self.sort_order {
            crate::sort_order::validate_as(k, crate::task_order::FIELD)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
