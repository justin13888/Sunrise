//! Sunrise core public API.
//!
//! Implements `spec/01-architecture/shared-core.md`. The [`Core`] type is the
//! single entry point UIs use: `open` the vault, `submit` commands, run
//! `query`s, observe `changes`/`sync_status`. `close` runs the graceful
//! shutdown.
//!
//! Determinism rules (per spec):
//! 1. No clock access except via `CoreConfig::clock`.
//! 2. No filesystem access except via `CoreConfig::storage` (well, the
//!    storage layer; this crate doesn't touch the FS directly).
//! 3. No randomness except via `CoreConfig::rng`.
//! 4. No threads spawned except by the core's tokio runtime.
//!
//! Single-writer guarantee: exactly one [`Core`] per vault path per
//! process. The vault lock file (`core.lock`) is acquired with `fcntl`
//! (Unix) or `LockFileEx` (Windows) — see [`vault_lock`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The Core API is async by design (FFI seam expects async); some current
// stub implementations don't yet await internally because the engine
// that drives storage / sync hooks in via Phase 11. Lint relaxations
// here are scoped to the in-flight stub state and tighten in Phase 17.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::unused_async,
    clippy::map_unwrap_or,
    clippy::manual_let_else,
    clippy::disallowed_methods
)]

pub mod commands;
pub mod config;
pub mod core;
pub mod events;
pub mod queries;
pub mod unlock;
pub mod vault_lock;

pub use commands::{Command, CommandResult};
pub use config::{Clock, CoreConfig, Rng, SystemClock, SystemRng};
pub use core::{Core, CoreError};
pub use events::{DomainEvent, SyncStatus};
pub use queries::{Query, QueryResult};
pub use unlock::Unlock;
pub use vault_lock::{VaultLock, VaultLockError};
