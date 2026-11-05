//! The tests of [`super`]: the fixtures every file here shares, and one child
//! per concern. [`edits`] holds the inverses read off rows before a write,
//! [`patch_fields`] the task patch inverse field by field, and [`creates`]
//! the inverses read off the result after one.

mod creates;
mod edits;
mod patch_fields;

use super::*;
use std::collections::BTreeSet;
use sunrise_domain::{TaskPatch, TaskState, Unknowns};
use sunrise_id::EntityKind;

/// The simplest possible holder of rows — what every client is, minus the
/// pixels.
#[derive(Default)]
struct Rows {
    tasks: Vec<Task>,
    streams: Vec<StreamRow>,
    contexts: Vec<ContextRow>,
    routines: Vec<RoutineRow>,
}

impl EntityLookup for Rows {
    fn task(&self, id: EntityRef) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
    fn stream(&self, id: EntityRef) -> Option<&StreamRow> {
        self.streams.iter().find(|s| s.id == id)
    }
    fn context(&self, id: EntityRef) -> Option<&ContextRow> {
        self.contexts.iter().find(|c| c.id == id)
    }
    fn routine(&self, id: EntityRef) -> Option<&RoutineRow> {
        self.routines.iter().find(|r| r.id == id)
    }
}

fn tid(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Task, [n; 16])
}
fn sid(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Stream, [n; 16])
}
fn cid(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Context, [n; 16])
}
fn rid(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Routine, [n; 16])
}

fn task(n: u8) -> Task {
    Task {
        reminder_lead_s: None,
        id: tid(n),
        created_at: jiff::Timestamp::UNIX_EPOCH,
        updated_at: jiff::Timestamp::UNIX_EPOCH,
        title: format!("task {n}"),
        body: None,
        stream_id: Some(sid(1)),
        contexts: BTreeSet::new(),
        state: TaskState::Todo,
        priority: None,
        energy: None,
        estimated_duration_s: None,
        planned_at: None,
        target_at: None,
        hard_due_at: None,
        scheduling_constraints: Vec::new(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: None,
        routine_occurrence: None,
        hard_constraint_violated: false,
        sort_order: None,
        triaged_at: None,
        cancel_reason: None,
        late_acknowledged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn rows_with(t: Task) -> Rows {
    Rows {
        tasks: vec![t],
        ..Rows::default()
    }
}
