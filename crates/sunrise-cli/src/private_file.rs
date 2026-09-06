//! One owner-only file write, for every secret this binary puts on disk.
//!
//! There were two copies of this — `vault::write_private` for the vault root
//! and a verbatim clone in `livesync` for the exported pairing payload — which
//! is one copy too many for a routine whose whole job is a permission bit. Both
//! also derived their temporary path with `Path::with_extension("tmp")`, which
//! *replaces* the extension: writing `alice.key` staged it through
//! `alice.tmp`, so a write could clobber an unrelated sibling and, on a failed
//! write, leave that sibling's name occupied by a partial secret.
//!
//! This stages through a hidden, per-process, per-call sibling instead, and
//! removes it if anything after the create fails.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes two writes in one process; the pid distinguishes processes.
static NONCE: AtomicU64 = AtomicU64::new(0);

/// The staging path for `path`: a hidden sibling that **adds** a suffix rather
/// than replacing the extension, so it can never be another file's name.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = format!(".{name}.{}.{nonce}.tmp", std::process::id());
    path.parent()
        .map_or_else(|| PathBuf::from(&tmp), |p| p.join(&tmp))
}

/// Create or replace `path` with `bytes`, readable and writable by its owner
/// only.
///
/// The mode is set **at creation**, not chmod'd afterwards, so the file is
/// never briefly world-readable — the same rule, and the same reason, as
/// `sunrise_auth::FileStore`.
///
/// # Errors
/// Any I/O failure from creating the parent directory, writing the staging
/// file, or renaming it into place. A staging file left by a failed write is
/// removed before the error is returned.
#[cfg(unix)]
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = staging_path(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let staged = (|| -> io::Result<()> {
        f.write_all(bytes)?;
        f.sync_all()
    })();
    drop(f);
    let result = staged.and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        // Best effort: the error being returned is the one worth reporting,
        // and a leftover staging file is exactly what this exists to avoid.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Windows has no mode bits to set. A client there should be using the OS
/// credential store, exactly as `sunrise_auth::FileStore` says.
///
/// # Errors
/// Any I/O failure from creating the parent directory or writing the file.
#[cfg(not(unix))]
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn the_file_is_owner_only_and_holds_exactly_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_private(&path, b"\x00\x01\xfftwo\nlines\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\x00\x01\xfftwo\nlines\n",
            "binary content survives byte for byte"
        );
    }

    /// The staging name adds a suffix rather than replacing the extension, so
    /// writing `alice.key` cannot go near `alice.tmp`, and nothing is left
    /// behind either way.
    #[test]
    fn staging_never_touches_a_sibling_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let bystander = dir.path().join("alice.tmp");
        std::fs::write(&bystander, b"someone else's file").unwrap();

        write_private(&dir.path().join("alice.key"), b"root").unwrap();

        assert_eq!(
            std::fs::read(&bystander).unwrap(),
            b"someone else's file",
            "the sibling `with_extension(\"tmp\")` used to claim is untouched"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "alice.tmp" && n != "alice.key")
            .collect();
        assert!(
            leftovers.is_empty(),
            "the directory holds only the target and the bystander: {leftovers:?}"
        );
    }

    #[test]
    fn a_second_write_replaces_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
