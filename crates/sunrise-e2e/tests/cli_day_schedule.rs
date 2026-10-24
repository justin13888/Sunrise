//! The day schedule end to end through the CLI (#338,
//! `docs/02-domain/day-schedule.md`): someone who sleeps at 01:00 is still on
//! the day they were living at 00:30, Today and the briefs read that planner
//! day, the schedule syncs to a second device, and a traveller's planner day
//! moves with the zone while nothing stored is rewritten.
//!
//! The CLI reads the real clock, so the test picks the zone instead: a fixed
//! `Etc/GMT` offset in which it is just past midnight now. Anything from
//! 00:00 to 06:59 there is before the 07:00 wake, so the minute the test runs
//! in does not matter.

use jiff::civil::Date;
use serde_json::Value;
use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle, Device};
use sunrise_e2e::spawn_relay;

/// The `Etc/GMT` zone `offset_h` hours east of UTC. The names are POSIX's,
/// so their signs are inverted: `Etc/GMT-5` is UTC+5.
fn etc_zone(offset_h: i32) -> String {
    match offset_h {
        0 => "Etc/GMT".to_owned(),
        h if h > 0 => format!("Etc/GMT-{h}"),
        h => format!("Etc/GMT+{}", -h),
    }
}

/// The whole-hour offset, in `-12..=14`, at which the local hour is 0 now.
fn midnight_offset(utc_hour: i32) -> i32 {
    if utc_hour <= 12 {
        -utc_hour
    } else {
        24 - utc_hour
    }
}

