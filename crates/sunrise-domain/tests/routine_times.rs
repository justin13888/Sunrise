//! The routine rows of the `docs/10-cross-cutting/time.md` §8 matrix, over the
//! `SunriseTime` routine fields of #506: expansion in civil space, keys, skips
//! and the streak key across a DST gap and fold, a pause that runs out, a zone
//! change that keeps the wall clock, Lord Howe's half-hour shift, the Apia
//! date line, and the wire form older builds read
//! (`docs/11-adr/0066-times-survive-travel.md`).

use jiff::civil::{date, DateTime};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::routine::{Routine, RoutineCatchupPolicy, TaskTemplate};
use sunrise_domain::{
    inbox_stream_ref, occurrence_task_id, parse_occurrence_key, RRule, SunriseTime, Unknowns,
};
use sunrise_id::{EntityKind, EntityRef};

fn zone(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap()
}

/// A daily 09:00 routine anchored on `day`, `floating` or `zoned` in `tz`.
fn routine(anchor: SunriseTime, tz: &str, rule: &str) -> Routine {
    Routine {
        id: EntityRef::new(EntityKind::Routine, [5; 16]),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        template: TaskTemplate {
            title: "Stretch".into(),
            stream_id: inbox_stream_ref(),
            contexts: Vec::new(),
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
            unknown: Unknowns::new(),
        },
        rrule: RRule::parse(rule).unwrap(),
        timezone: tz.into(),
        starts_at: anchor,
        ends_at: None,
        scheduling_constraints: Vec::new(),
        skip_dates: Vec::new(),
        skipped_keys: Vec::new(),
        catchup_policy: RoutineCatchupPolicy::Skip,
        streak_counter: 0,
        last_completed_at: None,
        grace_window_s: None,
        forgiveness_enabled: true,
        streak_started_at: None,
        forgivenesses_in_window: 0,
        streak_keys: Vec::new(),
        paused: false,
        paused_until: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn civil(y: i16, m: i8, d: i8, h: i8, mi: i8) -> DateTime {
    date(y, m, d).at(h, mi, 0, 0)
}

fn window(from: DateTime, days: i64, tz: &TimeZone) -> (Timestamp, Timestamp) {
    let start = tz.to_zoned(from).unwrap().timestamp();
    let end = start
        .checked_add(jiff::SignedDuration::from_hours(24 * days))
        .unwrap();
    (start, end)
}

fn keys(r: &Routine, w: (Timestamp, Timestamp), reader: &TimeZone) -> Vec<String> {
    r.occurrences_in(w, reader)
        .unwrap()
        .into_iter()
        .map(|o| o.key)
        .collect()
}

/// A `floating` 09:00 routine fires at 09:00 wherever the reader is, and its
/// keys are the same in every zone.
#[test]
fn a_floating_routine_fires_at_its_wall_clock_in_every_readers_zone() {
    let r = routine(
        SunriseTime::floating(civil(2026, 3, 1, 9, 0)),
        "America/Los_Angeles",
        "FREQ=DAILY",
    );
    let mut seen = None;
    for name in [
        "America/Los_Angeles",
        "America/New_York",
        "Asia/Kathmandu",
        "Pacific/Chatham",
        "Australia/Lord_Howe",
    ] {
        let tz = zone(name);
        let w = window(civil(2026, 3, 2, 0, 0), 14, &tz);
        let occ = r.occurrences_in(w, &tz).unwrap();
        assert_eq!(occ.len(), 14, "{name}");
        for o in &occ {
            assert_eq!(
                o.at.to_zoned(tz.clone()).time(),
                jiff::civil::time(9, 0, 0, 0),
                "{name}"
            );
        }
        let k: Vec<String> = occ.into_iter().map(|o| o.key).collect();
        if let Some(prev) = &seen {
            assert_eq!(&k, prev, "keys do not depend on the reader's zone");
        }
        seen = Some(k);
    }
}

/// Re-pinning "09:00 Los Angeles" to New York gives "09:00 New York": the
/// wall clock and the keys are kept, only the instants move.
#[test]
fn rezoning_a_zoned_routine_keeps_its_wall_clock_and_its_keys() {
    let la = zone("America/Los_Angeles");
    let ny = zone("America/New_York");
    let mut r = routine(
        SunriseTime::zoned(civil(2026, 3, 1, 9, 0), "America/Los_Angeles"),
        "America/Los_Angeles",
        "FREQ=DAILY",
    );
    let w = window(civil(2026, 3, 2, 0, 0), 7, &ny);
    let before = r.occurrences_in(w, &ny).unwrap();
    assert!(before
        .iter()
        .all(|o| o.at.to_zoned(la.clone()).time() == jiff::civil::time(9, 0, 0, 0)));
    r.rezone("America/New_York".into());
    assert_eq!(
        r.starts_at,
        SunriseTime::zoned(civil(2026, 3, 1, 9, 0), "America/New_York")
    );
    let after = r.occurrences_in(w, &la).unwrap();
    assert!(after
        .iter()
        .all(|o| o.at.to_zoned(ny.clone()).time() == jiff::civil::time(9, 0, 0, 0)));
    let k = |v: &[sunrise_domain::Occurrence]| v.iter().map(|o| o.key.clone()).collect::<Vec<_>>();
    assert_eq!(k(&before), k(&after));
}

/// A legacy routine (bare instant anchor) keeps firing when it did, and
/// re-zoning it keeps its wall clock too.
#[test]
fn a_legacy_instant_anchor_fires_as_zoned_in_its_zone() {
    let la = zone("America/Los_Angeles");
    let anchor = la.to_zoned(civil(2026, 3, 1, 9, 0)).unwrap().timestamp();
    let mut r = routine(
        SunriseTime::instant(anchor),
        "America/Los_Angeles",
        "FREQ=DAILY",
    );
    let w = window(civil(2026, 3, 2, 0, 0), 3, &la);
    for reader in [zone("UTC"), zone("Asia/Kathmandu")] {
        let occ = r.occurrences_in(w, &reader).unwrap();
        assert!(occ
            .iter()
            .all(|o| o.at.to_zoned(la.clone()).time() == jiff::civil::time(9, 0, 0, 0)));
    }
    r.rezone("Asia/Kathmandu".into());
    let ktm = zone("Asia/Kathmandu");
    assert!(r
        .occurrences_in(window(civil(2026, 3, 2, 0, 0), 3, &ktm), &ktm)
        .unwrap()
        .iter()
        .all(|o| o.at.to_zoned(ktm.clone()).time() == jiff::civil::time(9, 0, 0, 0)));
}

/// On New York's spring-forward day a 02:30 occurrence fires at 03:30, keeps
/// the key `…T02:30`, and the task id, the skip and the streak key all agree
/// on it (#331): the key recovered from the task is the intended one, not the
/// fired instant's wall clock.
#[test]
fn on_a_dst_gap_day_the_task_id_skip_and_streak_share_one_key() {
    for name in ["America/New_York", "Australia/Lord_Howe"] {
        let tz = zone(name);
        let gap_day = if name == "America/New_York" {
            date(2026, 3, 8)
        } else {
            date(2026, 10, 4)
        };
        let r = routine(
            SunriseTime::zoned(civil(2026, 1, 1, 2, 15), name),
            name,
            "FREQ=DAILY",
        );
        let w = window(gap_day.at(0, 0, 0, 0), 1, &tz);
        let occ = r.occurrences_in(w, &tz).unwrap();
        let o = occ.first().expect("the gap day has its occurrence");
        let key = format!("{gap_day}T02:15");
        assert_eq!(o.key, key, "{name}");
        let fired = o.at.to_zoned(tz.clone()).time();
        assert_ne!(
            fired,
            jiff::civil::time(2, 15, 0, 0),
            "{name}: the gap moved it"
        );
        // The old streak key: the fired instant's wall clock. Not the key.
        let wall = sunrise_domain::occurrence_key_at(name, o.at).unwrap();
        assert_ne!(wall, key);
        // The new one: recovered from the task id.
        let task = occurrence_task_id(&r.id, &o.key);
        assert_eq!(
            r.occurrence_key_of(&task, o.at).as_deref(),
            Some(key.as_str())
        );
        // The skip of that key drops the occurrence.
        let mut skipped = r.clone();
        skipped.skipped_keys.push(key.clone());
        assert!(skipped.occurrences_in(w, &tz).unwrap().is_empty(), "{name}");
        assert_eq!(parse_occurrence_key(&key), Some(o.civil));
    }
}

/// On a fall-back night a 01:30 occurrence happens once, at the earlier
/// offset, with its civil key.
#[test]
fn on_a_fold_night_an_occurrence_happens_once() {
    let ny = zone("America/New_York");
    let r = routine(
        SunriseTime::floating(civil(2026, 10, 1, 1, 30)),
        "America/New_York",
        "FREQ=DAILY",
    );
    let occ = r
        .occurrences_in(window(civil(2026, 11, 1, 0, 0), 1, &ny), &ny)
        .unwrap();
    assert_eq!(occ.len(), 1);
    assert_eq!(occ[0].key, "2026-11-01T01:30");
    assert_eq!(
        occ[0].at.to_zoned(ny).offset().seconds(),
        -4 * 3600,
        "EDT, the earlier"
    );
}

/// A pause with an expiry holds back what is before it and generates what is
/// at or after it, on both sides of the boundary, for each kind.
#[test]
fn a_pause_runs_out_at_its_expiry() {
    let tz = zone("Asia/Kathmandu");
    let mut r = routine(
        SunriseTime::floating(civil(2026, 3, 1, 9, 0)),
        "Asia/Kathmandu",
        "FREQ=DAILY",
    );
    let w = window(civil(2026, 3, 1, 0, 0), 10, &tz);
    r.paused = true;
    assert!(
        keys(&r, w, &tz).is_empty(),
        "no expiry: paused until unpaused"
    );

    // Floating: compared with the key's wall clock. 09:00 on the 5th is not
    // before 09:00 on the 5th.
    r.paused_until = Some(SunriseTime::floating(civil(2026, 3, 5, 9, 0)));
    assert_eq!(
        keys(&r, w, &tz).first().map(String::as_str),
        Some("2026-03-05T09:00")
    );
    r.paused_until = Some(SunriseTime::floating(civil(2026, 3, 5, 9, 1)));
    assert_eq!(
        keys(&r, w, &tz).first().map(String::as_str),
        Some("2026-03-06T09:00")
    );

    // All-day: resumes at the start of that day in the reader's zone.
    r.paused_until = Some(SunriseTime::all_day(date(2026, 3, 7)));
    assert_eq!(
        keys(&r, w, &tz).first().map(String::as_str),
        Some("2026-03-07T09:00")
    );

    // An instant: on the timeline.
    let at = tz.to_zoned(civil(2026, 3, 8, 9, 0)).unwrap().timestamp();
    r.paused_until = Some(SunriseTime::instant(at));
    assert_eq!(
        keys(&r, w, &tz).first().map(String::as_str),
        Some("2026-03-08T09:00")
    );
    r.paused_until = Some(SunriseTime::instant(
        at.checked_add(jiff::SignedDuration::from_secs(1)).unwrap(),
    ));
    assert_eq!(
        keys(&r, w, &tz).first().map(String::as_str),
        Some("2026-03-09T09:00")
    );

    // Not paused: the expiry means nothing.
    r.paused = false;
    assert_eq!(keys(&r, w, &tz).len(), 10);
}

/// `ends_at` and `UNTIL` are compared with keys in civil space, so the last
/// occurrence is the same in every reader's zone.
#[test]
fn an_end_and_until_bound_keys_in_civil_space() {
    let mut r = routine(
        SunriseTime::floating(civil(2026, 3, 1, 9, 0)),
        "UTC",
        "FREQ=DAILY;UNTIL=2026-03-05T09:00:00",
    );
    for name in ["Pacific/Chatham", "America/New_York", "UTC"] {
        let tz = zone(name);
        let w = window(civil(2026, 3, 1, 0, 0), 30, &tz);
        assert_eq!(
            keys(&r, w, &tz).last().map(String::as_str),
            Some("2026-03-05T09:00")
        );
    }
    r.rrule = RRule::parse("FREQ=DAILY").unwrap();
    r.ends_at = Some(SunriseTime::all_day(date(2026, 3, 3)));
    let tz = zone("Asia/Kathmandu");
    assert_eq!(
        keys(&r, window(civil(2026, 3, 1, 0, 0), 30, &tz), &tz),
        ["2026-03-01T09:00", "2026-03-02T09:00", "2026-03-03T09:00"]
    );
}

/// Across Apia's date-line move 2011-12-30 does not exist. A daily floating
/// routine still has the key for it, which resolves by the one rule to the
/// same wall clock on the next date that exists; no key is lost.
#[test]
fn a_daily_routine_across_the_apia_date_line_loses_no_key() {
    let apia = zone("Pacific/Apia");
    let r = routine(
        SunriseTime::floating(civil(2011, 12, 25, 9, 0)),
        "Pacific/Apia",
        "FREQ=DAILY",
    );
    let w = window(civil(2011, 12, 28, 0, 0), 5, &apia);
    let occ = r.occurrences_in(w, &apia).unwrap();
    let k: Vec<&str> = occ.iter().map(|o| o.key.as_str()).collect();
    assert!(k.contains(&"2011-12-30T09:00"), "{k:?}");
    let missing = occ.iter().find(|o| o.key == "2011-12-30T09:00").unwrap();
    assert_eq!(missing.at.to_zoned(apia).date(), date(2011, 12, 31));
}

/// The wire form older builds read: a zoned routine in its own zone lowers to
/// bare instants and reads back as itself; a floating one, or a zoned anchor
/// in a DST gap, has no such form.
#[test]
fn only_a_routine_an_older_build_can_read_has_a_legacy_wire_form() {
    let zoned = routine(
        SunriseTime::zoned(civil(2026, 3, 1, 9, 0), "America/New_York"),
        "America/New_York",
        "FREQ=DAILY;UNTIL=2026-06-01T09:00:00[America/New_York]",
    );
    let legacy = zoned.legacy_wire_form().expect("representable");
    assert!(matches!(legacy.starts_at, SunriseTime::Instant { .. }));
    assert!(matches!(
        legacy.rrule.until,
        Some(SunriseTime::Instant { .. })
    ));
    assert_eq!(legacy.normalized(), zoned);

    let floating = routine(
        SunriseTime::floating(civil(2026, 3, 1, 9, 0)),
        "UTC",
        "FREQ=DAILY",
    );
    assert!(floating.legacy_wire_form().is_none());

    let in_gap = routine(
        SunriseTime::zoned(civil(2026, 3, 8, 2, 30), "America/New_York"),
        "America/New_York",
        "FREQ=DAILY",
    );
    assert!(
        in_gap.legacy_wire_form().is_none(),
        "the gap would move 02:30"
    );

    let mut paused = zoned.clone();
    paused.paused_until = Some(SunriseTime::all_day(date(2026, 4, 1)));
    assert!(paused.legacy_wire_form().is_none());
}
