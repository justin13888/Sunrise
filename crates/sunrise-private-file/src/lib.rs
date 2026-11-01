//! One owner-only file write, for every secret Sunrise puts on disk.
//!
//! The vault key, the parked pairing secret, the exported pairing grant, the
//! sealed undo history (all `sunrise-cli`) and the credential file
//! `sunrise_auth::FileStore` keeps are each written through
//! [`write_private`]. There used to be two copies of the routine, one per
//! crate, and neither restricted the file on Windows.
//!
//! What "owner-only" means is per platform:
//!
//! * **Unix:** mode `0600`, set at creation, so the file is never briefly
//!   readable by anyone else.
//! * **Windows:** a protected DACL holding one entry, which allows the
//!   current user full control. It is set at creation through `CreateFileW`'s
//!   security attributes, before any secret byte is written. "Protected" means
//!   the file inherits nothing from its directory, so a vault directory under
//!   `C:\`, which hands `Authenticated Users` modify, hands it none of that.
//!   `SYSTEM` and `Administrators` get no entry either. An administrator can
//!   still take ownership and rewrite the DACL; nothing at file level stops
//!   that.
//! * **Anything else:** the file is created with the platform default. No
//!   shipping Sunrise binary targets such a platform.
//!
//! The Windows half needs `unsafe`, and this is the one crate in the
//! workspace allowed it. ADR-0067 records why it is here rather than in a
//! third-party crate or the Windows Credential Manager.
//!
//! [`verify_owner_only`] reads the protection back, so a test in any crate can
//! assert it on every platform without an `unsafe` block of its own.

#![cfg_attr(not(windows), forbid(unsafe_code))]

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(windows)]
mod windows;

/// Distinguishes two writes in one process; the pid distinguishes processes.
static NONCE: AtomicU64 = AtomicU64::new(0);

/// The staging path for `path`: a hidden sibling that **adds** a suffix rather
/// than replacing the extension, so it can never be another file's name.
///
/// `Path::with_extension("tmp")`, which both earlier copies used, *replaces*
/// the extension: writing `alice.key` staged it through `alice.tmp`, so a
/// write could clobber an unrelated sibling.
fn staging_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = format!(".{name}.{}.{nonce}.tmp", std::process::id());
    path.parent()
        .map_or_else(|| PathBuf::from(&tmp), |p| p.join(&tmp))
}

/// Create a new file at `path`, owner-only from its first byte.
///
/// Fails if `path` exists: the staging name is fresh, so an existing file
/// there is someone else's.
fn create_owner_only(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        windows::create_owner_only(path)
    }
    #[cfg(not(windows))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }
}

/// Create or replace `path` with `bytes`, readable and writable by the
/// current user only.
///
/// The bytes are staged in a hidden sibling, which is created owner-only (see
/// the crate documentation for what that means per platform), flushed to
/// disk, and renamed over `path`. A crash or a full disk mid-write therefore
/// leaves the previous file whole rather than truncated, which matters
/// because a truncated vault key is unrecoverable. The rename moves the file
/// together with its own security descriptor, so the replacement is as
/// restricted as the staging file was.
///
/// The parent directory is created if it is missing, with the platform's
/// default permissions: this restricts the file, not the directory.
///
/// # Errors
/// Any I/O failure from creating the parent directory, creating or writing
/// the staging file, or renaming it into place. A staging file left by a
/// failed write is removed before the error is returned.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = staging_path(path);
    let mut f = create_owner_only(&tmp)?;
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

