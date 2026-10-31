//! The commands that make the CLI a whole client: organising the vault,
//! the calendar, notes, undo, views, preferences and the daily reports.
//!
//! Like `tests/cli.rs`, every test runs the **real binary** against a real
//! vault, one process per command, so a command that only works inside one
//! process — undo is the obvious one — fails here.

use std::collections::BTreeSet;
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

/// `triage` and `untriage` are undone and redone like any other write, each
/// command its own process. A triage that changes nothing leaves nothing to
/// undo, and says so on stderr instead of leaving `undo` to reverse whatever
/// came before it unannounced.
#[test]
fn triage_and_untriage_are_undone_and_redone_and_a_refusal_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Call mum"]));
    let in_inbox = || ok(v, &["inbox"]).contains(&id);
    assert!(in_inbox());

    let out = run_in(v, &["triage", &id]);
    assert!(out.status.success());
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("cannot be undone"),
        "a triage is undoable"
    );
    assert!(!in_inbox());
    let undid = ok(v, &["undo"]);
    assert!(undid.contains("triage “Call mum”"), "{undid}");
    assert!(in_inbox(), "undo returned it to the Inbox");
    ok(v, &["redo"]);
    assert!(!in_inbox(), "redo triaged it again");

    ok(v, &["untriage", &id]);
    assert!(in_inbox());
    let undid = ok(v, &["undo"]);
    assert!(undid.contains("return “Call mum” to the Inbox"), "{undid}");
    assert!(!in_inbox(), "undo triaged it again");

    // Already triaged: the write changes nothing, so there is no inverse.
    let out = run_in(v, &["triage", &id]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("note: triage “Call mum” cannot be undone"),
        "{stderr}"
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

/// A relative phrase whose unit is a multi-byte character is refused as an
/// unreadable date. It used to panic: the unit was split off one byte from the
/// end, inside the character.
#[test]
fn defer_refuses_a_phrase_with_a_non_ascii_unit_instead_of_crashing() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Call the bank"]));
    let out = run_in(v, &["defer", &id, "+3\u{65e5}"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        stderr.contains("could not read the date \"+3\u{65e5}\""),
        "{stderr}"
    );
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
    let rent = first_id(&ok(v, &["capture", "Pay rent ^today"]));
    assert!(ok(v, &["brief", "morning"]).contains("To triage"));
    assert!(ok(v, &["brief", "evening"]).contains("Still open today"));
    assert_eq!(
        ok(v, &["review", "daily"]),
        format!(
            "Captured since yesterday\n  Nothing new in the Inbox.\n\
             Today's plan\n  {rent}  Pay rent\n\
             Blocked\n  Nothing in today's plan is blocked.\n"
        )
    );
    assert!(ok(v, &["review", "save", "a calm week"]).starts_with("rvw_"));

    // Two hours from now is inside the plan's 24-hour horizon at any hour of
    // the day, which "tomorrow 9am" is not. The all-day task sets no reminder.
    let soon = first_id(&ok(v, &["capture", "Call the bank ^+2h"]));
    let plan = ok(v, &["reminders", "plan"]);
    let row = plan
        .lines()
        .find(|l| l.contains(&soon))
        .unwrap_or_else(|| panic!("the reminder is planned: {plan}"));
    assert!(
        row.contains("task") && row.contains("Call the bank"),
        "{row}"
    );
    assert!(!plan.contains(&rent), "{plan}");
    assert!(plan.contains("evening_plan"), "{plan}");
}

/// Every kind of the catalog is listed with its state; a kind is turned off
/// and back on; a security kind refuses; the plan shows the catalog kinds
/// beside the reminders and keeps its ledger beside the vault.
#[test]
fn notification_kinds_are_listed_toggled_and_planned() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let kinds = json_of(v, &["reminders", "kinds"]);
    let kinds = kinds.as_array().unwrap();
    assert_eq!(kinds.len(), 18);
    let state = |kinds: &[serde_json::Value], tag: &str| {
        kinds
            .iter()
            .find(|k| k["kind"] == tag)
            .map(|k| (k["enabled"].as_bool(), k["always"].as_bool()))
    };
    assert_eq!(
        state(kinds, "timezone_changed"),
        Some((Some(false), Some(false)))
    );
    assert_eq!(
        state(kinds, "device_paired"),
        Some((Some(true), Some(true)))
    );
    assert!(ok(v, &["reminders", "kinds"]).contains("device_revoked"));

    let plan = json_of(v, &["reminders", "plan"]);
    let catalog: Vec<_> = plan["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["kind"].as_str().unwrap().to_owned())
        .collect();
    assert!(catalog.iter().any(|k| k == "evening_plan"), "{catalog:?}");
    assert!(plan["alerts"].as_array().unwrap().is_empty());
    assert!(v.join("notification-ledger.json").exists());

    ok(
        v,
        &[
            "reminders",
            "off",
            "evening_plan",
            "morning_brief",
            "--device",
        ],
    );
    let after = json_of(v, &["reminders", "kinds"]);
    assert_eq!(
        state(after.as_array().unwrap(), "evening_plan"),
        Some((Some(false), Some(false)))
    );
    let plan = json_of(v, &["reminders", "plan"]);
    assert!(plan["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .all(|n| n["kind"] != "evening_plan" && n["kind"] != "morning_brief"));
    ok(v, &["reminders", "on", "evening_plan"]);
    assert_eq!(
        json_of(v, &["prefs", "get", "notifications.evening_plan.enabled"])["source"],
        "device",
        "the device override from --device still wins"
    );
    assert!(!run_in(v, &["reminders", "off", "device_paired"])
        .status
        .success());
    assert!(!run_in(v, &["reminders", "off", "no_such_kind"])
        .status
        .success());
    assert!(!run_in(v, &["reminders", "off"]).status.success());
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

    // The flag is recognised anywhere on the line. The Inbox is a view
    // over tasks with no stream and no date (ADR-0046 §3), so the planned
    // capture is not in it and an undated one is.
    let undated = first_id(&ok(v, &["capture", "Find the passport"]));
    let inbox: serde_json::Value =
        serde_json::from_str(ok(v, &["inbox", "--json"]).trim()).unwrap();
    assert_eq!(inbox[0]["id"], undated.as_str());
    assert_eq!(inbox.as_array().map(Vec::len), Some(1));

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

/// `docs/07-clients/overview.md`, its lines joined, so a group the prose wraps
/// reads as the one written here.
fn json_contract() -> String {
    include_str!("../../../docs/07-clients/overview.md")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The names at the top level of a documented `{a, b: {c}, d: [{e}]}`.
fn documented_keys(group: &str) -> BTreeSet<String> {
    let inner = group
        .strip_prefix('{')
        .and_then(|g| g.strip_suffix('}'))
        .unwrap_or_else(|| panic!("not a group: {group}"));
    let mut entries = vec![String::new()];
    let mut depth = 0;
    for c in inner.chars() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => entries.push(String::new()),
            c if depth == 0 => entries.last_mut().unwrap().push(c),
            _ => {}
        }
    }
    entries
        .iter()
        .map(|e| e.split(':').next().unwrap().trim().to_owned())
        .collect()
}

const TASK: &str = "{id, title, state, stream, contexts, priority, energy, estimate_s, \
                    scheduled_at, due_at, target_at, completed_at, deferred_count, blocked_by, \
                    note, updated_at_ms, sort_order, cancel_reason, late_acknowledged_at_ms, \
                    routine, hard_constraint_violated}";
const TIME: &str = "{kind, value, zone, at_ms}";
const LATENESS: &str = "{state, since_ms}";
const BLOCK: &str = "{id, title, stream, starts_at, ends_at, tasks}";

/// Stand-ins in [`JSON_OBJECTS`] for the ids [`vault_for_the_json_table`]
/// returns.
const FIRST: &str = "<first>";
const SECOND: &str = "<second>";

/// The fixed-offset zone in which it is now between 12:00 and 12:59.
///
/// The positions in [`JSON_OBJECTS`] hold only by day: before the planner day
/// turns over, `upcoming` still starts on yesterday and `today` lists no
/// planned task first. The binary reads the real clock, so the fixture moves
/// the reader to midday rather than the clock.
fn zone_where_it_is_midday() -> String {
    let hour = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .hour();
    let hours_east = 12 - hour;
    // The tzdb's `Etc` names have the POSIX sign: `Etc/GMT-3` is three hours east.
    if hours_east > 0 {
        format!("Etc/GMT-{hours_east}")
    } else {
        format!("Etc/GMT+{}", -hours_east)
    }
}

/// A vault in which every object of [`JSON_OBJECTS`] exists, and the ids of
/// two tasks in one stream.
fn vault_for_the_json_table(v: &Path) -> (String, String) {
    ok(v, &["tz", "set", &zone_where_it_is_midday()]);
    ok(v, &["streams", "create", "Home"]);
    ok(v, &["contexts", "create", "errands"]);
    for day in ["MO", "TU", "WE", "TH", "FR", "SA", "SU"] {
        let key = format!("day_schedule.weekday.{day}");
        ok(v, &["prefs", "set", &key, "07:00-23:00"]);
    }
    let first = first_id(&ok(v, &["capture", "Call the bank ^today #home @errands"]));
    let second = first_id(&ok(v, &["capture", "Pay the bank #home"]));
    let late = first_id(&ok(v, &["capture", "File the taxes"]));
    ok(v, &["edit", &late, "due:2020-01-01"]);
    ok(v, &["capture", "Find the passport"]);
    ok(v, &["capture", "Phone home ^+2h"]);
    ok(v, &["capture", "Fly out ^tomorrow 9am"]);
    ok(
        v,
        &["routines", "create", "Stretch", "--rule", "FREQ=DAILY"],
    );
    ok(
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
    );
    ok(v, &["views", "save", "papers", "search", "--query", "bank"]);
    ok(v, &["views", "save", "loose", "inbox"]);
    let file = v.join("lease.txt");
    std::fs::write(&file, b"the lease").unwrap();
    ok(v, &["attachment", "add", &first, file.to_str().unwrap()]);
    (first, second)
}

/// The command, where in its document, the group the page gives that object,
/// and the keys the page adds to the group in words.
type JsonObject = (
    &'static [&'static str],
    &'static str,
    &'static str,
    &'static [&'static str],
);

const PLAIN: &[&str] = &[];
const WITH_LATENESS: &[&str] = &["lateness"];

const JSON_OBJECTS: &[JsonObject] = &[
    (
        &["today"],
        "/0",
        TASK,
        &["today_group", "deadline", "lateness"],
    ),
    (&["today"], "/0/scheduled_at", TIME, PLAIN),
    (&["today"], "/0/lateness", LATENESS, PLAIN),
    (&["inbox"], "/0", TASK, PLAIN),
    (&["stream", "Home"], "/0", TASK, PLAIN),
    (&["context", "errands"], "/0", TASK, PLAIN),
    (&["views", "apply", "loose"], "/0", TASK, PLAIN),
    (&["move", FIRST, "after", SECOND], "/0", TASK, PLAIN),
    (&["show", FIRST], "", TASK, WITH_LATENESS),
    (&["triage"], "/0", TASK, WITH_LATENESS),
    (&["triage", "list"], "/0", TASK, WITH_LATENESS),
    (
        &["search", "bank"],
        "",
        "{canonical, notices: [{code, start, end, detail}], groups: [{kind, total, hits: \
             [{id, opens, title, snippet, task}]}]}",
        PLAIN,
    ),
    (
        &["search", "bank"],
        "/groups/0",
        "{kind, total, hits: [{id, opens, title, snippet, task}]}",
        PLAIN,
    ),
    (
        &["search", "bank"],
        "/groups/0/hits/0",
        "{id, opens, title, snippet, task}",
        PLAIN,
    ),
    (
        &["search", "bank"],
        "/groups/0/hits/0/title",
        "{text, ranges}",
        PLAIN,
    ),
    (&["search", "bank"], "/groups/0/hits/0/task", TASK, PLAIN),
    (
        &["search", "stream:nosuch bank"],
        "/notices/0",
        "{code, start, end, detail}",
        PLAIN,
    ),
    (
        &["upcoming"],
        "",
        "{today, span_days, days: [{date, start_ms, end_ms, blocks, planned, due, targets}], \
             no_date: [{stream, no_stream, tasks, total}]}",
        PLAIN,
    ),
    (
        &["upcoming"],
        "/days/0",
        "{date, start_ms, end_ms, blocks, planned, due, targets}",
        PLAIN,
    ),
    (&["upcoming"], "/days/1/blocks/0", BLOCK, PLAIN),
    (&["upcoming"], "/days/1/planned/0", TASK, PLAIN),
    (
        &["upcoming"],
        "/no_date/0",
        "{stream, no_stream, tasks, total}",
        PLAIN,
    ),
    (&["next"], "/0", "{task, unblocks}", PLAIN),
    (&["next"], "/0/task", TASK, PLAIN),
    (
        &["streams"],
        "/0",
        "{id, name, color, open_tasks, archived, paused, sort_order}",
        PLAIN,
    ),
    (
        &["contexts"],
        "/0",
        "{id, name, description, tasks, archived}",
        PLAIN,
    ),
    (
        &["routines"],
        "/0",
        "{id, title, rule, next_ms, paused, streak}",
        PLAIN,
    ),
    (&["blocks", "tomorrow"], "/0", BLOCK, PLAIN),
    (&["blocks", "tomorrow"], "/0/starts_at", TIME, PLAIN),
    (
        &["devices"],
        "/0",
        "{id, nickname, platform, this_device, revoked, read_bounded, current, \
             admitted_after_revocation}",
        PLAIN,
    ),
    (
        &["identity"],
        "",
        "{account, current, rotations, this_device_is_current, holds_recovery_key, \
             can_sponsor}",
        PLAIN,
    ),
    (
        &["attachments", FIRST],
        "/0",
        "{id, filename, mime_type, size_bytes, state}",
        PLAIN,
    ),
    (
        &["review"],
        "",
        "{window: {start_ms, end_ms}, totals: {completed, deferred, dropped, created, \
             reopened}, streams: [{id, name, completed, deferred, untouched}], slipped: [task]}",
        PLAIN,
    ),
    (&["review"], "/window", "{start_ms, end_ms}", PLAIN),
    (
        &["review"],
        "/totals",
        "{completed, deferred, dropped, created, reopened}",
        PLAIN,
    ),
    (
        &["review"],
        "/streams/0",
        "{id, name, completed, deferred, untouched}",
        PLAIN,
    ),
    (
        &["review", "daily"],
        "",
        "{since_ms, inbox, today, blocked}",
        PLAIN,
    ),
    (&["review", "daily"], "/inbox/0", TASK, PLAIN),
    (&["review", "daily"], "/today/0", TASK, PLAIN),
    (
        &["brief", "morning"],
        "",
        "{brief, headline, sections: [{id, title, tasks}]}",
        PLAIN,
    ),
    (
        &["brief", "evening"],
        "/sections/0",
        "{id, title, tasks}",
        PLAIN,
    ),
    (
        &["reminders", "plan"],
        "",
        "{enabled, zone_unknown, planned: [{identifier, fire_at_ms, kind, entity, title, \
             body}], unscheduled: [{entity, kind, reason}], wind_down, catalog: [notification], \
             alerts: [notification]}",
        PLAIN,
    ),
    (
        &["reminders", "plan"],
        "/planned/0",
        "{identifier, fire_at_ms, kind, entity, title, body}",
        PLAIN,
    ),
    (
        &["reminders", "plan"],
        "/wind_down",
        "{identifier, date, fire_at_ms, sleep_at_ms}",
        PLAIN,
    ),
    (
        &["reminders", "plan"],
        "/catalog/0",
        "{identifier, kind, fire_at_ms, deferred_from_ms, entity, device, subject, count, \
             title, body, link, level, actions}",
        PLAIN,
    ),
    (
        &["reminders", "kinds"],
        "/0",
        "{kind, enabled, always, key, level, delivery, audience, actions}",
        PLAIN,
    ),
    (
        &["day-schedule"],
        "",
        "{date, start_ms, end_ms, window, plannable, wind_down_ms, schedule}",
        PLAIN,
    ),
    (&["day-schedule"], "/window", "{wake, sleep}", PLAIN),
    (&["day-schedule"], "/plannable", "{start_ms, end_ms}", PLAIN),
    (
        &["day-schedule"],
        "/schedule",
        r#"{weekday: {MO: window, …}, month_day: {"1": window, "-1": window, …}, date: {"2026-12-25": window \| "none", …}}"#,
        PLAIN,
    ),
    (
        &["day-schedule"],
        "/schedule/weekday/MO",
        "{wake, sleep}",
        PLAIN,
    ),
    (
        &["views"],
        "/0",
        "{id, name, view, spec, query, contexts, context_ids, streams, sort, sort_order}",
        PLAIN,
    ),
    (&["prefs"], "/0", "{key, value, source, scope}", PLAIN),
    (
        &["prefs", "get", "week_start"],
        "",
        "{key, value, source, scope}",
        PLAIN,
    ),
    (&["tz"], "", "{zone, source, zone_unknown}", PLAIN),
    (
        &["sync", "status"],
        "",
        "{badge, pending, known_incomplete}",
        PLAIN,
    ),
];

/// Every command path a row of the page's table names: each code span of its
/// first cell, up to the first word that is an operand or an option, with
/// `brief morning\|evening` read as both of its paths.
fn documented_commands(section: &str) -> BTreeSet<Vec<String>> {
    let mut commands = BTreeSet::new();
    for line in section.lines().filter(|l| l.starts_with("| `")) {
        let cell = line[2..].split(" | ").next().unwrap();
        for span in cell.split('`').skip(1).step_by(2) {
            let is_word =
                |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c == '-');
            let mut paths = vec![Vec::new()];
            for token in span.split_whitespace() {
                let words: Vec<&str> = token.split("\\|").collect();
                if !words.iter().copied().all(is_word) {
                    break;
                }
                paths = paths
                    .iter()
                    .flat_map(|path: &Vec<String>| {
                        words.iter().map(|word| {
                            let mut path = path.clone();
                            path.push((*word).to_owned());
                            path
                        })
                    })
                    .collect();
            }
            commands.extend(paths);
        }
    }
    commands
}

