//! Per-entity activity timeline per `docs/08-features/reviews-and-stats.md`
//! §Activity timeline: *"Each Task / Stream has an activity feed showing
//! user-visible ops only … Field-level edits are summarized as 'Updated
//! `<task>`' with the count of changed fields … Useful for 'what happened?'
//! not for analytics."*
//!
//! # The source of truth is the op log, not a new table
//!
//! Every mutation in Sunrise already lands in the op log with a timestamp, a
//! device id, and — because v1 ops are *full-state* (see
//! `sunrise_core::inner_op`) — the entity's complete value after the write.
//! Two consecutive full states are a diff, so "what changed and how many
//! fields" is recoverable from data that already exists. Nothing here is
//! persisted; a timeline is a fold, recomputed on read.
//!
//! That also means the timeline inherits the log's convergence for free: a
//! remote op is appended to the same table by the same path, so a merged
//! device's edits appear in the feed with *their* device id and *their*
//! timestamp without any extra plumbing.
//!
//! # Purity
//!
//! [`fold_activity`] takes a slice of decoded op rows and returns events. No
//! database, no clock, no ambient state — the same discipline
//! [`crate::focus::fold_focus_stats`] follows, and the reason both are
//! exhaustively testable without a fixture.

use crate::task::{Task, TaskState};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

use crate::focus::{FocusEnd, FocusStart};
use crate::stream::Stream;

/// The decoded payload of one op-log row, restricted to the families the
/// timeline reports on.
///
/// Everything else — routine bookkeeping, context edits, interruption logs,
/// key rotations — decodes to [`OpPayload::Ignored`], which is how the spec's
/// exclusion list is enforced at the type level rather than by a filter that
/// can drift.
#[derive(Debug, Clone, PartialEq)]
pub enum OpPayload {
    /// `task.create` — the task's full state as created.
    TaskCreated(Box<Task>),
    /// `task.update` — the task's full state after the write.
    TaskUpdated(Box<Task>),
    /// `task.delete` — a tombstone marker carrying no state.
    TaskDeleted,
    /// `stream.create`.
    StreamCreated(Box<Stream>),
    /// `stream.delete`.
    StreamDeleted,
    /// `focus.start`.
    FocusStarted(Box<FocusStart>),
    /// `focus.end`.
    FocusEnded(Box<FocusEnd>),
    /// An op the timeline deliberately does not report.
    Ignored,
}

/// One op-log row as the folds see it: the log's own metadata joined with the
/// decoded payload.
///
/// Flat and owned so the folds need no database and no decryption context.
#[derive(Debug, Clone, PartialEq)]
pub struct OpRecord {
    /// Op-log primary key.
    pub op_id: [u8; 16],
    /// The op's origin timestamp (ms since epoch), as stamped by the device
    /// that authored it.
    pub at_ms: u64,
    /// Authoring device.
    pub device: [u8; 16],
    /// The entity the op addresses.
    pub target: EntityRef,
    /// Decoded payload.
    pub payload: OpPayload,
}

/// What one op did, in the vocabulary the feed shows.
///
/// One op produces at most one event: an op is one user action, and
/// "Completed" already implies "Updated". Where an op could read several ways
/// the most specific wins — see [`fold_activity`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// Task created by the user.
    TaskCreated,
    /// Task transitioned into `done`.
    TaskCompleted,
    /// Task transitioned out of `done` — a resurrection.
    TaskReopened,
    /// Task transitioned into `cancelled` (the "dropped" outcome a review
    /// counts).
    TaskCancelled,
    /// Task deferred; carries the new deferral count.
    TaskDeferred {
        /// `deferred_count` after the write.
        count: i64,
    },
    /// Task moved between Streams.
    TaskMoved {
        /// Stream it left.
        from: EntityRef,
        /// Stream it joined.
        to: EntityRef,
    },
    /// A field-level edit, summarized: *"Updated `<task>` (3 fields)"*.
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
    /// A focus session opened on this task.
    FocusStarted {
        /// The session's own id.
        session: EntityRef,
        /// Planned length; `None` is an open-ended session.
        planned_ms: Option<u64>,
    },
    /// A focus session on this task closed.
    FocusEnded {
        /// The session's own id.
        session: EntityRef,
        /// Frozen focused time.
        focused_ms: u64,
        /// Whether the task was completed in that session.
        completed_task: bool,
    },
}

