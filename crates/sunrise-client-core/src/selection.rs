//! The keyboard's place in a list: a cursor, a ticked set, and an anchor.
//!
//! Three pieces of state, and keeping them apart is the whole design:
//!
//! * the **cursor** is the one row the keyboard is on. It exists whenever the
//!   list is non-empty and a key has been pressed, because every row-scoped
//!   key needs a subject.
//! * the **explicit** set is what Space has ticked. Empty is the normal case,
//!   and it is not "nothing is selected" — see [`ListSelection::targets`].
//! * the **anchor** is where a Shift+arrow range grows from. Every plain move
//!   resets it, so a range restarts at the row walked to.
//!
//! `docs/07-clients/parity-matrix.md` marks keyboard navigation a MUST, and a
//! list that moves, selects a range and acts on it is exactly the logic a view
//! body cannot be tested through. Ported from the Apple app's
//! `ListSelection.swift` with its tests; generic over the row id.

use std::collections::HashSet;
use std::hash::Hash;

/// Which way a movement key goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionMove {
    /// Towards the first row.
    Up,
    /// Towards the last row.
    Down,
}

/// Which end of a list a jump goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionEdge {
    /// The first row.
    First,
    /// The last row.
    Last,
}

/// A list's selection. See the module comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListSelection<Id: Clone + Eq + Hash> {
    ids: Vec<Id>,
    cursor: Option<Id>,
    explicit: HashSet<Id>,
    anchor: Option<Id>,
}

