//! Sunrise terminal UI library surface.
//!
//! Implements `docs/07-clients/tui.md` foundation. v1 ships:
//!
//! - Today / Inbox / Browse / Search / Focus / Routines view enum, with the
//!   Browse sidebar covering both domain axes (Streams and Contexts) and full
//!   CRUD over each.
//! - Routine CRUD, with plain-English recurrence ([`recur`]).
//! - The Review view: the five-step weekly review, the daily glance, the
//!   twelve-week trends, and the saved-snapshot history.
//! - Vim-style modal navigation (default-on per the parity matrix), with the
//!   keymap and the `?` help overlay derived from one binding table.
//! - Focus sessions ([`view::FocusState`]): the ranked planner queue, a live
//!   timer **derived** from the injected clock rather than accumulated,
//!   one-tap interruption capture, capture-aside into the Inbox, the pomodoro
//!   break cadence, the unblock cascade, and `:focus stats` calibration.
//! - A pure Action → `sunrise_core::Command` reducer ([`runtime::apply_action`])
//!   so every keybinding is unit-testable without a terminal or a `Core`.
//! - Capture through the shared domain parser, with a live inline preview
//!   ([`capture`]) — `#stream @context ^when !priority ~duration`.
//! - Visual (multi-select) and Inbox triage modes, both driven by the same
//!   reducer, so bulk operations are one testable command list.
//! - An **annotate** prompt ([`edit`]) that reaches every remaining
//!   `TaskPatch` facet — priority, energy, estimate, due date, contexts —
//!   with the capture sigils the user already knows, in bulk.
//! - A real single-line editor behind every prompt ([`input::InputLine`]):
//!   caret motions, word motions, the readline chords, and bracketed paste.
//! - User key remapping from `~/.config/sunrise/keys.toml` ([`keymap::Keymap`]).
//! - Note-body editing in `$EDITOR` ([`editor`]).
//! - Snapshot-testable Ratatui renderers for every primary view, behind an
//!   80x24 minimum-size guard.
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

pub mod capture;
pub mod command;
pub mod edit;
pub mod editor;
#[cfg(feature = "images")]
pub mod images;
pub mod input;
pub mod keymap;
pub mod livesync;
pub mod recur;
pub mod render;
pub mod runtime;
pub mod view;

pub use capture::{parse_line, preview_line, unresolved_note};
pub use command::{parse_command, Cmd, FocusCmd};
pub use edit::{parse_edit, EditError, TaskEdit};
pub use editor::{edit_bytes, resolve_editor, EditorExit};
pub use input::InputLine;
pub use keymap::{
    dispatch, help_sections, load_keymap, Action, Binding, Keymap, Mode, Scope, BINDINGS,
};
pub use recur::parse_recurrence;
pub use render::{
    fits, render, render_focus, render_inbox, render_review, render_routines, render_search,
    render_stream, render_today, review_rows, viewport_rows, MIN_HEIGHT, MIN_WIDTH,
};
pub use runtime::{apply_action, parse_defer_ms, Outcome};
pub use view::{
    energy_budget_label, fmt_duration_ms, length_label, routine_rows, rrule_summary, segment_label,
    BrowseTarget, CascadeReport, DeleteTarget, FocusState, Prompt, ReviewPane, ReviewState,
    RoutineRow, SidebarRow, StreamPane, StreamPicker, SyncIndicator, View, ViewState,
};

/// Help text listing the command-line commands, shown in the status line by
/// `:help`. Kept short enough to fit a typical status line.
pub const HELP_TEXT: &str = ":q  :view <v>  :capture <text>  :open <id>  :devices  :focus stats";

/// A side effect the runtime (`main`) must perform after a command is applied.
///
/// Kept separate from [`ViewState`] mutation so `apply_command` stays pure and
/// unit-testable without a running terminal or `Core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEffect {
    /// Tear down and exit the application.
    Quit,
    /// Load an image file into the Focus-view preview pane (`:preview`).
    /// Only emitted when the `images` cargo feature is enabled.
    Preview(std::path::PathBuf),
    /// Parse and commit one capture line (`:capture <text>`). The reducer runs
    /// it through the same parser as the `c` prompt and turns it into a
    /// `Command::CreateTask`.
    Capture(String),
    /// Jump to a task by id (`:open <tsk_…>`); the runtime resolves it with
    /// `Query::EntityById` and opens the Focus view on it.
    Open(sunrise_id::EntityRef),
    /// List the paired devices (`:devices`); the runtime runs
    /// `Query::DeviceList` and hands the rows to [`ViewState::show_devices`].
    Devices,
    /// Run `Query::FocusStats` and show the folded totals + calibration
    /// factor (`:focus stats`).
    FocusStats,
}

