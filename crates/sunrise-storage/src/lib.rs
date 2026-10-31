//! SQLite + SQLCipher local storage layer.
//!
//! Implements `docs/04-storage/`. Each device's vault opens exactly one
//! `Db` instance; the vault advisory lock is owned by `sunrise-core`'s
//! `vault_lock` module.
//!
//! Storage is per-device and per-account. Encryption at rest is provided
//! by SQLCipher; the key is derived from the vault root through
//! `sunrise_crypto::derive_key` under the context `sunrise.sqlcipher_key.v1`.
//!
//! The schema is a migration chain under `migrations/`, listed in
//! [`migrations::MIGRATIONS`] and applied in id order. It starts at the
//! `0013_baseline.sql` schema reset (ADR-0018); each `STORAGE_V` bump appends
//! a new file and never edits a prior one.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

pub mod backup;
pub mod blob_store;
pub mod db;
pub mod migrations;
pub mod oplog;
pub mod sync_local;

/// The committed old-vault fixtures and the migration chain run over them.
///
/// Test-only. It is a module inside the crate rather than a file under
/// `tests/` because generating a genuine old vault needs `Db`'s private
/// keying, and a fixture built by re-deriving that key in an integration test
/// would prove the wrong thing.
#[cfg(test)]
mod vault_fixtures;

pub use backup::{BackupKind, VaultBackup};
pub use blob_store::{BlobStore, BlobStoreError};
pub use db::{ensure_cipher_ready, Db, DbError};
pub use migrations::{current_storage_v, MIGRATIONS};
pub use oplog::{OpLog, OpLogError, ParkReason, ParkedCount, ParkedEnvelope, Parking, PARKED_KIND};
pub use sync_local::{Outbox, OutboxEntry, SyncCursors, SyncLocalError};

/// Re-export of `STORAGE_V` from `sunrise-cbor` for callers that don't
/// otherwise depend on it.
pub use sunrise_cbor::version::STORAGE_V;
