//! View enum + view-state.

use crate::keymap::Mode;
use sunrise_core::queries::StreamRow;
use sunrise_domain::Task;
use sunrise_sync::SyncState;

/// Compact live-sync indicator rendered on the right of the status line.
///
/// A `None` value on [`ViewState::sync`] hides the indicator entirely — that
/// is the default (unit/render tests and any surface that hasn't wired sync).
/// The binary always sets it: to [`SyncIndicator::off`] when `SUNRISE_SYNC_URL`
/// is unset, otherwise to [`SyncIndicator::live`] with the driver's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncIndicator {
    /// `None` = sync off (no URL configured); `Some(state)` = live driver state.
    pub state: Option<SyncState>,
    /// Persisted outbox depth (unacked local ops).
    pub pending: u32,
}

impl SyncIndicator {
    /// The "off" indicator (no `SUNRISE_SYNC_URL`). Still surfaces the pending
    /// outbox depth so a user knows unsynced local work exists.
    #[must_use]
    pub const fn off(pending: u32) -> Self {
        Self {
            state: None,
            pending,
        }
    }

    /// Live-driver indicator from a [`SyncState`] plus the outbox depth.
    #[must_use]
    pub const fn live(state: SyncState, pending: u32) -> Self {
        Self {
            state: Some(state),
            pending,
        }
    }

    /// Compact label: `live` | `catching-up` | `disconnected` | `off`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self.state {
            None => "off",
            Some(SyncState::Live) => "live",
            Some(SyncState::CatchingUp) => "catching-up",
            Some(SyncState::Disconnected) => "disconnected",
        }
    }

    /// Full status text, e.g. `sync: live (0 pending)`.
    #[must_use]
    pub fn text(self) -> String {
        format!("sync: {} ({} pending)", self.label(), self.pending)
    }
}

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

/// Which pane of the split Stream view has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPane {
    /// Left pane: the stream list.
    Streams,
    /// Right pane: tasks of the selected stream.
    Tasks,
}

impl StreamPane {
    /// The other pane (there are exactly two).
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Streams => Self::Tasks,
            Self::Tasks => Self::Streams,
        }
    }
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
    /// Stream rows for the Stream view (Inbox first, per `Query::StreamList`).
    pub streams: Vec<StreamRow>,
    /// Index of the selected stream in `streams`. `None` if the list is empty.
    pub selected_stream: Option<usize>,
    /// Focused pane in the Stream view.
    pub pane: StreamPane,
    /// Task shown fullscreen in the Focus view.
    pub focused_task: Option<Task>,
    /// View to return to when Focus is closed with Esc.
    pub prev_view: Option<View>,
    /// Active text input (capture buffer / search query).
    pub input: String,
    /// One-line status / error displayed at the bottom of every view.
    pub status: String,
    /// Live-sync indicator shown on the right of the status line. `None` hides
    /// it; the binary sets it every frame from `Core::query(SyncStatus)`.
    pub sync: Option<SyncIndicator>,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            view: View::Today,
            vim_mode: true,
            mode: Mode::Normal,
            tasks: Vec::new(),
            selected: None,
            streams: Vec::new(),
            selected_stream: None,
            pane: StreamPane::Streams,
            focused_task: None,
            prev_view: None,
            input: String::new(),
            status: String::new(),
            sync: None,
        }
    }
}

impl ViewState {
    /// Apply selection bookkeeping after `tasks` has changed.
    pub fn after_tasks_loaded(&mut self) {
        self.selected = clamp_selection(self.tasks.len(), self.selected);
    }

    /// Apply selection bookkeeping after `streams` has changed. Defaults the
    /// selection to the first row (Inbox — `StreamList` returns it first).
    pub fn after_streams_loaded(&mut self) {
        self.selected_stream = clamp_selection(self.streams.len(), self.selected_stream);
    }

    /// Move selection down by one, wrapping at the end.
    pub fn select_next(&mut self) {
        self.selected = wrap_next(self.tasks.len(), self.selected);
    }

    /// Move selection up by one, wrapping at the start.
    pub fn select_prev(&mut self) {
        self.selected = wrap_prev(self.tasks.len(), self.selected);
    }

    /// Move the stream selection down by one, wrapping at the end.
    pub fn stream_next(&mut self) {
        self.selected_stream = wrap_next(self.streams.len(), self.selected_stream);
    }