/// The page path `args` runs: the longest one it starts with, word by word, so
/// `review daily` never counts for `review`, nor `streams` for `stream`.
fn page_command_of<'a>(
    args: &[&str],
    documented: &'a BTreeSet<Vec<String>>,
) -> Option<&'a Vec<String>> {
    documented
        .iter()
        .filter(|path| path.len() <= args.len() && path.iter().zip(args).all(|(p, a)| p == a))
        .max_by_key(|path| path.len())
}

#[test]
fn a_row_of_the_page_is_read_as_every_command_it_names() {
    let section = "| Command | Document |\n\
         | `inbox`, `views apply` (task views) | array of `tasks` |\n\
         | `move <id>... before\\|after <id>` | a \\| b |\n\
         | `brief morning\\|evening` | `{brief}` |\n\
         | `blocks [day\\|week]` | array |\n\
         Prose with `not a row`.\n";
    let paths: Vec<&str> = vec![
        "blocks",
        "brief evening",
        "brief morning",
        "inbox",
        "move",
        "views apply",
    ];
    let read: Vec<String> = documented_commands(section)
        .iter()
        .map(|path| path.join(" "))
        .collect();
    assert_eq!(read, paths);

    let documented = documented_commands("| `review`, `review daily`, `streams` | x |\n");
    let of = |args: &[&str]| page_command_of(args, &documented).map(|path| path.join(" "));
    assert_eq!(of(&["review", "daily"]).as_deref(), Some("review daily"));
    assert_eq!(of(&["review"]).as_deref(), Some("review"));
    assert_eq!(of(&["stream", "Home"]), None);
}

