//! A drag in a task list, as the one `Command::MoveTasks` it is (#340).
//!
//! The core owns the key arithmetic (`Task.sort_order`,
//! `docs/02-domain/tasks.md`): a client names which tasks land between which
//! two neighbours, and never computes a key. What is left to a client is
//! reading its own list: where the dropped rows land, which neighbours that
//! makes, and which other rows have to move with them. That is this module,
//! so every client reads a drop the same way. It replaces the arithmetic the
//! Apple client kept in `ListOrder.swift`, whose arrangement lived in
//! `UserDefaults` and never left the device.
//!
//! # Rows that must move too
//!
//! A list in manual order shows its placed tasks first, by key, and its
//! unplaced ones after them ([`sunrise_domain::task_order`]). So:
//!
//! * a run dropped below an **unplaced** row can only land there if that row,
//!   and every unplaced row above it, is placed too, in the order the list
//!   shows them. They join the move, ahead of the dragged rows;
//! * a neighbour below whose key is **not greater** than the one above (two
//!   devices that dropped into one gap wrote one key) admits no key between
//!   them, so it joins the move, behind the dragged rows, until a neighbour
//!   with a greater key, an unplaced one or the end of the list bounds it.
//!
//! In the ordinary case, a placed list and one dragged row, the move is that
//! row alone, and the core rewrites exactly its key.
//!
//! # The one-time import
//!
//! [`import_list_order`] turns an order a device kept for itself before the
//! field existed (the Apple client's `ListOrderStore`) into one move, so the
//! arrangement survives the upgrade and then syncs. The Apple client calls
//! it once per list and deletes its store.

use sunrise_core::Command;
use sunrise_domain::Task;
use sunrise_id::EntityRef;

/// One row of a list as it is shown: the task, and its key if it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderedRow {
    /// The task.
    pub id: EntityRef,
    /// Its `sort_order`, or `None` when nobody placed it.
    pub sort_order: Option<String>,
}

impl From<&Task> for OrderedRow {
    fn from(t: &Task) -> Self {
        Self {
            id: t.id,
            sort_order: t.sort_order.clone(),
        }
    }
}

/// Where a drag lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drop {
    /// Immediately above this row.
    Before(EntityRef),
    /// Immediately below this row.
    After(EntityRef),
    /// At the top of the list.
    Top,
    /// At the bottom of the list.
    Bottom,
}

/// The move a drop makes: `Command::MoveTasks`'s three fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskMove {
    /// The tasks to place, in the order they will be in.
    pub ids: Vec<EntityRef>,
    /// The row they land below, or `None` for the top.
    pub after: Option<EntityRef>,
    /// The row they land above, or `None` for the bottom.
    pub before: Option<EntityRef>,
}

impl TaskMove {
    /// The command to submit.
    #[must_use]
    pub fn command(self) -> Command {
        Command::MoveTasks {
            ids: self.ids,
            after: self.after,
            before: self.before,
        }
    }
}