impl ActivityKind {
    /// Stable, machine-readable verb, used as the `kind` column in exports.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        match self {
            Self::TaskCreated => "task.created",
            Self::TaskCompleted => "task.completed",
            Self::TaskReopened => "task.reopened",
            Self::TaskCancelled => "task.cancelled",
            Self::TaskDeferred { .. } => "task.deferred",
            Self::TaskMoved { .. } => "task.moved",
            Self::TaskUpdated { .. } => "task.updated",
            Self::TaskDeleted => "task.deleted",
            Self::StreamCreated => "stream.created",
            Self::StreamDeleted => "stream.deleted",
            Self::FocusStarted { .. } => "focus.started",
            Self::FocusEnded { .. } => "focus.ended",
        }
    }
}

/// One row of an entity's activity feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    /// When it happened (ms since epoch), from the authoring device.
    pub at_ms: u64,
    /// Op-log id.
    ///
    /// **Replica-local.** The device that authored an op mints a ULID for it,
    /// while every receiver derives its log key from `(stream, device, seq)` —
    /// so the same event carries different `op_id`s on different devices. Use
    /// it to address a row on *this* replica, never to compare two replicas'
    /// feeds; `(at_ms, device, entity, kind)` is what agrees across them.
    pub op_id: [u8; 16],
    /// Device that authored it, so a multi-device feed can say *where*.
    pub device: [u8; 16],
    /// The **feed** this event belongs to: the Task for task and focus events,
    /// the Stream for stream events. Focus events are attributed to the task
    /// focused on, never to the session id, so a task's feed shows its
    /// sessions without a second query.
    pub entity: EntityRef,
    /// The entity's display name at the time of the op. Empty when the op
    /// carried no state (a tombstone with no prior state on this replica).
    pub label: String,
    /// What happened.
    pub kind: ActivityKind,
}

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
    let mut out = Vec::new();
    let mut check = |changed: bool, name: &'static str| {
        if changed {
            out.push(name);
        }
    };
    check(prev.title != next.title, "title");
    check(prev.body != next.body, "body");
    check(prev.stream_id != next.stream_id, "stream_id");
    check(prev.contexts != next.contexts, "contexts");
    check(prev.state != next.state, "state");
    check(prev.priority != next.priority, "priority");
    check(prev.energy != next.energy, "energy");
    check(
        prev.estimated_duration_s != next.estimated_duration_s,
        "estimated_duration_s",
    );
    check(prev.scheduled_at != next.scheduled_at, "scheduled_at");
    check(prev.due_at != next.due_at, "due_at");
    check(
        prev.scheduling_constraints != next.scheduling_constraints,
        "scheduling_constraints",
    );
    check(prev.blocked_by != next.blocked_by, "blocked_by");
    check(prev.assignee != next.assignee, "assignee");
    check(prev.archived != next.archived, "archived");
    check(prev.deferred_count != next.deferred_count, "deferred_count");
    out
}