/// §JSON output is a contract: a name is added, never renamed or removed, and
/// every name is documented. So each object a query prints has exactly the
/// keys the page gives it, and each group the table holds is quoted from the
/// page, so neither can move without the other.
///
/// Every command path of the page's table must have a row, and every row a
/// command path on the page. `conflicts` is the one whose objects need a second
/// device to exist, so it is only read empty.
#[test]
fn every_json_object_has_exactly_the_documented_keys() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let (first, second) = vault_for_the_json_table(v);

    let contract = json_contract();
    for (args, pointer, group, extra) in JSON_OBJECTS {
        let args: Vec<&str> = args
            .iter()
            .map(|a| match *a {
                FIRST => first.as_str(),
                SECOND => second.as_str(),
                a => a,
            })
            .collect();
        let args = args.as_slice();
        let command = args.join(" ");
        assert!(
            contract.contains(group),
            "`{command}`: the page does not document {group}"
        );
        let doc = json_of(v, args);
        let object = doc
            .pointer(pointer)
            .and_then(serde_json::Value::as_object)
            .unwrap_or_else(|| panic!("`{command}` has no object at {pointer:?}: {doc}"));
        let printed: BTreeSet<String> = object.keys().cloned().collect();
        let mut documented = documented_keys(group);
        for key in *extra {
            assert!(
                contract.contains(&format!("`{key}`")),
                "{key} is undocumented"
            );
            documented.insert((*key).to_owned());
        }
        assert_eq!(printed, documented, "`{command}` at {pointer:?}");
    }
    assert_eq!(json_of(v, &["conflicts"]), serde_json::json!([]));

    // A command the page's table gains cannot go unpinned.
    let page = include_str!("../../../docs/07-clients/overview.md");
    let section = page
        .split("### JSON output")
        .nth(1)
        .and_then(|rest| rest.split("\n### ").next())
        .expect("the page has a JSON output section");
    let documented = documented_commands(section);
    let pinned: BTreeSet<&Vec<String>> = JSON_OBJECTS
        .iter()
        .map(|(args, ..)| {
            page_command_of(args, &documented)
                .unwrap_or_else(|| panic!("the page does not document `{}`", args.join(" ")))
        })
        .collect();
    let unpinned: Vec<String> = documented
        .iter()
        .filter(|path| !pinned.contains(path) && path.join(" ") != "conflicts")
        .map(|path| path.join(" "))
        .collect();
    assert!(
        unpinned.is_empty(),
        "the page documents commands with no row here: {unpinned:?}"
    );
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

