//! SQLite/SQLCipher database wrapper.
//!
//! Per `docs/04-storage/local-database.md`:
//!
//! ```sql
//! PRAGMA journal_mode   = WAL;
//! PRAGMA synchronous    = NORMAL;
//! PRAGMA foreign_keys   = ON;
//! PRAGMA busy_timeout   = 5000;
//! PRAGMA auto_vacuum    = INCREMENTAL;
//! ```
//!
//! All multi-row writes use `BEGIN IMMEDIATE … COMMIT`.
//!
//! The SQLCipher key is derived deterministically from the vault root:
//! `BLAKE3.derive_key("sunrise.sqlcipher_key.v1", vault_root) → 32 bytes`,
//! passed to SQLCipher as a 64-char hex string with `PRAGMA kdf_iter = 1`
//! (we already pre-derive, so SQLCipher's own KDF doesn't need iterations).

use crate::migrations::MIGRATIONS;
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use sunrise_cbor::version::STORAGE_V;
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
    /// longer produced by [`Db::ensure_schema`], which now auto-applies pending
    /// migrations instead of erroring on an older DB.
    #[error("STORAGE_V too old: db is {db_v}, binary expects {binary_v}")]
    StorageVTooOld {
        /// Version found in the DB.
        db_v: u32,
        /// Version the binary needs.
        binary_v: u32,
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
}

impl DbError {
    /// Map to a canonical [`ErrorCode`] for the wire.
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        match self {
            Self::StorageVTooNew { .. } => ErrorCode::StorageVTooNew,
            Self::StorageVTooOld { .. } => ErrorCode::StorageVTooOld,
            _ => ErrorCode::FatalInternal,
        }
    }
}

/// Open / managed SQLCipher connection.
pub struct Db {
    conn: Connection,
}

