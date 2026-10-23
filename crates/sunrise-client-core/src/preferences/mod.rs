//! The bootstrap preferences file: what a device reads before it has a vault.
//!
//! `sync.relay_url`, `auth.oidc_issuer` and `auth.oidc_client_id` are what a
//! device needs to find and unlock a vault, so they cannot live inside one.
//! [`sunrise_core::BootstrapPreferences`] is the codec over the file's bytes
//! and touches no filesystem (`docs/01-architecture/shared-core.md` rule 2);
//! this module is the platform half every client shares: where the file is,
//! how it is read, and how it is replaced so a crash leaves one whole file or
//! the other.
//!
//! Two neighbours share that write:
//!
//! * [`client`] — the **client settings** file: device-local settings that are
//!   not OS-specific and have no key in the core's key table (the Focus
//!   filter's scope, the account address, keyboard tips). Kept beside the
//!   bootstrap file rather than added to the table, because a new key is a
//!   change to the registry every replica reads (#489).
//! * [`legacy`] — the Apple client's `UserDefaults` keys, each mapped to the
//!   core preference or the client setting that replaces it, for a one-time
//!   move.

pub mod client;
pub mod legacy;

pub use client::{
    read_client_settings, update_client_settings, ClientSettings, FocusStream, CLIENT_SETTINGS_FILE,
};

use std::io::Write as _;
use std::path::Path;

use sunrise_domain::ResolvedPref;

/// Why the bootstrap file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum PreferenceFileError {
    /// A key that is not a bootstrap key, or a value that does not fit it.
    #[error(transparent)]
    Invalid(#[from] sunrise_core::BootstrapError),
    /// The file exists and could not be read, or could not be written.
    #[error("bootstrap preferences: {0}")]
    Io(#[from] std::io::Error),
    /// The client settings file exists and is not one this build can read.
    #[error("client settings: {0}")]
    Malformed(String),
}

/// The bootstrap file's bytes in `dir`, `None` when there is none.
///
/// # Errors
/// [`PreferenceFileError::Io`] for a file that exists and cannot be read.
pub fn read_bootstrap_file(dir: &Path) -> Result<Option<Vec<u8>>, PreferenceFileError> {
    match std::fs::read(dir.join(sunrise_core::BOOTSTRAP_FILE)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Every bootstrap preference held in `dir`, resolved against its defaults.
///
/// `dir` is the directory the client keeps its vaults in.
///
/// # Errors
/// As [`read_bootstrap_file`].
pub fn bootstrap_preferences(dir: &Path) -> Result<Vec<ResolvedPref>, PreferenceFileError> {
    let file = read_bootstrap_file(dir)?;
    Ok(sunrise_core::BootstrapPreferences::resolve(file.as_deref()))
}

/// Set one bootstrap preference in `dir`, or remove it with `None`.
///
/// The file is written to a sibling, flushed to disk, and renamed over the old
/// one, so a crash leaves one whole file or the other.
///
/// # Errors
/// [`PreferenceFileError::Invalid`] for a key that is not a bootstrap key or a
/// value that does not fit it, and [`PreferenceFileError::Io`] for a file that
/// cannot be read or written.
pub fn set_bootstrap_preference(
    dir: &Path,
    key: &str,
    value: Option<&str>,
) -> Result<(), PreferenceFileError> {
    let file = read_bootstrap_file(dir)?;
    let body = sunrise_core::BootstrapPreferences::set(file.as_deref(), key, value)?;
    write_atomically(dir, sunrise_core::BOOTSTRAP_FILE, &body)?;
    Ok(())
}

/// Replace `dir/name` with `body`: written to a sibling, flushed to disk, and
/// renamed over the old file, so a crash leaves one whole file or the other.
fn write_atomically(dir: &Path, name: &str, body: &[u8]) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let tmp = path.with_extension("json.tmp");
    // Flushed before the rename: without it a power loss can make the rename
    // durable ahead of the bytes, leaving a truncated file that reads as empty
    // and loses the relay and sign-in settings.
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(dir: &Path) -> Option<sunrise_domain::PrefValue> {
        bootstrap_preferences(dir)
            .unwrap()
            .into_iter()
            .find(|p| p.key == "sync.relay_url")
            .and_then(|p| p.value)
    }

    #[test]
    fn the_bootstrap_file_is_written_whole_and_read_back() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("vaults");
        assert!(read_bootstrap_file(&dir).unwrap().is_none());
        assert!(relay(&dir).is_none());

        set_bootstrap_preference(&dir, "sync.relay_url", Some("wss://relay.example/sync")).unwrap();
        assert_eq!(
            relay(&dir),
            Some(sunrise_domain::PrefValue::Text(
                "wss://relay.example/sync".into()
            ))
        );
        assert!(dir.join(sunrise_core::BOOTSTRAP_FILE).exists());
        assert!(!dir.join("preferences.bootstrap.json.tmp").exists());

        set_bootstrap_preference(&dir, "sync.relay_url", None).unwrap();
        assert!(relay(&dir).is_none());
    }

    #[test]
    fn a_key_that_is_not_a_bootstrap_key_is_refused_and_writes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let err = set_bootstrap_preference(root.path(), "week_start", Some("MO")).unwrap_err();
        assert!(matches!(err, PreferenceFileError::Invalid(_)), "{err}");
        assert!(!root.path().join(sunrise_core::BOOTSTRAP_FILE).exists());
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_an_absence() {
        let root = tempfile::tempdir().unwrap();
        // A directory where the file should be cannot be read as one.
        std::fs::create_dir(root.path().join(sunrise_core::BOOTSTRAP_FILE)).unwrap();
        let err = read_bootstrap_file(root.path()).unwrap_err();
        assert!(
            err.to_string().starts_with("bootstrap preferences: "),
            "{err}"
        );
    }
}
