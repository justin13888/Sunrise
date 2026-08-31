//! Cross-process proof that the vault lock survives an abnormal death.
//!
//! This is the regression suite for the brick bug: the previous
//! `create_new` + `Drop`-only implementation left `core.lock` on disk after any
//! uncatchable termination, and every later `Core::open` failed with
//! `AlreadyHeld` forever.
//!
//! # How the crash is simulated
//!
//! `std::process::Child::kill()` sends `SIGKILL` on Unix and calls
//! `TerminateProcess` on Windows. Neither can be caught or handled: no
//! destructor, no `Drop`, no atexit hook, no unwinding. That is precisely the
//! failure mode that used to brick the vault, and it needs no extra dependency
//! and no `unsafe`.
//!
//! The child is this same test binary, re-executed with `--ignored --exact` to
//! select a `#[ignore]`d test that acquires the lock and parks. A normal
//! `cargo test` run therefore never executes the child body directly.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;
use sunrise_core::vault_lock::{VaultLock, VaultLockError};

/// Env var carrying the vault dir from parent to child.
const CHILD_DIR_ENV: &str = "SUNRISE_VAULT_LOCK_CHILD_DIR";
/// Sentinel the child writes once it holds the lock.
const READY_FILE: &str = "child.ready";
/// Polling budget: 300 × 10ms = 3s. A bounded count rather than a deadline
/// because `Instant::now` is denied by the determinism gate, and this needs no
/// clock — the sleep is constant.
const READY_POLLS: u32 = 300;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// Child bodies. `#[ignore]` so a plain `cargo test` never runs them; the parent
// re-invokes this binary and selects one by exact name.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "child process body, spawned by the parent tests in this file"]
fn child_holds_lock_forever() {
    let dir = child_dir();
    let _lock = VaultLock::acquire(&dir, std::process::id(), "2026-01-01T00:00:00Z")
        .expect("child must acquire the lock");
    std::fs::write(dir.join(READY_FILE), b"1").unwrap();
    // Park until the parent kills us. Never returns.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

#[test]
#[ignore = "child process body, spawned by the parent tests in this file"]
fn child_acquires_then_aborts() {
    let dir = child_dir();
    let _lock = VaultLock::acquire(&dir, std::process::id(), "2026-01-01T00:00:00Z")
        .expect("child must acquire the lock");
    std::fs::write(dir.join(READY_FILE), b"1").unwrap();
    // Models `panic = "abort"` from the release profile: the specific
    // interaction that made the old bug unrecoverable.
    std::process::abort();
}

#[test]
#[ignore = "child process body, spawned by the parent tests in this file"]
fn child_acquires_then_returns() {
    let dir = child_dir();
    let lock = VaultLock::acquire(&dir, std::process::id(), "2026-01-01T00:00:00Z")
        .expect("child must acquire the lock");
    std::fs::write(dir.join(READY_FILE), b"1").unwrap();
    drop(lock);
}

// ---------------------------------------------------------------------------
// Parent tests.
// ---------------------------------------------------------------------------

/// The headline test: a `SIGKILL`ed holder must not brick the vault.
#[test]
fn sigkilled_holder_releases_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_child("child_holds_lock_forever", dir.path());
    wait_ready(dir.path(), &mut child);

    child.kill().expect("kill child");
    child.wait().expect("reap child");

    let lock = VaultLock::acquire(dir.path(), 1, "2026-01-01T00:00:01Z")
        .expect("a SIGKILLed holder must leave the vault acquirable");
    drop(lock);
}

/// `panic = "abort"` is set in the release profile, so an aborting holder is
/// the realistic crash, not a theoretical one.
#[test]
fn aborted_holder_releases_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_child("child_acquires_then_aborts", dir.path());
    wait_ready(dir.path(), &mut child);
    let _ = child.wait();

    let lock = VaultLock::acquire(dir.path(), 1, "2026-01-01T00:00:01Z")
        .expect("an aborted holder must leave the vault acquirable");
    drop(lock);
}

#[test]
fn normal_child_exit_releases_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_child("child_acquires_then_returns", dir.path());
    wait_ready(dir.path(), &mut child);
    let status = child.wait().expect("reap child");
    assert!(status.success(), "child should exit cleanly");

    let lock = VaultLock::acquire(dir.path(), 1, "2026-01-01T00:00:01Z").unwrap();
    drop(lock);
}

/// A live cross-process holder must still be reported by PID — the payload
/// moved to `core.lock.owner`, so this proves the split didn't lose the
/// diagnostic.
#[test]
fn cross_process_contention_reports_child_pid() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_child("child_holds_lock_forever", dir.path());
    wait_ready(dir.path(), &mut child);
    let child_pid = child.id();

    match VaultLock::acquire(dir.path(), 1, "2026-01-01T00:00:01Z") {
        Err(VaultLockError::AlreadyHeld {
            holder_pid,
            holder_started_at,
        }) => {
            assert_eq!(holder_pid, child_pid, "must name the live holder");
            assert_eq!(holder_started_at, "2026-01-01T00:00:00Z");
        }
        other => panic!("expected AlreadyHeld while the child holds it, got {other:?}"),
    }

    child.kill().unwrap();
    child.wait().unwrap();
}

/// The `fcntl` footgun regression test.
///
/// POSIX `fcntl` locks are dropped when *any* descriptor for the file is closed
/// by the owning process. Under an `fcntl`-based design, a contender opening
/// and closing the lock path to read holder info would silently destroy the
/// holder's lock. `flock` locks belong to the open file description, so they
/// survive this. Assert it.
#[test]
fn lock_survives_contender_reading_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_child("child_holds_lock_forever", dir.path());
    wait_ready(dir.path(), &mut child);

    for _ in 0..20 {
        let _ = std::fs::read(dir.path().join("core.lock.owner"));
        let _ = std::fs::read(dir.path().join("core.lock"));
    }

    assert!(
        matches!(
            VaultLock::acquire(dir.path(), 1, "2026-01-01T00:00:01Z"),
            Err(VaultLockError::AlreadyHeld { .. })
        ),
        "the child's lock must survive a contender opening and closing the files"
    );

    child.kill().unwrap();
    child.wait().unwrap();
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn child_dir() -> PathBuf {
    PathBuf::from(
        std::env::var(CHILD_DIR_ENV)
            .expect("child body invoked without the vault dir env var; run via the parent tests"),
    )
}

fn spawn_child(test_name: &str, dir: &Path) -> Child {
    let exe = std::env::current_exe().expect("current test binary path");
    Command::new(exe)
        .args(["--ignored", "--exact", test_name, "--nocapture"])
        .env(CHILD_DIR_ENV, dir)
        .spawn()
        .expect("spawn child test process")
}

/// Block until the child signals that it holds the lock, failing loudly if it
/// dies first — otherwise a child that panicked would show up as a confusing
/// timeout instead of the real error.
fn wait_ready(dir: &Path, child: &mut Child) {
    let ready = dir.join(READY_FILE);
    for _ in 0..READY_POLLS {
        if ready.exists() {
            return;
        }
        if let Some(status) = child.try_wait().expect("poll child") {
            panic!("child exited before acquiring the lock: {status}");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let _ = child.kill();
    panic!("child never signalled ready");
}