/// `export all` through the binary: the directory on stdout, a manifest whose
/// sizes match the files, the task's record with its kind, and a refusal to
/// write into a non-empty directory. The format itself is tested in
/// `crates/sunrise-client-core/tests/export.rs`.
#[test]
fn export_all_writes_a_directory_whose_manifest_matches_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    let id = first_id(&ok(v, &["capture", "Renew passport ^tomorrow 9am"]));
    let dest = v.join("export");
    let dest_s = dest.to_str().unwrap();
    assert_eq!(
        ok(v, &["export", "all", dest_s, "--no-fetch"]).trim(),
        dest_s
    );

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dest.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["format"], "sunrise-export");
    for f in manifest["files"].as_array().unwrap() {
        let bytes = std::fs::read(dest.join(f["path"].as_str().unwrap())).unwrap();
        assert_eq!(f["size"], bytes.len());
    }
    let tasks = std::fs::read_to_string(dest.join("entities/task.jsonl")).unwrap();
    let task: serde_json::Value = serde_json::from_str(tasks.lines().next().unwrap()).unwrap();
    assert_eq!(task["id"], id);
    assert_eq!(task["scheduled_at"]["kind"], "floating");

    assert!(
        !run_in(v, &["export", "all", dest_s]).status.success(),
        "a non-empty directory is refused"
    );
    assert!(!run_in(v, &["export", "all"]).status.success());
}

