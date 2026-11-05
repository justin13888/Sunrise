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

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use sunrise_core::vault_lock::{VaultLock, VaultLockError};

/// Env var carrying the vault dir from parent to child.
const CHILD_DIR_ENV: &str = "SUNRISE_VAULT_LOCK_CHILD_DIR";
/// Line the child prints on stdout once it holds the lock. The parent blocks on
/// the pipe until it reads this line or the pipe closes, so readiness is an
/// event rather than a real-time budget a loaded host could overrun.
const READY_LINE: &str = "SUNRISE_VAULT_LOCK_CHILD_READY";

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
    signal_ready();
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
    signal_ready();
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
    signal_ready();
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
    wait_ready(&mut child);

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
    wait_ready(&mut child);
    let _ = child.wait();

    let lock = VaultLock::acquire(dir.path(), 1, "2026-01-01T00:00:01Z")
        .expect("an aborted holder must leave the vault acquirable");
    drop(lock);
}

#[test]
fn normal_child_exit_releases_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn_child("child_acquires_then_returns", dir.path());
    wait_ready(&mut child);
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
    wait_ready(&mut child);
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
    wait_ready(&mut child);

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
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn child test process")
}

/// Tell the parent this child holds the lock. The explicit flush puts the line
/// on the pipe before the child parks, aborts, or exits.
fn signal_ready() {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{READY_LINE}").expect("write ready line");
    out.flush().expect("flush ready line");
}

/// Block until the child signals that it holds the lock, failing loudly if it
/// dies first — otherwise a child that panicked would show up as a confusing
/// hang instead of the real error. The read ends at the ready line or at end
/// of file, which the child's exit guarantees, so there is no deadline to tune.
fn wait_ready(child: &mut Child) {
    let stdout = child.stdout.take().expect("child stdout is piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read child stdout") == 0 {
            break;
        }
        // libtest may print the test name on the same line, so match within it.
        if line.contains(READY_LINE) {
            // Keep the pipe drained until the child exits: closing it would
            // fail the child's later libtest output with a broken pipe.
            std::thread::spawn(move || std::io::copy(&mut reader, &mut std::io::sink()));
            return;
        }
    }
    let status = child.wait().expect("reap child");
    panic!("child exited before acquiring the lock: {status}");
}