impl core::fmt::Debug for Db {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Open (or create) the vault DB at `path` keyed by `vault_root`.
    ///
    /// The first time this is called for a fresh vault, all migrations are
    /// applied. On subsequent opens, the schema version is read and either
    /// the DB is current (returns `Ok`) or the version is mismatched and the
    /// caller must surface a clear error.
    pub fn open(path: &Path, vault_root: &VaultRootKey) -> Result<Self, DbError> {
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::apply_sqlcipher_key(&conn, vault_root)?;
        Self::apply_pragmas(&conn)?;
        Self::ensure_schema(&mut conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory DB (used by tests and for non-persisted vaults).
    pub fn open_memory(vault_root: &VaultRootKey) -> Result<Self, DbError> {
        let mut conn = Connection::open_in_memory()?;
        Self::apply_sqlcipher_key(&conn, vault_root)?;
        Self::apply_pragmas(&conn)?;
        Self::ensure_schema(&mut conn)?;
        Ok(Self { conn })
    }

    fn apply_sqlcipher_key(conn: &Connection, vault_root: &VaultRootKey) -> Result<(), DbError> {
        // Derive a 32-byte SQLCipher raw key from vault_root.
        let raw = sunrise_crypto::derive_key("sunrise.sqlcipher_key.v1", vault_root.as_bytes(), 32);
        let mut hex_buf = String::with_capacity(64);
        for b in &raw {
            use core::fmt::Write;
            let _ = write!(hex_buf, "{b:02x}");
        }
        // SQLCipher's `PRAGMA key = "x'<64 hex>'"` accepts a raw key.
        let key_pragma = format!("PRAGMA key = \"x'{hex_buf}'\"");
        conn.execute_batch(&key_pragma)?;
        // Pre-derived: skip SQLCipher's own KDF iterations.
        conn.execute_batch("PRAGMA kdf_iter = 1;")?;
        Ok(())
    }

    fn apply_pragmas(conn: &Connection) -> Result<(), DbError> {
        // SQLCipher requires the key BEFORE any other pragma; the caller
        // ensures that by calling apply_sqlcipher_key first.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             PRAGMA auto_vacuum = INCREMENTAL;",
        )?;
        Ok(())
    }

    fn ensure_schema(conn: &mut Connection) -> Result<(), DbError> {
        // Detect existing schema by probing for `schema_meta`.
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let binary_v: u32 = u32::from(STORAGE_V);
        if exists == 0 {
            // Fresh DB: apply all migrations in a single transaction.
            let tx = conn.transaction()?;
            for m in MIGRATIONS {
                tx.execute_batch(m.sql)
                    .map_err(|source| DbError::Migration {
                        id: m.id,
                        name: m.name,
                        source,
                    })?;
            }
            // Pin the storage version.
            tx.execute(
                "UPDATE schema_meta SET storage_v = ?, applied_at_ms = ?",
                rusqlite::params![binary_v, 0],
            )?;
            tx.commit()?;
            return Ok(());
        }
        let db_v: u32 =
            conn.query_row("SELECT storage_v FROM schema_meta", [], |row| row.get(0))?;
        if db_v > binary_v {
            return Err(DbError::StorageVTooNew { db_v, binary_v });
        }
        if db_v < binary_v {
            // Existing DB behind this binary: apply every pending migration
            // (id > db_v) in ascending order inside ONE transaction, then pin
            // the storage version to binary_v.
            let tx = conn.transaction()?;
            for m in MIGRATIONS.iter().filter(|m| m.id > db_v) {
                tx.execute_batch(m.sql)
                    .map_err(|source| DbError::Migration {
                        id: m.id,
                        name: m.name,
                        source,
                    })?;
            }
            // applied_at_ms stays 0: the storage layer has no injected clock,
            // matching the fresh-DB path which also writes 0.
            tx.execute(
                "UPDATE schema_meta SET storage_v = ?, applied_at_ms = ?",
                rusqlite::params![binary_v, 0],
            )?;
            tx.commit()?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn vault_key() -> VaultRootKey {
        VaultRootKey::from_bytes([0xab; 32])
    }

    #[test]
    fn open_in_memory_initializes_schema() {
        let db = Db::open_memory(&vault_key()).unwrap();
        let v: u32 = db
            .conn()
            .query_row("SELECT storage_v FROM schema_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(v, u32::from(STORAGE_V));
    }

    #[test]
    fn open_persistent_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        {
            let _ = Db::open(&path, &vault_key()).unwrap();
        }
        // Reopen with the same key.
        let _ = Db::open(&path, &vault_key()).unwrap();
    }

    #[test]
    fn wrong_key_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vault.db");
        {
            let _ = Db::open(&path, &vault_key()).unwrap();
        }
        let bogus = VaultRootKey::from_bytes([0u8; 32]);
        let res = Db::open(&path, &bogus);
        assert!(res.is_err(), "wrong vault root must not open the DB");
    }

    /// Build a DB that has ONLY migration 0001 applied and is stamped
    /// `storage_v = 1`, simulating a real v1 vault opened by a newer binary.
    fn seed_v1_db(conn: &Connection) {
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute_batch(MIGRATIONS[0].sql).unwrap();
        tx.execute(
            "UPDATE schema_meta SET storage_v = ?, applied_at_ms = ?",
            rusqlite::params![1_u32, 0],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn upgrades_v1_db_and_preserves_rows() {
        // Sanity: this test only exercises the upgrade path if the binary is
        // actually ahead of v1.
        assert!(u32::from(STORAGE_V) >= 2);

        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        seed_v1_db(&conn);

        // Insert a stream row against the v1 schema (no name/color columns).
        conn.execute(
            "INSERT INTO streams
             (stream_id, doc_blob, doc_blob_v, head_root, last_op_seq,
              created_at_ms, updated_at_ms)
             VALUES (?, ?, 1, ?, 0, 0, 0)",
            rusqlite::params![vec![7u8; 16], Vec::<u8>::new(), vec![0u8; 32]],
        )
        .unwrap();

        // Run the normal open path: pending migrations must auto-apply.
        Db::ensure_schema(&mut conn).unwrap();

        // Schema upgraded to the binary version.
        let v: u32 = conn
            .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, u32::from(STORAGE_V));

        // Existing row intact, new columns present with defaults.
        let (name, color): (String, String) = conn
            .query_row(
                "SELECT name, color FROM streams WHERE stream_id = ?",
                rusqlite::params![vec![7u8; 16]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "");
        assert_eq!(color, "slate");
    }

    /// Build a DB with migrations 0001+0002 applied, stamped `storage_v = 2`,
    /// simulating a real v2 vault opened by a newer binary.
    fn seed_v2_db(conn: &Connection) {
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute_batch(MIGRATIONS[0].sql).unwrap();
        tx.execute_batch(MIGRATIONS[1].sql).unwrap();
        tx.execute(
            "UPDATE schema_meta SET storage_v = ?, applied_at_ms = ?",
            rusqlite::params![2_u32, 0],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn upgrades_v2_db_adds_scheduling_constraints_column() {
        // Only meaningful if the binary is ahead of v2.
        assert!(u32::from(STORAGE_V) >= 3);

        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        seed_v2_db(&conn);

        // Seed a stream + task row against the v2 schema (no
        // scheduling_constraints column yet).
        conn.execute(
            "INSERT INTO streams
             (stream_id, doc_blob, doc_blob_v, head_root, last_op_seq,
              created_at_ms, updated_at_ms, name, color)
             VALUES (?, ?, 1, ?, 0, 0, 0, 'S', 'slate')",
            rusqlite::params![vec![1u8; 16], Vec::<u8>::new(), vec![0u8; 32]],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, stream_id, title, state)
             VALUES (?, ?, 'seed', 'todo')",
            rusqlite::params![vec![2u8; 16], vec![1u8; 16]],
        )
        .unwrap();

        // Normal open path applies pending migrations (0003).
        Db::ensure_schema(&mut conn).unwrap();

        let v: u32 = conn
            .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, u32::from(STORAGE_V));

        // The new columns exist and default to NULL for the pre-existing row.
        let (task_sc, routine_col): (Option<Vec<u8>>, i64) = conn
            .query_row(
                "SELECT scheduling_constraints,
                        (SELECT COUNT(*) FROM pragma_table_info('routines')
                         WHERE name = 'scheduling_constraints')
                 FROM tasks WHERE id = ?",
                rusqlite::params![vec![2u8; 16]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(task_sc.is_none(), "seed row's constraints default to NULL");
        assert_eq!(routine_col, 1, "routines gained the column too");
    }

    #[test]
    fn rejects_db_from_newer_binary() {
        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        seed_v1_db(&conn);
        // Stamp a version strictly newer than this binary supports.
        let too_new = u32::from(STORAGE_V) + 1;
        conn.execute(
            "UPDATE schema_meta SET storage_v = ?",
            rusqlite::params![too_new],
        )
        .unwrap();
        let err = Db::ensure_schema(&mut conn).unwrap_err();
        assert!(matches!(err, DbError::StorageVTooNew { .. }));
    }

    #[test]
    fn fts5_table_present() {
        let db = Db::open_memory(&vault_key()).unwrap();
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'search_idx'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // FTS5 creates several backing tables; at least one matches.
        assert!(count >= 1);
    }
}
