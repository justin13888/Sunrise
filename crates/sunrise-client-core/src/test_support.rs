//! Rows the unit tests build, in one place.

use std::collections::BTreeSet;

use sunrise_domain::{Task, TaskState, Unknowns};
use sunrise_id::{EntityKind, EntityRef};

/// An open task `n`, filed in stream `stream`, with nothing else set.
pub(crate) fn task(n: u8, stream: u8) -> Task {
    Task {
        reminder_lead_s: None,
        id: EntityRef::new(EntityKind::Task, [n; 16]),
        created_at: jiff::Timestamp::UNIX_EPOCH,
        updated_at: jiff::Timestamp::UNIX_EPOCH,
        title: format!("task {n}"),
        body: None,
        stream_id: Some(EntityRef::new(EntityKind::Stream, [stream; 16])),
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
        sort_order: None,
        triaged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}
