//! Vault advisory lock.
//!
//! Per `docs/01-architecture/shared-core.md`: exactly one [`crate::Core`] per
//! vault path per process. The lock file (`<vault>/core.lock`) is acquired
//! at open time; on contention we wait up to 250ms then surface a typed
//! error containing the holder's PID + start timestamp (read from the lock
//! file's payload bytes).
//!
//! v1 implementation: a simple cross-platform best-effort version that
//! creates `core.lock` exclusively. Filesystem-level OS advisory locking
//! (`fcntl(F_OFD_SETLK)` / `LockFileEx`) is the production target; v1 is
//! good enough for single-process tests and gets tightened in Phase 17.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;

const LOCK_FILE: &str = "core.lock";
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(250);

/// Vault-lock errors.
#[derive(Debug, Error)]
pub enum VaultLockError {
    /// IO error.
    #[error("vault lock io error: {0}")]
    Io(#[from] std::io::Error),
    /// Another holder owned the lock at acquire time. The held lock-file
    /// contents are returned for surfacing to the user.
    #[error("vault locked by pid {holder_pid}, started at {holder_started_at}")]
    AlreadyHeld {
        /// PID written by the holder.
        holder_pid: u32,
        /// ISO 8601 timestamp the holder wrote.
        holder_started_at: String,
    },
}

/// Acquired vault lock; releases on Drop.
#[derive(Debug)]
pub struct VaultLock {
    path: PathBuf,
}

impl VaultLock {
    /// Try to acquire the lock at `<vault_dir>/core.lock`.
    ///
    /// `pid` and `started_at_iso` are written into the lock file so the
    /// next attempted holder can surface them in [`VaultLockError::AlreadyHeld`].
    // `Instant::now` is the determinism gate's disallowed monotonic clock, and
    // the exemption is deliberate: this is an OS-resource acquisition retry
    // loop, not a domain operation. Nothing here reaches the op log, so it
    // cannot affect replica convergence. The injected `Clock` is explicitly the
    // wrong tool — it is a *wall* clock, so a clock adjustment mid-acquire
    // would skew or hang the timeout.
    #[allow(clippy::disallowed_methods)]
    pub fn acquire(
        vault_dir: &Path,
        pid: u32,
        started_at_iso: &str,
    ) -> Result<Self, VaultLockError> {
        fs::create_dir_all(vault_dir)?;
        let path = vault_dir.join(LOCK_FILE);
        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let payload = format!("{pid}\n{started_at_iso}\n");
                    f.write_all(payload.as_bytes())?;
                    f.sync_all()?;
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() > deadline {
                        return Err(read_holder_err(&path));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(VaultLockError::Io(e)),
            }
        }
    }
}

fn read_holder_err(path: &Path) -> VaultLockError {
    let mut payload = String::new();
    let _ = OpenOptions::new()
        .read(true)
        .open(path)
        .and_then(|mut f| f.read_to_string(&mut payload));
    let mut iter = payload.lines();
    let holder_pid: u32 = iter.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let holder_started_at = iter
        .next()
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string());
    VaultLockError::AlreadyHeld {
        holder_pid,
        holder_started_at,
    }
}

impl Drop for VaultLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_acquire_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
        assert!(dir.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn second_acquire_fails_with_holder_info() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
        let res = VaultLock::acquire(dir.path(), 99, "2026-05-08T12:00:01Z");
        match res {
            Err(VaultLockError::AlreadyHeld {
                holder_pid,
                holder_started_at,
            }) => {
                assert_eq!(holder_pid, 42);
                assert_eq!(holder_started_at, "2026-05-08T12:00:00Z");
            }
            other => panic!("expected AlreadyHeld, got {other:?}"),
        }
    }

    #[test]
    fn drop_releases() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
        }
        // After drop, a fresh acquire succeeds.
        let _lock2 = VaultLock::acquire(dir.path(), 100, "2026-05-08T12:00:02Z").unwrap();
    }
}