/// Check that `path` carries exactly the protection [`write_private`] gives
/// it.
///
/// On Unix that is mode `0600`. On Windows it is a protected DACL with exactly
/// one entry, an allow entry granting the current user's SID full control.
/// The error says which part does not hold.
///
/// This is for tests and diagnostics. It reads the file's metadata, not its
/// contents.
///
/// # Errors
/// [`io::ErrorKind::Other`] naming what is wrong when the file is not
/// owner-only. Any I/O error from reading its metadata or security
/// descriptor. [`io::ErrorKind::Unsupported`] on a platform with neither mode
/// bits nor DACLs.
pub fn verify_owner_only(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode == 0o600 {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "{} is not owner-only: its mode is {mode:o}, not 600",
                path.display()
            )))
        }
    }
    #[cfg(windows)]
    {
        windows::verify_owner_only(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this platform has neither mode bits nor DACLs",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_private(&path, b"secret").unwrap();

        verify_owner_only(&path).unwrap();
    }

    /// The check can fail. A file created the ordinary way takes the
    /// platform default: the umask's mode on Unix, set explicitly here so a
    /// `077` umask cannot make it pass, and on Windows a DACL inherited from
    /// the temporary directory, which is not protected.
    #[test]
    fn an_ordinary_file_is_not_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"not a secret").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        let error = verify_owner_only(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other, "{error}");
        assert!(error.to_string().contains("is not owner-only"), "{error}");
    }

    /// The protection does not depend on the directory. On Windows, a
    /// directory whose DACL grants everyone full control and passes that on
    /// to every file created in it still yields an owner-only file, because
    /// the file's DACL is protected. On Unix the directory mode never reaches
    /// a file's mode, so this holds trivially.
    #[test]
    fn a_permissive_directory_does_not_widen_the_file() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        windows::grant_everyone_inheritable(dir.path()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        let path = dir.path().join("secret.key");
        write_private(&path, b"secret").unwrap();

        verify_owner_only(&path).unwrap();
    }

    #[test]
    fn the_file_holds_exactly_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_private(&path, b"\x00\x01\xfftwo\nlines\n").unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"\x00\x01\xfftwo\nlines\n",
            "binary content survives byte for byte"
        );
    }

    #[test]
    fn a_missing_parent_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("secret.key");
        write_private(&path, b"secret").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
        verify_owner_only(&path).unwrap();
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

    /// A failed rename removes the staging file before the error comes back,
    /// so the secret bytes do not stay on disk under a hidden name. A file
    /// cannot be renamed over a non-empty directory on any platform, and the
    /// staging file has already been written and flushed by then.
    #[test]
    fn a_failed_rename_leaves_no_staging_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        let occupant = path.join("occupant");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(&occupant, b"occupant").unwrap();

        write_private(&path, b"secret").unwrap_err();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            ["secret.key"],
            "the directory holds only the rename's target"
        );
        assert_eq!(std::fs::read(&occupant).unwrap(), b"occupant");
    }

    /// A rewrite goes through a fresh staging file, so its protection has to
    /// be right too, not just the first one's.
    #[test]
    fn a_second_write_replaces_the_first_and_stays_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        verify_owner_only(&path).unwrap();
    }

    /// A file written before this protection existed is replaced by one that
    /// has it: the rename brings the new file's descriptor, not the old one's.
    #[test]
    fn a_rewrite_over_an_ordinary_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        std::fs::write(&path, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        write_private(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        verify_owner_only(&path).unwrap();
    }

    /// The replacement is a rename, not a rewrite in place, on every platform.
    ///
    /// A hard link to the old file tells the two apart: a rewrite in place
    /// changes what the link reads too, and a rename only moves the name, so
    /// the link keeps the old bytes. A truncate-then-write is the shape that
    /// leaves a half-written key behind a crash.
    #[test]
    fn a_rewrite_renames_a_new_file_over_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        let link = dir.path().join("old.key");
        write_private(&path, b"first").unwrap();
        std::fs::hard_link(&path, &link).unwrap();

        write_private(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(
            std::fs::read(&link).unwrap(),
            b"first",
            "the old file was rewritten in place rather than replaced"
        );
    }

    /// An existing file at the staging name is never opened: the create is
    /// exclusive, so a planted file cannot receive the secret.
    #[test]
    fn creating_over_an_existing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("planted");
        std::fs::write(&path, b"planted").unwrap();

        let error = create_owner_only(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"planted");
    }
}
