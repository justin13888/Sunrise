//! A vault that exists and will not open, across the seam (issue #441):
//! why, what to offer, and what each offer runs.
//!
//! [`SunriseCore::open`] raises one `BindingError::Vault…` variant per
//! [`OpenProblem`]. The client hands the problem to [`open_remedies`], which
//! is [`sunrise_client_core::vault_session::open_remedies`], and runs the
//! remedy the user picks:
//!
//! | Remedy | Runs |
//! |---|---|
//! | `Repair` | [`SunriseCore::open_repairing`] |
//! | `RestoreBackup` | [`vault_backups`], then [`restore_vault_backup`], then [`SunriseCore::open`] |
//! | `RecoverFromCode` | [`set_vault_aside`], then [`SunriseCore::recover_account_with_replay`] |
//! | `TryAgain` | [`SunriseCore::open`] |
//!
//! None of them deletes anything. The repair works under a byte-for-byte
//! copy of the file, a restore moves the vault it replaces aside, and a
//! recovery starts in a directory the old vault was moved out of.

use std::path::PathBuf;
use std::sync::Arc;

use sunrise_client_core::vault_session::{self as session, OpenProblem, OpenRemedy};
use sunrise_core::{BackupKind, Core, CoreConfig, CoreError, Unlock};
use sunrise_crypto::keys::VaultRootKey;

use crate::{vault_root_bytes, BindingError, SunriseCore};

/// See [`sunrise_client_core::vault_session::OpenProblem`].
#[uniffi::remote(Enum)]
pub enum OpenProblem {
    Damaged,
    BackupFailed,
    MigrationFailed,
    TooNew,
    TooOld,
    Busy,
    Other,
}

/// See [`sunrise_client_core::vault_session::OpenRemedy`].
#[uniffi::remote(Enum)]
pub enum OpenRemedy {
    TryAgain,
    Repair,
    RestoreBackup,
    RecoverFromCode,
    UpdateApp,
    SendDiagnostics,
}

/// See [`sunrise_client_core::vault_session::OpenContext`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, uniffi::Record)]
pub struct OpenContext {
    /// [`vault_backups`] is not empty.
    pub has_backup: bool,
    /// The account is on a relay and has a recovery code.
    pub can_recover: bool,
    /// The error came from [`SunriseCore::open_repairing`].
    pub repair_tried: bool,
}

/// What a copy beside the vault was taken before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum VaultBackupKind {
    /// A rebuild of the projection, or a repair.
    PreRebuild,
    /// A migration from `from_v`; the next open migrates the restored vault
    /// again.
    PreMigration {
        /// The `STORAGE_V` the vault was at.
        from_v: u32,
    },
}

/// A copy of the vault kept beside it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct VaultBackupItem {
    /// The file, to hand to [`restore_vault_backup`].
    pub path: String,
    /// What it was taken before.
    pub kind: VaultBackupKind,
}

/// The ways out of `problem`, best first; see
/// [`sunrise_client_core::vault_session::open_remedies`].
#[uniffi::export]
#[must_use]
pub fn open_remedies(problem: OpenProblem, context: OpenContext) -> Vec<OpenRemedy> {
    session::open_remedies(
        problem,
        session::OpenContext {
            has_backup: context.has_backup,
            can_recover: context.can_recover,
            repair_tried: context.repair_tried,
        },
    )
}

/// Whether `remedies` offers nothing but diagnostics; see
/// [`sunrise_client_core::vault_session::is_fatal`].
#[uniffi::export]
#[must_use]
pub fn open_failure_is_fatal(remedies: Vec<OpenRemedy>) -> bool {
    session::is_fatal(&remedies)
}

/// The copies of the vault in `vault_dir` kept beside it, newest first. Read
/// with nothing open; see [`sunrise_core::Core::vault_backups`].
#[uniffi::export]
#[must_use]
pub fn vault_backups(vault_dir: String) -> Vec<VaultBackupItem> {
    Core::vault_backups(&PathBuf::from(vault_dir))
        .into_iter()
        .map(|b| VaultBackupItem {
            path: b.path.to_string_lossy().into_owned(),
            kind: match b.kind {
                BackupKind::PreRebuild => VaultBackupKind::PreRebuild,
                BackupKind::PreMigration { from_v } => VaultBackupKind::PreMigration { from_v },
            },
        })
        .collect()
}

/// Put the copy at `backup_path` back in the place of the closed vault in
/// `vault_dir`, and return where the vault it replaced was moved; see
/// [`sunrise_core::Core::restore_vault_backup`]. Open it again with
/// [`SunriseCore::open`].
///
/// # Errors
///
/// [`BindingError::VaultBusy`] while the vault is open; [`BindingError::Core`]
/// for a path that is not one of its copies, or a move that failed.
#[uniffi::export]
pub fn restore_vault_backup(
    vault_dir: String,
    backup_path: String,
) -> Result<Option<String>, BindingError> {
    let replaced = Core::restore_vault_backup(&files_cfg(vault_dir), &PathBuf::from(backup_path))
        .map_err(open_failed)?;
    Ok(replaced.map(|p| p.to_string_lossy().into_owned()))
}

