//! Vault advisory lock.
//!
//! Per `docs/01-architecture/shared-core.md`: exactly one [`crate::Core`] per
//! vault path per process. On contention we retry briefly, then surface a
//! typed error naming the holder's PID and start timestamp.
//!
//! # Why two files
//!
//! - `<vault>/core.lock` — a zero-byte **lock target**. Opened once and held
//!   `flock`ed (Unix) / `LockFileEx`ed (Windows) for the process lifetime, and
//!   **never unlinked**.
//! - `<vault>/core.lock.owner` — the `pid\nISO8601\n` payload, advisory only.
//!
//! The payload cannot live in `core.lock` itself, because `LockFileEx` locking
//! is *mandatory* on the locked byte range: a second process on Windows would
//! get `ERROR_LOCK_VIOLATION` trying to read it and could only ever report
//! PID 0 — on the platform where a useful error message matters most.
//!
//! `core.lock` is never unlinked for two reasons. On Unix, unlinking
//! reintroduces an ABA race: B opens the path, A unlinks it, C creates a fresh
//! inode and locks that — B and C then hold locks on *different inodes* and
//! both believe they won. On Windows the unlink would fail anyway
//! (`ERROR_SHARING_VIOLATION`) while a handle is open. A leftover empty
//! `core.lock` is the intended steady state, not litter.
//!
//! # Why the OS lock, and why it is not sufficient alone
//!
//! The previous implementation used `create_new` as the lock signal and
//! `remove_file` in `Drop` as the release. Combined with `panic = "abort"` in
//! the release profile, any SIGKILL, OOM kill, panic, or power loss left the
//! file behind and bricked the vault permanently. An OS advisory lock is
//! released by the kernel when the holding process dies, however it dies —
//! that is the entire point of this module.
//!
//! But an OS lock alone does not enforce the *in-process* invariant:
//!
//! - `flock` over NFS on Linux is emulated with `fcntl`, which is per-process,
//!   so two in-process Cores would both succeed.
//! - Some FUSE and network filesystems no-op `flock` entirely.
//!
//! So [`HELD`] — a process-local registry of canonicalized vault paths — is the
//! authority for same-process contention, and the OS lock is the authority
//! across processes.

use fs4::{FileExt, TryLockError};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use thiserror::Error;

const LOCK_FILE: &str = "core.lock";
const OWNER_FILE: &str = "core.lock.owner";
/// Retry budget. A fixed count rather than a deadline: the sleep is constant,
/// so this is exactly equivalent to the old 250ms timeout while needing no
/// clock at all — which keeps the determinism gate satisfied without an
/// exemption.
const ACQUIRE_ATTEMPTS: u32 = 13;
const RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// Vault-lock errors.
#[derive(Debug, Error)]
pub enum VaultLockError {
    /// IO error.
    #[error("vault lock io error: {0}")]
    Io(#[from] std::io::Error),
    /// Another holder owned the lock at acquire time. The holder's recorded
    /// identity is returned for surfacing to the user.
    #[error("vault locked by pid {holder_pid}, started at {holder_started_at}")]
    AlreadyHeld {
        /// PID recorded by the holder.
        holder_pid: u32,
        /// ISO 8601 timestamp the holder recorded.
        holder_started_at: String,
    },
}

#[derive(Debug, Clone)]
struct HolderInfo {
    pid: u32,
    started_at: String,
}

/// Vault dirs held by a live [`VaultLock`] in *this* process, keyed by
/// canonicalized path. See the module docs for why the OS lock is not enough.
static HELD: LazyLock<Mutex<HashMap<PathBuf, HolderInfo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII claim on [`HELD`].
///
/// This exists so an early `?` anywhere in [`VaultLock::acquire`] cannot leak a
/// claim. A leaked claim would make the vault unopenable for the rest of the
/// process lifetime — precisely the bug this module was rewritten to remove,
/// scoped to one process. `armed` is cleared only on the success path.
#[derive(Debug)]
struct RegistryClaim {
    key: PathBuf,
    armed: bool,
}

impl RegistryClaim {
    /// `Err(existing)` if another live `VaultLock` in this process holds `key`.
    fn try_claim(key: PathBuf, me: HolderInfo) -> Result<Self, HolderInfo> {
        let mut g = HELD.lock();
        if let Some(existing) = g.get(&key) {
            return Err(existing.clone());
        }
        g.insert(key.clone(), me);
        Ok(Self { key, armed: true })
    }