    /// Move the stream selection up by one, wrapping at the start.
    pub fn stream_prev(&mut self) {
        self.selected_stream = wrap_prev(self.streams.len(), self.selected_stream);
    }

    /// Cursor-down in whichever list has keyboard focus (Stream view is
    /// pane-aware; every other view navigates the task list).
    pub fn nav_next(&mut self) {
        if self.view == View::Stream && self.pane == StreamPane::Streams {
            self.stream_next();
        } else {
            self.select_next();
        }
    }

    /// Cursor-up counterpart of [`Self::nav_next`].
    pub fn nav_prev(&mut self) {
        if self.view == View::Stream && self.pane == StreamPane::Streams {
            self.stream_prev();
        } else {
            self.select_prev();
        }
    }

    /// Toggle which Stream-view pane has focus (Tab).
    pub fn toggle_pane(&mut self) {
        self.pane = self.pane.other();
    }

    /// Focus a specific Stream-view pane (`h` → Streams, `l` → Tasks).
    pub fn focus_pane(&mut self, pane: StreamPane) {
        self.pane = pane;
    }

    /// Selected task ref, if any.
    #[must_use]
    pub fn selected_task(&self) -> Option<&Task> {
        self.selected.and_then(|i| self.tasks.get(i))
    }

    /// Selected stream row, if any.
    #[must_use]
    pub fn selected_stream_row(&self) -> Option<&StreamRow> {
        self.selected_stream.and_then(|i| self.streams.get(i))
    }

    /// Open the Focus view on the currently selected task, remembering the
    /// view to return to. No-op selection-wise when nothing is selected (the
    /// Focus view then renders its empty placeholder).
    pub fn open_focus(&mut self) {
        if self.view != View::Focus {
            self.prev_view = Some(self.view);
        }
        self.focused_task = self.selected_task().cloned();
        self.view = View::Focus;
    }

    /// Close the Focus view and return to the previous view (Today if the
    /// Focus view was entered directly). No-op outside the Focus view.
    pub fn close_focus(&mut self) {
        if self.view != View::Focus {
            return;
        }
        self.view = self.prev_view.take().unwrap_or(View::Today);
        self.focused_task = None;
    }
}

/// Clamp an optional selection index to a list of `len` items (first item
/// when unset, last when past the end, `None` when empty).
fn clamp_selection(len: usize, selected: Option<usize>) -> Option<usize> {
    match (len, selected) {
        (0, _) => None,
        (_, None) => Some(0),
        (n, Some(i)) if i >= n => Some(n - 1),
        (_, s) => s,
    }
}

/// Next index with wraparound; `None` when empty.
fn wrap_next(len: usize, selected: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.map_or(0, |i| (i + 1) % len))
}

/// Previous index with wraparound; `None` when empty.
fn wrap_prev(len: usize, selected: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(selected.map_or(0, |i| if i == 0 { len - 1 } else { i - 1 }))
}

/// Deterministic fixtures shared by the view/render test modules.
#[cfg(test)]
pub(crate) mod fixtures {
    use jiff::Timestamp;
    use std::collections::BTreeSet;
    use sunrise_core::queries::StreamRow;
    use sunrise_domain::{inbox_stream_ref, StreamColor, Task, TaskState};
    use sunrise_id::{EntityKind, EntityRef};