impl<Id: Clone + Eq + Hash> Default for ListSelection<Id> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Id: Clone + Eq + Hash> ListSelection<Id> {
    /// No rows, no cursor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ids: Vec::new(),
            cursor: None,
            explicit: HashSet::new(),
            anchor: None,
        }
    }

    /// The rows, in the order they are drawn.
    #[must_use]
    pub fn ids(&self) -> &[Id] {
        &self.ids
    }

    /// The row the keyboard is on.
    #[must_use]
    pub const fn cursor(&self) -> Option<&Id> {
        self.cursor.as_ref()
    }

    /// Where a range grows from.
    #[must_use]
    pub const fn anchor(&self) -> Option<&Id> {
        self.anchor.as_ref()
    }

    /// The ticked rows, in list order.
    #[must_use]
    pub fn explicit(&self) -> Vec<Id> {
        self.in_order(&self.explicit)
    }

    fn in_order(&self, set: &HashSet<Id>) -> Vec<Id> {
        self.ids
            .iter()
            .filter(|id| set.contains(*id))
            .cloned()
            .collect()
    }

    /// What a row action applies to, in list order: the ticked rows when
    /// there are any, and the cursor alone when there are not — which is why
    /// completing the row under the cursor is one key, not two.
    #[must_use]
    pub fn targets(&self) -> Vec<Id> {
        if self.explicit.is_empty() {
            self.cursor.iter().cloned().collect()
        } else {
            self.in_order(&self.explicit)
        }
    }

    /// Whether more than one row would be acted on.
    #[must_use]
    pub fn is_multiple(&self) -> bool {
        self.targets().len() > 1
    }

    /// Whether `id` draws as selected. The cursor counts while nothing is
    /// ticked, so the keyboard's position is always visible.
    #[must_use]
    pub fn is_selected(&self, id: &Id) -> bool {
        if self.explicit.is_empty() {
            self.cursor.as_ref() == Some(id)
        } else {
            self.explicit.contains(id)
        }
    }

    /// Take a new set of rows, keeping the cursor where a person expects.
    ///
    /// A row that left — the task just completed under the cursor — hands the
    /// cursor to whatever took its index, clamped to the new last row. Ticked
    /// rows that left are dropped: acting on them would be acting on nothing.
    pub fn reconcile(&mut self, rows: Vec<Id>) {
        let previous = self
            .cursor
            .as_ref()
            .and_then(|c| self.ids.iter().position(|id| id == c));
        self.ids = rows;
        let present: HashSet<&Id> = self.ids.iter().collect();
        self.explicit.retain(|id| present.contains(id));
        if self.anchor.as_ref().is_some_and(|a| !present.contains(a)) {
            self.anchor = None;
        }
        if self.ids.is_empty() {
            self.cursor = None;
            self.anchor = None;
            self.explicit.clear();
            return;
        }
        if self.cursor.as_ref().is_some_and(|c| present.contains(c)) {
            return;
        }
        self.cursor = previous.map(|i| self.ids[i.min(self.ids.len() - 1)].clone());
    }

    /// Move the cursor one row, dropping any range or tick. With no cursor an
    /// arrow adopts the near end; at an end the cursor stays and the range
    /// still collapses onto it.
    pub fn step(&mut self, direction: SelectionMove) {
        self.explicit.clear();
        match self.index_after(direction) {
            Some(next) => {
                self.cursor = Some(self.ids[next].clone());
                self.anchor = self.cursor.clone();
            }
            None => self.anchor = self.cursor.clone(),
        }
    }

    /// Jump to the first or the last row.
    pub fn step_to_edge(&mut self, edge: SelectionEdge) {
        let target = match edge {
            SelectionEdge::First => self.ids.first(),
            SelectionEdge::Last => self.ids.last(),
        };
        let Some(target) = target.cloned() else {
            return;
        };
        self.cursor = Some(target.clone());
        self.explicit.clear();
        self.anchor = Some(target);
    }

    /// Grow or shrink the range from the anchor to one row further on. The
    /// range is recomputed from the anchor each time, so reversing direction
    /// un-selects.
    pub fn extend(&mut self, direction: SelectionMove) {
        let Some(next) = self.index_after(direction) else {
            return;
        };
        let next_id = self.ids[next].clone();
        let anchor = self
            .anchor
            .clone()
            .or_else(|| self.cursor.clone())
            .unwrap_or_else(|| next_id.clone());
        self.anchor = Some(anchor.clone());
        self.cursor = Some(next_id.clone());
        self.explicit = match self.ids.iter().position(|id| *id == anchor) {
            Some(start) => {
                let (lo, hi) = (start.min(next), start.max(next));
                self.ids[lo..=hi].iter().cloned().collect()
            }
            None => HashSet::from([next_id]),
        };
    }

    /// Space: tick or untick the cursor's row. The first tick adds the
    /// cursor's own row, so Space never deselects the row the targets were
    /// already answering with.
    pub fn toggle_at_cursor(&mut self) {
        let Some(cursor) = self.cursor.clone() else {
            return;
        };
        if !self.explicit.remove(&cursor) {
            self.explicit.insert(cursor.clone());
        }
        self.anchor = Some(cursor);
    }

    /// A click or a reveal: one row, nothing ticked. Nothing for a row that
    /// is not in the list.
    pub fn focus(&mut self, id: &Id) {
        if !self.ids.contains(id) {
            return;
        }
        self.cursor = Some(id.clone());
        self.explicit.clear();
        self.anchor = Some(id.clone());
    }

    /// Take a selection the list made itself — a click, a shift-click, a
    /// drag. Rows not in the list are ignored; a cursor outside the new set
    /// moves to its first row, so a following Shift+arrow grows from there.
    pub fn adopt(&mut self, selected: &[Id]) {
        let present: HashSet<&Id> = self.ids.iter().collect();
        self.explicit = selected
            .iter()
            .filter(|id| present.contains(id))
            .cloned()
            .collect();
        if self.explicit.is_empty()
            || self
                .cursor
                .as_ref()
                .is_some_and(|c| self.explicit.contains(c))
        {
            return;
        }
        self.cursor = self
            .ids
            .iter()
            .find(|id| self.explicit.contains(*id))
            .cloned();
        self.anchor = self.cursor.clone();
    }

    /// Tick every row.
    pub fn select_all(&mut self) {
        self.explicit = self.ids.iter().cloned().collect();
        self.anchor = self.ids.first().cloned();
        if self.cursor.is_none() {
            self.cursor = self.ids.first().cloned();
        }
    }

    /// Escape: drop the ticks and keep the place.
    pub fn clear_selection(&mut self) {
        self.explicit.clear();
        self.anchor = self.cursor.clone();
    }

    fn index_after(&self, direction: SelectionMove) -> Option<usize> {
        if self.ids.is_empty() {
            return None;
        }
        let current = self
            .cursor
            .as_ref()
            .and_then(|c| self.ids.iter().position(|id| id == c));
        let Some(current) = current else {
            return Some(match direction {
                SelectionMove::Down => 0,
                SelectionMove::Up => self.ids.len() - 1,
            });
        };
        match direction {
            SelectionMove::Up => current.checked_sub(1),
            SelectionMove::Down => (current + 1 < self.ids.len()).then_some(current + 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(ids: &[&'static str]) -> ListSelection<&'static str> {
        let mut s = ListSelection::new();
        s.reconcile(ids.to_vec());
        s
    }

    #[test]
    fn a_new_list_starts_with_no_cursor() {
        let s = list(&["a1", "b2"]);
        assert_eq!(s.cursor(), None);
        assert!(s.targets().is_empty());
    }

    #[test]
    fn the_first_arrow_adopts_the_near_end() {
        let mut down = list(&["a1", "b2", "c3"]);
        down.step(SelectionMove::Down);
        assert_eq!(down.cursor(), Some(&"a1"));
        let mut up = list(&["a1", "b2", "c3"]);
        up.step(SelectionMove::Up);
        assert_eq!(up.cursor(), Some(&"c3"));
    }

    #[test]
    fn moving_walks_the_list_and_stops_at_both_ends() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.step(SelectionMove::Down);
        s.step(SelectionMove::Down);
        assert_eq!(s.cursor(), Some(&"b2"));
        s.step(SelectionMove::Down);
        s.step(SelectionMove::Down);
        assert_eq!(s.cursor(), Some(&"c3"), "the end does not wrap");
        for _ in 0..4 {
            s.step(SelectionMove::Up);
        }
        assert_eq!(s.cursor(), Some(&"a1"));
    }

    #[test]
    fn the_cursor_alone_is_the_target_until_something_is_ticked() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"b2");
        assert_eq!(s.targets(), ["b2"]);
        assert!(!s.is_multiple());
        assert!(s.is_selected(&"b2"));
        assert!(!s.is_selected(&"a1"));
    }

    #[test]
    fn space_ticks_the_cursor_row_rather_than_starting_empty() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"b2");
        s.toggle_at_cursor();
        assert_eq!(s.explicit(), ["b2"]);
        assert_eq!(s.targets(), ["b2"]);
        s.toggle_at_cursor();
        assert!(s.explicit().is_empty());
        assert_eq!(s.targets(), ["b2"], "un-ticking falls back to the cursor");
    }

    #[test]
    fn a_plain_move_between_two_ticks_drops_the_first() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"a1");
        s.toggle_at_cursor();
        s.step(SelectionMove::Down);
        s.step(SelectionMove::Down);
        s.toggle_at_cursor();
        assert_eq!(s.targets(), ["c3"]);
    }

    #[test]
    fn targets_come_back_in_screen_order() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"c3");
        s.toggle_at_cursor();
        s.adopt(&["c3", "a1"]);
        assert_eq!(s.targets(), ["a1", "c3"]);
        assert!(s.is_multiple());
    }

    #[test]
    fn shift_arrow_grows_shrinks_and_grows_upwards_from_the_anchor() {
        let mut s = list(&["a1", "b2", "c3", "d4"]);
        s.focus(&"a1");
        s.extend(SelectionMove::Down);
        assert_eq!(s.targets(), ["a1", "b2"]);
        s.extend(SelectionMove::Down);
        assert_eq!(s.targets(), ["a1", "b2", "c3"]);
        assert_eq!(s.cursor(), Some(&"c3"));
        s.extend(SelectionMove::Up);
        assert_eq!(s.targets(), ["a1", "b2"], "reversing un-selects");
        let mut up = list(&["a1", "b2", "c3", "d4"]);
        up.focus(&"d4");
        up.extend(SelectionMove::Up);
        up.extend(SelectionMove::Up);
        assert_eq!(up.targets(), ["b2", "c3", "d4"]);
    }

    #[test]
    fn a_plain_arrow_collapses_the_range_even_at_the_end() {
        let mut s = list(&["a1", "b2", "c3", "d4"]);
        s.focus(&"a1");
        s.extend(SelectionMove::Down);
        s.extend(SelectionMove::Down);
        s.step(SelectionMove::Down);
        assert_eq!(s.targets(), ["d4"]);
        assert!(s.explicit().is_empty());
        let mut end = list(&["a1", "b2"]);
        end.focus(&"a1");
        end.extend(SelectionMove::Down);
        end.step(SelectionMove::Down);
        assert_eq!(end.cursor(), Some(&"b2"));
        assert!(end.explicit().is_empty());
    }

    #[test]
    fn escape_drops_the_ticks_and_keeps_the_place() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"a1");
        s.extend(SelectionMove::Down);
        s.clear_selection();
        assert!(s.explicit().is_empty());
        assert_eq!(s.cursor(), Some(&"b2"));
    }

    #[test]
    fn edges_reach_both_ends() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.step_to_edge(SelectionEdge::Last);
        assert_eq!(s.cursor(), Some(&"c3"));
        s.step_to_edge(SelectionEdge::First);
        assert_eq!(s.cursor(), Some(&"a1"));
        let mut empty: ListSelection<&str> = ListSelection::new();
        empty.step_to_edge(SelectionEdge::Last);
        assert_eq!(empty.cursor(), None);
    }

    #[test]
    fn a_vanished_cursor_lands_on_whatever_took_its_place_or_the_new_last() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"b2");
        s.reconcile(vec!["a1", "c3"]);
        assert_eq!(s.cursor(), Some(&"c3"));
        let mut last = list(&["a1", "b2", "c3"]);
        last.focus(&"c3");
        last.reconcile(vec!["a1", "b2"]);
        assert_eq!(last.cursor(), Some(&"b2"));
        let mut stays = list(&["a1", "b2", "c3"]);
        stays.focus(&"b2");
        stays.reconcile(vec!["z0", "a1", "b2", "c3"]);
        assert_eq!(stays.cursor(), Some(&"b2"));
    }

    #[test]
    fn departed_rows_leave_the_selection_and_an_emptied_list_has_none() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"a1");
        s.extend(SelectionMove::Down);
        s.extend(SelectionMove::Down);
        assert_eq!(s.targets().len(), 3);
        s.reconcile(vec!["a1", "c3"]);
        assert_eq!(s.targets(), ["a1", "c3"]);
        s.toggle_at_cursor();
        s.reconcile(vec![]);
        assert_eq!((s.cursor(), s.anchor()), (None, None));
        assert!(s.targets().is_empty());
    }

    #[test]
    fn a_mouse_selection_moves_the_cursor_with_it_and_ignores_strangers() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.adopt(&["b2", "c3"]);
        assert_eq!(s.cursor(), Some(&"b2"));
        assert_eq!(s.targets(), ["b2", "c3"]);
        s.extend(SelectionMove::Down);
        assert_eq!(s.targets(), ["b2", "c3"]);
        let mut strangers = list(&["a1", "b2"]);
        strangers.adopt(&["b2", "gone"]);
        assert_eq!(strangers.targets(), ["b2"]);
    }

    #[test]
    fn focusing_a_row_that_is_not_there_changes_nothing_and_select_all_takes_every_row() {
        let mut s = list(&["a1", "b2", "c3"]);
        s.focus(&"a1");
        s.focus(&"nope");
        assert_eq!(s.cursor(), Some(&"a1"));
        let mut all = list(&["a1", "b2", "c3"]);
        all.select_all();
        assert_eq!(all.targets(), ["a1", "b2", "c3"]);
        assert_eq!(all.cursor(), Some(&"a1"));
    }
}
