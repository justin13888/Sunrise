//! SQLite + SQLCipher local storage layer.
//!
//! Implements `docs/04-storage/`. Each device's vault opens exactly one
//! `Db` instance; the in-process advisory lock is owned by `sunrise-core`
//! (Phase 10).
//!
//! Storage is per-device and per-account. Encryption at rest is provided
//! by SQLCipher; the key is derived from the vault root via
//! `BLAKE3.derive_key("sunrise.sqlcipher_key.v1", vault_root)`.
//!
//! v1 ships a single migration (`migrations/0001_init.sql`); future
//! `STORAGE_V` bumps add new files and never edit prior ones.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

pub mod blob_store;
pub mod db;
pub mod migrations;
pub mod oplog;
pub mod sync_local;

pub use blob_store::{BlobStore, BlobStoreError};
pub use db::{Db, DbError};
pub use migrations::{current_storage_v, MIGRATIONS};
pub use oplog::{OpLog, OpLogError};
pub use sync_local::{Outbox, OutboxEntry, SyncCursors, SyncLocalError};

/// Re-export of `STORAGE_V` from `sunrise-cbor` for callers that don't
/// otherwise depend on it.
pub use sunrise_cbor::version::STORAGE_V;