/// The move that drops `moved` at `to` in `list`, the list as it is shown in
/// manual order, or `None` when the drop changes nothing.
///
/// A multi-row drag keeps its rows in the order they are in `list`, which is
/// what makes dragging a selected run feel like moving one thing. Ids not in
/// `list` are ignored. A drop onto a row that is itself being moved, or onto
/// a row not in `list`, changes nothing.
#[must_use]
pub fn plan_move(list: &[OrderedRow], moved: &[EntityRef], to: Drop) -> Option<TaskMove> {
    let moving: Vec<&OrderedRow> = list.iter().filter(|r| moved.contains(&r.id)).collect();
    if moving.is_empty() {
        return None;
    }
    let rest: Vec<&OrderedRow> = list.iter().filter(|r| !moved.contains(&r.id)).collect();
    let at = match to {
        Drop::Top => 0,
        Drop::Bottom => rest.len(),
        Drop::Before(t) => rest.iter().position(|r| r.id == t)?,
        Drop::After(t) => rest.iter().position(|r| r.id == t)? + 1,
    };
    let (prefix, suffix) = rest.split_at(at);
    let unchanged = prefix
        .iter()
        .chain(&moving)
        .chain(suffix)
        .map(|r| r.id)
        .eq(list.iter().map(|r| r.id));
    if unchanged {
        return None;
    }

    // Above the drop: the last placed row before any unplaced one bounds the
    // keys from below, and every row from the first unplaced one on joins
    // the move, ahead of the dragged rows.
    let first_unplaced = prefix.iter().position(|r| r.sort_order.is_none());
    let (anchored, lead) = prefix.split_at(first_unplaced.unwrap_or(prefix.len()));
    let after = anchored.last();
    let mut ids: Vec<EntityRef> = lead.iter().chain(&moving).map(|r| r.id).collect();

    // Below the drop: the first row with a key strictly above `after`'s
    // bounds them from above; a placed row that is not above it joins the
    // move, behind them. An unplaced row ends the walk with no bound.
    let floor = after.and_then(|r| r.sort_order.as_deref());
    let mut before = None;
    if lead.is_empty() {
        for row in suffix {
            match row.sort_order.as_deref() {
                None => break,
                Some(k) if floor.is_none_or(|f| k > f) => {
                    before = Some(row.id);
                    break;
                }
                Some(_) => ids.push(row.id),
            }
        }
    }
    Some(TaskMove {
        ids,
        after: after.map(|r| r.id),
        before,
    })
}

/// [`plan_move`] over the tasks a list query returned.
#[must_use]
pub fn plan_move_in(tasks: &[Task], moved: &[EntityRef], to: Drop) -> Option<TaskMove> {
    let rows: Vec<OrderedRow> = tasks.iter().map(OrderedRow::from).collect();
    plan_move(&rows, moved, to)
}

