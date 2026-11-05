//! SQLite/SQLCipher database wrapper.
//!
//! Per `docs/04-storage/local-database.md`:
//!
//! ```sql
//! PRAGMA auto_vacuum    = INCREMENTAL;  -- first: see `apply_pragmas`
//! PRAGMA journal_mode   = WAL;
//! PRAGMA synchronous    = NORMAL;
//! PRAGMA foreign_keys   = ON;
//! PRAGMA busy_timeout   = 5000;
//! ```
//!
//! All multi-row writes use `BEGIN IMMEDIATE … COMMIT`.
//!
//! The SQLCipher key is derived deterministically from the vault root:
//! `BLAKE3.derive_key("sunrise.sqlcipher_key.v1", vault_root) → 32 bytes`,
//! passed to SQLCipher as a 64-char hex string with `PRAGMA kdf_iter = 1`
//! (we already pre-derive, so SQLCipher's own KDF doesn't need iterations).
//!
//! This module holds the type, its errors and its opens. Keying is in
//! `cipher`, the pragmas in `pragma`, the integrity check in `integrity` and
//! the migration runner in `schema`; the copies beside a vault are
//! [`crate::backup`]'s.

mod cipher;
mod integrity;
mod pragma;
mod schema;
#[cfg(test)]
mod tests;

pub use cipher::ensure_cipher_ready;
use schema::Schema;

use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_error::ErrorCode;
use thiserror::Error;

/// Storage layer errors.
#[derive(Debug, Error)]
pub enum DbError {
    /// SQLite/rusqlite error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Schema version mismatch (DB newer than this binary supports).
    #[error("STORAGE_V too new: db is {db_v}, binary supports up to {binary_v}")]
    StorageVTooNew {
        /// Version found in the DB.
        db_v: u32,
        /// Latest version this binary knows.
        binary_v: u32,
    },
    /// Schema version mismatch (DB older; needs upgrade).
    ///
    /// Retained for wire-code stability ([`ErrorCode::StorageVTooOld`]) but no
    /// longer produced: [`Db::open`] auto-applies pending migrations instead of
    /// erroring on an older DB.
    #[error("STORAGE_V too old: db is {db_v}, binary expects {binary_v}")]
    StorageVTooOld {
        /// Version found in the DB.
        db_v: u32,
        /// Version the binary needs.
        binary_v: u32,
    },
    /// The vault predates the storage baseline reset (ADR-0018).
    ///
    /// Distinct from [`DbError::StorageVTooOld`] because it is not a
    /// "run the migrations" condition: the migrations that would upgrade such
    /// a vault were deleted, so there is nothing to run. It is terminal, and
    /// the only remedy is a fresh vault.
    #[error(
        "vault predates the storage baseline: db is storage_v {db_v}, \
         this build's baseline is {baseline_v}; older vaults are not upgraded"
    )]
    StorageVPreBaseline {
        /// Version found in the DB.
        db_v: u32,
        /// [`crate::migrations::BASELINE_STORAGE_V`].
        baseline_v: u32,
    },
    /// Migration failed.
    #[error("migration {id} ({name}) failed: {source}")]
    Migration {
        /// Migration id.
        id: u32,
        /// Migration name.
        name: &'static str,
        /// Underlying error.
        source: rusqlite::Error,
    },
    /// `PRAGMA quick_check` found the database file damaged.
    ///
    /// Distinct from [`DbError::Sqlite`] because the file *was* read: the key
    /// is right and SQLite answered, and what it answered is that pages or
    /// indexes are inconsistent. A check that runs before a migration and
    /// fails stops the migration from starting, so the vault is left at the
    /// version it was found at
    /// (`docs/04-storage/migrations.md` §Migration rigor rule 3).
    #[error("vault integrity check failed: {detail}")]
    IntegrityCheckFailed {
        /// The first lines `quick_check` reported. SQLite's own wording about
        /// pages and indexes; it names no row content.
        detail: String,
    },
    /// The copy taken before a migration could not be written, so nothing
    /// was migrated.
    ///
    /// "No backup, no migration": the vault is left untouched at `from_v`
    /// (`docs/04-storage/migrations.md` §Migration rigor rule 2).
    #[error("could not back up the vault before migrating it from storage_v {from_v}: {source}")]
    Backup {
        /// The version the vault was found at, and stays at.
        from_v: u32,
        /// Why the copy failed: the disk filled, the directory is read-only,
        /// or SQLite refused the `VACUUM INTO`.
        source: std::io::Error,
    },
    /// The copy taken before a projection rebuild could not be written, so
    /// nothing was rebuilt.
    ///
    /// Rule 2's "no backup, no migration", applied to the rebuild that rule 4
    /// runs under it (`docs/04-storage/migrations.md` §Migration rigor): the
    /// vault is left as it was. See [`Db::back_up_for_rebuild`].
    #[error("could not back up the vault before rebuilding it: {source}")]
    RebuildBackup {
        /// Why the copy failed.
        source: std::io::Error,
    },
    /// A copy beside the vault could not be put back in its place, or the
    /// vault could not be moved aside.
    ///
    /// Whatever was moved before the failure stays where it was moved to, and
    /// nothing is deleted: the vault, the copy and anything set aside are all
    /// still on disk under the names [`Db::restore_backup`] gave them.
    #[error("could not restore the vault from {backup}: {source}")]
    Restore {
        /// The copy being restored, or the directory being moved aside.
        backup: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// SQLCipher never finished initialising in this process, so no
    /// database can be keyed.
    ///
    /// Distinct from [`DbError::Sqlite`] because SQLCipher reports it as a
    /// `PRAGMA key` failure whose message ("PRAGMA key requires a key of one
    /// or more characters") blames the key, and the key is not at fault
    /// (issue #521). It is terminal for the process: SQLite does not run
    /// SQLCipher's initialisation a second time.
    #[error("SQLCipher could not be initialised, so no vault can be keyed: {detail}")]
    CipherUnavailable {
        /// The last error keying a throwaway in-memory database returned.
        detail: String,
    },
}

