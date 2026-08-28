//! End-to-end tests for the command-line client.
//!
//! These run the **real binary** against a real vault via
//! `CARGO_BIN_EXE_sunrise` (a path cargo sets for integration tests), so they
//! cover the whole stack — arg parsing, `Core::open`, the vault lock,
//! `SQLCipher` persistence, the capture parser, and the query path — in a
//! separate process, with no terminal and no GUI.
//!
//! Every client goes through the same `Core` API, so a green run here is real
//! evidence the vault works end to end, not just that it compiles. This is
//! what keeps the core reachable and provable without a client.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sunrise")
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
        .expect("run sunrise")
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
    assert!(stdout(&v).starts_with("sunrise "));
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

/// `docs/07-clients/tui.md` §Capture from anywhere names `sunrise focus next`
/// as a first-class surface. The picks must be the *core's* ranking, not a
/// re-sort in the CLI: "what should I do next" has to give the same answer in
/// the terminal and in the TUI, or one of them is lying.
#[test]
fn next_ranks_by_leverage_and_focus_next_opens_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = id_of(&run(dir.path(), &["capture", "wait for the survey"]));
    let leaf = id_of(&run(dir.path(), &["capture", "book the movers"]));
    assert!(!blocker.is_empty() && !leaf.is_empty());

    let picks = stdout(&run(dir.path(), &["next"]));
    assert!(picks.contains("wait for the survey"), "got {picks:?}");
    assert!(picks.contains("unblocks"), "each row says why: {picks:?}");

    // `focus next` opens a session on the top pick and says so.
    let out = run(dir.path(), &["focus", "next"]);
    assert!(out.status.success(), "focus next failed: {out:?}");
    assert!(stdout(&out).contains("focus started on"), "{out:?}");
}

#[test]
fn done_completes_by_id_and_the_review_counts_it() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "file the tax return"]));
    let out = run(dir.path(), &["done", &id]);
    assert!(out.status.success(), "done failed: {out:?}");

    let review = stdout(&run(dir.path(), &["review"]));
    assert!(review.contains("completed 1"), "got {review:?}");
    assert!(review.contains("created 1"), "got {review:?}");
}

#[test]
fn done_refuses_something_that_is_not_a_task_id() {
    // A script that mistypes an id must fail loudly, not complete nothing and
    // exit 0.
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["done", "not-an-id"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
}

/// A CLI's stdout is its contract: `export … | jq` has to work, which is the
/// opposite of the interactive `:export` (that one writes a file, because the
/// alternate screen is no place for a CSV).
#[test]
fn export_writes_a_parseable_document_to_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let id = id_of(&run(dir.path(), &["capture", "something to finish"]));
    run(dir.path(), &["done", &id]);

    let json = stdout(&run(dir.path(), &["export", "trends", "json"]));
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(parsed["table"], "trends");
    assert!(parsed["rows"].as_array().is_some_and(|r| !r.is_empty()));

    let csv = stdout(&run(dir.path(), &["export", "trends", "csv"]));
    assert!(
        csv.lines().next().unwrap().contains("week_start_ms"),
        "{csv}"
    );

    // …and a path argument writes there instead.
    let path = dir.path().join("out.csv");
    let out = run(
        dir.path(),
        &["export", "trends", "csv", path.to_str().unwrap()],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("week_start_ms"));
}

#[test]
fn export_rejects_a_dataset_it_does_not_have() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["export", "vibes"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
}

#[test]
fn contexts_and_routines_are_listable_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing yet: an empty listing is a successful listing.
    assert!(run(dir.path(), &["contexts"]).status.success());
    assert!(run(dir.path(), &["routines"]).status.success());
}

/// `sync --once` is for cron. Without a relay configured it must fail fast and
/// loudly rather than blocking a scheduled job forever.
#[test]
fn sync_once_refuses_to_hang_when_no_relay_is_configured() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["sync", "--once"]);
    assert!(!out.status.success(), "expected a non-zero exit: {out:?}");
    let err = format!("{}{}", String::from_utf8_lossy(&out.stderr), stdout(&out));
    assert!(err.contains("SUNRISE_SYNC_URL"), "got {err:?}");
}

/// First whitespace-separated token of the first stdout line — the id every
/// mutating subcommand prints first.
fn id_of(o: &Output) -> String {
    stdout(o)
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `SUNRISE_EXPORT_CERT_FILE` is documented as writing this device's cert "on
/// startup", for every subcommand.
///
/// Regression: `run` used to open the vault with `SyncPlan::default()` and
/// never build one from the environment, so the binary ignored all three
/// cert/relay variables. The unit tests for `plan_from_env` passed the whole
/// time — nothing called it.
#[test]
fn a_subcommand_exports_this_devices_cert_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("device.cert");

    let out = Command::new(bin())
        .args(["inbox"])
        .env("SUNRISE_VAULT", dir.path())
        .env("SUNRISE_EXPORT_CERT_FILE", &cert)
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_TRUST_CERT_FILE")
        .output()
        .expect("run sunrise");

    assert!(out.status.success(), "inbox failed: {out:?}");
    let bytes = std::fs::read(&cert).expect("the cert must have been written");
    assert!(!bytes.is_empty(), "an empty cert is not a cert");
    // stdout stays the contract: the demo banner goes to stderr.
    assert!(
        !stdout(&out).contains("exported device cert"),
        "the startup banner must not pollute stdout"
    );
}

