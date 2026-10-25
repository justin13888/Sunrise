//! A task's three times and its lateness between two CLI vaults on one
//! account, through a real relay and the real `sunrise` binary (ADR-0047,
//! issue #334).
//!
//! A floating soft deadline an hour ago, by the wall clock, is missed for a
//! reader in UTC and still ahead for one five hours west: both devices hold
//! the same stored value, and each reads the lateness that is right where
//! it is. Lateness is derived on read and never synced.

use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle};
use sunrise_e2e::spawn_relay;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_floating_target_is_late_in_one_zone_and_on_track_in_another() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = bootstrap_and_recover(dir.path(), &format!("http://{addr}")).await;
    // B reads five hours west of UTC (`Etc/GMT+5` is UTC−5).
    let b = b.in_zone("Etc/GMT+5");

    // The wall clock an hour ago in UTC: past for A, four hours ahead for B.
    let wall = (jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1))
        .to_zoned(jiff::tz::TimeZone::UTC)
        .datetime();
    let target = format!("target:{} {}", wall.date(), wall.time().strftime("%H:%M"));

    let task = first_id(&a.ok(&["capture", "Send the report"]).await);
    a.ok(&["edit", &task, &target]).await;
    settle(&a, &b).await;

    let on_a = a.json(&["show", &task]).await;
    let on_b = b.json(&["show", &task]).await;
    assert_eq!(on_a["target_at"]["kind"], "floating");
    assert_eq!(
        on_a["target_at"]["value"], on_b["target_at"]["value"],
        "one stored value on both devices"
    );
    assert_eq!(on_a["due_at"], serde_json::Value::Null, "no hard deadline");
    assert_eq!(on_a["lateness"]["state"], "late_soft");
    assert_eq!(on_b["lateness"]["state"], "on_track");
    assert!(on_a["lateness"]["since_ms"].is_i64());
    assert!(on_b["lateness"]["since_ms"].is_null());

    // Search's `is:late` and Today's Late group read the same answer.
    let late_on = |v: &serde_json::Value| -> bool {
        v["groups"].as_array().is_some_and(|gs| {
            gs.iter().any(|g| {
                g["hits"]
                    .as_array()
                    .is_some_and(|hs| hs.iter().any(|h| h["id"] == task.as_str()))
            })
        })
    };
    assert!(late_on(&a.json(&["search", "is:late"]).await));
    assert!(!late_on(&b.json(&["search", "is:late"]).await));
    let today_a = a.json(&["today"]).await;
    let row = today_a
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"] == task.as_str()))
        .expect("the task is in A's Today");
    assert_eq!(row["today_group"], "late");
    assert_eq!(row["lateness"]["state"], "late_soft");

    // A deadline set on B syncs as the hard deadline, and defer on A moves
    // only the plan.
    b.ok(&["edit", &task, "due:tomorrow"]).await;
    settle(&a, &b).await;
    let hard = a.json(&["show", &task]).await["due_at"].clone();
    assert_eq!(hard["kind"], "all_day");
    a.ok(&["defer", &task, "+1d"]).await;
    settle(&a, &b).await;
    let after = b.json(&["show", &task]).await;
    // `at_ms` is where the value falls for each reader; the stored value is
    // its kind and value.
    for key in ["kind", "value"] {
        assert_eq!(
            after["due_at"][key], hard[key],
            "defer never moves a deadline"
        );
    }
    assert_eq!(after["target_at"]["value"], on_a["target_at"]["value"]);
    assert_eq!(after["deferred_count"], 1);
    relay.abort();
}
