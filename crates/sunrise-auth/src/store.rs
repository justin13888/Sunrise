//! Persisting [`Credentials`] between runs.
//!
//! A login is worth nothing if the next process start has to repeat it. What a
//! client should persist *to*, though, is a platform question: on macOS the app
//! owns the OS Keychain through Swift and does not need Rust to reach it, and
//! the CLI has no keychain to speak of.
//!
//! So this is a seam, plus the one implementation the CLI needs: a file the
//! owner alone can read.
//!
//! # What this is not
//!
//! It is **not** the OS keychain. The workspace has no `keyring` /
//! `security-framework` / `secret-service` dependency, and
//! `sunrise-core::keychain` — despite the name — is the device identity and
//! Stream-key store, not a credential vault. A macOS client should implement
//! [`CredentialStore`] over the real Keychain on the Swift side rather than
//! using [`FileStore`].
//!
//! # Threat model for [`FileStore`]
//!
//! The file holds a live bearer and a refresh token in plaintext, protected by
//! nothing but its mode. That is weaker than the vault database, which is
//! SQLCipher-encrypted under the vault root — but the vault root is not
//! available at the point the CLI needs to *find out whether it is logged in*,
//! and putting the tokens there would also mean a storage migration and a
//! `STORAGE_V` bump. `0600` in the vault directory is the same protection the
//! vault lock file and WAL already rely on, and it is what this crate's own
//! module documentation specified for the CLI.
//!
//! It is not sufficient against a local attacker who can read as this user.
//! Nothing short of the OS keychain is, which is exactly why the seam exists.

use std::path::{Path, PathBuf};

use crate::credentials::Credentials;
use crate::error::LoginError;

/// Somewhere [`Credentials`] survive a process restart.
pub trait CredentialStore: Send + Sync {
    /// The stored credentials, or `None` when nothing has been saved.
    ///
    /// A stored blob that cannot be parsed reads as `None` rather than an
    /// error: the remedy for an unreadable credential is to log in again, and
    /// making the caller distinguish "absent" from "corrupt" only to do the
    /// same thing in both cases buys nothing.
    fn load(&self) -> Result<Option<Credentials>, LoginError>;

    /// Replace the stored credentials.
    fn save(&self, creds: &Credentials) -> Result<(), LoginError>;

    /// Remove them. Succeeds when there was nothing stored.
    fn clear(&self) -> Result<(), LoginError>;
}

/// The file name [`FileStore::in_dir`] uses.
pub const CREDENTIALS_FILE: &str = "credentials.json";

/// A [`CredentialStore`] backed by one owner-only file.
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// A store at exactly `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A store at `dir/credentials.json` — the vault directory, for the CLI.
    #[must_use]
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self::new(dir.as_ref().join(CREDENTIALS_FILE))
    }

    /// Where this store writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Create/truncate `path` with owner-only permissions, atomically where the
/// platform allows it.
///
/// The mode is set **at creation** rather than after writing: a `write` then
/// `set_permissions` leaves a window in which the token is on disk
/// world-readable, and that window is exactly when a token is most interesting.
#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Write to a sibling temp file, then rename. A crash mid-write then leaves
    // the previous credentials intact instead of a truncated file that reads
    // as "logged out".
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // No mode bits to set. The platform clients that matter here (macOS, Linux)
    // are unix; a Windows client should be using the OS credential store.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

impl CredentialStore for FileStore {
    fn load(&self) -> Result<Option<Credentials>, LoginError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(LoginError::Malformed(format!(
                "cannot read {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn save(&self, creds: &Credentials) -> Result<(), LoginError> {
        let bytes = serde_json::to_vec_pretty(creds)
            .map_err(|e| LoginError::Malformed(format!("cannot encode credentials: {e}")))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                LoginError::Malformed(format!("cannot create {}: {e}", parent.display()))
            })?;
        }
        write_private(&self.path, &bytes).map_err(|e| {
            LoginError::Malformed(format!("cannot write {}: {e}", self.path.display()))
        })
    }

    fn clear(&self) -> Result<(), LoginError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(LoginError::Malformed(format!(
                "cannot remove {}: {e}",
                self.path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials::new("access".into(), Some("refresh".into()), Some(3600), 1_000)
    }

    #[test]
    fn a_saved_credential_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::in_dir(dir.path());
        assert!(s.load().unwrap().is_none(), "nothing stored yet");
        s.save(&creds()).unwrap();
        assert_eq!(s.load().unwrap().unwrap(), creds());
    }

    #[test]
    fn clearing_removes_it_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::in_dir(dir.path());
        s.save(&creds()).unwrap();
        s.clear().unwrap();
        assert!(s.load().unwrap().is_none());
        s.clear().expect("clearing nothing is not an error");
    }

    /// The file holds a live bearer. Anything but owner-only is a finding.
    #[cfg(unix)]
    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::in_dir(dir.path());
        s.save(&creds()).unwrap();
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// And a re-save does not widen it — the second write goes through a fresh
    /// temp file, so its mode has to be right too, not just the first one's.
    #[cfg(unix)]
    #[test]
    fn a_rewrite_stays_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::in_dir(dir.path());
        s.save(&creds()).unwrap();
        s.save(&creds()).unwrap();
        let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// An unreadable blob means "log in again", which is what `None` already
    /// means. Distinguishing them would make every caller handle two cases
    /// that have one remedy.
    #[test]
    fn a_corrupt_file_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::in_dir(dir.path());
        std::fs::write(s.path(), b"{not json").unwrap();
        assert!(s.load().unwrap().is_none());
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::in_dir(dir.path());
        s.save(&creds()).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [CREDENTIALS_FILE]);
    }
}
