//! Vim-style keymap for the TUI.
//!
//! Matches `docs/07-clients/tui.md`. v1 covers a small set of
//! navigation/mutation actions; further operators (text-object motions,
//! macros) are out of scope for v1.

use crate::view::View;

/// Mode for the modal editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Default mode: keystrokes map to navigation/commands.
    Normal,
    /// Insert mode: keystrokes feed the active text input (capture, search).
    Insert,
    /// Command-line mode: `:` started a command (`:q`, `:w`, `:focus`).
    Command,
}

/// Discrete actions fired by the keymap. The runtime maps each action
/// onto a [`sunrise_core::Command`] or a view-state mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Quit the app.
    Quit,
    /// Switch to a primary view.
    SwitchView(View),
    /// Cursor down one item.
    Next,
    /// Cursor up one item.
    Prev,
    /// Mark current task as done.
    Toggle,
    /// Open the capture prompt.
    Capture,
    /// Begin a search (switches to Insert in the search bar).
    BeginSearch,
    /// Begin a command-line entry (`:` switches to Command mode).
    BeginCommand,
    /// Enter Insert mode.
    EnterInsert,
    /// Toggle which Stream-view pane has focus (Tab).
    TogglePane,
    /// Focus the left (Streams) pane of the Stream view.
    PaneLeft,
    /// Focus the right (Tasks) pane of the Stream view.
    PaneRight,
    /// Activate the highlighted item (Enter in Normal mode): select a
    /// stream in the Streams pane, open Focus on a task elsewhere.
    Activate,
    /// Leave Insert/Command mode back to Normal (or close Focus).
    Escape,
    /// Append a character to the active input (Insert/Command mode).
    InsertChar(char),
    /// Backspace in the active input.
    Backspace,
    /// Submit the current input (Enter).
    Submit,
}