    fn disarm(mut self) -> PathBuf {
        self.armed = false;
        self.key.clone()
    }
}

impl Drop for RegistryClaim {
    fn drop(&mut self) {
        if self.armed {
            HELD.lock().remove(&self.key);
        }
    }
}

/// Acquired vault lock. Releases on `Drop`, and on process death by any means.
#[derive(Debug)]
pub struct VaultLock {
    /// Held open for the lock's lifetime. Closing this handle — for any
    /// reason, including SIGKILL, `abort()`, or power loss — is what releases
    /// the OS lock.
    file: File,
    /// Canonicalized vault dir; the [`HELD`] key.
    key: PathBuf,
}

impl VaultLock {
    /// Try to acquire the vault lock.
    ///
    /// `pid` and `started_at_iso` are recorded in `core.lock.owner` so the next
    /// attempted holder can surface them in [`VaultLockError::AlreadyHeld`].
    pub fn acquire(
        vault_dir: &Path,
        pid: u32,
        started_at_iso: &str,
    ) -> Result<Self, VaultLockError> {
        fs::create_dir_all(vault_dir)?;
        // Canonicalize after create_dir_all so the registry key is stable
        // across relative paths, `..` segments, and symlinks. On Windows this
        // also normalizes drive-letter case, so one dir maps to one key.
        let key = fs::canonicalize(vault_dir)?;
        let lock_path = key.join(LOCK_FILE);
        let owner_path = key.join(OWNER_FILE);
        let me = HolderInfo {
            pid,
            started_at: started_at_iso.to_string(),
        };

        for attempt in 0..ACQUIRE_ATTEMPTS {
            let last = attempt + 1 == ACQUIRE_ATTEMPTS;

            // 1. In-process claim first: exact, immediate, filesystem-independent.
            let claim = match RegistryClaim::try_claim(key.clone(), me.clone()) {
                Ok(c) => c,
                Err(existing) => {
                    if last {
                        return Err(VaultLockError::AlreadyHeld {
                            holder_pid: existing.pid,
                            holder_started_at: existing.started_at,
                        });
                    }
                    std::thread::sleep(RETRY_INTERVAL);
                    continue;
                }
            };

            // 2. Open the lock target. `create`, not `create_new` — the file
            //    legitimately survives every previous run, and its existence
            //    carries no meaning. Only the OS lock does. Never truncate: a
            //    live holder may have the same file open.
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)?; // `?` drops `claim`, releasing it — by design

            // 3. Non-blocking OS advisory lock.
            match FileExt::try_lock(&file) {
                Ok(()) => {
                    // Only now, holding the lock, is it safe to publish identity.
                    write_owner(&owner_path, pid, started_at_iso);
                    return Ok(Self {
                        file,
                        key: claim.disarm(),
                    });
                }
                Err(TryLockError::WouldBlock) => {
                    // Release the in-process claim before sleeping, so a
                    // same-process contender can make progress.
                    drop(claim);
                    drop(file);
                    if last {
                        return Err(read_holder_err(&owner_path));
                    }
                    std::thread::sleep(RETRY_INTERVAL);
                }
                Err(TryLockError::Error(e)) => return Err(VaultLockError::Io(e)),
            }
        }
        // Unreachable: the final attempt always returns.
        Err(read_holder_err(&owner_path))
    }
}

/// Best-effort. The payload is explicitly non-authoritative — it exists only
/// for the error message — so a write failure must never fail an otherwise-good
/// acquire. No fsync: after a crash the contents are meaningless anyway.
fn write_owner(owner_path: &Path, pid: u32, started_at_iso: &str) {
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(owner_path)
        .and_then(|mut f| f.write_all(format!("{pid}\n{started_at_iso}\n").as_bytes()));
}

