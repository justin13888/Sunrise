//! Vim-style keymap for the TUI.
//!
//! Matches `spec/07-clients/tui.md`. v1 covers a small set of
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
    /// Enter Insert mode.
    EnterInsert,
    /// Leave Insert/Command mode back to Normal.
    Escape,
    /// Append a character to the active input (Insert/Command mode).
    InsertChar(char),
    /// Backspace in the active input.
    Backspace,
    /// Submit the current input (Enter).
    Submit,
}

/// Translate a key event in a given mode into an [`Action`].
///
/// `vim_mode = false` collapses normal-mode bindings down to a friendlier
/// "always insert" feel: `j/k` produce InsertChar instead of Next/Prev.
#[must_use]
pub fn dispatch(key: crossterm::event::KeyCode, mode: Mode, vim_mode: bool) -> Option<Action> {
    use crossterm::event::KeyCode::*;
    match (mode, key) {
        (Mode::Normal, Char('q')) | (_, Esc) if mode == Mode::Normal => Some(Action::Quit),
        (Mode::Insert | Mode::Command, Esc) => Some(Action::Escape),
        (Mode::Normal, Esc) => Some(Action::Quit),
        (Mode::Normal, Char('q')) => Some(Action::Quit),
        (Mode::Normal, Char('1')) => Some(Action::SwitchView(View::Today)),
        (Mode::Normal, Char('2')) => Some(Action::SwitchView(View::Inbox)),
        (Mode::Normal, Char('3')) => Some(Action::SwitchView(View::Stream)),
        (Mode::Normal, Char('4')) => Some(Action::SwitchView(View::Search)),
        (Mode::Normal, Char('5')) => Some(Action::SwitchView(View::Focus)),
        (Mode::Normal, Char('j')) if vim_mode => Some(Action::Next),
        (Mode::Normal, Char('k')) if vim_mode => Some(Action::Prev),
        (Mode::Normal, Down) => Some(Action::Next),
        (Mode::Normal, Up) => Some(Action::Prev),
        (Mode::Normal, Char('x' | ' ')) => Some(Action::Toggle),
        (Mode::Normal, Char('c')) => Some(Action::Capture),
        (Mode::Normal, Char('/')) => Some(Action::BeginSearch),
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

    #[test]
    fn quit_in_normal_mode() {
        assert_eq!(
            dispatch(KeyCode::Char('q'), Mode::Normal, true),
            Some(Action::Quit)
        );
        assert_eq!(
            dispatch(KeyCode::Esc, Mode::Normal, true),
            Some(Action::Quit)
        );
    }

    #[test]
    fn vim_movement() {
        assert_eq!(
            dispatch(KeyCode::Char('j'), Mode::Normal, true),
            Some(Action::Next)
        );
        assert_eq!(
            dispatch(KeyCode::Char('k'), Mode::Normal, true),
            Some(Action::Prev)
        );
    }

    #[test]
    fn arrow_movement_works_in_both_modes() {
        assert_eq!(
            dispatch(KeyCode::Down, Mode::Normal, false),
            Some(Action::Next)
        );
        assert_eq!(
            dispatch(KeyCode::Up, Mode::Normal, false),
            Some(Action::Prev)
        );
    }

    #[test]
    fn switch_view_bindings() {
        assert_eq!(
            dispatch(KeyCode::Char('2'), Mode::Normal, true),
            Some(Action::SwitchView(View::Inbox))
        );
        assert_eq!(
            dispatch(KeyCode::Char('1'), Mode::Normal, true),
            Some(Action::SwitchView(View::Today))
        );
    }

    #[test]
    fn insert_mode_buffers_chars() {
        assert_eq!(
            dispatch(KeyCode::Char('x'), Mode::Insert, true),
            Some(Action::InsertChar('x'))
        );
        assert_eq!(
            dispatch(KeyCode::Backspace, Mode::Insert, true),
            Some(Action::Backspace)
        );
        assert_eq!(
            dispatch(KeyCode::Esc, Mode::Insert, true),
            Some(Action::Escape)
        );
    }

    #[test]
    fn capture_and_search_bindings() {
        assert_eq!(
            dispatch(KeyCode::Char('c'), Mode::Normal, true),
            Some(Action::Capture)
        );
        assert_eq!(
            dispatch(KeyCode::Char('/'), Mode::Normal, true),
            Some(Action::BeginSearch)
        );
    }
}
