//! A traveller, end to end through the CLI: device A writes in New York, then
//! reads and edits in Kolkata, and device B reads both in UTC.
//!
//! `docs/11-adr/0066-times-survive-travel.md`: a floating time keeps its wall
//! clock wherever the reader is, a zoned one keeps its own zone, and a zone
//! change is not an edit — nothing stored is rewritten by moving, only by
//! changing something. The CLI reports its zone through
//! `Core::on_time_zone_changed` (`SUNRISE_TZ`, `--tz`, `sunrise tz set`), so
//! this is the core's rule observed through the binary.

use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle, Device};
use sunrise_e2e::spawn_relay;

const NEW_YORK: &str = "America/New_York";
const KOLKATA: &str = "Asia/Kolkata";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn floating_times_keep_their_wall_clock_and_nothing_stored_is_rewritten() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = bootstrap_and_recover(dir.path(), &format!("http://{addr}")).await;
    let (ny, kolkata) = (a.in_zone(NEW_YORK), a.in_zone(KOLKATA));

    // Written in New York: a floating 09:00 and a block pinned to New York.
    let task = first_id(&ny.ok(&["capture", "Call mom ^tomorrow 9am"]).await);
    let block = first_id(
        &ny.ok(&["block", "create", "--from", "tomorrow 2pm", "--for", "1h"])
            .await,
    );
    let written = ny.task(&task).await;
    assert_eq!(written["scheduled_at"]["kind"], "floating");
    let wall = written["scheduled_at"]["value"].clone();
    assert!(wall.as_str().is_some_and(|v| v.ends_with("T09:00:00")));

    // The zone the CLI reads in is the one it was told.
    let tz = kolkata.json(&["tz"]).await;
    assert_eq!(tz["zone"], KOLKATA);
    assert_eq!(tz["source"], "env");

    // Read in Kolkata: the same 09:00, at another instant, and untouched.
    let read = kolkata.task(&task).await;
    assert_eq!(
        read["scheduled_at"]["value"], wall,
        "the wall clock travelled"
    );
    assert_eq!(read["scheduled_at"]["kind"], "floating");
    assert_ne!(
        read["scheduled_at"]["at_ms"], written["scheduled_at"]["at_ms"],
        "09:00 in Kolkata is not 09:00 in New York"
    );
    assert_eq!(
        read["updated_at_ms"], written["updated_at_ms"],
        "a read rewrote nothing"
    );

    // The block stays in New York: 14:00 there, whoever reads it.
    let blocks = kolkata.json(&["blocks", "week"]).await;
    let pinned = blocks
        .as_array()
        .and_then(|bs| bs.iter().find(|b| b["id"] == block.as_str()))
        .unwrap_or_else(|| panic!("the block is on the grid: {blocks}"));
    assert_eq!(pinned["starts_at"]["kind"], "zoned");
    assert_eq!(pinned["starts_at"]["zone"], NEW_YORK);

    // An edit made in Kolkata changes what it names and nothing else: the
    // plan is still the floating 09:00 written in New York.
    kolkata.ok(&["edit", &task, "!2"]).await;
    let edited = kolkata.task(&task).await;
    assert_eq!(edited["priority"], 2);
    assert_eq!(edited["scheduled_at"]["value"], wall);
    assert_eq!(edited["scheduled_at"]["kind"], "floating");

    // B, in UTC, reads the same stored values.
    settle(&kolkata, &b).await;
    let on_b = b.task(&task).await;
    assert_eq!(on_b["scheduled_at"]["value"], wall);
    assert_eq!(on_b["scheduled_at"]["kind"], "floating");
    assert_eq!(on_b["updated_at_ms"], edited["updated_at_ms"]);
    relay.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_zone_is_chosen_by_flag_then_env_then_tz_set_and_an_unknown_one_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut d = Device::new(&dir.path().join("v"), "http://127.0.0.1:9");
    d.relay = None;
    d.zone = None;

    d.ok(&["tz", "set", KOLKATA]).await;
    assert_eq!(d.json(&["tz"]).await["source"], "set");
    let env = d.in_zone(NEW_YORK);
    assert_eq!(env.json(&["tz"]).await["zone"], NEW_YORK);
    let flag = env.json(&["--tz", "Europe/Berlin", "tz"]).await;
    assert_eq!(flag["zone"], "Europe/Berlin");
    assert_eq!(flag["source"], "flag");

    let bad = env.run(&["--tz", "Mars/Olympus", "today"]).await;
    assert!(
        !bad.status.success(),
        "an unknown zone is refused, never UTC"
    );
    assert!(String::from_utf8_lossy(&bad.stderr).contains("Mars/Olympus"));
    assert!(!d.run(&["tz", "set", "Not/AZone"]).await.status.success());

    d.ok(&["tz", "clear"]).await;
    assert_eq!(d.json(&["tz"]).await["source"], "system");
}
