//! Routine entity per `docs/02-domain/routines-and-recurrence.md`.

use crate::common::{Energy, NoteBody};
use crate::constraint::{validate_list as validate_constraint_list, ScheduleConstraint};
use crate::rrule::RRule;
use crate::task::TaskDraft;
use crate::validation::{validate_title, ValidationError, MAX_TASK_TITLE_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// What to do when an occurrence is missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineCatchupPolicy {
    /// Skip missed occurrences silently.
    Skip,
    /// Merge missed occurrences into a single "catch up" task.
    Merge,
    /// Queue every missed occurrence as a separate task.
    Queue,
}

/// Review cadence for routines (mirrors Stream cadences).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutineReviewCadence {
    /// Review weekly.
    Weekly,
    /// Review monthly.
    Monthly,
    /// No reviews.
    None,
}

/// Task template embedded in a Routine. Used to materialize occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTemplate {
    /// Title (required).
    pub title: String,
    /// Owning Stream.
    pub stream_id: EntityRef,
    /// Optional contexts.
    #[serde(default)]
    pub contexts: Vec<EntityRef>,
    /// Optional energy facet.
    #[serde(default)]
    pub energy: Option<Energy>,
    /// Optional priority 1..=5.
    #[serde(default)]
    pub priority: Option<u8>,
    /// Optional duration in seconds.
    #[serde(default)]
    pub estimated_duration_s: Option<u64>,
    /// Optional body.
    #[serde(default)]
    pub body: Option<NoteBody>,
}

impl TaskTemplate {
    /// Build a `TaskDraft` for this occurrence.
    #[must_use]
    pub fn to_draft(&self, scheduled_at: Option<Timestamp>) -> TaskDraft {
        // A materialized occurrence lands on a computed INSTANT: the recurrence
        // engine has already resolved the routine's own timezone (`Routine.tz`)
        // against the occurrence date, so there is nothing left floating.
        TaskDraft {
            title: self.title.clone(),
            body: self.body.clone(),
            stream_id: Some(self.stream_id),
            contexts: self.contexts.clone(),
            priority: self.priority,
            energy: self.energy,
            estimated_duration_s: self.estimated_duration_s,
            scheduled_at: scheduled_at.map(Into::into),
            due_at: None,
            scheduling_constraints: Vec::new(),
            assignee: None,
        }
    }
}

/// serde `default` for [`Routine::forgiveness_enabled`] (the rule is on by
/// default, per `docs/02-domain/routines-and-recurrence.md`).
const fn default_true() -> bool {
    true
}

/// serde `skip_serializing_if` companion to [`default_true`].
///
/// serde hands `skip_serializing_if` a reference, so the by-reference signature
/// is forced here rather than chosen.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_true(b: &bool) -> bool {
    *b
}

/// serde `skip_serializing_if` for counters that default to zero. Same
/// by-reference signature requirement as [`is_true`].
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Persisted Routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routine {
    /// Routine id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last update.
    pub updated_at: Timestamp,
    /// Task template materialized per occurrence.
    pub template: TaskTemplate,
    /// Recurrence rule.
    pub rrule: RRule,
    /// IANA timezone string (e.g., `"America/Los_Angeles"`).
    pub timezone: String,
    /// Inclusive start.
    pub starts_at: Timestamp,
    /// Optional inclusive end.
    #[serde(default)]
    pub ends_at: Option<Timestamp>,
    /// Scheduling constraints (value list; whole list is one LWW register).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduling_constraints: Vec<ScheduleConstraint>,
    /// Dates explicitly skipped. Retained for chrono-era wire compatibility
    /// and iCal `EXDATE` import; matched against occurrences by identical civil
    /// minute in the routine's timezone.
    #[serde(default)]
    pub skip_dates: Vec<Timestamp>,
    /// Occurrence keys (`YYYY-MM-DDTHH:MM`) explicitly skipped via
    /// `SkipRoutineOccurrence`. Preferred over [`Self::skip_dates`] for new
    /// skips: a key is tzdb-drift-immune and needs no instant re-resolution.
    /// Defaults to empty and is omitted on the wire, keeping the chrono-era
    /// fixtures byte-identical.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_keys: Vec<String>,
    /// What to do when an occurrence is missed.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Streak counter (PN-counter; signed for safety).
    #[serde(default)]
    pub streak_counter: i64,
    /// Last successful completion (for streak grace).
    #[serde(default)]
    pub last_completed_at: Option<Timestamp>,
    /// Completion grace window in seconds. `None` = the spec default of 24h
    /// ([`crate::streak::DEFAULT_GRACE_WINDOW_S`]); clamped to
    /// [`crate::streak::MAX_GRACE_WINDOW_S`] on read. Absent on the wire when
    /// unset, so pre-streak fixtures still round-trip byte-identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_window_s: Option<u64>,
    /// Forgiveness rule (one missed occurrence per 30-day window does not reset
    /// the streak). Enabled by default; only the disabled case hits the wire.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub forgiveness_enabled: bool,
    /// Anchor of the current streak *and* of its 30-day forgiveness window.
    /// See [`crate::streak`] for the sliding rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streak_started_at: Option<Timestamp>,
    /// Forgivenesses consumed since [`Self::streak_started_at`]; reset to 0 on
    /// every anchor advance.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub forgivenesses_in_window: u32,
    /// Idempotency keys of the occurrences already counted toward the streak,
    /// sorted. Entries are occurrence keys (`YYYY-MM-DDTHH:MM`); the full key
    /// in the spec is this routine's id joined with the entry. Membership is
    /// permanent (no GC), per `docs/08-features/recurrence-engine.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streak_keys: Vec<String>,
    /// Paused.
    #[serde(default)]
    pub paused: bool,
    /// Pause expiry.
    #[serde(default)]
    pub paused_until: Option<Timestamp>,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
}

