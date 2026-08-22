//! Sunrise core public API.
//!
//! Implements `docs/01-architecture/shared-core.md`. The [`Core`] type is the
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
// The Core API is async by design (the FFI seam expects async); a few methods
// don't await internally yet, hence `unused_async`. The remaining entries are
// style-only.
//
// `clippy::disallowed_methods` is deliberately NOT allowed here. It is the
// determinism gate from docs/01-architecture/shared-core.md, and this is the
// crate that gate exists to protect — a blanket allow disabled it in exactly
// the wrong place. The two legitimate clock call sites carry their own
// narrowly-scoped `#[allow]` with a justification.
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::unused_async,
    clippy::map_unwrap_or,
    clippy::manual_let_else
)]

pub mod commands;
pub mod config;
pub mod core;
pub mod engine;
pub mod events;
pub mod inner_op;
pub mod keychain;
pub mod queries;
pub mod sync_driver;
pub mod unlock;
pub mod vault_lock;

pub use commands::{Command, CommandResult};
pub use config::{Clock, CoreConfig, Rng, SystemClock, SystemRng};
pub use core::{Core, CoreError};
pub use engine::{Engine, EngineError};
pub use events::{DomainEvent, SyncStatus};
pub use keychain::{Keychain, KeychainError};
pub use queries::{Query, QueryResult};
pub use sync_driver::{BoxTransport, ConnectFuture, SyncConfig, TransportFactory};
pub use unlock::Unlock;
pub use vault_lock::{VaultLock, VaultLockError};
