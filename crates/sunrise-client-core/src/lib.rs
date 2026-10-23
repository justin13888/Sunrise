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
//! * [`history`] — a session's undo and redo stacks, recorded and replayed
//!   against a live vault.
//! * [`views`] — named saved views, stored per device as a preference, and
//!   the [`views::SavedViewStore`] that reads and writes them.
//! * [`preferences`] — the bootstrap preferences file a device reads before
//!   it has a vault, written so a crash leaves one whole file or the other.
//! * [`gate`] — what this build may edit in a vault (ADR-0045 §8).
//! * [`config`] — the tiny TOML subset both preference files are written in.
//! * `recovery` (feature `relay`) — `docs/03-crypto/recovery.md` §Recovery
//!   flow steps 3 to 8, from the typed code to a vault that has read its
//!   history, over the REST client in `sunrise-relay-client`.
//!
//! Everything here is pure over its inputs. Nothing reads a clock, opens a
//! socket, or touches a `Core`; [`views::load`] is the only function that
//! touches the filesystem, and it takes the path.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::module_name_repetitions)]

pub mod config;
pub mod gate;
pub mod history;
pub mod preferences;
#[cfg(feature = "relay")]
pub mod recovery;
pub mod undo;
pub mod views;

pub use config::parse_pairs;
pub use undo::{
    invert, invert_create, is_create, rebind_creates, EntityLookup, NotUndoable, UndoEntry,
    MAX_DEPTH,
};
pub use views::{SavedView, View};
