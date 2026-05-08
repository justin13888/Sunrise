//! Sunrise terminal UI library surface.
//!
//! Implements `spec/07-clients/tui.md` foundation. v1 ships:
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
    clippy::module_name_repetitions
)]

pub mod render;
pub mod view;

pub use render::render_today;
pub use view::{View, ViewState};
