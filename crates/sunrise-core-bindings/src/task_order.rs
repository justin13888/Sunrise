//! A drag in a task list across the seam (#340):
//! [`sunrise_client_core::task_order`], lowered. The core computes every
//! key; a client hands over the list it shows and where the rows were
//! dropped, and submits [`crate::CoreCommand::MoveTasks`] with what comes
//! back.

use crate::dto::TaskItem;
use sunrise_client_core::task_order::{self, Drop, OrderedRow};
use sunrise_id::EntityRef;

/// Where a drag lands. See [`sunrise_client_core::task_order::Drop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TaskDrop {
    /// Immediately above this row.
    Before {
        /// The row.
        id: EntityRef,
    },
    /// Immediately below this row.
    After {
        /// The row.
        id: EntityRef,
    },
    /// At the top of the list.
    Top,
    /// At the bottom of the list.
    Bottom,
}

impl From<TaskDrop> for Drop {
    fn from(d: TaskDrop) -> Self {
        match d {
            TaskDrop::Before { id } => Self::Before(id),
            TaskDrop::After { id } => Self::After(id),
            TaskDrop::Top => Self::Top,
            TaskDrop::Bottom => Self::Bottom,
        }
    }
}

/// The three fields of [`crate::CoreCommand::MoveTasks`] a drop makes.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TaskMove {
    /// The tasks to place, in the order they will be in.
    pub ids: Vec<EntityRef>,
    /// The row they land below, or none for the top.
    pub after: Option<EntityRef>,
    /// The row they land above, or none for the bottom.
    pub before: Option<EntityRef>,
}

impl From<task_order::TaskMove> for TaskMove {
    fn from(m: task_order::TaskMove) -> Self {
        Self {
            ids: m.ids,
            after: m.after,
            before: m.before,
        }
    }
}

fn rows(list: &[TaskItem]) -> Vec<OrderedRow> {
    list.iter()
        .map(|t| OrderedRow {
            id: t.id,
            sort_order: t.sort_order.clone(),
        })
        .collect()
}

/// The move that drops `moved` at `to` in `list`, the list as the core
/// returned it (in manual order), or none when the drop changes nothing.
/// See [`sunrise_client_core::task_order::plan_move`].
#[uniffi::export]
#[must_use]
pub fn plan_task_move(
    list: Vec<TaskItem>,
    moved: Vec<EntityRef>,
    to: TaskDrop,
) -> Option<TaskMove> {
    task_order::plan_move(&rows(&list), &moved, to.into()).map(TaskMove::from)
}

/// The one-time import of the order this device kept for itself before
/// tasks had one (`stored`, the `ListOrderStore` ids of one list), into
/// `list` as the core now shows it; none when there is nothing to import.
/// See [`sunrise_client_core::task_order::import_list_order`].
#[uniffi::export]
#[must_use]
pub fn import_task_list_order(stored: Vec<EntityRef>, list: Vec<TaskItem>) -> Option<TaskMove> {
    task_order::import_list_order(&stored, &rows(&list)).map(TaskMove::from)
}
