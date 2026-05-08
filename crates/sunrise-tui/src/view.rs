//! View enum + view-state.

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
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            view: View::Today,
            vim_mode: true,
        }
    }
}