impl DbError {
    /// Map to a canonical [`ErrorCode`] for the wire.
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        match self {
            Self::StorageVTooNew { .. } => ErrorCode::StorageVTooNew,
            Self::StorageVTooOld { .. } | Self::StorageVPreBaseline { .. } => {
                ErrorCode::StorageVTooOld
            }
            Self::IntegrityCheckFailed { .. } => ErrorCode::StorageIntegrityCheckFailed,
            Self::Backup { .. } | Self::RebuildBackup { .. } => ErrorCode::StorageBackupFailed,
            Self::Migration { .. } => ErrorCode::StorageMigrationFailed,
            _ => ErrorCode::FatalInternal,
        }
    }
}

/// Open / managed SQLCipher connection.
pub struct Db {
    pub(crate) conn: Connection,
    /// The file behind `conn`, or `None` for an in-memory vault.
    pub(crate) path: Option<PathBuf>,
    /// The copy of the vault this open took before it changed anything: the
    /// pre-migration copy, or the pre-repair one. The first rebuild after the
    /// open runs under it rather than taking a second, and takes it
    /// ([`Self::back_up_for_rebuild`]).
    pub(crate) taken_backup: Option<PathBuf>,
    /// This open created the file: there is nothing in it a copy would keep.
    pub(crate) created: bool,
}

impl core::fmt::Debug for Db {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Open (or create) the vault DB at `path` keyed by `vault_root`.
    ///
    /// In order:
    ///
    /// 1. `PRAGMA quick_check`. A damaged file is refused with
    ///    [`DbError::IntegrityCheckFailed`] before anything writes to it.
    /// 2. A fresh file gets every migration. A current one gets none, and
    ///    any pre-migration copy beside it is deleted: this open is the
    ///    second one at the new version, which is what proves the migrated
    ///    vault usable. An older one is first copied to
    ///    [`Self::backup_path`] and then migrated, and `quick_check` runs again
    ///    after the batch commits. A copy that cannot be written stops the
    ///    open with [`DbError::Backup`], and the vault stays at its version.
    /// 3. A newer or pre-baseline stamp is refused with its own error.
    /// 4. A file that does not report `auto_vacuum = INCREMENTAL`, which is
    ///    every vault created before issue #461, is rewritten once by a
    ///    `VACUUM` so that it does.
    pub fn open(path: &Path, vault_root: &VaultRootKey) -> Result<Self, DbError> {
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::apply_sqlcipher_key(&conn, vault_root)?;
        Self::apply_pragmas(&conn)?;
        Self::quick_check(&conn, true)?;
        let schema = Self::ensure_schema(&mut conn, Some(path), false)?;
        Self::ensure_incremental_auto_vacuum(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            created: matches!(schema, Schema::Created),
            taken_backup: schema.taken_backup(),
        })
    }

    /// Open an in-memory DB (used by tests and for non-persisted vaults).
    ///
    /// Checked like [`Self::open`], and never backed up: there is no file to
    /// copy, and nothing outlives the connection.
    pub fn open_memory(vault_root: &VaultRootKey) -> Result<Self, DbError> {
        let mut conn = Connection::open_in_memory()?;
        Self::apply_sqlcipher_key(&conn, vault_root)?;
        Self::apply_pragmas(&conn)?;
        Self::quick_check(&conn, false)?;
        Self::ensure_schema(&mut conn, None, false)?;
        Ok(Self {
            conn,
            path: None,
            taken_backup: None,
            created: true,
        })
    }

    /// Key `path` and apply the pragmas, but run no migration and check no
    /// version.
    ///
    /// Test-only. It is how `crate::vault_fixtures` reads a committed old
    /// vault's stamp without upgrading the file it is asserting about —
    /// `Db::open` would migrate it first, and the question is what it held
    /// before that.
    #[cfg(test)]
    pub(crate) fn open_unmigrated(path: &Path, vault_root: &VaultRootKey) -> Result<Self, DbError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::apply_sqlcipher_key(&conn, vault_root)?;
        Self::apply_pragmas(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            taken_backup: None,
            created: false,
        })
    }

    /// Borrow the underlying `rusqlite::Connection`.
    #[must_use]
    pub const fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutable borrow of the connection (for transactions).
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Execute `f` inside a `BEGIN IMMEDIATE` transaction.
    ///
    /// The closure receives a `rusqlite::Transaction`; returning `Ok(_)`
    /// commits, returning `Err(_)` rolls back.
    pub fn with_tx<R, F>(&mut self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<R>,
    {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let r = f(&tx)?;
        tx.commit()?;
        Ok(r)
    }
}
