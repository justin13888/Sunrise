//! Bringing a vault's schema to this build's `STORAGE_V`: the migration runner
//! and the refusals in front of it.

use super::{Db, DbError};
use crate::migrations::{BASELINE_STORAGE_V, MIGRATIONS};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use sunrise_cbor::version::STORAGE_V;
#[cfg(test)]
use sunrise_crypto::keys::VaultRootKey;
use sunrise_error::ErrorCode;

/// What [`Db::ensure_schema`] found, and the copy it took.
#[derive(Debug)]
pub(crate) enum Schema {
    /// No schema: every migration ran over an empty file.
    Created,
    /// An older schema, migrated under the copy at this path (`None` for a
    /// connection with no file).
    Upgraded(Option<PathBuf>),
    /// This build's.
    Current,
}

impl Schema {
    /// The copy taken, if one was.
    pub(crate) fn taken_backup(self) -> Option<PathBuf> {
        match self {
            Self::Upgraded(taken) => taken,
            Self::Created | Self::Current => None,
        }
    }
}

impl Db {
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

    /// Bring the schema to this build's `STORAGE_V`, and say what it found
    /// and the pre-migration copy it took.
    ///
    /// `vault` is the file behind `conn`, or `None` for a connection with no
    /// file. It is what an upgrade is copied from first and what a current
    /// open deletes the copies beside.
    ///
    /// `repairing` is [`Self::open_for_repair`]'s open, whose caller has not
    /// checked the file: an upgrade is checked here before it starts, and a
    /// current open keeps the copies beside it, among them the one the repair
    /// has just taken.
    pub(crate) fn ensure_schema(
        conn: &mut Connection,
        vault: Option<&Path>,
        repairing: bool,
    ) -> Result<Schema, DbError> {
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
            // Nothing to back up: there is no data yet.
            Self::run_migrations(conn, 0, binary_v, "fresh")?;
            Self::quick_check(conn, vault.is_some())?;
            return Ok(Schema::Created);
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
            // A failed check must not start a migration (rule 3). An ordinary
            // open has checked already.
            if repairing {
                Self::quick_check(conn, vault.is_some())?;
            }
            // No backup, no migration: a copy that cannot be made returns
            // here with the vault still at `db_v`.
            let taken = match vault {
                Some(vault) => {
                    Self::back_up(conn, vault, db_v)?;
                    Some(Self::backup_path(vault, db_v))
                }
                None => None,
            };
            Self::run_migrations(conn, db_v, binary_v, "upgrade")?;
            Self::quick_check(conn, vault.is_some())?;
            return Ok(Schema::Upgraded(taken));
        }
        // Current, and the caller's `quick_check` passed: this is the open
        // that proves a migrated or rebuilt vault usable, so its copies have
        // done their job.
        if let Some(vault) = vault.filter(|_| !repairing) {
            Self::remove_backups(vault);
        }
        Ok(Schema::Current)
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
                    err_code = %ErrorCode::StorageMigrationFailed,
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
}
