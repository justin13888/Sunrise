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

pub mod keymap;
pub mod render;
pub mod view;

pub use keymap::{dispatch, Action, Mode};
pub use render::{render, render_focus, render_inbox, render_search, render_stream, render_today};
pub use view::{View, ViewState};
