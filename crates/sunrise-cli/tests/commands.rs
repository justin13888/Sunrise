//! The commands that make the CLI a whole client: organising the vault,
//! the calendar, notes, undo, views, preferences and the daily reports.
//!
//! Like `tests/cli.rs`, every test runs the **real binary** against a real
//! vault, one process per command, so a command that only works inside one
//! process — undo is the obvious one — fails here.

use std::path::Path;
use std::process::{Command, Output};

fn run_in(vault: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sunrise"))
        .args(args)
        .env("SUNRISE_VAULT", vault)
        .env("SUNRISE_KEYSTORE", vault.join("keystore"))
        // Saved views live in the per-device config directory.
        .env("XDG_CONFIG_HOME", vault.join("config"))
        .env("SUNRISE_LOG_FILE", vault.join("log.ndjson"))
        .env_remove("SUNRISE_SYNC_URL")
        .env_remove("SUNRISE_VAULT_ROOT")
        .env_remove("SUNRISE_TZ")
        .output()
        .expect("run sunrise")
}

/// Run and require success, returning stdout.
fn ok(vault: &Path, args: &[&str]) -> String {
    let out = run_in(vault, args);
    assert!(
        out.status.success(),
        "`sunrise {}` failed: {}\n{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The first whitespace-separated word of the first line: the id a create
/// prints.
fn first_id(stdout: &str) -> String {
    stdout.split_whitespace().next().expect("an id").to_owned()
}

fn field<'a>(show: &'a str, name: &str) -> &'a str {
    show.lines()
        .find_map(|l| l.strip_prefix(name))
        .map_or("", str::trim)
}

#[test]
fn undo_reverses_a_write_made_by_an_earlier_process_and_redo_replays_it() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Renew passport"]));

    // Setting a priority on a task with none: its undo is a *clear*, which a
    // lossy encoding of the history would turn into a no-op.
    ok(v, &["edit", &id, "!1"]);
    assert_eq!(field(&ok(v, &["show", &id]), "priority"), "1");
    let undid = ok(v, &["undo"]);
    assert!(undid.contains("edit"), "{undid}");
    assert_eq!(field(&ok(v, &["show", &id]), "priority"), "-");
    ok(v, &["redo"]);
    assert_eq!(field(&ok(v, &["show", &id]), "priority"), "1");

    // The history file is ciphertext: a vault directory copied on its own
    // must not name its tasks.
    let raw = std::fs::read(v.join("cli-undo")).expect("the history is kept");
    assert!(!raw.windows(8).any(|w| w == b"passport"));

    // Undo all the way back past the capture: the task is gone.
    ok(v, &["undo"]);
    ok(v, &["undo"]);
    assert!(!ok(v, &["inbox"]).contains(&id));
    assert!(
        !run_in(v, &["undo"]).status.success(),
        "nothing left to undo"
    );
}

#[test]
fn defer_counts_civil_days_and_keeps_the_plans_wall_clock_and_kind() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Call the bank ^tomorrow 9am"]));
    let before = ok(v, &["show", &id]);
    assert!(
        field(&before, "scheduled").ends_with("09:00 [floating]"),
        "{before}"
    );

    let out = ok(v, &["defer", &id, "+2d"]);
    assert!(out.contains("deferred to"), "{out}");
    let after = ok(v, &["show", &id]);
    let scheduled = field(&after, "scheduled");
    assert!(
        scheduled.ends_with("09:00 [floating]"),
        "a floating 09:00 stays a floating 09:00: {after}"
    );
    assert_ne!(scheduled, field(&before, "scheduled"), "the day moved");
    assert_eq!(field(&after, "deferred"), "1");

    // A named time is the user choosing the time as well.
    ok(v, &["defer", &id, "tomorrow 3pm"]);
    assert!(field(&ok(v, &["show", &id]), "scheduled").ends_with("15:00 [floating]"));
}

