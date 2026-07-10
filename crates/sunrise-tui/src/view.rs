//! View enum + view-state.

use crate::keymap::Mode;
use sunrise_domain::Task;

/// Primary views per the parity matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Today: scheduled blocks + due-today tasks + manually-pulled tasks.
    Today,
    /// Inbox: unassigned tasks awaiting triage.
    Inbox,
    /// Per-Stream view (target stream id is part of [`ViewState`]).
    Stream,
    /// Free-text search.
    Search,
    /// Focus mode: one-task fullscreen.
    Focus,
}

/// Owning struct for the active view.
#[derive(Debug, Clone)]
pub struct ViewState {
    /// Current view.
    pub view: View,
    /// Vim-mode toggle. Defaults to true on TUI per spec.
    pub vim_mode: bool,
    /// Current keymap mode.
    pub mode: Mode,
    /// Tasks currently visible in the view (refreshed after each command).
    pub tasks: Vec<Task>,
    /// Index of the selected task in `tasks`. `None` if the list is empty.
    pub selected: Option<usize>,
    /// Active text input (capture buffer / search query).
    pub input: String,
    /// One-line status / error displayed at the bottom of every view.
    pub status: String,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            view: View::Today,
            vim_mode: true,
            mode: Mode::Normal,
            tasks: Vec::new(),
            selected: None,
            input: String::new(),
            status: String::new(),
        }
    }
}

impl ViewState {
    /// Apply selection bookkeeping after `tasks` has changed.
    pub fn after_tasks_loaded(&mut self) {
        self.selected = match (self.tasks.is_empty(), self.selected) {
            (true, _) => None,
            (false, None) => Some(0),
            (false, Some(i)) if i >= self.tasks.len() => Some(self.tasks.len() - 1),
            (false, Some(i)) => Some(i),
        };
    }

    /// Move selection down by one, wrapping at the end.
    pub fn select_next(&mut self) {
        if self.tasks.is_empty() {
            self.selected = None;
            return;
        }
        let next = self.selected.map_or(0, |i| (i + 1) % self.tasks.len());
        self.selected = Some(next);
    }

    /// Move selection up by one, wrapping at the start.
    pub fn select_prev(&mut self) {
        if self.tasks.is_empty() {
            self.selected = None;
            return;
        }
        let prev = self
            .selected
            .map_or(0, |i| if i == 0 { self.tasks.len() - 1 } else { i - 1 });
        self.selected = Some(prev);
    }

    /// Selected task ref, if any.
    #[must_use]
    pub fn selected_task(&self) -> Option<&Task> {
        self.selected.and_then(|i| self.tasks.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;
    use std::collections::BTreeSet;
    use sunrise_domain::TaskState;
    use sunrise_id::{EntityKind, EntityRef};

    fn fake(idx: u8) -> Task {
        Task {
            id: EntityRef::new(EntityKind::Task, [idx; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            title: format!("task {idx}"),
            body: None,
            stream_id: EntityRef::new(EntityKind::Stream, [0u8; 16]),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            archived: false,
            deleted: false,
        }
    }

    #[test]
    fn select_next_wraps() {
        let mut s = ViewState::default();
        s.tasks = (0u8..3).map(fake).collect();
        s.after_tasks_loaded();
        assert_eq!(s.selected, Some(0));
        s.select_next();
        assert_eq!(s.selected, Some(1));
        s.select_next();
        s.select_next();
        // wraps after 3
        assert_eq!(s.selected, Some(0));
    }

    #[test]
    fn select_prev_wraps() {
        let mut s = ViewState::default();
        s.tasks = (0u8..3).map(fake).collect();
        s.after_tasks_loaded();
        s.select_prev();
        assert_eq!(s.selected, Some(2));
    }

    #[test]
    fn empty_clears_selection() {
        let mut s = ViewState::default();
        s.after_tasks_loaded();
        assert_eq!(s.selected, None);
        s.select_next();
        assert_eq!(s.selected, None);
    }
}
