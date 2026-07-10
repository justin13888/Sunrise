//! Routine entity per `docs/02-domain/routines-and-recurrence.md`.

use crate::common::{Energy, NoteBody};
use crate::constraint::ScheduleConstraint;
use crate::rrule::RRule;
use crate::task::TaskDraft;
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
        TaskDraft {
            title: self.title.clone(),
            body: self.body.clone(),
            stream_id: Some(self.stream_id),
            contexts: self.contexts.clone(),
            priority: self.priority,
            energy: self.energy,
            estimated_duration_s: self.estimated_duration_s,
            scheduled_at,
            due_at: None,
            scheduling_constraints: Vec::new(),
            assignee: None,
        }
    }
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
    /// Dates explicitly skipped.
    #[serde(default)]
    pub skip_dates: Vec<Timestamp>,
    /// What to do when an occurrence is missed.
    pub catchup_policy: RoutineCatchupPolicy,
    /// Streak counter (PN-counter; signed for safety).
    #[serde(default)]
    pub streak_counter: i64,
    /// Last successful completion (for streak grace).
    #[serde(default)]
    pub last_completed_at: Option<Timestamp>,
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