    /// A minimal task; `idx` seeds the id and title.
    pub(crate) fn fake_task(idx: u8) -> Task {
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
        }
    }

    /// The synthetic Inbox stream row (`StreamList` returns it first).
    pub(crate) fn inbox_row(open: u64) -> StreamRow {
        StreamRow {
            id: inbox_stream_ref(),
            name: "Inbox".into(),
            color: StreamColor::Slate,
            open_task_count: open,
            archived: false,
        }
    }

    /// A named user stream row; `idx` seeds the id.
    pub(crate) fn stream_row(idx: u8, name: &str, open: u64) -> StreamRow {
        StreamRow {
            id: EntityRef::new(EntityKind::Stream, [idx; 16]),
            name: name.into(),
            color: StreamColor::Sky,
            open_task_count: open,
            archived: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{fake_task, inbox_row, stream_row};
    use super::*;

    #[test]
    fn select_next_wraps() {
        let mut s = ViewState::default();
        s.tasks = (0u8..3).map(fake_task).collect();
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
        s.tasks = (0u8..3).map(fake_task).collect();
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

    #[test]
    fn streams_default_to_first_row() {
        let mut s = ViewState::default();
        s.streams = vec![inbox_row(2), stream_row(1, "Work", 3)];
        s.after_streams_loaded();
        assert_eq!(s.selected_stream, Some(0));
        assert_eq!(s.selected_stream_row().unwrap().name, "Inbox");
    }

    #[test]
    fn stream_selection_wraps_both_ways() {
        let mut s = ViewState::default();
        s.streams = vec![inbox_row(0), stream_row(1, "Work", 1)];
        s.after_streams_loaded();
        s.stream_next();
        assert_eq!(s.selected_stream, Some(1));
        s.stream_next();
        assert_eq!(s.selected_stream, Some(0));
        s.stream_prev();
        assert_eq!(s.selected_stream, Some(1));
    }

    #[test]
    fn stream_selection_clamps_after_reload() {
        let mut s = ViewState::default();
        s.streams = vec![inbox_row(0), stream_row(1, "Work", 1)];
        s.selected_stream = Some(5);
        s.after_streams_loaded();
        assert_eq!(s.selected_stream, Some(1));
        s.streams.clear();
        s.after_streams_loaded();
        assert_eq!(s.selected_stream, None);
    }

    #[test]
    fn pane_toggle_and_focus() {
        let mut s = ViewState::default();
        assert_eq!(s.pane, StreamPane::Streams);
        s.toggle_pane();
        assert_eq!(s.pane, StreamPane::Tasks);
        s.toggle_pane();
        assert_eq!(s.pane, StreamPane::Streams);
        s.focus_pane(StreamPane::Tasks);
        assert_eq!(s.pane, StreamPane::Tasks);
        s.focus_pane(StreamPane::Tasks);
        assert_eq!(s.pane, StreamPane::Tasks);
    }

    #[test]
    fn nav_is_pane_aware_in_stream_view() {
        let mut s = ViewState::default();
        s.view = View::Stream;
        s.streams = vec![inbox_row(0), stream_row(1, "Work", 1)];
        s.tasks = (0u8..2).map(fake_task).collect();
        s.after_streams_loaded();
        s.after_tasks_loaded();

        // Streams pane: j/k move the stream cursor, not the task cursor.
        s.pane = StreamPane::Streams;
        s.nav_next();
        assert_eq!(s.selected_stream, Some(1));
        assert_eq!(s.selected, Some(0));

        // Tasks pane: j/k move the task cursor.
        s.pane = StreamPane::Tasks;
        s.nav_next();
        assert_eq!(s.selected, Some(1));
        assert_eq!(s.selected_stream, Some(1));
    }

    #[test]
    fn nav_ignores_pane_outside_stream_view() {
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.pane = StreamPane::Streams;
        s.tasks = (0u8..2).map(fake_task).collect();
        s.after_tasks_loaded();
        s.nav_next();
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn focus_state_machine_roundtrip() {
        // Select in Inbox → open Focus shows that task → Esc returns.
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        s.select_next();
        let expected = s.selected_task().unwrap().id;

        s.open_focus();
        assert_eq!(s.view, View::Focus);
        assert_eq!(s.focused_task.as_ref().unwrap().id, expected);
        assert_eq!(s.prev_view, Some(View::Inbox));

        s.close_focus();
        assert_eq!(s.view, View::Inbox);
        assert_eq!(s.focused_task, None);
        assert_eq!(s.prev_view, None);
        // Selection in the originating view is untouched.
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn close_focus_defaults_to_today_and_noops_elsewhere() {
        let mut s = ViewState::default();
        s.view = View::Focus; // entered without open_focus (e.g. `:view focus`)
        s.close_focus();
        assert_eq!(s.view, View::Today);

        s.view = View::Inbox;
        s.close_focus();
        assert_eq!(s.view, View::Inbox);
    }

    #[test]
    fn reopening_focus_from_focus_keeps_original_return_view() {
        let mut s = ViewState::default();
        s.view = View::Search;
        s.tasks = vec![fake_task(1)];
        s.after_tasks_loaded();
        s.open_focus();
        // A second open_focus while already in Focus must not clobber
        // prev_view with View::Focus.
        s.open_focus();
        assert_eq!(s.prev_view, Some(View::Search));
        s.close_focus();
        assert_eq!(s.view, View::Search);
    }
}