#[test]
fn schedule_sets_a_day_without_counting_a_deferral_and_dash_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Water the plants"]));
    ok(v, &["schedule", &id, "tomorrow"]);
    let shown = ok(v, &["show", &id]);
    assert!(field(&shown, "scheduled").ends_with("[all_day]"), "{shown}");
    assert_eq!(field(&shown, "deferred"), "0");
    ok(v, &["schedule", &id, "-"]);
    assert_eq!(field(&ok(v, &["show", &id]), "scheduled"), "-");
}

#[test]
fn a_note_is_written_read_back_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Plan the trip"]));
    ok(v, &["note", &id, "Book", "the", "train"]);
    assert!(ok(v, &["note", &id]).contains("Book the train"));
    assert!(ok(v, &["show", &id]).contains("Book the train"));
    ok(v, &["note", &id, "-"]);
    assert!(ok(v, &["note", &id]).trim().is_empty());
}

#[test]
fn streams_and_contexts_are_created_edited_archived_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let s = first_id(&ok(v, &["streams", "create", "Home", "--color", "emerald"]));
    assert!(s.starts_with("str_"), "{s}");
    ok(v, &["streams", "edit", "Home", "--name", "House"]);
    assert!(ok(v, &["streams"]).contains("House"));
    ok(v, &["streams", "archive", "House"]);
    assert!(ok(v, &["streams"]).contains("[archived]"));
    ok(v, &["streams", "unarchive", "House"]);
    ok(v, &["streams", "delete", "House"]);
    assert!(!ok(v, &["streams"]).contains("House"));
    assert!(!run_in(v, &["streams", "create", "X", "--color", "plaid"])
        .status
        .success());

    let c = first_id(&ok(v, &["contexts", "create", "@errands"]));
    assert!(c.starts_with("ctx_"), "{c}");
    ok(v, &["contexts", "archive", "errands"]);
    assert!(ok(v, &["contexts"]).contains("[archived]"));
    ok(v, &["contexts", "delete", "errands"]);
    assert!(!ok(v, &["contexts"]).contains("errands"));
}

#[test]
fn routines_are_created_edited_skipped_and_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(
        v,
        &[
            "routines",
            "create",
            "Stretch",
            "--rule",
            "FREQ=DAILY",
            "--zone",
            "Europe/Berlin",
        ],
    ));
    assert!(id.starts_with("rtn_"), "{id}");
    ok(
        v,
        &["routines", "edit", "Stretch", "--title", "Stretch well"],
    );
    assert!(ok(v, &["routines"]).contains("Stretch well"));
    let skipped = ok(v, &["routines", "skip", "Stretch well"]);
    assert!(skipped.contains("skipped"), "{skipped}");
    ok(v, &["routines", "archive", &id]);
    ok(v, &["routines", "delete", &id]);
    assert!(!ok(v, &["routines"]).contains(&id));
    assert!(!run_in(v, &["routines", "create", "No rule"])
        .status
        .success());
}

#[test]
fn blocks_are_created_moved_bound_and_merged() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let task = first_id(&ok(v, &["capture", "Write the report"]));
    let a = first_id(&ok(
        v,
        &[
            "block",
            "create",
            "--from",
            "tomorrow 9am",
            "--for",
            "1h",
            "--title",
            "Focus",
        ],
    ));
    let b = first_id(&ok(
        v,
        &[
            "block",
            "create",
            "--from",
            "tomorrow 9:30am",
            "--to",
            "tomorrow 11am",
            "--title",
            "Deep work",
        ],
    ));
    assert!(a.starts_with("blk_") && b.starts_with("blk_"));
    ok(v, &["block", "bind", &a, &task]);
    let listed = ok(v, &["blocks", "tomorrow"]);
    assert!(listed.contains(&a) && listed.contains(&task), "{listed}");

    ok(v, &["block", "move", &a, "--from", "tomorrow 8am"]);
    assert!(ok(v, &["blocks", "tomorrow"]).contains("08:00"));

    let merged = first_id(&ok(v, &["block", "merge", &a, &b]));
    let after = ok(v, &["blocks", "tomorrow"]);
    assert!(after.contains(&merged), "{after}");
    assert!(!after.contains(&a) && !after.contains(&b), "{after}");
}