/// A peer cert named in the environment is trusted at startup, so the
/// two-replica walkthrough works from the binary and not only from the
/// library.
#[test]
fn a_subcommand_trusts_a_peer_cert_when_asked() {
    let peer = tempfile::tempdir().unwrap();
    let peer_cert = peer.path().join("peer.cert");
    let out = Command::new(bin())
        .args(["inbox"])
        .env("SUNRISE_VAULT", peer.path())
        .env("SUNRISE_EXPORT_CERT_FILE", &peer_cert)
        .env_remove("SUNRISE_SYNC_URL")
        .output()
        .expect("run sunrise");
    assert!(out.status.success(), "peer setup failed: {out:?}");

    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["inbox"])
        .env("SUNRISE_VAULT", dir.path())
        .env("SUNRISE_TRUST_CERT_FILE", &peer_cert)
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_EXPORT_CERT_FILE")
        .output()
        .expect("run sunrise");

    assert!(out.status.success(), "trust failed: {out:?}");
    let banner = String::from_utf8_lossy(&out.stderr);
    assert!(
        banner.contains("trusted peer cert"),
        "the peer cert was not trusted, stderr was {banner:?}"
    );
}

// ---------------------------------------------------------------------------
// iCalendar interchange (`docs/09-integrations/icalendar.md`)
// ---------------------------------------------------------------------------

/// An `.ics` document with one timed event inside the current civil week, so
/// `ical export week` covers it whenever the suite happens to run.
///
/// The binary reads the real clock — every subcommand does — so the fixture is
/// built against the same clock rather than against a frozen date that would
/// leave this test passing only in one particular week of 2026.
fn ics_this_week(uid: &str, summary: &str) -> String {
    let start = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .start_of_day()
        .expect("start of day")
        .timestamp();
    let end = start + jiff::SignedDuration::from_hours(1);
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n\
BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\n\
DTSTART:{}\r\nDTEND:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        start.strftime("%Y%m%dT%H%M%SZ"),
        end.strftime("%Y%m%dT%H%M%SZ"),
    )
}

fn write_ics(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fixture");
    path
}

/// The property the whole import path exists to hold: importing the same file
/// twice gives one calendar, not two.
#[test]
fn ical_import_is_idempotent_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(
        dir.path(),
        "cal.ics",
        &ics_this_week("ev1@example.com", "Quarterly planning"),
    );
    let path = ics.to_str().unwrap();

    let first = run(dir.path(), &["ical", "import", path]);
    assert!(first.status.success(), "import failed: {first:?}");
    let first_out = stdout(&first);
    assert!(
        first_out.starts_with("new  blk_"),
        "a first import must report a new block: {first_out:?}"
    );
    assert!(first_out.contains("Quarterly planning"));

    // A separate process, so the dedup cannot be coming from in-memory state.
    let second = run(dir.path(), &["ical", "import", path]);
    assert!(second.status.success(), "re-import failed: {second:?}");
    let second_out = stdout(&second);
    assert!(
        second_out.starts_with("upd  blk_"),
        "a re-import must update, not create: {second_out:?}"
    );
    assert_eq!(
        second_out.split_whitespace().nth(1),
        first_out.split_whitespace().nth(1),
        "the same UID must land on the same block id"
    );

    // And the calendar really does hold one event, not two.
    let exported = stdout(&run(dir.path(), &["ical", "export", "week"]));
    assert_eq!(
        exported.matches("BEGIN:VEVENT").count(),
        1,
        "one event, not two: {exported}"
    );
}

/// `-` reads stdin, so `curl … | sunrise ical import -` needs no temp file.
#[test]
fn ical_import_reads_stdin() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let body = ics_this_week("stdin@example.com", "From a pipe");

    let mut child = Command::new(bin())
        .args(["ical", "import", "-"])
        .env("SUNRISE_VAULT", dir.path())
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_EXPORT_CERT_FILE")
        .env_remove("SUNRISE_TRUST_CERT_FILE")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sunrise");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(body.as_bytes())
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait");

    assert!(out.status.success(), "stdin import failed: {out:?}");
    assert!(
        stdout(&out).contains("From a pipe"),
        "got {:?}",
        stdout(&out)
    );
}

