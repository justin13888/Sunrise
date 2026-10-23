//! The client layer: everything between `sunrise-core` and a user interface
//! that is not any one client's.
//!
//! None of it is a domain rule (the vault has no entity for an undo step or a
//! saved view), and none of it is rendering (no pixels, no keys, no terminal).
//! Put in a client, it gets reimplemented by the next one, and the two then
//! disagree about what undo does, what a saved view means, or when a recovery
//! has finished. So every client links this crate — the `UniFFI` seam the
//! Apple apps call (`sunrise-core-bindings`), the CLI, and the web core
//! (`sunrise-core-wasm`, without `relay`) — and a seam's own code is the
//! lowering of its records onto these functions and nothing else
//! (`docs/01-architecture/shared-core.md` §The client layer).
//!
//! * [`undo`] — undo/redo built from **inverse commands**, read out of the
//!   rows the client is already holding.
//! * [`history`] — a session's undo and redo stacks, recorded and replayed
//!   against a live vault.
//! * [`views`] — named saved views, stored per device as a preference, and
//!   the [`views::SavedViewStore`] that reads and writes them.
//! * [`preferences`] — the bootstrap preferences file a device reads before
//!   it has a vault, written so a crash leaves one whole file or the other.
//! * [`ids`] — lowercase hex for the ids that have no `EntityRef` prefix.
//! * [`host`] — what a host hands the core at open: an injected clock, and a
//!   vault root checked for its width.
//! * [`gate`] — what this build may edit in a vault (ADR-0045 §8).
//! * [`vocab`] — the shared vocabulary read in the caller's zone: Today's
//!   sections, relative days, snoozes, block conflicts, session timers,
//!   routine occurrence keys.
//! * [`reminders`] — the core's reminder intents as an OS schedule: the cap,
//!   the order, the body text, identifiers stable across re-plans, the
//!   reconcile against what the OS holds, and the notification preferences
//!   read off the core's `Preferences`.
//! * [`focus`] — what an OS Focus filter lets through, for Today and for the
//!   reminder schedule alike.
//! * [`brief`] — the morning and evening briefs' sections and headlines.
//! * [`review`] — the daily glance's civil window, the review snapshot, and
//!   an export's file name.
//! * [`civil`] — a picked moment turned into a time value of the kind the
//!   user meant, in a zone the caller names, with no silent fallback.
//! * [`editor`] — the task, block and routine editors' forms, turned into
//!   patches that carry only what changed and keep every time's kind.
//! * [`schedule`] — defer, snooze, schedule and the Today capture default,
//!   in civil days and in the kind the plan already has.
//! * [`calendar`] — the grid's days and week start, block layout, snapping,
//!   drags, and the Resolve menu's merge.
//! * [`relay_url`] — the one rule for what a relay URL is, shared with the
//!   web client through `vectors/relay-url.json`.
//! * [`sync_status`] — the sync badge and the sync plan, as codes.
//! * `account` (feature `relay`) — publishing a vault to the relay: seal a
//!   recovery blob, create or adopt the account, register this device.
//! * `sync` (feature `relay`) — the SSE transport factory every client dials
//!   with, the bounded background `sync_once`, and the wait for an outbox or
//!   a replay to catch up.
//! * `recovery` (feature `relay`) — `docs/03-crypto/recovery.md` §Recovery
//!   flow steps 3 to 8, from the typed code to a vault that has read its
//!   history, over the REST client in `sunrise-relay-client`.
//!
//! # Determinism
//!
//! Nothing here reads the host's clock or zone. Time is the core's injected
//! clock ([`sunrise_core::Core::now_ms`]) or a zone name the caller passes;
//! randomness is an injected [`sunrise_core::Rng`]; a file is a path the
//! caller names. The workspace's `disallowed-methods` rules hold here as they
//! do in the core (`docs/01-architecture/shared-core.md`), and every module is
//! unit-tested without a seam.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::module_name_repetitions)]

#[cfg(feature = "relay")]
pub mod account;
pub mod brief;
pub mod calendar;
pub mod civil;
pub mod editor;
pub mod focus;
pub mod gate;
pub mod history;
pub mod host;
pub mod ids;
pub mod preferences;
#[cfg(feature = "relay")]
pub mod recovery;
pub mod relay_url;
pub mod reminders;
pub mod review;
pub mod schedule;
#[cfg(feature = "relay")]
pub mod sync;
pub mod sync_status;
#[cfg(test)]
mod test_support;
pub mod undo;
pub mod views;
pub mod vocab;

pub use undo::{
    invert, invert_create, is_create, rebind_creates, EntityLookup, NotUndoable, UndoEntry,
    MAX_DEPTH,
};
pub use views::{SavedView, View};