/// Fold decoded op rows into an activity feed.
///
/// `ops` must be in ascending `(at_ms, op_id)` order — the order the caller's
/// query already imposes. The fold carries the previous full state of each
/// Task so an update can be diffed against it, and the `session → task`
/// mapping so a `focus.end` lands on the right feed.
///
/// # What is excluded, and why
///
/// * **Routine generation.** A `task.create` whose task carries a `routine_id`
///   was written by the recurrence engine, not by the user. The spec excludes
///   it explicitly; edits the user later makes to that task *are* reported.
/// * **No-op writes.** A `task.update` that changed no tracked field produces
///   no event, so a sync-driven rewrite never shows up as activity.
/// * **Unattributable focus ends.** A `focus.end` whose `start` is not in the
///   input has no task to attach to and is dropped rather than guessed at.
#[must_use]
pub fn fold_activity(ops: &[OpRecord]) -> Vec<ActivityEvent> {
    let mut prev_task: BTreeMap<EntityRef, Task> = BTreeMap::new();
    let mut session_task: BTreeMap<EntityRef, (EntityRef, String)> = BTreeMap::new();
    let mut out: Vec<ActivityEvent> = Vec::new();

    for op in ops {
        match &op.payload {
            OpPayload::TaskCreated(t) => {
                let generated = t.routine_id.is_some();
                prev_task.insert(t.id, (**t).clone());
                if !generated {
                    out.push(event(op, t.id, &t.title, ActivityKind::TaskCreated));
                }
            }
            OpPayload::TaskUpdated(t) => {
                let Some(prev) = prev_task.insert(t.id, (**t).clone()) else {
                    // No baseline on this replica (history truncated before the
                    // create). Record the state; do not invent a transition.
                    continue;
                };
                if let Some(kind) = classify_update(&prev, t) {
                    out.push(event(op, t.id, &t.title, kind));
                }
            }
            OpPayload::TaskDeleted => {
                let label = prev_task
                    .get(&op.target)
                    .map_or_else(String::new, |t| t.title.clone());
                out.push(event(op, op.target, &label, ActivityKind::TaskDeleted));
            }
            OpPayload::StreamCreated(s) => {
                out.push(event(op, s.id, &s.name, ActivityKind::StreamCreated));
            }
            OpPayload::StreamDeleted => {
                out.push(event(op, op.target, "", ActivityKind::StreamDeleted));
            }
            OpPayload::FocusStarted(f) => {
                let label = prev_task
                    .get(&f.task_id)
                    .map_or_else(String::new, |t| t.title.clone());
                session_task.insert(f.id, (f.task_id, label.clone()));
                out.push(event(
                    op,
                    f.task_id,
                    &label,
                    ActivityKind::FocusStarted {
                        session: f.id,
                        planned_ms: f.planned_ms,
                    },
                ));
            }
            OpPayload::FocusEnded(f) => {
                let Some((task, label)) = session_task.get(&f.session_id) else {
                    continue;
                };
                out.push(event(
                    op,
                    *task,
                    label,
                    ActivityKind::FocusEnded {
                        session: f.session_id,
                        focused_ms: f.actual_focused_ms,
                        completed_task: f.completed_task,
                    },
                ));
            }
            OpPayload::Ignored => {}
        }
    }
    out
}

/// Restrict a folded feed to one entity, newest first, capped at `limit`.
#[must_use]
pub fn for_entity(events: &[ActivityEvent], entity: EntityRef, limit: usize) -> Vec<ActivityEvent> {
    for_entities(events, &BTreeSet::from([entity]), limit)
}

/// Restrict a folded feed to a set of entities, newest first, capped at
/// `limit`.
///
/// A Stream's feed is this: the Stream's own lifecycle events **plus** the
/// events of the Tasks that live in it. "What happened in this stream?" is a
/// question about its contents, and a feed that showed only two rows — created
/// and deleted — would answer nothing.
#[must_use]
pub fn for_entities(
    events: &[ActivityEvent],
    entities: &BTreeSet<EntityRef>,
    limit: usize,
) -> Vec<ActivityEvent> {
    let mut rows: Vec<ActivityEvent> = events
        .iter()
        .filter(|e| entities.contains(&e.entity))
        .cloned()
        .collect();
    rows.reverse();
    rows.truncate(limit);
    rows
}

