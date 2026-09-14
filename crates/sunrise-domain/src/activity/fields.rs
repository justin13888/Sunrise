//! Which Task fields a user can see change, and how a change is detected.
//!
//! Split out of the activity fold because it is a different concern: the fold
//! turns op rows into feed events, while this is the fixed table that defines
//! what "a changed field" means. They share only the `Task` type, they change
//! for different reasons, and keeping the table here leaves each file scoped
//! to one question.

use crate::task::Task;

/// The Task fields a user can see change, in a fixed order.
///
/// This list is the definition of "count of changed fields": derived state
/// (`blocks`, which is a Block-side fact) and pure bookkeeping (`updated_at`,
/// `routine_occurrence`) are deliberately absent, because a feed that counted
/// them would report edits the user never made.
pub const TRACKED_TASK_FIELDS: [&str; 15] = [
    "title",
    "body",
    "stream_id",
    "contexts",
    "state",
    "priority",
    "energy",
    "estimated_duration_s",
    "scheduled_at",
    "due_at",
    "scheduling_constraints",
    "blocked_by",
    "assignee",
    "archived",
    "deferred_count",
];

/// Which of [`TRACKED_TASK_FIELDS`] differ between two states of one Task.
///
/// Returned as names rather than a bare count so a caller can render *what*
/// changed and so tests read as facts rather than as arithmetic.
#[must_use]
pub fn changed_task_fields(prev: &Task, next: &Task) -> Vec<&'static str> {
    // One comparison per entry of `TRACKED_TASK_FIELDS`, in that order. The
    // length is the constant's own, so the *arity* cannot drift. The *pairing*
    // is not compiler-checked — `zip` pairs by position — and is held instead
    // by `tests/activity_field_names.rs`, which says why.
    let changed: [bool; TRACKED_TASK_FIELDS.len()] = [
        prev.title != next.title,
        prev.body != next.body,
        prev.stream_id != next.stream_id,
        prev.contexts != next.contexts,
        prev.state != next.state,
        prev.priority != next.priority,
        prev.energy != next.energy,
        prev.estimated_duration_s != next.estimated_duration_s,
        prev.scheduled_at != next.scheduled_at,
        prev.due_at != next.due_at,
        prev.scheduling_constraints != next.scheduling_constraints,
        prev.blocked_by != next.blocked_by,
        prev.assignee != next.assignee,
        prev.archived != next.archived,
        prev.deferred_count != next.deferred_count,
    ];
    TRACKED_TASK_FIELDS
        .iter()
        .zip(changed)
        .filter_map(|(name, changed)| changed.then_some(*name))
        .collect()
}