#[test]
fn a_saved_view_is_recalled_by_name_and_preferences_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    ok(v, &["capture", "Renew passport"]);
    ok(v, &["capture", "Email Sara"]);
    ok(
        v,
        &["views", "save", "papers", "search", "--query", "passport"],
    );
    assert!(ok(v, &["views"]).contains("papers"));
    let recalled = ok(v, &["views", "apply", "papers"]);
    assert!(recalled.contains("Renew passport") && !recalled.contains("Email Sara"));
    assert!(!run_in(v, &["views", "save", "x", "nowhere"])
        .status
        .success());

    ok(v, &["prefs", "set", "week_start", "MO"]);
    assert_eq!(ok(v, &["prefs", "get", "week_start"]).trim(), "MO");
    ok(v, &["prefs", "set", "time_format", "h24", "--device"]);
    assert!(ok(v, &["prefs"]).contains("time_format"));
    assert!(!run_in(v, &["prefs", "set", "stale_after_days", "9999"])
        .status
        .success());
    assert!(!run_in(v, &["prefs", "get", "no_such_key"]).status.success());
}

#[test]
fn the_daily_reports_run_on_a_fresh_vault() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    ok(v, &["capture", "Pay rent ^today"]);
    assert!(ok(v, &["brief", "morning"]).contains("To triage"));
    assert!(ok(v, &["brief", "evening"]).contains("Still open today"));
    ok(v, &["review", "daily"]);
    assert!(ok(v, &["review", "save", "a calm week"]).starts_with("rvw_"));
    ok(v, &["reminders", "plan"]);
}

#[test]
fn a_file_is_attached_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let task = first_id(&ok(v, &["capture", "Scan the lease"]));
    let file = v.join("lease.txt");
    std::fs::write(&file, b"the lease").unwrap();
    let att = first_id(&ok(
        v,
        &["attachment", "add", &task, file.to_str().unwrap()],
    ));
    assert!(ok(v, &["attachments", &task]).contains("lease.txt"));
    ok(v, &["attachment", "remove", &att]);
    assert!(!ok(v, &["attachments", &task]).contains("lease.txt"));
}

fn json_of(vault: &Path, args: &[&str]) -> serde_json::Value {
    let mut with = vec!["--json"];
    with.extend_from_slice(args);
    let out = ok(vault, &with);
    serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
}

#[test]
fn every_query_command_prints_one_json_document_with_the_documented_fields() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Call the bank ^tomorrow 9am !2"]));
    ok(v, &["streams", "create", "Home"]);
    ok(v, &["contexts", "create", "errands"]);

    let task = json_of(v, &["show", &id]);
    assert_eq!(task["id"], id.as_str());
    assert_eq!(task["priority"], 2);
    assert_eq!(task["scheduled_at"]["kind"], "floating");
    assert!(task["scheduled_at"]["value"]
        .as_str()
        .is_some_and(|s| s.ends_with("T09:00:00")));

    // The flag is recognised anywhere on the line.
    let inbox: serde_json::Value =
        serde_json::from_str(ok(v, &["inbox", "--json"]).trim()).unwrap();
    assert_eq!(inbox[0]["id"], id.as_str());

    for args in [
        &["today"][..],
        &["search", "bank"],
        &["streams"],
        &["contexts"],
        &["routines"],
        &["blocks", "week"],
        &["devices"],
        &["identity"],
        &["attachments", &id],
        &["next"],
        &["review"],
        &["review", "daily"],
        &["brief", "morning"],
        &["brief", "evening"],
        &["reminders", "plan"],
        &["views"],
        &["prefs"],
        &["prefs", "get", "week_start"],
        &["tz"],
        &["sync", "status"],
    ] {
        let doc = json_of(v, args);
        assert!(
            doc.is_array() || doc.is_object(),
            "`{}` printed {doc}",
            args.join(" ")
        );
    }
    let streams = json_of(v, &["streams"]);
    assert!(streams
        .as_array()
        .is_some_and(|s| s.iter().any(|s| s["name"] == "Home")));
    assert_eq!(
        json_of(v, &["prefs", "get", "week_start"])["source"],
        "default"
    );
    assert_eq!(json_of(v, &["brief", "evening"])["brief"], "evening");
}

