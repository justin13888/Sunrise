//! Manual task order: how a list that honours `Task.sort_order` is sorted
//! (`docs/02-domain/tasks.md`, issue #340).
//!
//! A task's `sort_order` is a key in the encoding of [`crate::sort_order`],
//! or `None` for a task nobody has placed. A list in manual order is:
//!
//! 1. every **placed** task, by key, and by id where two keys are equal;
//! 2. then every **unplaced** task, in the order the list had before.
//!
//! Equal keys are not an error. The key arithmetic has no jitter
//! ([`crate::sort_order`] §No jitter), so two devices that drop a task into
//! the same gap while apart write the same key, and the id decides between
//! them the same way on every replica.
//!
//! Unplaced tasks last is what the Apple client's per-device order did
//! (`ListOrder.applying`): a task that arrives by sync, or is captured, does
//! not land in the middle of a hand-arranged run.

use crate::Task;
use std::cmp::Ordering;

/// The dotted field path a `Task.sort_order` validation failure names.
pub const FIELD: &str = "task.sort_order";

/// How two tasks compare in manual order, for tasks that are both placed;
/// `None` when either is not.
#[must_use]
pub fn cmp_placed(a: &Task, b: &Task) -> Option<Ordering> {
    match (&a.sort_order, &b.sort_order) {
        (Some(x), Some(y)) => Some(x.cmp(y).then_with(|| a.id.cmp(&b.id))),
        _ => None,
    }
}

/// Sort `tasks` into manual order: the placed ones first by key and then by
/// id, the unplaced ones after them in the order they already had.
///
/// Stable, so a caller sorts by its own order first (`sort_by_landing` in the
/// core) and this keeps that order among the unplaced.
pub fn sort_manually(tasks: &mut [Task]) {
    tasks.sort_by(|a, b| match (&a.sort_order, &b.sort_order) {
        (Some(_), Some(_)) => cmp_placed(a, b).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskState, Unknowns};
    use std::collections::BTreeSet;
    use sunrise_id::{EntityKind, EntityRef};

    fn task(n: u8, key: Option<&str>) -> Task {
        Task {
            id: EntityRef::new(EntityKind::Task, [n; 16]),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            title: format!("t{n}"),
            body: None,
            stream_id: EntityRef::new(EntityKind::Stream, [0; 16]),
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
            reminder_lead_s: None,
            sort_order: key.map(str::to_owned),
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn titles(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|t| t.title.clone()).collect()
    }

    #[test]
    fn placed_tasks_lead_by_key_and_the_rest_keep_their_order() {
        let mut list = vec![
            task(9, None),
            task(1, Some("T")),
            task(3, None),
            task(2, Some("G")),
            task(8, None),
        ];
        sort_manually(&mut list);
        assert_eq!(titles(&list), ["t2", "t1", "t9", "t3", "t8"]);
    }

    /// Two devices that drop into one gap write one key; the id decides, the
    /// same way on every replica, whatever order the rows arrived in.
    #[test]
    fn equal_keys_tie_break_by_id_in_any_input_order() {
        let a = vec![task(5, Some("N")), task(4, Some("N")), task(6, Some("G"))];
        let mut b = a.clone();
        b.reverse();
        let mut a = a;
        sort_manually(&mut a);
        sort_manually(&mut b);
        assert_eq!(titles(&a), ["t6", "t4", "t5"]);
        assert_eq!(titles(&a), titles(&b));
        assert_eq!(cmp_placed(&a[1], &a[2]), Some(Ordering::Less));
        assert_eq!(cmp_placed(&a[1], &task(1, None)), None);
    }
}
