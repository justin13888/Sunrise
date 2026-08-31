//! Client-side logic that is not any one client's.
//!
//! Two things sit between `sunrise-core` and a user interface: they are not
//! domain rules (the vault has no entity for either), and they are not
//! rendering (no pixels, no keys, no terminal). Putting them in a client makes
//! the next client reimplement them, and the two implementations then disagree
//! about what `u` does or what a saved view means.
//!
//! * [`undo`] — undo/redo built from **inverse commands**, read out of the
//!   rows the client is already holding.
//! * [`views`] — named saved views, stored per device as a preference.
//! * [`config`] — the tiny TOML subset both preference files are written in.
//!
//! Everything here is pure over its inputs. Nothing reads a clock, opens a
//! socket, or touches a `Core`; [`views::load`] is the only function that
//! touches the filesystem, and it takes the path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::module_name_repetitions)]

pub mod config;
pub mod undo;
pub mod views;

pub use config::parse_pairs;
pub use undo::{
    invert, invert_create, is_create, rebind_creates, EntityLookup, NotUndoable, UndoEntry,
    MAX_DEPTH,
};
pub use views::{SavedView, View};