/// Translate a key event in a given mode (and active view) into an
/// [`Action`].
///
/// `vim_mode = false` collapses normal-mode bindings down to a friendlier
/// "always insert" feel: `j/k` produce InsertChar instead of Next/Prev.
/// `view` disambiguates the few view-local bindings: Tab / `h` / `l`
/// switch panes only in the Stream view, and Esc closes the Focus view
/// instead of quitting.
#[must_use]
pub fn dispatch(
    key: crossterm::event::KeyCode,
    mode: Mode,
    vim_mode: bool,
    view: View,
) -> Option<Action> {
    use crossterm::event::KeyCode::*;
    match (mode, key) {
        (Mode::Insert | Mode::Command, Esc) => Some(Action::Escape),
        (Mode::Normal, Esc) if view == View::Focus => Some(Action::Escape),
        (Mode::Normal, Esc | Char('q')) => Some(Action::Quit),
        (Mode::Normal, Char('1')) => Some(Action::SwitchView(View::Today)),
        (Mode::Normal, Char('2')) => Some(Action::SwitchView(View::Inbox)),
        (Mode::Normal, Char('3')) => Some(Action::SwitchView(View::Stream)),
        (Mode::Normal, Char('4')) => Some(Action::SwitchView(View::Search)),
        (Mode::Normal, Char('5' | 'f')) => Some(Action::SwitchView(View::Focus)),
        (Mode::Normal, Tab) if view == View::Stream => Some(Action::TogglePane),
        (Mode::Normal, Char('h')) if view == View::Stream && vim_mode => Some(Action::PaneLeft),
        (Mode::Normal, Char('l')) if view == View::Stream && vim_mode => Some(Action::PaneRight),
        (Mode::Normal, Left) if view == View::Stream => Some(Action::PaneLeft),
        (Mode::Normal, Right) if view == View::Stream => Some(Action::PaneRight),
        (Mode::Normal, Enter) => Some(Action::Activate),
        (Mode::Normal, Char('j')) if vim_mode => Some(Action::Next),
        (Mode::Normal, Char('k')) if vim_mode => Some(Action::Prev),
        (Mode::Normal, Down) => Some(Action::Next),
        (Mode::Normal, Up) => Some(Action::Prev),
        (Mode::Normal, Char('x' | ' ')) => Some(Action::Toggle),
        (Mode::Normal, Char('c')) => Some(Action::Capture),
        (Mode::Normal, Char('/')) => Some(Action::BeginSearch),
        (Mode::Normal, Char(':')) => Some(Action::BeginCommand),
        (Mode::Normal, Char('i')) if vim_mode => Some(Action::EnterInsert),
        (Mode::Insert | Mode::Command, Backspace) => Some(Action::Backspace),
        (Mode::Insert | Mode::Command, Enter) => Some(Action::Submit),
        (Mode::Insert | Mode::Command, Char(c)) => Some(Action::InsertChar(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    /// Shorthand: dispatch in the Today view (the default context).
    fn d(key: KeyCode, mode: Mode, vim: bool) -> Option<Action> {
        dispatch(key, mode, vim, View::Today)
    }

    #[test]
    fn quit_in_normal_mode() {
        assert_eq!(
            d(KeyCode::Char('q'), Mode::Normal, true),
            Some(Action::Quit)
        );
        assert_eq!(d(KeyCode::Esc, Mode::Normal, true), Some(Action::Quit));
    }

    #[test]
    fn vim_movement() {
        assert_eq!(
            d(KeyCode::Char('j'), Mode::Normal, true),
            Some(Action::Next)
        );
        assert_eq!(
            d(KeyCode::Char('k'), Mode::Normal, true),
            Some(Action::Prev)
        );
    }

    #[test]
    fn arrow_movement_works_in_both_modes() {
        assert_eq!(d(KeyCode::Down, Mode::Normal, false), Some(Action::Next));
        assert_eq!(d(KeyCode::Up, Mode::Normal, false), Some(Action::Prev));
    }

    #[test]
    fn switch_view_bindings() {
        assert_eq!(
            d(KeyCode::Char('2'), Mode::Normal, true),
            Some(Action::SwitchView(View::Inbox))
        );
        assert_eq!(
            d(KeyCode::Char('1'), Mode::Normal, true),
            Some(Action::SwitchView(View::Today))
        );
    }

    #[test]
    fn insert_mode_buffers_chars() {
        assert_eq!(
            d(KeyCode::Char('x'), Mode::Insert, true),
            Some(Action::InsertChar('x'))
        );
        assert_eq!(
            d(KeyCode::Backspace, Mode::Insert, true),
            Some(Action::Backspace)
        );
        assert_eq!(d(KeyCode::Esc, Mode::Insert, true), Some(Action::Escape));
    }

    #[test]
    fn colon_enters_command_mode() {
        assert_eq!(
            d(KeyCode::Char(':'), Mode::Normal, true),
            Some(Action::BeginCommand)
        );
    }

    #[test]
    fn command_mode_buffers_and_submits() {
        assert_eq!(
            d(KeyCode::Char('v'), Mode::Command, true),
            Some(Action::InsertChar('v'))
        );
        assert_eq!(
            d(KeyCode::Backspace, Mode::Command, true),
            Some(Action::Backspace)
        );
        assert_eq!(d(KeyCode::Enter, Mode::Command, true), Some(Action::Submit));
        assert_eq!(d(KeyCode::Esc, Mode::Command, true), Some(Action::Escape));
    }

    #[test]
    fn capture_and_search_bindings() {
        assert_eq!(
            d(KeyCode::Char('c'), Mode::Normal, true),
            Some(Action::Capture)
        );
        assert_eq!(
            d(KeyCode::Char('/'), Mode::Normal, true),
            Some(Action::BeginSearch)
        );
    }

    #[test]
    fn pane_switching_only_in_stream_view() {
        assert_eq!(
            dispatch(KeyCode::Tab, Mode::Normal, true, View::Stream),
            Some(Action::TogglePane)
        );
        assert_eq!(
            dispatch(KeyCode::Char('h'), Mode::Normal, true, View::Stream),
            Some(Action::PaneLeft)
        );
        assert_eq!(
            dispatch(KeyCode::Char('l'), Mode::Normal, true, View::Stream),
            Some(Action::PaneRight)
        );
        assert_eq!(
            dispatch(KeyCode::Left, Mode::Normal, false, View::Stream),
            Some(Action::PaneLeft)
        );
        // Outside the Stream view none of these bind.
        assert_eq!(
            dispatch(KeyCode::Tab, Mode::Normal, true, View::Today),
            None
        );
        assert_eq!(
            dispatch(KeyCode::Char('h'), Mode::Normal, true, View::Inbox),
            None
        );
    }

    #[test]
    fn enter_activates_in_normal_mode() {
        assert_eq!(
            d(KeyCode::Enter, Mode::Normal, true),
            Some(Action::Activate)
        );
        assert_eq!(
            dispatch(KeyCode::Enter, Mode::Normal, true, View::Stream),
            Some(Action::Activate)
        );
    }

    #[test]
    fn f_opens_focus_view() {
        assert_eq!(
            d(KeyCode::Char('f'), Mode::Normal, true),
            Some(Action::SwitchView(View::Focus))
        );
    }

    #[test]
    fn esc_in_focus_view_escapes_instead_of_quitting() {
        assert_eq!(
            dispatch(KeyCode::Esc, Mode::Normal, true, View::Focus),
            Some(Action::Escape)
        );
        assert_eq!(
            dispatch(KeyCode::Esc, Mode::Normal, true, View::Today),
            Some(Action::Quit)
        );
    }
}
