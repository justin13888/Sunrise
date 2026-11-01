//! The copies of a vault kept beside it, and putting one back
//! (`docs/04-storage/migrations.md` §Migration rigor rules 2 to 4).
//!
//! Two kinds of copy sit beside `vault.db`, both whole vaults under the
//! vault's own SQLCipher key:
//!
//! - `<vault>.pre-v<from>.bak`, taken by [`Db::open`] before it migrates a
//!   vault from `STORAGE_V` `from` ([`Db::backup_path`]);
//! - `<vault>.pre-rebuild.bak`, taken before the projection is rebuilt from
//!   the op log, and before a repair ([`Db::rebuild_backup_path`]). A repair
//!   copies the file as it is, so its copy may carry a `-wal` beside it.
//!
//! Both are kept until the next ordinary open that finds the vault current,
//! the open that proves the changed vault usable, and deleted then.
//!
//! [`Db::open_for_repair`] opens a vault the integrity check refused, and
//! [`Db::restore_backup`] puts a copy back in the vault's place. Neither
//! deletes anything: what a restore replaces is moved aside, under
//! `<vault>.replaced-<n>`.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use sunrise_crypto::keys::VaultRootKey;

use crate::db::{Db, DbError};

/// The suffix of the copy taken before a rebuild or a repair.
const PRE_REBUILD: &str = ".pre-rebuild.bak";

/// SQLite's write-ahead log beside a database file: `<file>-wal`.
const WAL: &str = "-wal";

/// What a copy beside the vault was taken before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackupKind {
    /// A rebuild of the projection or a repair. The vault was at this
    /// build's `STORAGE_V`, or the one a repaired vault was found at.
    PreRebuild,
    /// A migration from `from_v`. Restoring it puts the vault back at that
    /// version, and the next open migrates it again.
    PreMigration {
        /// The version the vault was at when it was copied.
        from_v: u32,
    },
}

/// A copy of the vault beside it ([`Db::backups`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VaultBackup {
    /// The file.
    pub path: PathBuf,
    /// What it was taken before.
    pub kind: BackupKind,
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

/// The directory `path` is in, `.` for a bare file name.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    }
}

/// Make the directory entries under `path`'s directory durable: a rename is
/// durable only once its directory is. A no-op off Unix, where a directory
/// cannot be opened to be synced.
fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    fs::File::open(parent_of(path))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// What a file named `name` beside a vault named `vault_name` is, when it is
/// a copy [`Db::backups`] lists.
fn kind_of(vault_name: &str, name: &str) -> Option<BackupKind> {
    let rest = name.strip_prefix(vault_name)?;
    if rest == PRE_REBUILD {
        return Some(BackupKind::PreRebuild);
    }
    let version = rest.strip_prefix(".pre-v")?.strip_suffix(".bak")?;
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    version
        .parse()
        .ok()
        .map(|from_v| BackupKind::PreMigration { from_v })
}

/// Whether `name`, beside a vault named `vault_name`, is a copy of it, the
/// `-wal` of one, or either half-written: what an open that finds the vault
/// current deletes.
fn is_backup_file(vault_name: &str, name: &str) -> bool {
    let name = name.strip_suffix(".tmp").unwrap_or(name);
    let name = name.strip_suffix(WAL).unwrap_or(name);
    kind_of(vault_name, name).is_some()
}

/// Copy `from` to `to` durably: under a temporary name, fsynced, then
/// renamed into place.
fn copy_durably(from: &Path, to: &Path) -> std::io::Result<()> {
    let tmp = sibling(to, ".tmp");
    let copy = || -> std::io::Result<()> {
        fs::copy(from, &tmp)?;
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, to)
    };
    copy().inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Remove `path`, which may not exist.
fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Log a pre-rebuild copy that could not be written, and build its error.
fn rebuild_backup_failed(source: std::io::Error) -> DbError {
    tracing::error!(
        ev = "db.backup.failed",
        err_code = %sunrise_error::ErrorCode::StorageBackupFailed,
        err_kind = "user",
        retryable = false,
        cause = %source,
        "could not back up the vault; it was not rebuilt"
    );
    DbError::RebuildBackup { source }
}