/// Apply a parsed [`Cmd`] to `state`, returning an [`AppEffect`] the runtime
/// must act on ([`AppEffect::Quit`], [`AppEffect::Preview`]).
///
/// Pure over `state`: view switches and status-line messages are written here;
/// I/O (task reloads, image decoding, terminal teardown) is left to `main`.
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
        Cmd::Preview(path) => {
            if state.view != View::Focus {
                state.status = "preview only in Focus view".into();
                None
            } else if cfg!(feature = "images") {
                Some(AppEffect::Preview(path))
            } else {
                state.status = "images feature disabled".into();
                None
            }
        }
        Cmd::Capture(text) => Some(AppEffect::Capture(text)),
        Cmd::Open(id) => Some(AppEffect::Open(id)),
        Cmd::Devices => Some(AppEffect::Devices),
        Cmd::Focus(focus) => apply_focus_command(focus, state),
        Cmd::Error(msg) => {
            state.status = format!("error: {msg}");
            None
        }
    }
}

/// Apply a `:focus` sub-command.
///
/// Everything except `stats` is a pure state change the next refresh reads:
/// re-running `Query::FocusPlan` with a new energy budget or session length is
/// what makes the queue re-rank, so the reducer only has to say "refresh".
fn apply_focus_command(cmd: FocusCmd, state: &mut ViewState) -> Option<AppEffect> {
    match cmd {
        FocusCmd::Stats => Some(AppEffect::FocusStats),
        FocusCmd::Plan => {
            if state.view != View::Focus {
                state.prev_view = Some(state.view);
                state.view = View::Focus;
            }
            state.status = "focus planner — Enter or F starts a session".into();
            None
        }
        FocusCmd::Energy(e) => {
            state.focus.energy = e;
            state.status = format!("focus energy budget: {}", view::energy_budget_label(e));
            None
        }
        FocusCmd::Length(l) => {
            state.focus.length = l;
            state.status = format!("focus session length: {}", view::length_label(l));
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
    fn focus_stats_is_an_effect_the_runtime_performs() {
        let mut state = ViewState::default();
        assert_eq!(
            apply_command(parse_command(":focus stats"), &mut state),
            Some(AppEffect::FocusStats)
        );
    }

    #[test]
    fn focus_plan_opens_the_planner_and_remembers_where_to_go_back_to() {
        let mut state = ViewState::default();
        state.view = View::Inbox;
        assert_eq!(
            apply_command(parse_command(":focus plan"), &mut state),
            None
        );
        assert_eq!(state.view, View::Focus);
        assert_eq!(state.prev_view, Some(View::Inbox));
    }

    #[test]
    fn focus_energy_and_length_are_pure_state_the_next_query_reads() {
        let mut state = ViewState::default();
        assert_eq!(
            apply_command(parse_command(":focus energy high"), &mut state),
            None
        );
        assert_eq!(state.focus.energy, Some(sunrise_domain::Energy::High));
        assert!(state.status.contains("high"));
        assert_eq!(
            apply_command(parse_command(":focus length until-done"), &mut state),
            None
        );
        assert_eq!(state.focus.length, sunrise_domain::SessionLength::UntilDone);
        assert!(state.status.contains("until done"));
    }

    #[test]
    fn the_command_help_line_advertises_focus_stats() {
        // Stats are only "reachable" if something says how to reach them.
        assert!(HELP_TEXT.contains(":focus stats"));
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

    #[test]
    fn preview_outside_focus_is_status_error() {
        let mut state = ViewState::default();
        state.view = View::Inbox;
        let effect = apply_command(parse_command(":preview /tmp/x.png"), &mut state);
        assert_eq!(effect, None);
        assert_eq!(state.status, "preview only in Focus view");
    }

    #[cfg(feature = "images")]
    #[test]
    fn preview_in_focus_returns_preview_effect() {
        let mut state = ViewState::default();
        state.view = View::Focus;
        let effect = apply_command(parse_command(":preview /tmp/x.png"), &mut state);
        assert_eq!(
            effect,
            Some(AppEffect::Preview(std::path::PathBuf::from("/tmp/x.png")))
        );
    }

    #[cfg(not(feature = "images"))]
    #[test]
    fn preview_in_focus_without_feature_reports_disabled() {
        let mut state = ViewState::default();
        state.view = View::Focus;
        let effect = apply_command(parse_command(":preview /tmp/x.png"), &mut state);
        assert_eq!(effect, None);
        assert_eq!(state.status, "images feature disabled");
    }
}