fn titles(tasks: &Value) -> Vec<String> {
    tasks
        .as_array()
        .expect("an array of tasks")
        .iter()
        .map(|t| t["title"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn section<'a>(brief: &'a Value, id: &str) -> &'a Value {
    brief["sections"]
        .as_array()
        .and_then(|s| s.iter().find(|s| s["id"] == id))
        .map_or_else(|| panic!("no {id} section: {brief}"), |s| &s["tasks"])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_night_is_the_day_before_and_the_planner_day_travels() {
    let (addr, relay) = spawn_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = bootstrap_and_recover(dir.path(), &format!("http://{addr}")).await;

    let utc = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    let offset = midnight_offset(i32::from(utc.hour()));
    let zone = etc_zone(offset);
    let late = a.in_zone(&zone);
    let today: Date = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::get(&zone).unwrap())
        .date();
    let yesterday = today.yesterday().unwrap();

    // No schedule: the planner day is the civil day.
    let unset = late.json(&["day-schedule"]).await;
    assert_eq!(unset["date"], today.to_string(), "{unset}");
    assert_eq!(unset["window"], Value::Null);

    // A vault preference is written once every device has said it reads one,
    // so let each hear the other first.
    settle(&late, &b).await;
    late.ok(&["day-schedule", "set", "daily", "07:00-01:00"])
        .await;
    let day = late.json(&["day-schedule"]).await;
    assert_eq!(
        day["date"],
        yesterday.to_string(),
        "00:xx before a 01:00 sleep is still yesterday's planner day: {day}"
    );
    assert_eq!(day["window"]["wake"], "07:00");
    assert_eq!(day["window"]["sleep"], "01:00");
    assert_eq!(day["schedule"]["weekday"].as_object().unwrap().len(), 7);
    let start = day["start_ms"].as_i64().unwrap();
    let end = day["end_ms"].as_i64().unwrap();
    assert_eq!(
        end - start,
        24 * 3_600_000,
        "wake to wake, no DST in Etc/GMT"
    );

    let evening = today_and_briefs(&late, today, yesterday).await;
    an_override_wins_and_clears(&late, yesterday).await;

    // The wind-down: tonight's sleep (01:00) is behind the reader less its
    // hour, so the next one is the coming night's, an hour before 01:00.
    let reminders = late.json(&["reminders", "plan"]).await;
    let wind_down = &reminders["wind_down"];
    assert_eq!(wind_down["date"], today.to_string(), "{reminders}");
    assert_eq!(
        wind_down["sleep_at_ms"].as_i64().unwrap() - wind_down["fire_at_ms"].as_i64().unwrap(),
        3_600_000
    );

    // The traveller lands eight hours east (or, where that runs off the
    // offsets, nineteen hours west): the same instant is another planner
    // day, the wind-down moves with it, and nothing stored was rewritten.
    let (away_offset, away_day) = if offset + 8 <= 14 {
        (offset + 8, today)
    } else {
        (offset - 19, yesterday.yesterday().unwrap())
    };
    let away = a.in_zone(&etc_zone(away_offset));
    let there = away.json(&["day-schedule"]).await;
    assert_eq!(there["date"], away_day.to_string(), "{there}");
    let away_today = titles(&away.json(&["today"]).await);
    assert_eq!(
        away_today.contains(&"Morning run".to_owned()),
        away_day == today,
        "{away_today:?}"
    );
    let moved = away.json(&["reminders", "plan"]).await;
    assert_ne!(
        moved["wind_down"]["fire_at_ms"], wind_down["fire_at_ms"],
        "01:00 there is another instant: {moved}"
    );
    assert_eq!(
        away.task(&evening).await["updated_at_ms"],
        late.task(&evening).await["updated_at_ms"],
        "a zone change is not an edit"
    );

    // The schedule is the account's: B, in UTC, reads the same entries.
    settle(&late, &b).await;
    let on_b = b.json(&["day-schedule"]).await;
    assert_eq!(on_b["schedule"], day["schedule"], "{on_b}");
    relay.abort();
}

/// Today and the briefs at 00:xx, with a 01:00 sleep: the planner day is
/// yesterday's. Returns last evening's task.
async fn today_and_briefs(late: &Device, today: Date, yesterday: Date) -> String {
    // Today is that planner day: last evening's plan is in it, this
    // morning's is not, and neither is a deadline on the civil today.
    let evening = first_id(
        &late
            .ok(&["capture", &format!("Evening call ^{yesterday} 10pm")])
            .await,
    );
    late.ok(&["capture", &format!("Morning run ^{today} 9am")])
        .await;
    late.ok(&["capture", &format!("Rent ^{today}")]).await;
    let on_today = titles(&late.json(&["today"]).await);
    assert!(
        on_today.contains(&"Evening call".to_owned()),
        "{on_today:?}"
    );
    assert!(
        !on_today.contains(&"Morning run".to_owned()),
        "{on_today:?}"
    );
    assert!(!on_today.contains(&"Rent".to_owned()), "{on_today:?}");

    // A task done now counts for the day being lived: already done today in
    // the morning brief, and the evening plan's day ends at the next wake.
    let late_one = first_id(&late.ok(&["capture", "Late one"]).await);
    late.ok(&["done", &late_one]).await;
    let morning = late.json(&["brief", "morning"]).await;
    assert!(
        titles(section(&morning, "today")).contains(&"Late one".to_owned()),
        "{morning}"
    );
    let plan = late.json(&["brief", "evening"]).await;
    let open = titles(section(&plan, "open"));
    assert!(open.contains(&"Evening call".to_owned()), "{plan}");
    assert!(!open.contains(&"Morning run".to_owned()), "{plan}");
    evening
}

/// A per-date override, set and cleared.
async fn an_override_wins_and_clears(late: &Device, yesterday: Date) {
    // A per-date override wins for its date, and clearing it restores the
    // weekday's window.
    late.ok(&[
        "day-schedule",
        "set",
        "date",
        &yesterday.to_string(),
        "09:00-02:00",
    ])
    .await;
    let over = late.json(&["day-schedule"]).await;
    assert_eq!(over["window"]["wake"], "09:00", "{over}");
    assert_eq!(
        over["schedule"]["date"][yesterday.to_string()]["sleep"],
        "02:00"
    );
    late.ok(&["day-schedule", "clear", "date", &yesterday.to_string()])
        .await;
    assert_eq!(
        late.json(&["day-schedule"]).await["window"]["wake"],
        "07:00"
    );
}

#[test]
fn the_offsets_put_every_hour_at_midnight() {
    for h in 0..24 {
        let off = midnight_offset(h);
        assert!((-12..=14).contains(&off), "{h}: {off}");
        assert_eq!((h + off).rem_euclid(24), 0, "{h}");
        assert!(jiff::tz::TimeZone::get(&etc_zone(off)).is_ok(), "{off}");
    }
}