impl Db {
    /// Where [`Self::open`] copies the vault at `vault` before migrating it
    /// from `from_v`: `<vault>.pre-v<from_v>.bak`, beside it.
    ///
    /// The copy is a whole vault, encrypted under the same key, so restoring
    /// it is replacing `vault` with it while nothing has the vault open.
    #[must_use]
    pub fn backup_path(vault: &Path, from_v: u32) -> PathBuf {
        sibling(vault, &format!(".pre-v{from_v}.bak"))
    }

    /// Copy the vault to [`Self::backup_path`] with `VACUUM INTO`, durably.
    ///
    /// Written under a temporary name, fsynced, then renamed over any older
    /// copy of the same version, so a crash mid-copy never leaves a truncated
    /// file under the name a restore would trust. A copy already there from an
    /// earlier attempt is a copy of the same version — that attempt's
    /// migration rolled back — so replacing it loses nothing.
    ///
    /// `VACUUM INTO` writes the output under the connection's own SQLCipher
    /// key, which `the_backup_is_encrypted_under_the_vaults_own_key` pins.
    pub(crate) fn back_up(conn: &Connection, vault: &Path, from_v: u32) -> Result<(), DbError> {
        let backup = Self::backup_path(vault, from_v);
        if let Err(source) = Self::vacuum_into(conn, &backup) {
            tracing::error!(
                ev = "db.backup.failed",
                from_v = u64::from(from_v),
                err_code = %sunrise_error::ErrorCode::StorageBackupFailed,
                err_kind = "user",
                retryable = false,
                cause = %source,
                "could not back up the vault; it was not migrated"
            );
            return Err(DbError::Backup { from_v, source });
        }
        tracing::info!(
            ev = "db.backup.ok",
            from_v = u64::from(from_v),
            "backed up the vault before migrating it"
        );
        Ok(())
    }