/// Draft used by the UI when creating a Routine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineDraft {
    /// Task template.
    pub template: TaskTemplate,
    /// RRULE body (parsed from the user's input).
    pub rrule: RRule,
    /// IANA timezone.
    pub timezone: String,
    /// Inclusive start.
    pub starts_at: Timestamp,
    /// Optional inclusive end.
    pub ends_at: Option<Timestamp>,
    /// Optional scheduling constraints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduling_constraints: Vec<ScheduleConstraint>,
    /// Catchup policy.
    pub catchup_policy: RoutineCatchupPolicy,
}

impl RoutineDraft {
    /// Validate the draft: title shape, timezone resolvability, constraint
    /// list, and `ends_at ≥ starts_at`.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(
            &self.template.title,
            "routine.template.title",
            MAX_TASK_TITLE_LEN,
        )?;
        if jiff::tz::TimeZone::get(&self.timezone).is_err() {
            return Err(ValidationError::Field {
                field: "routine.timezone",
                constraint: "iana_timezone",
            });
        }
        if let Some(end) = self.ends_at {
            if end < self.starts_at {
                return Err(ValidationError::Field {
                    field: "routine.ends_at",
                    constraint: "after_starts_at",
                });
            }
        }
        validate_constraint_list(&self.scheduling_constraints)?;
        Ok(())
    }
}

/// Patch applied via `Command::UpdateRoutine`. `Some(None)` clears an optional
/// field; `None` leaves it unchanged (mirrors [`crate::task::TaskPatch`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutinePatch {
    /// Replace the task template.
    pub template: Option<TaskTemplate>,
    /// Replace the recurrence rule (triggers regeneration of future tasks).
    pub rrule: Option<RRule>,
    /// Replace the IANA timezone string.
    pub timezone: Option<String>,
    /// Replace the anchor start.
    pub starts_at: Option<Timestamp>,
    /// Set/clear the inclusive end.
    pub ends_at: Option<Option<Timestamp>>,
    /// Replace the whole scheduling-constraints list (LWW).
    pub scheduling_constraints: Option<Vec<ScheduleConstraint>>,
    /// Replace the catchup policy.
    pub catchup_policy: Option<RoutineCatchupPolicy>,
    /// Set/clear the streak grace window in seconds (`Some(None)` restores the
    /// 24h default). Clamped to [`crate::streak::MAX_GRACE_WINDOW_S`].
    pub grace_window_s: Option<Option<u64>>,
    /// Toggle the streak forgiveness rule.
    pub forgiveness_enabled: Option<bool>,
    /// Pause / unpause.
    pub paused: Option<bool>,
    /// Set/clear the pause expiry.
    pub paused_until: Option<Option<Timestamp>>,
    /// Archive / unarchive.
    pub archived: Option<bool>,
}

impl RoutinePatch {
    /// Validate the patch's individual fields.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(t) = &self.template {
            let _ = validate_title(&t.title, "routine.template.title", MAX_TASK_TITLE_LEN)?;
        }
        if let Some(tz) = &self.timezone {
            if jiff::tz::TimeZone::get(tz).is_err() {
                return Err(ValidationError::Field {
                    field: "routine.timezone",
                    constraint: "iana_timezone",
                });
            }
        }
        if let Some(list) = &self.scheduling_constraints {
            validate_constraint_list(list)?;
        }
        Ok(())
    }
}

/// Per-FREQ materialization horizon (per spec: DAILY=14d, WEEKLY=60d,
/// MONTHLY=180d, YEARLY=540d). Returned in days.
#[must_use]
pub fn materialization_horizon_days(freq: crate::rrule::Frequency) -> u32 {
    match freq {
        crate::rrule::Frequency::Daily => 14,
        crate::rrule::Frequency::Weekly => 60,
        crate::rrule::Frequency::Monthly => 180,
        crate::rrule::Frequency::Yearly => 540,
    }
}
