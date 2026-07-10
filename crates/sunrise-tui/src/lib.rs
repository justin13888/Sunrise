//! Sunrise terminal UI library surface.
//!
//! Implements `docs/07-clients/tui.md` foundation. v1 ships:
//!
//! - Today / Inbox / Stream / Search / Focus view enum.
//! - Vim-style modal navigation (default-on per the parity matrix).
//! - Snapshot-testable Ratatui renderers for every primary view.
//!
//! The actual TUI runtime (terminal raw mode, event loop, drawing) is in
//! the bin crate. This library exposes the pure render functions so they
//! can be exercised without a real terminal.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::field_reassign_with_default,
    clippy::needless_pass_by_value,
    clippy::missing_const_for_fn,
    clippy::manual_let_else,
    clippy::redundant_closure_for_method_calls,
    clippy::single_match_else,
    clippy::map_unwrap_or,
    clippy::enum_glob_use,
    clippy::match_same_arms,
    clippy::semicolon_if_nothing_returned,
    clippy::too_many_lines
)]

pub mod command;
pub mod keymap;
pub mod render;
pub mod view;

pub use command::{parse_command, Cmd};
pub use keymap::{dispatch, Action, Mode};
pub use render::{render, render_focus, render_inbox, render_search, render_stream, render_today};
pub use view::{StreamPane, View, ViewState};

/// Help text listing the command-line commands, shown in the status line by
/// `:help`. Kept short enough to fit a typical status line.
pub const HELP_TEXT: &str = ":q quit  :view <today|inbox|stream|search|focus>  :help";

/// A side effect the runtime (`main`) must perform after a command is applied.
///
/// Kept separate from [`ViewState`] mutation so `apply_command` stays pure and
/// unit-testable without a running terminal or `Core`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEffect {
    /// Tear down and exit the application.
    Quit,
}

/// Apply a parsed [`Cmd`] to `state`, returning an [`AppEffect`] the runtime
/// must act on (currently only [`AppEffect::Quit`]).
///
/// Pure over `state`: view switches and status-line messages are written here;
/// I/O (task reloads, terminal teardown) is left to `main`.
#[must_use]
pub fn apply_command(cmd: Cmd, state: &mut ViewState) -> Option<AppEffect> {
    match cmd {
        Cmd::Quit => Some(AppEffect::Quit),
        Cmd::SwitchView(view) => {
            state.view = view;
            state.status.clear();
            None
        }
        Cmd::ShowHelp => {
            state.status = HELP_TEXT.to_string();
            None
        }
        Cmd::Error(msg) => {
            state.status = format!("error: {msg}");
            None
        }
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;

    #[test]
    fn quit_returns_quit_effect() {
        let mut state = ViewState::default();
        assert_eq!(apply_command(Cmd::Quit, &mut state), Some(AppEffect::Quit));
    }

    #[test]
    fn switch_view_mutates_state_no_effect() {
        let mut state = ViewState::default();
        assert_eq!(state.view, View::Today);
        assert_eq!(
            apply_command(Cmd::SwitchView(View::Inbox), &mut state),
            None
        );
        assert_eq!(state.view, View::Inbox);
    }

    #[test]
    fn help_sets_status() {
        let mut state = ViewState::default();
        assert_eq!(apply_command(Cmd::ShowHelp, &mut state), None);
        assert_eq!(state.status, HELP_TEXT);
    }

    #[test]
    fn error_sets_status() {
        let mut state = ViewState::default();
        assert_eq!(
            apply_command(Cmd::Error("unknown command: foo".into()), &mut state),
            None
        );
        assert!(state.status.contains("foo"));
    }

    #[test]
    fn parse_then_apply_view_switch() {
        let mut state = ViewState::default();
        let effect = apply_command(parse_command(":view focus"), &mut state);
        assert_eq!(effect, None);
        assert_eq!(state.view, View::Focus);
    }
}
