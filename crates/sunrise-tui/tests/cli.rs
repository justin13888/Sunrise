//! End-to-end tests for the non-interactive subcommands.
//!
//! These run the **real binary** against a real vault via
//! `CARGO_BIN_EXE_sunrise-tui` (a path cargo sets for integration tests), so
//! they cover the whole stack — arg parsing, `Core::open`, the vault lock,
//! `SQLCipher` persistence, the capture parser, and the query path — without a
//! terminal and without a new dependency.
//!
//! Everything the interactive TUI does goes through the same `Core` API, so a
//! green run here is real evidence the app works, not just that it compiles.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sunrise-tui")
}

fn run(vault: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("SUNRISE_VAULT", vault)
        // Keep the relay out of it: subcommands are one-shot and offline.
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_EXPORT_CERT_FILE")
        .env_remove("SUNRISE_TRUST_CERT_FILE")
        .output()
        .expect("run sunrise-tui")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn capture_commits_and_is_readable_back() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["capture", "Renew passport !1 ~1h"]);
    assert!(out.status.success(), "capture failed: {out:?}");
    let line = stdout(&out);
    assert!(
        line.starts_with("tsk_"),
        "capture must print the new task id, got {line:?}"
    );
    assert!(line.contains("Renew passport"));

    // A separate process must see it — proves it was persisted and that the
    // vault lock was released on exit rather than stranding the vault.
    let inbox = stdout(&run(dir.path(), &["inbox"]));
    assert!(
        inbox.contains("Renew passport"),
        "a second process must see the committed task, got {inbox:?}"
    );
}

/// The capture parser's annotations must survive all the way to storage, not
/// just to the draft.
#[test]
fn capture_applies_annotations_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    // `^+6h` lands inside Today's rolling 24h window; `^+30h` does not. This
    // pins the window boundary through the real query path.
    run(dir.path(), &["capture", "Standup ^+6h !2"]);
    run(dir.path(), &["capture", "Far future ^+30h"]);
    run(dir.path(), &["capture", "Unscheduled thing"]);

    let today = stdout(&run(dir.path(), &["today"]));
    assert!(
        today.contains("Standup"),
        "in-window task missing: {today:?}"
    );
    assert!(
        !today.contains("Far future"),
        "a task 30h out must not appear in Today: {today:?}"
    );
    assert!(
        !today.contains("Unscheduled thing"),
        "an unscheduled task must not appear in Today: {today:?}"
    );
}

#[test]
fn capture_resolves_a_stream_by_name() {
    let dir = tempfile::tempdir().unwrap();
    // The synthetic Inbox row is resolvable by name with no setup.
    let out = run(dir.path(), &["capture", "Triage me #inbox"]);
    assert!(out.status.success());
    let streams = stdout(&run(dir.path(), &["streams"]));
    assert!(streams.contains("Inbox"), "got {streams:?}");
    assert!(
        streams.contains("1 open"),
        "the captured task must count against Inbox: {streams:?}"
    );
}

/// The parser's governing invariant, asserted through the binary: an
/// unresolvable annotation warns but never silently eats the user's text.
#[test]
fn unresolved_annotation_warns_but_still_commits_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["capture", "Thing #nosuchstream"]);
    assert!(out.status.success(), "an unknown stream must not fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("UnknownStream"),
        "the warning must go to stderr so stdout stays parseable: {err:?}"
    );
    assert!(
        stdout(&out).contains("#nosuchstream"),
        "the typed text must survive into the title"
    );
}

#[test]
fn search_finds_a_captured_task() {
    let dir = tempfile::tempdir().unwrap();
    run(dir.path(), &["capture", "Renew passport"]);
    run(dir.path(), &["capture", "Email Sara"]);
    let hits = stdout(&run(dir.path(), &["search", "passport"]));
    assert!(hits.contains("Renew passport"), "got {hits:?}");
    assert!(!hits.contains("Email Sara"), "search must filter: {hits:?}");
}

#[test]
fn empty_capture_is_rejected_rather_than_creating_a_blank_task() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["capture", "   "]);
    assert!(!out.status.success(), "a blank capture must fail loudly");
    assert!(stdout(&run(dir.path(), &["inbox"])).trim().is_empty());
}

#[test]
fn unknown_subcommand_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["frobnicate"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown subcommand"));
}

#[test]
fn help_and_version_succeed() {
    let dir = tempfile::tempdir().unwrap();
    for args in [vec!["help"], vec!["--help"], vec!["-h"]] {
        let out = run(dir.path(), &args);
        assert!(out.status.success(), "{args:?} should succeed");
        assert!(stdout(&out).contains("USAGE"));
    }
    let v = run(dir.path(), &["--version"]);
    assert!(v.status.success());
    assert!(stdout(&v).contains("sunrise-tui"));
}

/// Regression for the vault-lock brick bug, at the level a user would hit it:
/// running the CLI repeatedly against one vault must keep working.
#[test]
fn repeated_runs_reacquire_the_vault_lock() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        let out = run(dir.path(), &["capture", &format!("task {i}")]);
        assert!(out.status.success(), "run {i} failed: {out:?}");
    }
    assert!(
        dir.path().join("core.lock").exists(),
        "lock target persists"
    );
    assert_eq!(stdout(&run(dir.path(), &["inbox"])).lines().count(), 5);
}
