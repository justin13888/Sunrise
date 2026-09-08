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

use crate::migrations::{BASELINE_STORAGE_V, MIGRATIONS};
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
    /// longer produced: [`Db::open`] auto-applies pending migrations instead of
    /// erroring on an older DB.
    #[error("STORAGE_V too old: db is {db_v}, binary expects {binary_v}")]
    StorageVTooOld {
        /// Version found in the DB.
        db_v: u32,
        /// Version the binary needs.
        binary_v: u32,
    },
    /// The vault predates the pre-1.0 storage baseline reset (ADR-0018).
    ///
    /// Distinct from [`DbError::StorageVTooOld`] because it is not a
    /// "run the migrations" condition: the migrations that would upgrade such
    /// a vault were deleted, so there is nothing to run. It is terminal, and
    /// the only remedy is a fresh vault.
    #[error(
        "vault predates the storage baseline: db is storage_v {db_v}, \
         this build's baseline is {baseline_v}; pre-1.0 vaults are not upgraded"
    )]
    StorageVPreBaseline {
        /// Version found in the DB.
        db_v: u32,
        /// [`BASELINE_STORAGE_V`].
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
        Ok(Self { conn })
    }

    /// Create an encrypted vault at `path` whose schema is exactly what this
    /// build's migrations produce at `target_v`, stamped at `target_v`.
    ///
    /// Test-only, and it stays that way. Nothing a user runs has a reason to
    /// write a vault at a version this binary has already migrated past; the
    /// one caller is the fixture generator in `crate::vault_fixtures`, which
    /// needs to produce a genuine old vault so the migration chain can be run
    /// against one.
    ///
    /// It lives here rather than in that module because keying a database is
    /// this type's private business. A fixture built by re-deriving the
    /// SQLCipher key in the test would prove that *some* derivation opens the
    /// file, not that `Db::open`'s does — and the day the derivation changed,
    /// the generator would go on producing files `Db::open` refuses.
    #[cfg(test)]
    pub(crate) fn create_at_storage_v(
        path: &Path,
        vault_root: &VaultRootKey,
        target_v: u32,
    ) -> Result<Self, DbError> {
        let mut db = Self::open_unmigrated(path, vault_root)?;
        let tx = db.conn.transaction()?;
        for m in MIGRATIONS.iter().filter(|m| m.id <= target_v) {
            tx.execute_batch(m.sql)
                .map_err(|source| DbError::Migration {
                    id: m.id,
                    name: m.name,
                    source,
                })?;
        }
        tx.execute(
            "UPDATE schema_meta SET storage_v = ?, applied_at_ms = ?",
            rusqlite::params![target_v, 0],
        )?;
        tx.commit()?;
        Ok(db)
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
            return Self::run_migrations(conn, 0, binary_v, "fresh");
        }

        let db_v: u32 =
            conn.query_row("SELECT storage_v FROM schema_meta", [], |row| row.get(0))?;
        if db_v > binary_v {
            return Err(DbError::StorageVTooNew { db_v, binary_v });
        }
        // A vault stamped below the baseline cannot be carried forward: the
        // incremental migrations that once described its shape were collapsed
        // into `0013_baseline.sql` and deleted (ADR-0018). Refusing loudly is
        // the only honest answer — running the baseline over it would try to
        // CREATE tables that already exist, and skipping it would hand the
        // engine a schema missing half its columns.
        if db_v > 0 && db_v < BASELINE_STORAGE_V {
            tracing::error!(
                ev = "db.migrate.refused",
                from_v = u64::from(db_v),
                to_v = u64::from(binary_v),
                err_code = "STORAGE_V_TOO_OLD",
                err_kind = "permanent",
                retryable = false,
                "vault predates the storage baseline"
            );
            return Err(DbError::StorageVPreBaseline {
                db_v,
                baseline_v: BASELINE_STORAGE_V,
            });
        }
        if db_v < binary_v {
            return Self::run_migrations(conn, db_v, binary_v, "upgrade");
        }
        Ok(())
    }

    /// Apply every migration with `id > from_v` in ascending order inside ONE
    /// transaction, then pin `schema_meta.storage_v` to `to_v`.
    ///
    /// Migrations are logged because they are the one storage operation that
    /// can leave a user unable to open their vault at all, and the failure
    /// arrives with no UI to report it through. Nothing here touches row
    /// content: only migration ids, names, and versions.
    fn run_migrations(
        conn: &mut Connection,
        from_v: u32,
        to_v: u32,
        mode: &'static str,
    ) -> Result<(), DbError> {
        tracing::info!(
            ev = "db.migrate.start",
            from_v = u64::from(from_v),
            to_v = u64::from(to_v),
            mode,
            "applying migrations"
        );
        let tx = conn.transaction()?;
        for m in MIGRATIONS.iter().filter(|m| m.id > from_v) {
            tx.execute_batch(m.sql).map_err(|source| {
                tracing::error!(
                    ev = "db.migrate.failed",
                    from_v = u64::from(from_v),
                    to_v = u64::from(m.id),
                    err_code = "DB_MIGRATION_FAILED",
                    err_kind = "permanent",
                    retryable = false,
                    cause = %source,
                    "migration failed"
                );
                DbError::Migration {
                    id: m.id,
                    name: m.name,
                    source,
                }
            })?;
        }
        // applied_at_ms stays 0: the storage layer has no injected clock.
        tx.execute(
            "UPDATE schema_meta SET storage_v = ?, applied_at_ms = ?",
            rusqlite::params![to_v, 0],
        )?;
        tx.commit()?;
        tracing::info!(
            ev = "db.migrate.ok",
            from_v = u64::from(from_v),
            to_v = u64::from(to_v),
            mode,
            "migrations applied"
        );
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

    /// A fresh vault applies the baseline and everything appended after it,
    /// landing at the binary's `STORAGE_V` rather than at the baseline.
    #[test]
    fn fresh_vault_applies_the_baseline_and_everything_after_it() {
        let db = Db::open_memory(&vault_key()).unwrap();
        let v: u32 = db
            .conn()
            .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, u32::from(STORAGE_V));
        assert!(v >= BASELINE_STORAGE_V);

        // Spot-check that the collapse did not lose a table from the middle of
        // the old sequence: one from 0001, one from 0005, one from 0010, one
        // from 0011.
        for table in [
            "ops",
            "streams",
            "tasks",
            "local_identity",
            "sync_cursors",
            "contexts",
            "task_blockers",
            "focus_sessions",
            "review_snapshots",
        ] {
            let n: i64 = db
                .conn()
                .query_row(
                    "SELECT count(*) FROM sqlite_master
                     WHERE type = 'table' AND name = ?",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "baseline must create `{table}`");
        }
    }

    /// ...and it does NOT recreate the schema the reset dropped.
    #[test]
    fn baseline_omits_the_dead_schema() {
        let db = Db::open_memory(&vault_key()).unwrap();
        let journal: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'merge_journal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            journal, 0,
            "merge_journal is a per-field journal for an entity-level merge model"
        );

        // `routines.extra` is deliberately NOT in this list any more. The
        // baseline dropped it because it had no reader — 0004 recorded it as
        // "left NULL" and it still was — and `0015_entity_extra_columns.sql`
        // reinstates it with one, which is the exact condition its removal was
        // predicated on. A column that is written and read is not dead schema.
        for (table, column) in [
            ("streams", "doc_blob"),
            ("streams", "doc_blob_v"),
            ("routines", "rrule"),
        ] {
            let n: i64 = db
                .conn()
                .query_row(
                    "SELECT count(*) FROM pragma_table_info(?) WHERE name = ?",
                    rusqlite::params![table, column],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "`{table}.{column}` was dropped by the reset");
        }
    }

    /// 0015 gives every column-projected entity an `extra` blob, so forward-
    /// compat unknowns survive materialization rather than only `tasks`,
    /// `blocks` and `attachments` doing so.
    ///
    /// The two exclusions are asserted too, because both are decisions rather
    /// than omissions: `focus_interruptions` holds the one entity with no
    /// `unknown` map (its whole value is its key), and `review_snapshots`
    /// already round-trips through its whole-record CBOR `body` blob, so a
    /// column there would be a second home for the same data.
    #[test]
    fn every_column_projected_entity_has_an_extra_blob() {
        let db = Db::open_memory(&vault_key()).unwrap();
        let has_extra = |table: &str| -> i64 {
            db.conn()
                .query_row(
                    "SELECT count(*) FROM pragma_table_info(?) WHERE name = 'extra'",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap()
        };

        for table in [
            "tasks",
            "blocks",
            "attachments",
            "streams",
            "contexts",
            "routines",
            "focus_sessions",
            "focus_session_ends",
        ] {
            assert_eq!(has_extra(table), 1, "`{table}.extra` must exist");
        }

        for table in ["focus_interruptions", "review_snapshots"] {
            assert_eq!(
                has_extra(table),
                0,
                "`{table}` is deliberately exempt; see 0015's header"
            );
        }
    }

    /// 0014 adds `streams.sort_order`, defaulting to the "never ordered"
    /// sentinel rather than to a key.
    #[test]
    fn stream_sort_order_column_exists_and_defaults_to_unset() {
        let db = Db::open_memory(&vault_key()).unwrap();
        db.conn()
            .execute(
                "INSERT INTO streams
                 (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
                 VALUES (X'01', X'00', 0, 'Work', 0, 0)",
                [],
            )
            .unwrap();
        let got: String = db
            .conn()
            .query_row("SELECT sort_order FROM streams", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            got, "",
            "an un-keyed row must be distinguishable from a first-position one"
        );
    }

    /// 0014's backfill hands every pre-existing row a key, in the order those
    /// rows were already being displayed — so a vault's sidebar does not
    /// rearrange itself on upgrade.
    ///
    /// Exercised by replaying 0013 and then 0014 by hand, because a fresh
    /// vault has no rows for the backfill to touch.
    #[test]
    fn migration_0014_backfills_existing_streams_in_display_order() {
        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(MIGRATIONS[0].sql).unwrap();
        // Deliberately inserted out of order, and with a case that only a
        // NOCASE collation gets right.
        for (id, name) in [(1u8, "zebra"), (2, "Apple"), (3, "mango")] {
            tx.execute(
                "INSERT INTO streams
                 (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
                 VALUES (?, X'00', 0, ?, 0, 0)",
                rusqlite::params![vec![id], name],
            )
            .unwrap();
        }
        tx.execute_batch(MIGRATIONS[1].sql).unwrap();

        let mut stmt = tx
            .prepare("SELECT name, sort_order FROM streams ORDER BY sort_order")
            .unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        drop(stmt);
        assert_eq!(
            rows,
            vec![
                ("Apple".to_string(), "AAAN".to_string()),
                ("mango".to_string(), "AABN".to_string()),
                ("zebra".to_string(), "AACN".to_string()),
            ],
            "sorting by the new key must reproduce the old name ordering"
        );
    }

    /// A 0016 vault upgrades to 0017: the identity, `deferred_ops`,
    /// and `device_revocations` arrive, and `stream_keys` is
    /// re-keyed on `(stream_id, epoch, key_id)`.
    ///
    /// The pre-0017 rows are dropped on purpose and this asserts it: every one
    /// of them wrapped a key *derived* from the vault root, so
    /// `Keychain::open`'s legacy adoption recomputes the identical key on the
    /// next open. Carrying them forward would mean inventing a `key_id` for a
    /// row during a migration that has no access to the vault root, and so no
    /// way to unwrap the key the id is computed from.
    #[test]
    fn migration_0017_rekeys_stream_keys_and_adds_the_hierarchy_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        let tx = conn.transaction().unwrap();
        // Everything up to and including 0016. Selected by id, not by
        // position: `MIGRATIONS[..len - 1]` meant "everything but 0017" only
        // while 0017 happened to be last, and silently became "everything but
        // 0018" -- which re-ran 0017 against a schema it had already migrated
        // -- the first time a migration was appended.
        for m in MIGRATIONS.iter().filter(|m| m.id <= 16) {
            tx.execute_batch(m.sql).unwrap();
        }
        tx.execute(
            "INSERT INTO stream_keys (stream_id, epoch, wrapped, created_at_ms)
             VALUES (?, 1, X'00', 0)",
            rusqlite::params![vec![7u8; 16]],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO devices
             (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms)
             VALUES (?, X'00', 'old', 'test', 0, NULL)",
            rusqlite::params![vec![9u8; 16]],
        )
        .unwrap();

        tx.execute_batch(
            MIGRATIONS
                .iter()
                .find(|m| m.id == 17)
                .expect("0017 is registered")
                .sql,
        )
        .unwrap();

        for table in [
            "identity",
            "stream_keys",
            "deferred_ops",
            "device_revocations",
        ] {
            let n: i64 = tx
                .query_row(
                    "SELECT count(*) FROM sqlite_master
                     WHERE type = 'table' AND name = ?",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "0017 must create `{table}`");
        }
        let leftover: i64 = tx
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'stream_keys_old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            leftover, 0,
            "the renamed table must not survive the upgrade"
        );

        // The re-keyed table is empty and takes a `key_id` + `source`.
        let rows: i64 = tx
            .query_row("SELECT count(*) FROM stream_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows, 0,
            "derived pre-0017 keys are re-derived on open, not migrated"
        );
        tx.execute(
            "INSERT INTO stream_keys (stream_id, epoch, key_id, wrapped, source, created_at_ms)
             VALUES (?, 1, X'0102030405060708', X'00', 'legacy', 0)",
            rusqlite::params![vec![7u8; 16]],
        )
        .unwrap();
        // Same (stream_id, epoch), different key_id: both concurrent mints live.
        tx.execute(
            "INSERT INTO stream_keys (stream_id, epoch, key_id, wrapped, source, created_at_ms)
             VALUES (?, 1, X'0807060504030201', X'00', 'envelope', 0)",
            rusqlite::params![vec![7u8; 16]],
        )
        .unwrap();
        let rows: i64 = tx
            .query_row("SELECT count(*) FROM stream_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows, 2,
            "(stream_id, epoch, key_id) must admit two keys per epoch"
        );

        // The devices row survived and grew its identity columns. Revocation
        // is no longer among them: it lives in `device_revocations`, so that a
        // `device_revoke` naming a device this vault has never seen cannot
        // mint a phantom entry in the user's device list.
        let (nickname, identity_id): (String, Option<Vec<u8>>) = tx
            .query_row("SELECT nickname, identity_id FROM devices", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(nickname, "old");
        assert!(identity_id.is_none());
        let revocations: i64 = tx
            .query_row("SELECT count(*) FROM device_revocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(revocations, 0, "an unrevoked device carries no register");
    }

    /// 0019 clears `identity.id_d_priv_wrapped` on a device that was *paired*
    /// at STORAGE_V 17, and leaves it alone on the account's creator.
    ///
    /// This is issue #87. 0018 declined to touch the column because the schema
    /// recorded nothing that told the two apart; `stream_keys.source` does,
    /// because only a founding vault mints the vault-meta stream's first epoch
    /// itself. Both halves are asserted, because either alone is worthless: a
    /// migration that clears every row trades a revocation gap for an account
    /// whose `ID_D_priv` no longer exists anywhere.
    #[test]
    fn migration_0019_clears_a_paired_devices_identity_key_and_keeps_the_creators() {
        // `source` is the discriminator, so the two vaults differ in exactly
        // that one column and in nothing else.
        let build = |meta_key_source: &str| -> Connection {
            let mut conn = Connection::open_in_memory().unwrap();
            Db::apply_pragmas(&conn).unwrap();
            let tx = conn.transaction().unwrap();
            for m in MIGRATIONS.iter().filter(|m| m.id <= 18) {
                tx.execute_batch(m.sql).unwrap();
            }
            tx.execute(
                "INSERT INTO local_identity
                 (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
                 VALUES (1, ?, X'00', X'00', 0)",
                rusqlite::params![vec![3u8; 16]],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO identity
                 (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped,
                  id_d_priv_wrapped, created_at_ms)
                 VALUES (1, ?, ?, ?, X'aa', X'bb', 0)",
                rusqlite::params![vec![4u8; 16], vec![5u8; 32], vec![6u8; 32]],
            )
            .unwrap();
            // Epoch 1 of the vault-meta stream: sixteen zero bytes.
            tx.execute(
                "INSERT INTO stream_keys
                 (stream_id, epoch, key_id, wrapped, source, created_at_ms)
                 VALUES (?, 1, X'0102030405060708', X'00', ?, 0)",
                rusqlite::params![vec![0u8; 16], meta_key_source],
            )
            .unwrap();
            tx.execute_batch(
                MIGRATIONS
                    .iter()
                    .find(|m| m.id == 19)
                    .expect("0019 is registered")
                    .sql,
            )
            .unwrap();
            tx.commit().unwrap();
            conn
        };
        let read = |conn: &Connection| -> (Vec<u8>, Option<Vec<u8>>) {
            conn.query_row(
                "SELECT id_d_priv_wrapped, minted_by_device_id FROM identity WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };

        let (wrapped, minted_by) = read(&build("pairing"));
        assert!(
            wrapped.is_empty(),
            "a device whose vault-meta key came from a pairing payload never \
             minted the identity, and must not keep its unwrapping key"
        );
        assert!(minted_by.is_none(), "and it is recorded as not the minter");

        let (wrapped, minted_by) = read(&build("envelope"));
        assert!(
            wrapped.is_empty(),
            "nor may one whose vault-meta key arrived in a `key_envelope` op"
        );
        assert!(minted_by.is_none());

        for source in ["local", "legacy"] {
            let (wrapped, minted_by) = read(&build(source));
            assert_eq!(
                wrapped,
                vec![0xbbu8],
                "the creator's copy is the only one in existence and must survive \
                 ({source})"
            );
            assert_eq!(
                minted_by,
                Some(vec![3u8; 16]),
                "and the row now names it, so nothing has to infer it again"
            );
        }
    }

    /// A vault from before the reset is REFUSED, with its own error — not
    /// silently upgraded, and not confused with a too-new one.
    #[test]
    fn refuses_a_pre_baseline_vault() {
        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        // A v12 vault: the last shape that existed before the baseline. Only
        // `schema_meta` is needed — the refusal happens on the stamped version,
        // before any DDL is considered.
        conn.execute_batch(
            "CREATE TABLE schema_meta (storage_v INTEGER NOT NULL,
                                       applied_at_ms INTEGER NOT NULL);
             INSERT INTO schema_meta (storage_v, applied_at_ms) VALUES (12, 0);",
        )
        .unwrap();

        let err = Db::ensure_schema(&mut conn).unwrap_err();
        assert!(
            matches!(
                err,
                DbError::StorageVPreBaseline {
                    db_v: 12,
                    baseline_v: 13
                }
            ),
            "expected a pre-baseline refusal, got {err:?}"
        );
        assert_eq!(err.as_error_code(), ErrorCode::StorageVTooOld);
    }

    /// Every pre-baseline version is refused, not just the last one.
    #[test]
    fn refuses_every_pre_baseline_version() {
        for db_v in 1..BASELINE_STORAGE_V {
            let mut conn = Connection::open_in_memory().unwrap();
            Db::apply_pragmas(&conn).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_meta (storage_v INTEGER NOT NULL,
                                           applied_at_ms INTEGER NOT NULL);
                 INSERT INTO schema_meta (storage_v, applied_at_ms) VALUES (0, 0);",
            )
            .unwrap();
            conn.execute(
                "UPDATE schema_meta SET storage_v = ?",
                rusqlite::params![db_v],
            )
            .unwrap();
            assert!(
                matches!(
                    Db::ensure_schema(&mut conn),
                    Err(DbError::StorageVPreBaseline { .. })
                ),
                "storage_v = {db_v} must be refused"
            );
        }
    }
    #[test]
    fn rejects_db_from_newer_binary() {
        let mut conn = Connection::open_in_memory().unwrap();
        Db::apply_pragmas(&conn).unwrap();
        Db::ensure_schema(&mut conn).unwrap();
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