    /// Write `conn`'s database to `target` with `VACUUM INTO`, durably: under
    /// a temporary name, fsynced, renamed over any older file of that name,
    /// and the directory fsynced. See [`Self::back_up`].
    pub(crate) fn vacuum_into(conn: &Connection, target: &Path) -> std::io::Result<()> {
        let tmp = sibling(target, ".tmp");
        let copy = || -> std::io::Result<()> {
            match fs::remove_file(&tmp) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let to = tmp.to_str().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "the vault path is not UTF-8, which VACUUM INTO needs",
                )
            })?;
            conn.execute("VACUUM INTO ?1", [to])
                .map_err(std::io::Error::other)?;
            fs::File::open(&tmp)?.sync_all()?;
            fs::rename(&tmp, target)?;
            sync_parent(target)
        };
        copy().inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
    }

    /// Delete every pre-migration and pre-rebuild copy beside `vault`, and
    /// any half-written one a crash left ([`is_backup_file`]).
    ///
    /// Called only on an open that found the vault current. Best-effort: a
    /// copy that cannot be deleted costs disk, never the open, and the next
    /// open tries again.
    pub(crate) fn remove_backups(vault: &Path) {
        let Some(file_name) = vault.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        let Ok(entries) = fs::read_dir(parent_of(vault)) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_backup_file(file_name, name) {
                if let Err(e) = fs::remove_file(entry.path()) {
                    tracing::warn!(
                        ev = "db.backup.remove_failed",
                        cause = %e,
                        "could not delete a copy of the vault; the next open retries"
                    );
                }
            }
        }
    }

    /// Where the vault at `vault` is copied before its projection is rebuilt
    /// or it is repaired: `<vault>.pre-rebuild.bak`, beside it.
    #[must_use]
    pub fn rebuild_backup_path(vault: &Path) -> PathBuf {
        sibling(vault, PRE_REBUILD)
    }

    /// Copy the vault before its projection is rebuilt, and return the copy
    /// the rebuild runs under: "no backup, no rebuild"
    /// (`docs/04-storage/migrations.md` §Migration rigor rule 4).
    ///
    /// The first rebuild after an open that copied the vault, before
    /// migrating it or before repairing it, runs under that copy, which is
    /// from before the rebuild too, and no second one is written: nothing but
    /// the open has written to the vault since. Every other rebuild writes the
    /// vault to [`Self::rebuild_backup_path`] with `VACUUM INTO`, as a
    /// pre-migration copy is, so the copy holds what the session wrote. A
    /// vault in memory has no file to copy, and returns `None`.
    ///
    /// # Errors
    /// [`DbError::RebuildBackup`] when the copy cannot be written. Nothing has
    /// changed, and the caller does not rebuild.
    pub fn back_up_for_rebuild(&mut self) -> Result<Option<PathBuf>, DbError> {
        if let Some(taken) = self.taken_backup.take() {
            return Ok(Some(taken));
        }
        let Some(vault) = &self.path else {
            return Ok(None);
        };
        let backup = Self::rebuild_backup_path(vault);
        // A `-wal` beside a copy is read as that copy's own log.
        remove_if_present(&sibling(&backup, WAL)).map_err(rebuild_backup_failed)?;
        Self::vacuum_into(self.conn(), &backup).map_err(rebuild_backup_failed)?;
        tracing::info!(
            ev = "db.backup.ok",
            "backed up the vault before rebuilding it"
        );
        Ok(Some(backup))
    }

    /// Every copy of the vault at `vault` beside it: a pre-rebuild copy
    /// first, the newest state, then the pre-migration ones from the latest
    /// version down. A half-written copy is not listed.
    #[must_use]
    pub fn backups(vault: &Path) -> Vec<VaultBackup> {
        let Some(vault_name) = vault.file_name().and_then(|n| n.to_str()) else {
            return Vec::new();
        };
        let Ok(entries) = fs::read_dir(parent_of(vault)) else {
            return Vec::new();
        };
        let mut found: Vec<VaultBackup> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let kind = kind_of(vault_name, name.to_str()?)?;
                Some(VaultBackup {
                    path: vault.with_file_name(name),
                    kind,
                })
            })
            .collect();
        found.sort_by_key(|b| match b.kind {
            BackupKind::PreRebuild => (0, 0),
            BackupKind::PreMigration { from_v } => (1, u32::MAX - from_v),
        });
        found
    }

    /// Put the copy `backup` back in the place of the vault at `vault`, and
    /// return where the vault it replaces was moved, if there was one.
    ///
    /// Only a copy [`Self::backups`] lists is restored. The vault and its
    /// `-wal` are moved aside first, to `<vault>.replaced-<n>` for the first
    /// `n` not taken, because the log of the damaged file must not be read as
    /// the restored one's; its `-shm`, an index of that log, is deleted. Then
    /// the copy, and a `-wal` beside it, are renamed into place. Nothing is
    /// deleted that holds data: a restore can be undone by hand.
    ///
    /// The caller holds the vault's lock and has no connection open on it.
    ///
    /// # Errors
    /// [`DbError::Restore`], naming the copy, when it is not one of the
    /// vault's or a rename fails. What was moved before the failure keeps the
    /// name it was moved to.
    pub fn restore_backup(vault: &Path, backup: &Path) -> Result<Option<PathBuf>, DbError> {
        let fail = |source: std::io::Error| DbError::Restore {
            backup: backup.to_path_buf(),
            source,
        };
        if !Self::backups(vault).iter().any(|b| b.path == backup) {
            return Err(fail(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not a copy of this vault beside it",
            )));
        }
        let replaced = if vault.exists() {
            let aside = (1..=u32::MAX)
                .map(|n| sibling(vault, &format!(".replaced-{n}")))
                .find(|p| !p.exists() && !sibling(p, WAL).exists())
                .ok_or_else(|| fail(std::io::Error::other("no free name to move the vault to")))?;
            let wal = sibling(vault, WAL);
            if wal.exists() {
                fs::rename(&wal, sibling(&aside, WAL)).map_err(fail)?;
            }
            fs::rename(vault, &aside).map_err(fail)?;
            Some(aside)
        } else {
            None
        };
        remove_if_present(&sibling(vault, "-shm")).map_err(fail)?;
        let backup_wal = sibling(backup, WAL);
        if backup_wal.exists() {
            fs::rename(&backup_wal, sibling(vault, WAL)).map_err(fail)?;
        }
        fs::rename(backup, vault).map_err(fail)?;
        sync_parent(vault).map_err(fail)?;
        tracing::info!(
            ev = "db.backup.restored",
            "put a copy of the vault back in its place"
        );
        Ok(replaced)
    }

    /// Move the vault directory `dir` aside, to `<dir>.set-aside-<n>` for the
    /// first `n` not taken, and return where it went.
    ///
    /// What a client does before restoring an account into the same place
    /// from its recovery code, which needs an empty directory: the vault that
    /// would not open is kept, not deleted. The caller holds no connection on
    /// it.
    ///
    /// # Errors
    /// [`DbError::Restore`], naming `dir`, when it cannot be moved.
    pub fn set_vault_dir_aside(dir: &Path) -> Result<PathBuf, DbError> {
        let fail = |source: std::io::Error| DbError::Restore {
            backup: dir.to_path_buf(),
            source,
        };
        let aside = (1..=u32::MAX)
            .map(|n| sibling(dir, &format!(".set-aside-{n}")))
            .find(|p| !p.exists())
            .ok_or_else(|| fail(std::io::Error::other("no free name to move the vault to")))?;
        fs::rename(dir, &aside).map_err(fail)?;
        sync_parent(dir).map_err(fail)?;
        Ok(aside)
    }

    /// Open the vault at `path` that [`Self::open`] refused as damaged, to
    /// repair it (`docs/04-storage/migrations.md` §Migration rigor rule 3).
    ///
    /// In order:
    ///
    /// 1. The file, and a `-wal` beside it, are copied byte for byte to
    ///    [`Self::rebuild_backup_path`], before anything opens it. Not with
    ///    `VACUUM INTO`, which reads every page and stops at a damaged one.
    /// 2. Every index is rebuilt from its table (`REINDEX`).
    /// 3. The schema is brought to this build's version as [`Self::open`]
    ///    brings it, except that an upgrade is checked first and refused on a
    ///    failed check, and the copies beside the vault are kept.
    ///
    /// The integrity check is not run on a current vault: what the caller
    /// does next, rebuilding the projection, is what may clear the damage,
    /// and it checks the vault afterwards with [`Self::check_integrity`]. The
    /// returned handle's first [`Self::back_up_for_rebuild`] is the step 1
    /// copy.
    ///
    /// No vault is created: a `path` with no file is an error.
    ///
    /// # Errors
    /// [`DbError::Sqlite`] (`SQLITE_CANTOPEN`) when there is no file at
    /// `path`. [`DbError::RebuildBackup`] when the copy cannot be written,
    /// and the file is not opened. [`DbError::IntegrityCheckFailed`] when an index
    /// cannot be rebuilt over a damaged table, or an upgrade's check fails.
    /// As [`Self::open`] otherwise.
    pub fn open_for_repair(path: &Path, vault_root: &VaultRootKey) -> Result<Self, DbError> {
        let backup = Self::rebuild_backup_path(path);
        let wal = sibling(path, WAL);
        let backup_wal = sibling(&backup, WAL);
        if !path.exists() {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                Some("there is no vault to repair".into()),
            )
            .into());
        }
        let copy = || -> std::io::Result<()> {
            if wal.exists() {
                copy_durably(&wal, &backup_wal)?;
            } else {
                remove_if_present(&backup_wal)?;
            }
            copy_durably(path, &backup)?;
            sync_parent(&backup)
        };
        copy().map_err(rebuild_backup_failed)?;
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::apply_sqlcipher_key(&conn, vault_root)?;
        Self::apply_pragmas(&conn)?;
        match conn.execute_batch("REINDEX") {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(e, msg))
                if e.code == rusqlite::ErrorCode::DatabaseCorrupt =>
            {
                return Err(Self::integrity_failed(&[
                    msg.unwrap_or_else(|| e.to_string())
                ]));
            }
            Err(e) => return Err(e.into()),
        }
        Self::ensure_schema(&mut conn, Some(path), true)?;
        Self::ensure_incremental_auto_vacuum(&conn)?;
        tracing::info!(
            ev = "db.repair.opened",
            "opened a damaged vault to repair it"
        );
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
            taken_backup: Some(backup),
            created: false,
        })
    }

    /// Whether this open created the file. Until something is written to it
    /// there is nothing in it a copy would keep, which is how the rebuild at
    /// the first open of a new vault skips [`Self::back_up_for_rebuild`].
    #[must_use]
    pub const fn created_by_this_open(&self) -> bool {
        self.created
    }

    /// Run `PRAGMA quick_check` on the open vault, as [`Self::open`] does
    /// before anything else.
    ///
    /// # Errors
    /// [`DbError::IntegrityCheckFailed`] for a damaged file; storage failures.
    pub fn check_integrity(&self) -> Result<(), DbError> {
        Self::quick_check(self.conn(), self.path.is_some())
    }
}

#[cfg(test)]
mod tests;