/// Decide what a `task.update` op *was*, most specific reading first.
///
/// The ordering is the editorial judgement in this module: a write that both
/// completes a task and renames it reads as "Completed", because that is what
/// the user did and the rename is incidental. Only a write with no more
/// specific reading falls through to the field-count summary.
fn classify_update(prev: &Task, next: &Task) -> Option<ActivityKind> {
    let changed = changed_task_fields(prev, next);
    if changed.is_empty() {
        return None;
    }
    if prev.state != TaskState::Done && next.state == TaskState::Done {
        return Some(ActivityKind::TaskCompleted);
    }
    if prev.state == TaskState::Done && next.state != TaskState::Done {
        return Some(ActivityKind::TaskReopened);
    }
    if prev.state != TaskState::Cancelled && next.state == TaskState::Cancelled {
        return Some(ActivityKind::TaskCancelled);
    }
    if next.deferred_count > prev.deferred_count {
        return Some(ActivityKind::TaskDeferred {
            count: next.deferred_count,
        });
    }
    if prev.stream_id != next.stream_id {
        return Some(ActivityKind::TaskMoved {
            from: prev.stream_id,
            to: next.stream_id,
        });
    }
    Some(ActivityKind::TaskUpdated {
        fields: u32::try_from(changed.len()).unwrap_or(u32::MAX),
    })
}