/// The ids a `--json` listing prints.
fn ids_of(vault: &Path, args: &[&str]) -> Vec<String> {
    json_of(vault, args)
        .as_array()
        .map(|ts| {
            ts.iter()
                .filter_map(|t| t["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `#inbox` names no stream in `capture` and in `edit` (ADR-0046) until a
/// real stream is called Inbox, and from then on names that stream in both.
#[test]
fn inbox_names_no_stream_until_a_stream_is_called_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    ok(v, &["streams", "create", "Work"]);
    let stream_of = |id: &str| json_of(v, &["show", id])["stream"].clone();

    let captured = first_id(&ok(v, &["capture", "Call mum #inbox"]));
    assert_eq!(stream_of(&captured), serde_json::Value::Null, "capture");
    let edited = first_id(&ok(v, &["capture", "Pack the boxes #work"]));
    ok(v, &["edit", &edited, "#inbox"]);
    assert_eq!(stream_of(&edited), serde_json::Value::Null, "edit");

    ok(v, &["streams", "create", "Inbox"]);
    let real = json_of(v, &["streams"])
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "Inbox").cloned())
        .expect("Inbox is listed")["id"]
        .clone();
    let captured = first_id(&ok(v, &["capture", "Water the plants #inbox"]));
    assert_eq!(stream_of(&captured), real, "capture beside a real Inbox");
    ok(v, &["edit", &edited, "#inbox"]);
    assert_eq!(stream_of(&edited), real, "edit beside a real Inbox");
    // No second "Inbox" candidate is left beside the real one, so a prefix
    // names it alone (search's `stream:inb` does not; see search_command.rs).
    let prefixed = first_id(&ok(v, &["capture", "Sort the post #inb"]));
    assert_eq!(stream_of(&prefixed), real, "a prefix beside a real Inbox");
}

/// A task belongs to no stream until it is given one (ADR-0046): `stream
/// none` lists the tasks of no stream, `triage` takes one out of the Inbox
/// without giving it a home, `#inbox` sends one back to no stream, and
/// deleting a stream moves what it held, `--move-to` another stream or to
/// no stream without it.
#[test]
fn a_task_can_belong_to_no_stream_and_a_stream_delete_rehomes_it() {
    let dir = tempfile::tempdir().unwrap();
    let v = dir.path();
    ok(v, &["streams", "create", "Old"]);
    ok(v, &["streams", "create", "New"]);
    let loose = first_id(&ok(v, &["capture", "Call mum"]));
    let filed = first_id(&ok(v, &["capture", "Pack the boxes #old"]));
    assert_eq!(
        json_of(v, &["show", &loose])["stream"],
        serde_json::Value::Null
    );
    for none in ["none", "inbox", "-"] {
        assert_eq!(ids_of(v, &["stream", none]), [loose.as_str()], "{none}");
    }
    assert_eq!(ids_of(v, &["inbox"]), [loose.as_str()]);

    ok(v, &["triage", &loose]);
    assert!(
        ids_of(v, &["inbox"]).is_empty(),
        "triaged, it leaves the Inbox"
    );
    assert_eq!(ids_of(v, &["stream", "none"]), [loose.as_str()]);
    ok(v, &["untriage", &loose]);
    assert_eq!(ids_of(v, &["inbox"]), [loose.as_str()]);

    ok(v, &["streams", "delete", "Old", "--move-to", "New"]);
    let new = json_of(v, &["streams"])
        .as_array()
        .and_then(|s| s.iter().find(|s| s["name"] == "New").cloned())
        .expect("New is listed")["id"]
        .clone();
    assert_eq!(json_of(v, &["show", &filed])["stream"], new);
    assert_eq!(json_of(v, &["streams"]).as_array().map(Vec::len), Some(1));

    ok(v, &["edit", &filed, "#inbox"]);
    assert_eq!(
        json_of(v, &["show", &filed])["stream"],
        serde_json::Value::Null
    );
    ok(v, &["edit", &filed, "#new"]);
    ok(v, &["streams", "delete", "New"]);
    assert_eq!(
        json_of(v, &["show", &filed])["stream"],
        serde_json::Value::Null
    );
    assert!(json_of(v, &["streams"])
        .as_array()
        .is_some_and(Vec::is_empty));
}