fn read_holder_err(owner_path: &Path) -> VaultLockError {
    let mut payload = String::new();
    let _ = OpenOptions::new()
        .read(true)
        .open(owner_path)
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
        // Order matters: release the OS lock before the registry entry, so a
        // same-process retry loop cannot win the registry and then lose the OS
        // lock in a tight spin. Neither file is unlinked (see module docs).
        let _ = FileExt::unlock(&self.file);
        HELD.lock().remove(&self.key);
        // `self.file` closes here; on abnormal exit the OS does this for us.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_acquire_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
        assert!(dir.path().join(LOCK_FILE).exists());
        let owner = fs::read_to_string(dir.path().join(OWNER_FILE)).unwrap();
        assert_eq!(owner, "42\n2026-05-08T12:00:00Z\n");
        drop(lock);
    }

    #[test]
    fn second_acquire_fails_with_holder_info() {
        let dir = tempfile::tempdir().unwrap();
        let lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
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
        drop(lock);
    }

    #[test]
    fn drop_releases() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
        }
        let lock2 = VaultLock::acquire(dir.path(), 100, "2026-05-08T12:00:02Z").unwrap();
        drop(lock2);
    }

    /// The regression test for the brick bug: a `core.lock` left behind by a
    /// killed process must not block a fresh acquire.
    #[test]
    fn stale_lock_file_from_previous_run_is_acquirable() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(LOCK_FILE), b"garbage from a crashed run").unwrap();
        fs::write(
            dir.path().join(OWNER_FILE),
            b"999999\n2020-01-01T00:00:00Z\n",
        )
        .unwrap();
        let lock = VaultLock::acquire(dir.path(), 7, "2026-05-08T12:00:00Z")
            .expect("a stale lock file must not brick the vault");
        drop(lock);
    }

    /// Pins the ABA-hazard invariant: releasing must not unlink the target.
    #[test]
    fn lock_file_is_not_unlinked_on_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
        drop(lock);
        assert!(
            dir.path().join(LOCK_FILE).exists(),
            "core.lock must survive release; unlinking reopens an ABA race"
        );
        let again = VaultLock::acquire(dir.path(), 43, "2026-05-08T12:00:01Z").unwrap();
        drop(again);
    }

    /// Proves `RegistryClaim` releases on the `?` path. Without the RAII guard
    /// an IO failure would leak a claim and brick the vault process-wide.
    #[test]
    fn io_error_does_not_poison_registry() {
        let dir = tempfile::tempdir().unwrap();
        // Make core.lock a directory so opening it as a file fails.
        fs::create_dir(dir.path().join(LOCK_FILE)).unwrap();
        for _ in 0..2 {
            match VaultLock::acquire(dir.path(), 1, "2026-05-08T12:00:00Z") {
                Err(VaultLockError::Io(_)) => {}
                other => panic!("expected Io error, got {other:?}"),
            }
        }
    }

    /// Exactly one winner under concurrent same-process contention — the
    /// registry, not just the OS lock.
    ///
    /// Each thread must *hold* its guard until every thread has finished
    /// trying. Dropping it inside the closure would release the lock
    /// immediately and let all N threads succeed in sequence, which measures
    /// nothing.
    #[test]
    fn concurrent_threads_exactly_one_wins() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let n = 8;
        let start = Arc::new(Barrier::new(n));
        let hold = Arc::new(Barrier::new(n));
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let path = path.clone();
                let start = Arc::clone(&start);
                let hold = Arc::clone(&hold);
                std::thread::spawn(move || {
                    start.wait();
                    let guard = VaultLock::acquire(&path, u32::try_from(i).unwrap(), "2026-05-08T12:00:00Z");
                    let won = guard.is_ok();
                    // Keep any acquired lock alive until every thread has had
                    // its turn, then release.
                    hold.wait();
                    drop(guard);
                    won
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .fold(0usize, |acc, h| acc + usize::from(h.join().unwrap()));
        assert_eq!(wins, 1, "exactly one of {n} threads may hold the vault");
    }

    /// Distinct spellings of the same directory must collide, or two Cores
    /// could open one vault via different paths.
    #[test]
    fn distinct_paths_to_same_dir_collide() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let lock = VaultLock::acquire(dir.path(), 42, "2026-05-08T12:00:00Z").unwrap();
        let indirect = sub.join("..");
        match VaultLock::acquire(&indirect, 99, "2026-05-08T12:00:01Z") {
            Err(VaultLockError::AlreadyHeld { holder_pid, .. }) => assert_eq!(holder_pid, 42),
            other => panic!("expected AlreadyHeld via a non-canonical path, got {other:?}"),
        }
        drop(lock);
    }

    /// The payload format is a contract: `read_holder_err` parses it, and the
    /// architecture doc states the 64-byte bound.
    #[test]
    fn owner_payload_format_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let lock = VaultLock::acquire(dir.path(), 123_456, "2026-05-08T12:00:00Z").unwrap();
        let owner = fs::read(dir.path().join(OWNER_FILE)).unwrap();
        assert_eq!(owner, b"123456\n2026-05-08T12:00:00Z\n");
        assert!(owner.len() <= 64, "payload must stay within 64 bytes");
        drop(lock);
    }
}
