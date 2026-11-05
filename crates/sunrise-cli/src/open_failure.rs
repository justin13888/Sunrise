//! What the CLI says when a vault will not open, and the two verbs that
//! answer it without opening one: `sunrise rebuild --repair` and
//! `sunrise backups` (issue #441).
//!
//! The ways out are the client layer's
//! ([`sunrise_client_core::vault_session::open_remedies`]), the same table the
//! app asks, so the two never offer different things for one failure. This
//! module only words them, one line each under the error.

use std::path::Path;

use sunrise_client_core::vault_session::{open_remedies, OpenContext, OpenProblem, OpenRemedy};
use sunrise_core::{BackupKind, Core, CoreConfig, CoreError, Unlock};
use sunrise_crypto::keys::VaultRootKey;

use crate::i18n::strings;

/// A failed open of the vault in `vault_dir`, worded with what to do next.
///
/// The error's own message comes first, then one line per remedy, best
/// first. `repair_tried` is whether the failure came from
/// `sunrise rebuild --repair`, which is then not offered again.
#[must_use]
pub fn explain(vault_dir: &Path, err: &CoreError, repair_tried: bool) -> String {
    let backups = Core::vault_backups(vault_dir);
    let cx = OpenContext {
        has_backup: !backups.is_empty(),
        can_recover: crate::livesync::load_relay_device_id(vault_dir).is_some(),
        repair_tried,
    };
    let problem = OpenProblem::of(err);
    let mut out = err.to_string();
    let remedies = open_remedies(problem, cx);
    out.push('\n');
    out.push_str(&strings::open::remedies());
    for remedy in remedies {
        let line = match remedy {
            OpenRemedy::TryAgain if problem == OpenProblem::BackupFailed => {
                strings::open::free_space()
            }
            OpenRemedy::TryAgain => strings::open::try_again(),
            OpenRemedy::Repair => strings::open::repair(),
            OpenRemedy::RestoreBackup => {
                strings::open::restore(&backups[0].path.display().to_string())
            }
            OpenRemedy::RecoverFromCode => strings::open::recover(),
            OpenRemedy::UpdateApp => strings::open::update(),
            OpenRemedy::SendDiagnostics => strings::open::diagnostics(),
        };
        out.push_str("\n  - ");
        out.push_str(&line);
    }
    out
}

/// `sunrise rebuild --repair`: open the damaged vault in `vault_dir` for
/// repair, and report what it did. Dispatched before the ordinary open,
/// which refuses the vault this exists for. The caller has checked there
/// is a vault: resolving its root mints one for a directory with none.
///
/// # Errors
/// The failed repair, worded by [`explain`].
pub async fn repair(vault_dir: &Path, app: &str, root: [u8; 32]) -> Result<String, String> {
    let cfg = CoreConfig::production(vault_dir.to_path_buf(), app.to_owned());
    let (core, report) = Core::open_repairing(
        cfg,
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes(root),
            paired: None,
        },
    )
    .await
    .map_err(|e| explain(vault_dir, &e, true))?;
    let copy = sunrise_core::Core::vault_backups(vault_dir)
        .into_iter()
        .find(|b| b.kind == BackupKind::PreRebuild)
        .map(|b| b.path.display().to_string())
        .unwrap_or_default();
    core.close().await.map_err(|e| e.to_string())?;
    Ok(strings::rebuild::repaired(
        i64::try_from(report.ops_replayed).unwrap_or(i64::MAX),
        &copy,
    ))
}

/// `sunrise backups [restore <file>]`: list the copies of the vault in
/// `vault_dir` kept beside it, one per line as `<path>  <what it was taken
/// before>`, or put one back. Dispatched before the ordinary open: it is
/// what a user reaches for when the vault will not open.
///
/// # Errors
/// A usage error, or the failed restore.
pub fn backups(vault_dir: &Path, app: &str, rest: &[String]) -> Result<Vec<String>, String> {
    match rest {
        [] => Ok(Core::vault_backups(vault_dir)
            .into_iter()
            .map(|b| {
                let what = match b.kind {
                    BackupKind::PreRebuild => strings::backups::pre_rebuild(),
                    BackupKind::PreMigration { from_v } => {
                        strings::backups::pre_migration(&from_v.to_string())
                    }
                };
                format!("{}  {what}", b.path.display())
            })
            .collect()),
        [verb, file] if verb == "restore" => {
            let cfg = CoreConfig::production(vault_dir.to_path_buf(), app.to_owned());
            let copy = Path::new(file);
            // A bare file name is the one `sunrise backups` printed the
            // directory of; resolve it there rather than in the cwd.
            let copy = if copy.is_absolute() {
                copy.to_path_buf()
            } else {
                vault_dir.join(copy.file_name().unwrap_or_default())
            };
            let replaced = Core::restore_vault_backup(&cfg, &copy).map_err(|e| e.to_string())?;
            let shown = copy.display().to_string();
            Ok(vec![match replaced {
                Some(aside) => strings::backups::restored(&shown, &aside.display().to_string()),
                None => strings::backups::restored_alone(&shown),
            }])
        }
        _ => Err(strings::backups::usage()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_core::DbError;

    fn damaged() -> CoreError {
        CoreError::Db(DbError::IntegrityCheckFailed {
            detail: "NULL value in t.a".into(),
        })
    }

    #[test]
    fn a_damaged_vault_is_explained_with_the_repair_first() {
        let dir = tempfile::tempdir().unwrap();
        let text = explain(dir.path(), &damaged(), false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], damaged().to_string());
        assert_eq!(lines[1], strings::open::remedies());
        assert_eq!(lines[2], format!("  - {}", strings::open::repair()));
        assert_eq!(
            lines.last().copied(),
            Some(format!("  - {}", strings::open::diagnostics()).as_str())
        );
        assert!(
            !text.contains(&strings::open::recover()),
            "a vault on no relay has no code to recover from"
        );
    }

    #[test]
    fn a_failed_repair_offers_the_copy_beside_the_vault_and_not_itself() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("vault.db.pre-rebuild.bak");
        std::fs::write(&copy, b"x").unwrap();
        let text = explain(dir.path(), &damaged(), true);
        assert!(!text.contains(&strings::open::repair()), "{text}");
        assert!(
            text.contains(&strings::open::restore(&copy.display().to_string())),
            "{text}"
        );
    }

    #[test]
    fn a_copy_that_could_not_be_written_asks_for_disk_space() {
        let dir = tempfile::tempdir().unwrap();
        let err = CoreError::Db(DbError::RebuildBackup {
            source: std::io::Error::other("No space left on device"),
        });
        let text = explain(dir.path(), &err, false);
        assert!(text.contains(&strings::open::free_space()), "{text}");
        let bare = format!("  - {}", strings::open::try_again());
        assert!(!text.lines().any(|l| l == bare), "{text}");
    }

    #[test]
    fn backups_lists_nothing_beside_a_fresh_directory_and_refuses_bad_usage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(backups(dir.path(), "t", &[]), Ok(vec![]));
        assert_eq!(
            backups(dir.path(), "t", &["restore".into()]),
            Err(strings::backups::usage())
        );
    }
}
