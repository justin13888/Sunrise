//! Opening a vault the integrity check refused, and the copies beside a
//! vault that will not open (`docs/04-storage/migrations.md` §Migration
//! rigor rules 2 to 4, issue #441).
//!
//! The file handling is `sunrise-storage`'s ([`sunrise_storage::backup`]);
//! what this adds is the vault lock, so that nothing restores or moves a
//! vault another process has open, and the repair's rebuild.

use std::path::{Path, PathBuf};

use sunrise_storage::{Db, VaultBackup};

use super::{format_iso8601, holder_pid, Core, CoreError};
use crate::config::CoreConfig;
use crate::engine::{Engine, RebuildReport};
use crate::unlock::Unlock;
use crate::vault_lock::VaultLock;

/// How [`Core::open_as`] treats the vault file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenMode {
    /// [`Core::open`]: refuse a damaged file, rebuild a stale projection.
    Ordinary,
    /// [`Core::open_repairing`]: open a damaged file and rebuild.
    Repair,
}

/// A repair's rebuild: refold the projection from the op log under the copy
/// the repair took, then check the file. Either failing fails the open.
pub(super) fn rebuild_and_check(engine: &Engine, db: &mut Db) -> Result<RebuildReport, CoreError> {
    db.back_up_for_rebuild()?;
    let report = engine.rebuild_projection(db)?;
    db.check_integrity()?;
    tracing::info!(
        ev = "core.repair.done",
        n_ops = report.ops_replayed,
        "the vault was repaired by rebuilding its projection from the op log"
    );
    Ok(report)
}

impl Core {
    /// Open the vault that [`Self::open`] refused as damaged, repairing it:
    /// the file is copied byte for byte to `<vault>.pre-rebuild.bak`, its
    /// indexes are rebuilt, the projection is rebuilt from the op log,
    /// whatever fold built it, and the file is checked again
    /// ([`Db::open_for_repair`]). The open then goes on as [`Self::open`]'s.
    ///
    /// What it mends is damage in what the op log re-derives: the entity
    /// rows, their side tables and indexes, the merge state and both search
    /// indexes. Damage to the op log, the keys or another source table is
    /// still there after the rebuild, and the check after it reports it.
    ///
    /// The copy stays until the next ordinary open, which deletes it.
    /// Returns the open vault and what the rebuild did.
    ///
    /// # Errors
    /// [`sunrise_storage::DbError::RebuildBackup`] when the copy cannot be
    /// written, and nothing is touched;
    /// [`sunrise_storage::DbError::IntegrityCheckFailed`] when the vault is
    /// still damaged after the rebuild; as [`Self::open`] otherwise, and for
    /// a directory with no vault in it.
    pub async fn open_repairing(
        cfg: CoreConfig,
        unlock: Unlock,
    ) -> Result<(Self, RebuildReport), CoreError> {
        let (core, report) = Self::open_as(cfg, unlock, OpenMode::Repair).await?;
        Ok((core, report.unwrap_or_default()))
    }

    /// The copies of the vault in `vault_dir` kept beside it, newest first
    /// ([`Db::backups`]): what a client offers to restore when the vault
    /// will not open.
    #[must_use]
    pub fn vault_backups(vault_dir: &Path) -> Vec<VaultBackup> {
        Db::backups(&vault_dir.join("vault.db"))
    }

    /// Put the copy at `backup` back in the place of the vault in
    /// `cfg.vault_dir`, under the vault's lock, and return where the vault
    /// it replaced was moved ([`Db::restore_backup`]). The next
    /// [`Self::open`] opens the copy, and migrates it if it is behind.
    ///
    /// # Errors
    /// [`CoreError::VaultLock`] while the vault is open, here or in another
    /// process, and nothing is moved; [`sunrise_storage::DbError::Restore`]
    /// for a path that is not one of the vault's copies, or a move that
    /// failed.
    pub fn restore_vault_backup(
        cfg: &CoreConfig,
        backup: &Path,
    ) -> Result<Option<PathBuf>, CoreError> {
        let _lock = Self::lock_closed_vault(cfg)?;
        Ok(Db::restore_backup(&cfg.vault_dir.join("vault.db"), backup)?)
    }

    /// Move the vault directory `cfg.vault_dir` aside whole, and return where
    /// it went ([`Db::set_vault_dir_aside`]): what a client does before it
    /// restores the account into the same place from its recovery code, which
    /// needs an empty directory. Nothing in it is deleted.
    ///
    /// # Errors
    /// [`CoreError::VaultLock`] while the vault is open, and nothing is moved;
    /// [`sunrise_storage::DbError::Restore`] when the move fails.
    pub fn set_vault_aside(cfg: &CoreConfig) -> Result<PathBuf, CoreError> {
        // The lock's file is inside the directory, and a directory cannot be
        // moved on every platform while a file in it is open: it is taken to
        // prove nothing has the vault open, and let go before the move.
        drop(Self::lock_closed_vault(cfg)?);
        Ok(Db::set_vault_dir_aside(&cfg.vault_dir)?)
    }

    /// Take the vault's lock, as an open would, for an operation on its files.
    fn lock_closed_vault(cfg: &CoreConfig) -> Result<VaultLock, CoreError> {
        let started_at = format_iso8601(cfg.clock.now_ms());
        Ok(VaultLock::acquire(
            &cfg.vault_dir,
            holder_pid(),
            &started_at,
        )?)
    }
}