/// Move the closed vault directory `vault_dir` aside whole, and return where
/// it went: before [`SunriseCore::recover_account_with_replay`] restores the
/// account into the same place, which needs it empty. See
/// [`sunrise_core::Core::set_vault_aside`].
///
/// # Errors
///
/// [`BindingError::VaultBusy`] while the vault is open; [`BindingError::Core`]
/// when the move fails.
#[uniffi::export]
pub fn set_vault_aside(vault_dir: String) -> Result<String, BindingError> {
    let aside = Core::set_vault_aside(&files_cfg(vault_dir)).map_err(open_failed)?;
    Ok(aside.to_string_lossy().into_owned())
}

/// A configuration for the operations on a closed vault's files, which read
/// only its directory and the clock the vault lock records.
fn files_cfg(vault_dir: String) -> CoreConfig {
    CoreConfig::production(PathBuf::from(vault_dir), String::new())
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Open the vault at `vault_dir` that [`Self::open`] refused as damaged
    /// ([`BindingError::VaultDamaged`]), repairing it from its own op log:
    /// the file is copied byte for byte beside it first, the projection is
    /// rebuilt and the file checked again. See
    /// [`sunrise_core::Core::open_repairing`]. A device opened this way has
    /// joined its account already, so there is no pairing bundle.
    ///
    /// # Errors
    ///
    /// As [`Self::open`]. A vault still damaged after the rebuild raises
    /// [`BindingError::VaultDamaged`] again, for which
    /// [`OpenContext::repair_tried`] drops the repair from what is offered.
    #[uniffi::constructor]
    pub async fn open_repairing(
        vault_dir: String,
        vault_root: Vec<u8>,
        app_version: String,
    ) -> Result<Arc<Self>, BindingError> {
        let root = vault_root_bytes(&vault_root)?;
        let cfg = CoreConfig::production(PathBuf::from(vault_dir), app_version);
        let (core, _report) = Core::open_repairing(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(root),
                paired: None,
            },
        )
        .await
        .map_err(open_failed)?;
        Ok(Self::wrap(core))
    }
}

/// A failed open, as the `BindingError::Vault…` variant for its problem, or
/// [`BindingError::Core`] when it has none.
pub(crate) fn open_failed(e: CoreError) -> BindingError {
    let message = e.to_string();
    match OpenProblem::of(&e) {
        OpenProblem::Damaged => BindingError::VaultDamaged(message),
        OpenProblem::BackupFailed => BindingError::VaultBackupFailed(message),
        OpenProblem::MigrationFailed => BindingError::VaultMigrationFailed(message),
        OpenProblem::TooNew => BindingError::VaultTooNew(message),
        OpenProblem::TooOld => BindingError::VaultTooOld(message),
        OpenProblem::Busy => BindingError::VaultBusy(message),
        OpenProblem::Other => e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_core::DbError;

    #[test]
    fn a_failed_open_crosses_as_its_problem_and_reads_as_before() {
        let e = CoreError::Db(DbError::IntegrityCheckFailed {
            detail: "NULL value in t.a".into(),
        });
        let before = BindingError::Core(e.to_string()).to_string();
        let crossed = open_failed(e);
        assert!(
            matches!(crossed, BindingError::VaultDamaged(_)),
            "{crossed:?}"
        );
        assert_eq!(crossed.to_string(), before, "the message is unchanged");

        let other = open_failed(CoreError::Closed);
        assert!(matches!(other, BindingError::Core(_)), "{other:?}");
    }

    #[test]
    fn the_remedies_are_the_client_layers() {
        assert_eq!(
            open_remedies(
                OpenProblem::Damaged,
                OpenContext {
                    has_backup: true,
                    can_recover: false,
                    repair_tried: false,
                }
            ),
            [
                OpenRemedy::Repair,
                OpenRemedy::RestoreBackup,
                OpenRemedy::SendDiagnostics
            ]
        );
        assert!(open_failure_is_fatal(vec![OpenRemedy::SendDiagnostics]));
    }

    /// The whole path an app takes for a damaged vault, through the seam:
    /// refused with its own variant, repaired, its copy listed and restored.
    #[tokio::test]
    async fn a_damaged_vault_is_refused_repaired_and_its_copy_restored() {
        let dir = tempfile::tempdir().unwrap();
        let vault_dir = dir.path().to_string_lossy().into_owned();
        let root = vec![9u8; 32];
        let open = || SunriseCore::open(vault_dir.clone(), root.clone(), "test".into(), None);
        drop(open().await.unwrap());
        assert!(vault_backups(vault_dir.clone()).is_empty());
        // A missing vault is nothing to repair, and nothing is created.
        let empty = tempfile::tempdir().unwrap();
        let nothing = SunriseCore::open_repairing(
            empty.path().to_string_lossy().into_owned(),
            root.clone(),
            "test".into(),
        )
        .await;
        assert!(
            matches!(nothing, Err(BindingError::Core(_))),
            "nothing to repair"
        );

        let core = SunriseCore::open_repairing(vault_dir.clone(), root.clone(), "test".into())
            .await
            .unwrap();
        let busy = restore_vault_backup(vault_dir.clone(), String::new()).unwrap_err();
        assert!(matches!(busy, BindingError::VaultBusy(_)), "{busy:?}");
        drop(core);

        let copies = vault_backups(vault_dir.clone());
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert_eq!(copies[0].kind, VaultBackupKind::PreRebuild);
        let replaced = restore_vault_backup(vault_dir.clone(), copies[0].path.clone())
            .unwrap()
            .expect("the repaired vault was moved aside");
        assert!(std::path::Path::new(&replaced).exists());
        drop(open().await.unwrap());
    }
}