#[test]
fn the_zone_comes_from_the_flag_the_environment_or_tz_set_and_an_unknown_one_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let out = ok(v, &["tz", "set", "Asia/Kolkata"]);
    assert!(out.contains("Asia/Kolkata"), "{out}");
    let shown = json_of(v, &["tz"]);
    assert_eq!(shown["zone"], "Asia/Kolkata");
    assert_eq!(shown["source"], "set");
    let flag = json_of(v, &["--tz", "Europe/Berlin", "tz"]);
    assert_eq!(
        (flag["zone"].as_str(), flag["source"].as_str()),
        (Some("Europe/Berlin"), Some("flag"))
    );

    let bad = run_in(v, &["--tz", "Mars/Olympus", "today"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("Mars/Olympus"));
    assert!(!run_in(v, &["tz", "set", "Not/AZone"]).status.success());
    assert_eq!(
        json_of(v, &["tz"])["zone"],
        "Asia/Kolkata",
        "a refused set changes nothing"
    );

    // A capture reads "9am" in the zone in force: 09:00 Kolkata is 03:30Z.
    let id = first_id(&ok(v, &["capture", "Stand-up ^tomorrow 9am"]));
    let at = json_of(v, &["show", &id])["scheduled_at"]["at_ms"]
        .as_i64()
        .unwrap();
    let utc = jiff::Timestamp::from_millisecond(at).unwrap();
    assert_eq!(utc.strftime("%H:%M").to_string(), "03:30");

    ok(v, &["tz", "clear"]);
    assert_eq!(json_of(v, &["tz"])["source"], "system");
}

/// `upcoming` (#351): the span asked for, else the device's preference, days
/// from the reader's today, each plan on the day it falls on with its kind,
/// and undated tasks in the no-date lane.
#[test]
fn upcoming_lists_the_span_day_by_day_then_the_undated_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let in_ny = |args: &[&str]| -> Vec<String> {
        ["--tz", "America/New_York"]
            .iter()
            .chain(args)
            .map(|a| (*a).to_owned())
            .collect()
    };
    let run = |args: &[&str]| {
        ok(
            v,
            &in_ny(args).iter().map(String::as_str).collect::<Vec<_>>(),
        )
    };
    let json = |args: &[&str]| {
        let mut with = vec!["--json"];
        with.extend_from_slice(args);
        let out = run(&with);
        serde_json::from_str::<serde_json::Value>(out.trim())
            .unwrap_or_else(|e| panic!("not JSON ({e}): {out}"))
    };
    let call = first_id(&run(&["capture", "Call mom ^tomorrow 9am"]));
    let someday = first_id(&run(&["capture", "Learn the cello"]));

    let view = json(&["upcoming", "14"]);
    assert_eq!(view["span_days"], 14);
    let days = view["days"].as_array().expect("days");
    assert_eq!(days.len(), 14);
    assert_eq!(view["today"], days[0]["date"]);
    let tomorrow = &days[1]["planned"][0];
    assert_eq!(tomorrow["id"], call.as_str());
    assert_eq!(tomorrow["scheduled_at"]["kind"], "floating");
    let lane = view["no_date"].as_array().expect("no_date");
    assert_eq!(lane.len(), 1);
    assert_eq!(lane[0]["no_stream"], true);
    assert_eq!(lane[0]["tasks"][0]["id"], someday.as_str());
    assert_eq!(lane[0]["total"], 1);

    let text = run(&["upcoming"]);
    assert!(text.contains(&call) && text.contains("No date"), "{text}");
    assert_eq!(json(&["upcoming"])["days"].as_array().unwrap().len(), 7);
    run(&["prefs", "set", "views.upcoming.span_days", "30", "--device"]);
    assert_eq!(json(&["upcoming"])["span_days"], 30);
    for bad in ["9", "week", "0"] {
        assert!(!run_in(v, &["upcoming", bad]).status.success(), "{bad}");
    }
}