/// Assemble one event from an op row.
fn event(op: &OpRecord, entity: EntityRef, label: &str, kind: ActivityKind) -> ActivityEvent {
    ActivityEvent {
        at_ms: op.at_ms,
        op_id: op.op_id,
        device: op.device,
        entity,
        label: label.to_string(),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Energy;
    use crate::focus::FocusKind;
    use crate::stream::{StreamColor, StreamReviewCadence};
    use crate::unknown::Unknowns;
    use jiff::Timestamp;
    use std::collections::BTreeSet;
    use sunrise_id::EntityKind;

    const T0: u64 = 1_700_000_000_000;

    fn eref(kind: EntityKind, b: u8) -> EntityRef {
        EntityRef::new(kind, [b; 16])
    }

    fn ts(ms: u64) -> Timestamp {
        Timestamp::from_millisecond(i64::try_from(ms).unwrap()).unwrap()
    }

    fn task(id: u8, stream: u8, title: &str) -> Task {
        Task {
            id: eref(EntityKind::Task, id),
            created_at: ts(T0),
            updated_at: ts(T0),
            title: title.into(),
            body: None,
            stream_id: eref(EntityKind::Stream, stream),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn stream(id: u8, name: &str) -> Stream {
        Stream {
            id: eref(EntityKind::Stream, id),
            created_at: ts(T0),
            updated_at: ts(T0),
            name: name.into(),
            description: None,
            color: StreamColor::Slate,
            icon: None,
            parent_id: None,
            sort_order: "a0".into(),
            archived: false,
            paused: false,
            paused_until: None,
            review_cadence: StreamReviewCadence::Weekly,
            default_context: None,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    /// Op rows are minted with an ascending op-id so ordering is unambiguous.
    fn op(n: u8, at_ms: u64, target: EntityRef, payload: OpPayload) -> OpRecord {
        OpRecord {
            op_id: [n; 16],
            at_ms,
            device: [0xD1; 16],
            target,
            payload,
        }
    }

    #[test]
    fn create_then_complete_reads_as_two_events() {
        let t = task(1, 2, "Ship it");
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.completed_at = Some(ts(T0 + 5_000).into());

        let events = fold_activity(&[
            op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t.clone()))),
            op(2, T0 + 5_000, t.id, OpPayload::TaskUpdated(Box::new(done))),
        ]);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, ActivityKind::TaskCreated);
        assert_eq!(events[0].label, "Ship it");
        assert_eq!(events[1].kind, ActivityKind::TaskCompleted);
        assert_eq!(events[1].at_ms, T0 + 5_000);
    }

    #[test]
    fn a_field_edit_is_summarized_with_its_exact_field_count() {
        let t = task(1, 2, "Ship it");
        let mut edited = t.clone();
        edited.title = "Ship it properly".into();
        edited.priority = Some(2);
        edited.energy = Some(Energy::High);

        // Three tracked fields changed; nothing else did.
        assert_eq!(
            changed_task_fields(&t, &edited),
            vec!["title", "priority", "energy"]
        );

        let events = fold_activity(&[
            op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t.clone()))),
            op(2, T0 + 1, t.id, OpPayload::TaskUpdated(Box::new(edited))),
        ]);
        assert_eq!(events[1].kind, ActivityKind::TaskUpdated { fields: 3 });
        assert_eq!(events[1].label, "Ship it properly", "label is post-edit");
    }

    #[test]
    fn an_update_that_changes_nothing_produces_no_event() {
        let t = task(1, 2, "Ship it");
        let events = fold_activity(&[
            op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t.clone()))),
            op(2, T0 + 1, t.id, OpPayload::TaskUpdated(Box::new(t.clone()))),
        ]);
        assert_eq!(events.len(), 1, "only the create: {events:#?}");
    }

    #[test]
    fn completion_wins_over_the_rename_that_rode_along_with_it() {
        let t = task(1, 2, "Ship it");
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.title = "Shipped".into();
        let events = fold_activity(&[
            op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t.clone()))),
            op(2, T0 + 1, t.id, OpPayload::TaskUpdated(Box::new(done))),
        ]);
        assert_eq!(
            events[1].kind,
            ActivityKind::TaskCompleted,
            "one op is one user action, and the specific reading wins"
        );
    }

    #[test]
    fn move_defer_reopen_and_cancel_each_get_their_own_verb() {
        let t = task(1, 2, "Ship it");

        let mut moved = t.clone();
        moved.stream_id = eref(EntityKind::Stream, 9);

        let mut deferred = moved.clone();
        deferred.deferred_count = 1;
        deferred.scheduled_at = Some(ts(T0 + 86_400_000).into());

        let mut done = deferred.clone();
        done.state = TaskState::Done;

        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;

        let mut cancelled = reopened.clone();
        cancelled.state = TaskState::Cancelled;

        let events = fold_activity(&[
            op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t.clone()))),
            op(2, T0 + 1, t.id, OpPayload::TaskUpdated(Box::new(moved))),
            op(3, T0 + 2, t.id, OpPayload::TaskUpdated(Box::new(deferred))),
            op(4, T0 + 3, t.id, OpPayload::TaskUpdated(Box::new(done))),
            op(5, T0 + 4, t.id, OpPayload::TaskUpdated(Box::new(reopened))),
            op(6, T0 + 5, t.id, OpPayload::TaskUpdated(Box::new(cancelled))),
        ]);

        let kinds: Vec<&str> = events.iter().map(|e| e.kind.verb()).collect();
        assert_eq!(
            kinds,
            vec![
                "task.created",
                "task.moved",
                "task.deferred",
                "task.completed",
                "task.reopened",
                "task.cancelled",
            ]
        );
        assert_eq!(
            events[1].kind,
            ActivityKind::TaskMoved {
                from: eref(EntityKind::Stream, 2),
                to: eref(EntityKind::Stream, 9),
            }
        );
        assert_eq!(events[2].kind, ActivityKind::TaskDeferred { count: 1 });
    }

    #[test]
    fn routine_generated_creates_are_excluded_but_later_user_edits_are_not() {
        let mut generated = task(1, 2, "Stretch");
        generated.routine_id = Some(eref(EntityKind::Routine, 7));
        let mut done = generated.clone();
        done.state = TaskState::Done;

        let events = fold_activity(&[
            op(
                1,
                T0,
                generated.id,
                OpPayload::TaskCreated(Box::new(generated.clone())),
            ),
            op(
                2,
                T0 + 1,
                generated.id,
                OpPayload::TaskUpdated(Box::new(done)),
            ),
        ]);
        assert_eq!(events.len(), 1, "generation itself is not user activity");
        assert_eq!(events[0].kind, ActivityKind::TaskCompleted);
    }

    #[test]
    fn a_delete_carries_the_title_the_task_had_when_it_died() {
        let t = task(1, 2, "Ship it");
        let events = fold_activity(&[
            op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t.clone()))),
            op(2, T0 + 1, t.id, OpPayload::TaskDeleted),
        ]);
        assert_eq!(events[1].kind, ActivityKind::TaskDeleted);
        assert_eq!(events[1].label, "Ship it");
    }

    #[test]
    fn focus_events_land_on_the_tasks_feed_not_the_sessions() {
        let t = task(1, 2, "Ship it");
        let session = eref(EntityKind::FocusSession, 0x40);
        let start = FocusStart {
            id: session,
            task_id: t.id,
            stream_id: t.stream_id,
            started_at: crate::epoch_ms::from_u64(T0 + 10),
            planned_ms: Some(1_500_000),
            energy: Some(Energy::High),
            kind: FocusKind::Work,
            chunk: None,
            unknown: Unknowns::new(),
        };
        let end = FocusEnd {
            session_id: session,
            ended_at: crate::epoch_ms::from_u64(T0 + 20),
            actual_focused_ms: 900_000,
            interruptions: Vec::new(),
            completed_task: true,
            unknown: Unknowns::new(),
        };
        let events = fold_activity(&[
            op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t.clone()))),
            op(
                2,
                T0 + 10,
                session,
                OpPayload::FocusStarted(Box::new(start)),
            ),
            op(3, T0 + 20, session, OpPayload::FocusEnded(Box::new(end))),
        ]);
        assert_eq!(events.len(), 3);
        assert!(
            events[1..].iter().all(|e| e.entity == t.id),
            "a session's events belong to the task's feed: {events:#?}"
        );
        assert_eq!(
            events[2].kind,
            ActivityKind::FocusEnded {
                session,
                focused_ms: 900_000,
                completed_task: true,
            }
        );
        // And `for_entity` returns them newest-first for that task.
        let feed = for_entity(&events, t.id, 10);
        assert_eq!(feed.len(), 3);
        assert_eq!(feed[0].kind.verb(), "focus.ended");
    }

    #[test]
    fn a_focus_end_whose_start_is_absent_is_dropped_rather_than_guessed() {
        let session = eref(EntityKind::FocusSession, 0x40);
        let end = FocusEnd {
            session_id: session,
            ended_at: crate::epoch_ms::from_u64(T0),
            actual_focused_ms: 1,
            interruptions: Vec::new(),
            completed_task: false,
            unknown: Unknowns::new(),
        };
        let events = fold_activity(&[op(1, T0, session, OpPayload::FocusEnded(Box::new(end)))]);
        assert!(events.is_empty());
    }

    #[test]
    fn stream_creates_and_deletes_are_reported_on_the_streams_own_feed() {
        let s = stream(3, "Work");
        let events = fold_activity(&[
            op(1, T0, s.id, OpPayload::StreamCreated(Box::new(s.clone()))),
            op(2, T0 + 1, s.id, OpPayload::StreamDeleted),
            op(3, T0 + 2, s.id, OpPayload::Ignored),
        ]);
        assert_eq!(events.len(), 2, "the ignored op contributes nothing");
        assert_eq!(events[0].kind, ActivityKind::StreamCreated);
        assert_eq!(events[0].label, "Work");
        assert_eq!(events[1].kind, ActivityKind::StreamDeleted);
        assert_eq!(for_entity(&events, s.id, 1).len(), 1, "limit is honoured");
    }

    #[test]
    fn an_update_with_no_baseline_records_state_without_inventing_a_transition() {
        let mut done = task(1, 2, "Ship it");
        done.state = TaskState::Done;
        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;

        let events = fold_activity(&[
            // The create is missing (history truncated); the first row we see
            // is already `done`.
            op(1, T0, done.id, OpPayload::TaskUpdated(Box::new(done))),
            op(
                2,
                T0 + 1,
                reopened.id,
                OpPayload::TaskUpdated(Box::new(reopened)),
            ),
        ]);
        assert_eq!(events.len(), 1, "no phantom completion: {events:#?}");
        assert_eq!(events[0].kind, ActivityKind::TaskReopened);
    }

    #[test]
    fn events_carry_the_authoring_device_so_a_merged_feed_can_say_where() {
        let t = task(1, 2, "Ship it");
        let mut remote = op(1, T0, t.id, OpPayload::TaskCreated(Box::new(t)));
        remote.device = [0xB2; 16];
        let events = fold_activity(&[remote]);
        assert_eq!(events[0].device, [0xB2; 16]);
        assert_eq!(events[0].op_id, [1u8; 16]);
    }
}
