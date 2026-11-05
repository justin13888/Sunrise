//! The keyboard layer's decisions, lowered onto UniFFI:
//! [`sunrise_client_core::palette`] (ranking a typed query against command
//! names) and [`sunrise_client_core::selection`] (a list's cursor, ticked rows
//! and range anchor).
//!
//! The selection object is `CoreListSelection`, and its moves `ListMove` and
//! `ListEdge`, because the Apple app already declares `ListSelection`,
//! `SelectionMove` and `SelectionEdge`, which these replace in a later pass;
//! the generated Swift must not shadow them first.

use std::sync::{Arc, Mutex, MutexGuard};

use sunrise_client_core::palette;
use sunrise_client_core::selection::{ListSelection, SelectionEdge, SelectionMove};

/// How well `query` matches the command name `title`: higher is better, and
/// `None` is no match.
#[uniffi::export]
#[must_use]
pub fn palette_score(query: String, title: String) -> Option<i64> {
    palette::palette_score(&query, &title)
}

/// The indices of `titles` that match `query`, best first; ties and an empty
/// query keep the catalogue's order.
#[uniffi::export]
#[must_use]
pub fn palette_rank(query: String, titles: Vec<String>) -> Vec<u32> {
    let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
    palette::palette_rank(&query, &titles)
        .into_iter()
        .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
        .collect()
}

/// The highlight after one step over `count` rows, clamped at both ends.
#[uniffi::export]
#[must_use]
pub fn palette_highlight(current: u32, count: u32, down: bool) -> u32 {
    let moved = palette::palette_highlight(current as usize, count as usize, down);
    u32::try_from(moved).unwrap_or(u32::MAX)
}

/// See [`sunrise_client_core::selection::SelectionMove`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ListMove {
    /// Towards the first row.
    Up,
    /// Towards the last row.
    Down,
}

impl From<ListMove> for SelectionMove {
    fn from(m: ListMove) -> Self {
        match m {
            ListMove::Up => Self::Up,
            ListMove::Down => Self::Down,
        }
    }
}

/// See [`sunrise_client_core::selection::SelectionEdge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ListEdge {
    /// The first row.
    First,
    /// The last row.
    Last,
}

impl From<ListEdge> for SelectionEdge {
    fn from(e: ListEdge) -> Self {
        match e {
            ListEdge::First => Self::First,
            ListEdge::Last => Self::Last,
        }
    }
}

/// See [`sunrise_client_core::selection::ListSelection`], over string ids.
#[derive(Debug, Default, uniffi::Object)]
pub struct CoreListSelection(Mutex<ListSelection<String>>);

impl CoreListSelection {
    fn inner(&self) -> MutexGuard<'_, ListSelection<String>> {
        // A panic inside a move leaves the selection as it was before it.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[uniffi::export]
impl CoreListSelection {
    /// No rows, no cursor.
    #[uniffi::constructor]
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Take a new set of rows, in drawn order.
    pub fn reconcile(&self, rows: Vec<String>) {
        self.inner().reconcile(rows);
    }

    /// The row the keyboard is on.
    #[must_use]
    pub fn cursor(&self) -> Option<String> {
        self.inner().cursor().cloned()
    }

    /// Where a range grows from.
    #[must_use]
    pub fn anchor(&self) -> Option<String> {
        self.inner().anchor().cloned()
    }

    /// The ticked rows, in list order.
    #[must_use]
    pub fn explicit(&self) -> Vec<String> {
        self.inner().explicit()
    }

    /// What a row action applies to, in list order.
    #[must_use]
    pub fn targets(&self) -> Vec<String> {
        self.inner().targets()
    }

    /// Whether more than one row would be acted on.
    #[must_use]
    pub fn is_multiple(&self) -> bool {
        self.inner().is_multiple()
    }

    /// Whether `id` draws as selected.
    #[must_use]
    pub fn is_selected(&self, id: String) -> bool {
        self.inner().is_selected(&id)
    }

    /// An arrow key: one row, dropping any range or tick.
    pub fn step(&self, direction: ListMove) {
        self.inner().step(direction.into());
    }

    /// Jump to the first or last row.
    pub fn step_to_edge(&self, edge: ListEdge) {
        self.inner().step_to_edge(edge.into());
    }

    /// Shift+arrow: grow or shrink the range from the anchor.
    pub fn extend(&self, direction: ListMove) {
        self.inner().extend(direction.into());
    }

    /// Space: tick or untick the cursor's row.
    pub fn toggle_at_cursor(&self) {
        self.inner().toggle_at_cursor();
    }

    /// A click or a reveal: one row, nothing ticked.
    pub fn focus(&self, id: String) {
        self.inner().focus(&id);
    }

    /// A selection the list made itself.
    pub fn adopt(&self, selected: Vec<String>) {
        self.inner().adopt(&selected);
    }

    /// Tick every row.
    pub fn select_all(&self) {
        self.inner().select_all();
    }

    /// Escape: drop the ticks and keep the place.
    pub fn clear_selection(&self) {
        self.inner().clear_selection();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exports_lower_the_client_layer_unchanged() {
        let titles = vec![
            "Open Command Palette".to_owned(),
            "Open".to_owned(),
            "Inbox".to_owned(),
        ];
        assert_eq!(palette_rank("open".into(), titles), vec![1, 0]);
        assert_eq!(palette_score("op".into(), "Open".into()), Some(14));
        assert_eq!(palette_highlight(2, 3, true), 2);

        let s = CoreListSelection::new();
        s.reconcile(vec!["a".into(), "b".into(), "c".into()]);
        s.step(ListMove::Down);
        s.extend(ListMove::Down);
        assert_eq!(s.targets(), ["a", "b"]);
        s.step_to_edge(ListEdge::Last);
        assert_eq!(s.cursor().as_deref(), Some("c"));
    }
}