/// The one-time import of an order a device kept for itself (the Apple
/// client's `ListOrderStore`, `stored`, most significant first) into `list`,
/// the same list as the core now shows it, or `None` when there is nothing
/// to import.
///
/// The rows `stored` names that `list` still holds are placed, in `stored`'s
/// order, ahead of every other row: exactly where the device used to show
/// them (`ListOrder.applying`). Nothing is imported into a list that already
/// has a placed row: that order came from another device after the field
/// existed, it is newer than anything this device kept, and laying an old
/// local order over it would rearrange the user's synced list.
#[must_use]
pub fn import_list_order(stored: &[EntityRef], list: &[OrderedRow]) -> Option<TaskMove> {
    if list.iter().any(|r| r.sort_order.is_some()) {
        return None;
    }
    let mut ids: Vec<EntityRef> = Vec::new();
    for id in stored {
        if list.iter().any(|r| r.id == *id) && !ids.contains(id) {
            ids.push(*id);
        }
    }
    if ids.is_empty() {
        return None;
    }
    Some(TaskMove {
        ids,
        after: None,
        before: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn id(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [n; 16])
    }

    fn row(n: u8, key: Option<&str>) -> OrderedRow {
        OrderedRow {
            id: id(n),
            sort_order: key.map(str::to_owned),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn mv(ids: &[u8], after: Option<u8>, before: Option<u8>) -> Option<TaskMove> {
        Some(TaskMove {
            ids: ids.iter().map(|n| id(*n)).collect(),
            after: after.map(id),
            before: before.map(id),
        })
    }

    /// The ordinary case: a placed list and one dragged row moves that row
    /// alone, between its two new neighbours.
    #[test]
    fn one_row_in_a_placed_list_moves_alone() {
        let list = [row(1, Some("G")), row(2, Some("N")), row(3, Some("T"))];
        assert_eq!(
            plan_move(&list, &[id(3)], Drop::Before(id(2))),
            mv(&[3], Some(1), Some(2))
        );
        assert_eq!(
            plan_move(&list, &[id(1)], Drop::After(id(2))),
            mv(&[1], Some(2), Some(3))
        );
        assert_eq!(
            plan_move(&list, &[id(3)], Drop::Top),
            mv(&[3], None, Some(1))
        );
        assert_eq!(
            plan_move(&list, &[id(1)], Drop::Bottom),
            mv(&[1], Some(3), None)
        );
    }

    #[test]
    fn a_run_keeps_its_order_whatever_order_it_was_named_in() {
        let list = [
            row(1, Some("D")),
            row(2, Some("G")),
            row(3, Some("N")),
            row(4, Some("T")),
        ];
        assert_eq!(
            plan_move(&list, &[id(4), id(2)], Drop::Top),
            mv(&[2, 4], None, Some(1))
        );
    }

    #[test]
    fn a_drop_that_changes_nothing_is_no_move() {
        let list = [row(1, Some("G")), row(2, Some("N"))];
        assert_eq!(plan_move(&list, &[id(1)], Drop::Before(id(2))), None);
        assert_eq!(plan_move(&list, &[id(1)], Drop::Top), None);
        assert_eq!(plan_move(&list, &[id(2)], Drop::Bottom), None);
        assert_eq!(
            plan_move(&list, &[id(1)], Drop::Before(id(1))),
            None,
            "onto itself"
        );
        assert_eq!(
            plan_move(&list, &[id(9)], Drop::Top),
            None,
            "not in the list"
        );
        assert_eq!(
            plan_move(&list, &[id(1)], Drop::Before(id(9))),
            None,
            "no such target"
        );
    }

    /// Dropped below unplaced rows: they are placed too, ahead of it, in the
    /// order they are shown, and nothing bounds the keys from above.
    #[test]
    fn unplaced_rows_above_the_drop_join_the_move() {
        let list = [row(1, Some("N")), row(2, None), row(3, None), row(4, None)];
        assert_eq!(
            plan_move(&list, &[id(4)], Drop::Before(id(3))),
            mv(&[2, 4], Some(1), None)
        );
        let none_placed = [row(1, None), row(2, None), row(3, None)];
        assert_eq!(
            plan_move(&none_placed, &[id(3)], Drop::After(id(1))),
            mv(&[1, 3], None, None)
        );
        // To the top of a list with nothing placed: the row alone.
        assert_eq!(
            plan_move(&none_placed, &[id(3)], Drop::Top),
            mv(&[3], None, None)
        );
    }

    /// A move into an empty list, a list holding only the dragged rows.
    #[test]
    fn a_move_into_an_empty_list_spans_the_key_space() {
        let list = [row(2, None), row(1, None)];
        assert_eq!(plan_move(&list, &[id(1), id(2)], Drop::Top), None);
        assert_eq!(plan_move(&list, &[id(1)], Drop::Top), mv(&[1], None, None));
    }

    /// Two equal keys admit nothing between them: the lower neighbour moves
    /// too, behind the dragged row, until a greater key bounds the run.
    #[test]
    fn a_tied_neighbour_below_joins_the_move() {
        let list = [
            row(1, Some("N")),
            row(2, Some("N")),
            row(3, Some("N")),
            row(4, Some("T")),
            row(5, Some("W")),
        ];
        assert_eq!(
            plan_move(&list, &[id(5)], Drop::Before(id(2))),
            mv(&[5, 2, 3], Some(1), Some(4))
        );
    }

    #[test]
    fn the_import_places_what_the_device_kept_ahead_of_the_rest() {
        let list = [row(1, None), row(2, None), row(3, None)];
        assert_eq!(
            import_list_order(&[id(3), id(9), id(1), id(3)], &list),
            mv(&[3, 1], None, None),
            "absent ids are dropped and a repeat counts once"
        );
        assert_eq!(import_list_order(&[], &list), None);
        assert_eq!(import_list_order(&[id(9)], &list), None);
    }

    #[test]
    fn nothing_is_imported_over_a_synced_order() {
        let list = [row(1, Some("N")), row(2, None)];
        assert_eq!(import_list_order(&[id(2), id(1)], &list), None);
    }

    #[test]
    fn a_move_is_the_command_it_names() {
        let m = mv(&[1, 2], Some(3), None).unwrap();
        assert!(matches!(
            m.command(),
            Command::MoveTasks { ids, after: Some(a), before: None }
                if ids == vec![id(1), id(2)] && a == id(3)
        ));
    }
}