/// stdout is the contract. The document goes there; the notices do not.
#[test]
fn ical_export_writes_the_document_to_stdout_and_notices_to_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(
        dir.path(),
        "cal.ics",
        // DESCRIPTION and LOCATION are real content a Block cannot hold; the
        // import must say so on stderr and still commit the event.
        &ics_this_week("ev1@example.com", "Design review").replace(
            "END:VEVENT",
            "DESCRIPTION:Bring the roadmap\r\nLOCATION:Room 4B\r\nEND:VEVENT",
        ),
    );
    let imported = run(dir.path(), &["ical", "import", ics.to_str().unwrap()]);
    assert!(imported.status.success());
    let notes = String::from_utf8_lossy(&imported.stderr);
    assert!(notes.contains("DESCRIPTION"), "got {notes:?}");
    assert!(notes.contains("LOCATION"), "got {notes:?}");
    assert!(
        !stdout(&imported).contains("DESCRIPTION"),
        "notices must not pollute stdout"
    );

    let out = run(dir.path(), &["ical", "export", "week"]);
    assert!(out.status.success(), "export failed: {out:?}");
    let doc = stdout(&out);
    assert!(doc.starts_with("BEGIN:VCALENDAR\r\n"), "got {doc:?}");
    assert!(doc.trim_end().ends_with("END:VCALENDAR"));
    assert!(doc.contains("SUMMARY:Design review"));
}

/// With a path the document is written there and only the path is printed,
/// exactly as `sunrise export <dataset> … <path>` behaves.
#[test]
fn ical_export_writes_to_a_path_when_given_one() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(dir.path(), "cal.ics", &ics_this_week("ev1", "Standup"));
    run(dir.path(), &["ical", "import", ics.to_str().unwrap()]);

    let target = dir.path().join("out.ics");
    let out = run(
        dir.path(),
        &["ical", "export", "week", target.to_str().unwrap()],
    );
    assert!(out.status.success(), "export failed: {out:?}");
    assert_eq!(stdout(&out).trim(), target.to_str().unwrap());
    let written = std::fs::read_to_string(&target).expect("the file must exist");
    assert!(written.contains("SUMMARY:Standup"), "got {written:?}");
}

/// Exporting and re-importing must be the identity, or a user who round-trips
/// their own calendar doubles it.
#[test]
fn ical_export_then_import_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(dir.path(), "cal.ics", &ics_this_week("ev1", "Standup"));
    run(dir.path(), &["ical", "import", ics.to_str().unwrap()]);

    let target = dir.path().join("out.ics");
    run(
        dir.path(),
        &["ical", "export", "week", target.to_str().unwrap()],
    );
    let back = run(dir.path(), &["ical", "import", target.to_str().unwrap()]);
    assert!(back.status.success(), "re-import failed: {back:?}");
    assert!(
        stdout(&back).starts_with("upd  "),
        "an exported calendar must re-import onto itself: {:?}",
        stdout(&back)
    );

    let doc = stdout(&run(dir.path(), &["ical", "export", "week"]));
    assert_eq!(doc.matches("BEGIN:VEVENT").count(), 1, "got {doc}");
}

/// `--source` is the escape hatch for two calendars that share a UID.
#[test]
fn ical_import_keys_on_the_source_as_well_as_the_uid() {
    let dir = tempfile::tempdir().unwrap();
    let ics = write_ics(dir.path(), "cal.ics", &ics_this_week("shared", "Standup"));
    let path = ics.to_str().unwrap();
    run(dir.path(), &["ical", "import", path]);
    let other = run(dir.path(), &["ical", "import", path, "--source", "team"]);
    assert!(other.status.success(), "{other:?}");
    assert!(
        stdout(&other).starts_with("new  "),
        "a second source is a second block: {:?}",
        stdout(&other)
    );

    let doc = stdout(&run(dir.path(), &["ical", "export", "week"]));
    assert_eq!(doc.matches("BEGIN:VEVENT").count(), 2, "got {doc}");
}

/// A malformed file must fail loudly rather than reporting an import of zero
/// events, and a bad argument must not be guessed at.
#[test]
fn ical_refuses_input_that_is_not_a_calendar() {
    let dir = tempfile::tempdir().unwrap();
    let junk = write_ics(dir.path(), "junk.ics", "this is not a calendar\n");
    let out = run(dir.path(), &["ical", "import", junk.to_str().unwrap()]);
    assert!(!out.status.success(), "expected failure: {out:?}");

    assert!(!run(dir.path(), &["ical"]).status.success());
    assert!(!run(dir.path(), &["ical", "wat"]).status.success());
    assert!(!run(dir.path(), &["ical", "import", "/no/such/file.ics"])
        .status
        .success());
}

/// The subcommand has to be discoverable, or it does not exist for a user.
#[test]
fn ical_is_documented_in_the_usage_block() {
    let dir = tempfile::tempdir().unwrap();
    let help = stdout(&run(dir.path(), &["help"]));
    assert!(help.contains("sunrise ical import"), "got {help}");
    assert!(help.contains("sunrise ical export"), "got {help}");
}
