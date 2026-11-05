//! Table tests for the day schedule (`docs/02-domain/day-schedule.md`): the
//! precedence order, a sleep past midnight, DST gaps and folds in three
//! zones, a skipped date, and an unset schedule.

use super::*;
use jiff::civil::{date, time};

fn tz(name: &str) -> TimeZone {
    TimeZone::get(name).expect(name)
}

fn ts(s: &str) -> Timestamp {
    s.parse().expect(s)
}

fn w(wake: (i8, i8), sleep: (i8, i8)) -> DayWindow {
    DayWindow::new(time(wake.0, wake.1, 0, 0), time(sleep.0, sleep.1, 0, 0)).expect("distinct")
}

/// Every weekday the same window.
fn daily(window: DayWindow) -> DaySchedule {
    DaySchedule {
        weekday: [Some(window); 7],
        ..DaySchedule::default()
    }
}

#[test]
fn the_first_match_wins_date_then_month_day_then_weekday_then_unset() {
    let weekday = w((7, 0), (23, 0));
    let first = w((5, 0), (22, 0));
    let last = w((9, 0), (23, 30));
    let holiday = w((10, 0), (2, 0));
    let mut s = DaySchedule::default();
    // 2026-12-01 is a Tuesday.
    s.weekday[1] = Some(weekday);
    s.month_day.insert(1, first);
    s.month_day.insert(-1, last);
    s.date
        .insert(date(2026, 12, 25), DayOverride::Window(holiday));
    s.date.insert(date(2026, 12, 8), DayOverride::None);
    s.date
        .insert(date(2026, 12, 1), DayOverride::Window(holiday));
    let cases = [
        // (date, expected, why)
        (
            date(2026, 12, 1),
            Some(holiday),
            "a date override beats day 1",
        ),
        (date(2026, 11, 1), Some(first), "day of month beats nothing"),
        (date(2026, 9, 1), Some(first), "day 1 beats Tuesday"),
        (
            date(2026, 12, 25),
            Some(holiday),
            "the override, on a Friday",
        ),
        (
            date(2026, 12, 8),
            None,
            "`none` stops the search on a Tuesday",
        ),
        (date(2026, 12, 15), Some(weekday), "a plain Tuesday"),
        (date(2026, 12, 16), None, "a Wednesday has nothing"),
        (date(2026, 12, 31), Some(last), "-1 on the last day"),
        (
            date(2026, 2, 28),
            Some(last),
            "-1 on a 28-day month's last day",
        ),
        (
            date(2028, 2, 28),
            None,
            "not the last day of a leap February",
        ),
        (
            date(2028, 2, 29),
            Some(last),
            "the leap day is the last day",
        ),
    ];
    for (d, expected, why) in cases {
        assert_eq!(s.window_for(d), expected, "{d}: {why}");
    }
    // Day 31 beats -1 on the 31st, and simply never matches a 30-day month.
    s.month_day.insert(31, first);
    assert_eq!(s.window_for(date(2027, 1, 31)), Some(first));
    assert_eq!(s.window_for(date(2026, 11, 30)), Some(last));
    assert_eq!(DaySchedule::default().window_for(date(2026, 12, 1)), None);
}

#[test]
fn equal_wake_and_sleep_is_refused() {
    assert_eq!(DayWindow::new(time(7, 0, 0, 0), time(7, 0, 0, 0)), None);
    assert!(w((7, 0), (1, 0)).crosses_midnight());
    assert!(!w((7, 0), (23, 0)).crosses_midnight());
}

#[test]
fn an_unset_schedule_is_the_civil_day_in_every_zone() {
    let s = DaySchedule::default();
    for (zone, d, start, end) in [
        (
            "UTC",
            date(2026, 3, 8),
            "2026-03-08T00:00Z",
            "2026-03-09T00:00Z",
        ),
        // 23 hours: the spring-forward day.
        (
            "America/New_York",
            date(2026, 3, 8),
            "2026-03-08T05:00Z",
            "2026-03-09T04:00Z",
        ),
        // 25 hours: the fall-back day.
        (
            "America/New_York",
            date(2026, 11, 1),
            "2026-11-01T04:00Z",
            "2026-11-02T05:00Z",
        ),
        (
            "Asia/Kolkata",
            date(2026, 3, 8),
            "2026-03-07T18:30Z",
            "2026-03-08T18:30Z",
        ),
    ] {
        let z = tz(zone);
        assert_eq!(
            day_bounds(&s, d, &z).unwrap(),
            (ts(start), ts(end)),
            "{zone} {d}"
        );
        assert_eq!(plannable(&s, d, &z).unwrap(), None);
        assert_eq!(wind_down_at(&s, d, &z, 3600).unwrap(), None);
    }
}

/// The issue's case: someone who sleeps at 01:00. A task done at 00:30 lands
/// on the day they were living, and so does the night after 01:00: the next
/// planner day starts at the next wake.
#[test]
fn a_sleep_past_midnight_extends_the_day_until_the_next_wake() {
    let s = daily(w((7, 0), (1, 0)));
    let z = tz("UTC");
    let (start, end) = day_bounds(&s, date(2026, 3, 2), &z).unwrap();
    // 07:00 Monday, since Sunday's sleep (01:00 Monday) is earlier.
    assert_eq!(start, ts("2026-03-02T07:00Z"));
    assert_eq!(end, ts("2026-03-03T07:00Z"));
    assert_eq!(
        plannable(&s, date(2026, 3, 2), &z).unwrap(),
        Some((ts("2026-03-02T07:00Z"), ts("2026-03-03T01:00Z")))
    );
    for (at, day) in [
        ("2026-03-03T00:30Z", date(2026, 3, 2)),
        ("2026-03-03T00:59:59Z", date(2026, 3, 2)),
        ("2026-03-03T03:00Z", date(2026, 3, 2)),
        ("2026-03-03T06:59:59Z", date(2026, 3, 2)),
        ("2026-03-03T07:00Z", date(2026, 3, 3)),
        ("2026-03-02T23:00Z", date(2026, 3, 2)),
    ] {
        assert_eq!(planner_date(&s, ts(at), &z).unwrap(), day, "{at}");
    }
    let done = SunriseTime::Instant {
        at: ts("2026-03-03T00:30Z"),
    };
    assert_eq!(planner_day_of(&s, &done, &z), Some(date(2026, 3, 2)));
    let all_day = SunriseTime::AllDay {
        date: date(2026, 3, 3),
    };
    assert_eq!(planner_day_of(&s, &all_day, &z), Some(date(2026, 3, 3)));
}

/// A late sleep runs past the next day's wake when the next day starts early:
/// the boundary is the later of the two.
#[test]
fn the_boundary_is_the_later_of_the_next_wake_and_the_previous_sleep() {
    let mut s = daily(w((7, 0), (23, 0)));
    s.date
        .insert(date(2026, 3, 2), DayOverride::Window(w((9, 0), (6, 0))));
    let z = tz("UTC");
    // Monday's sleep is 06:00 Tuesday; Tuesday's wake 07:00 is later.
    assert_eq!(
        day_bounds(&s, date(2026, 3, 3), &z).unwrap().0,
        ts("2026-03-03T07:00Z")
    );
    // A Tuesday that starts at 05:00 waits for Monday's 06:00 sleep.
    s.date
        .insert(date(2026, 3, 3), DayOverride::Window(w((5, 0), (23, 0))));
    assert_eq!(
        day_bounds(&s, date(2026, 3, 3), &z).unwrap().0,
        ts("2026-03-03T06:00Z")
    );
    assert_eq!(
        plannable(&s, date(2026, 3, 3), &z).unwrap(),
        Some((ts("2026-03-03T05:00Z"), ts("2026-03-03T23:00Z"))),
        "the plannable window is still wake to sleep"
    );
}

/// DST in three zones: a wake in a gap shifts forward by the gap's length,
/// a sleep in a fold takes the earlier instant (`time.md` §3), and the
/// planner days stay contiguous.
#[test]
fn dst_gaps_and_folds_resolve_by_the_one_rule() {
    let cases = [
        // (zone, window, date, plannable start, plannable end, why)
        (
            "America/New_York",
            w((2, 30), (23, 0)),
            date(2026, 3, 8),
            "2026-03-08T07:30Z",
            "2026-03-09T03:00Z",
            "02:30 does not exist: 03:30 EDT",
        ),
        (
            "America/New_York",
            w((8, 0), (1, 30)),
            date(2026, 10, 31),
            "2026-10-31T12:00Z",
            "2026-11-01T05:30Z",
            "01:30 happens twice: the first, EDT",
        ),
        (
            "Australia/Lord_Howe",
            w((2, 10), (22, 0)),
            date(2026, 10, 4),
            "2026-10-03T15:40Z",
            "2026-10-04T11:00Z",
            "a 30-minute gap: 02:10 is 02:40 +11",
        ),
        (
            "Australia/Lord_Howe",
            w((8, 0), (1, 45)),
            date(2026, 4, 4),
            "2026-04-03T21:00Z",
            "2026-04-04T14:45Z",
            "01:45 twice: the first, +11",
        ),
        (
            "Pacific/Chatham",
            w((3, 0), (22, 0)),
            date(2026, 9, 27),
            "2026-09-26T14:15Z",
            "2026-09-27T08:15Z",
            "02:45 to 03:45 skipped: 03:00 is 04:00 +13:45",
        ),
        (
            "Pacific/Chatham",
            w((8, 0), (3, 0)),
            date(2026, 4, 4),
            "2026-04-03T18:15Z",
            "2026-04-04T13:15Z",
            "03:00 twice: the first, +13:45",
        ),
    ];
    for (zone, window, d, start, end, why) in cases {
        let z = tz(zone);
        let s = daily(window);
        assert_eq!(
            plannable(&s, d, &z).unwrap(),
            Some((ts(start), ts(end))),
            "{zone} {d}: {why}"
        );
        assert_contiguous_and_total(&s, &z, d);
    }
}

/// Over a week around `around`, each planner day ends where the next starts,
/// and every instant at a 7-minute step falls in the day [`planner_date`]
/// names.
fn assert_contiguous_and_total(s: &DaySchedule, z: &TimeZone, around: Date) {
    let first = around.checked_sub(jiff::Span::new().days(3)).unwrap();
    let mut d = first;
    let mut prev_end = None;
    for _ in 0..7 {
        let (start, end) = day_bounds(s, d, z).unwrap();
        assert!(start <= end, "{d} runs backwards");
        if let Some(p) = prev_end {
            assert_eq!(p, start, "a gap or overlap before {d}");
        }
        let mut t = start;
        while t < end {
            assert_eq!(planner_date(s, t, z).unwrap(), d, "{t} in {z:?}");
            t = t.checked_add(SignedDuration::from_mins(7)).unwrap();
        }
        prev_end = Some(end);
        d = d.tomorrow().unwrap();
    }
}

#[test]
fn a_date_the_zone_skips_is_an_empty_planner_day() {
    let z = tz("Pacific/Apia");
    for s in [DaySchedule::default(), daily(w((7, 0), (23, 0)))] {
        let (start, end) = day_bounds(&s, date(2011, 12, 30), &z).unwrap();
        assert_eq!(start, end, "2011-12-30 did not happen in Apia");
        assert_eq!(plannable(&s, date(2011, 12, 30), &z).unwrap(), None);
        assert_eq!(wind_down_at(&s, date(2011, 12, 30), &z, 60).unwrap(), None);
        assert_contiguous_and_total(&s, &z, date(2011, 12, 30));
    }
    // A late sleep on the 29th carries across the skipped date.
    let s = daily(w((7, 0), (1, 0)));
    assert_eq!(
        planner_date(&s, ts("2011-12-30T10:30Z"), &z).unwrap(),
        date(2011, 12, 29),
        "00:30 on the 31st, before the 29th's 01:00 sleep"
    );
    assert_contiguous_and_total(&s, &z, date(2011, 12, 30));
}

#[test]
fn the_schedule_follows_the_reader_into_another_zone() {
    let s = daily(w((7, 0), (1, 0)));
    let instant = ts("2026-03-03T05:30Z");
    // 00:30 in New York, before the 01:00 sleep: still Monday.
    assert_eq!(
        planner_date(&s, instant, &tz("America/New_York")).unwrap(),
        date(2026, 3, 2)
    );
    // 05:30 in London, after the sleep but before the wake: still Monday,
    // whose planner day runs to Tuesday's wake.
    assert_eq!(
        planner_date(&s, instant, &tz("Europe/London")).unwrap(),
        date(2026, 3, 2)
    );
    // 14:30 in Tokyo: Tuesday.
    assert_eq!(
        planner_date(&s, instant, &tz("Asia/Tokyo")).unwrap(),
        date(2026, 3, 3)
    );
}

#[test]
fn wind_down_counts_back_from_the_resolved_sleep() {
    let s = daily(w((7, 0), (1, 0)));
    let z = tz("America/New_York");
    let wd = wind_down_at(&s, date(2026, 3, 2), &z, 3600)
        .unwrap()
        .unwrap();
    assert_eq!(wd.sleep_at, ts("2026-03-03T06:00Z"));
    assert_eq!(wd.fire_at, ts("2026-03-03T05:00Z"));
    // At 23:00 Monday the next one is tonight's; just after it fired, the
    // next is Tuesday's.
    let next = |at: &str| next_wind_down(&s, ts(at), &z, 3600).unwrap().unwrap();
    assert_eq!(next("2026-03-03T04:00Z").date, date(2026, 3, 2));
    assert_eq!(next("2026-03-03T05:00:01Z").date, date(2026, 3, 3));
    // A night with no schedule fires nothing.
    let mut off = s.clone();
    off.date.insert(date(2026, 3, 2), DayOverride::None);
    assert_eq!(
        wind_down_at(&off, date(2026, 3, 2), &z, 3600).unwrap(),
        None
    );
}

#[test]
fn entry_keys_round_trip_and_reject_what_they_do_not_name() {
    for (entry, key) in [
        (
            DayScheduleEntry::Weekday(Weekday::Mo),
            "day_schedule.weekday.MO",
        ),
        (DayScheduleEntry::MonthDay(1), "day_schedule.month_day.1"),
        (DayScheduleEntry::MonthDay(-1), "day_schedule.month_day.-1"),
        (
            DayScheduleEntry::Date(date(2026, 12, 25)),
            "day_schedule.date.2026-12-25",
        ),
    ] {
        assert_eq!(entry.key(), key);
        assert_eq!(DayScheduleEntry::parse_key(key), Some(entry), "{key}");
        assert!(crate::pref_spec(key).is_some(), "{key} is a key");
    }
    for bad in [
        "day_schedule.weekday.XX",
        "day_schedule.month_day.0",
        "day_schedule.month_day.32",
        "day_schedule.month_day.01",
        "day_schedule.month_day.-2",
        "day_schedule.date.2026-02-30",
        "day_schedule.date.tomorrow",
        "day_schedule.weekday",
        "day_schedule.other.MO",
    ] {
        assert_eq!(DayScheduleEntry::parse_key(bad), None, "{bad}");
        assert!(crate::pref_spec(bad).is_none(), "{bad} is not a key");
    }
}

#[test]
fn the_schedule_reads_out_of_the_preferences_map_and_skips_what_it_cannot() {
    let mut values = BTreeMap::new();
    let put = |values: &mut BTreeMap<String, CborValue>, key: &str, v: PrefValue| {
        values.insert(key.to_owned(), v.encode());
    };
    let window = PrefValue::DayWindow {
        wake: time(7, 0, 0, 0),
        sleep: time(1, 0, 0, 0),
    };
    put(&mut values, "day_schedule.weekday.MO", window.clone());
    put(&mut values, "day_schedule.month_day.-1", window.clone());
    put(
        &mut values,
        "day_schedule.date.2026-12-25",
        PrefValue::NoSchedule,
    );
    put(&mut values, "day_schedule.date.2026-12-24", window);
    // A shape this build cannot read, and a key it does not know.
    values.insert(
        "day_schedule.weekday.TU".into(),
        CborValue(ciborium::value::Value::Text("early".into())),
    );
    put(
        &mut values,
        "day_schedule.weekday.XX",
        PrefValue::Bool(true),
    );
    put(&mut values, "week_start", PrefValue::Weekday(Weekday::Mo));

    let s = DaySchedule::from_values(&values);
    let expected = w((7, 0), (1, 0));
    assert_eq!(s.weekday_window(&Weekday::Mo), Some(expected));
    assert_eq!(s.weekday_window(&Weekday::Tu), None);
    assert_eq!(s.month_day.get(&-1), Some(&expected));
    assert_eq!(s.date.get(&date(2026, 12, 25)), Some(&DayOverride::None));
    assert_eq!(
        s.date.get(&date(2026, 12, 24)),
        Some(&DayOverride::Window(expected))
    );
    assert_eq!(s.date.len(), 2);
    assert!(DaySchedule::from_values(&BTreeMap::new()).is_empty());
}

#[test]
fn the_override_type_reads_none_and_refuses_a_false_none() {
    use crate::PrefType;
    let none = PrefValue::NoSchedule.encode();
    assert_eq!(
        PrefType::DayOverride.decode(&none),
        Some(PrefValue::NoSchedule)
    );
    assert_eq!(PrefType::DayWindow.decode(&none), None);
    let false_none = CborValue(ciborium::value::Value::Map(vec![(
        ciborium::value::Value::Text("none".into()),
        ciborium::value::Value::Bool(false),
    )]));
    assert_eq!(PrefType::DayOverride.decode(&false_none), None);
    let same = PrefValue::DayWindow {
        wake: time(7, 0, 0, 0),
        sleep: time(7, 0, 0, 0),
    };
    assert_eq!(PrefType::DayWindow.decode(&same.encode()), None);
}

/// The morning brief is wake plus its offset and the evening plan sleep less
/// its offset, the sleep resolved on the next date when it crosses midnight;
/// a date with no schedule fires at 08:00 and 20:00, whatever the offset.
#[test]
fn briefs_count_from_wake_and_sleep_and_fall_back_to_eight_and_twenty() {
    let s = daily(w((7, 0), (1, 0)));
    let z = tz("America/New_York");
    let d = date(2026, 3, 2);
    let at = |which, off| brief_at(&s, d, &z, which, off).unwrap().unwrap();
    assert_eq!(at(Brief::Morning, 900), ts("2026-03-02T12:15Z"));
    assert_eq!(at(Brief::Morning, -600), ts("2026-03-02T11:50Z"));
    assert_eq!(
        at(Brief::Evening, 7200),
        ts("2026-03-03T04:00Z"),
        "23:00 Monday"
    );
    let none = DaySchedule::default();
    assert_eq!(
        brief_at(&none, d, &z, Brief::Morning, 900).unwrap(),
        Some(ts("2026-03-02T13:00Z"))
    );
    assert_eq!(
        brief_at(&none, d, &z, Brief::Evening, 7200).unwrap(),
        Some(ts("2026-03-03T01:00Z"))
    );
    // A date the zone skips has no brief.
    assert_eq!(
        brief_at(
            &none,
            date(2011, 12, 30),
            &tz("Pacific/Apia"),
            Brief::Morning,
            0
        )
        .unwrap(),
        None
    );
}

/// On the spring-forward day a 02:30 wake does not exist: the brief lands
/// at the shifted wake, 03:30 EDT, plus its offset.
#[test]
fn a_brief_on_a_dst_day_follows_the_shifted_wake() {
    let s = daily(w((2, 30), (22, 0)));
    let z = tz("America/New_York");
    assert_eq!(
        brief_at(&s, date(2026, 3, 8), &z, Brief::Morning, 900).unwrap(),
        Some(ts("2026-03-08T07:45Z"))
    );
}

#[test]
fn the_next_brief_is_todays_until_it_fires_then_tomorrows() {
    let s = daily(w((7, 0), (23, 0)));
    let z = tz("America/New_York");
    let next = |at: &str, which| next_brief(&s, ts(at), &z, which, 900).unwrap().unwrap();
    assert_eq!(
        next("2026-03-02T12:00Z", Brief::Morning),
        (date(2026, 3, 2), ts("2026-03-02T12:15Z"))
    );
    assert_eq!(
        next("2026-03-02T12:15:01Z", Brief::Morning),
        (date(2026, 3, 3), ts("2026-03-03T12:15Z"))
    );
    assert_eq!(
        next("2026-03-02T12:00Z", Brief::Evening),
        (date(2026, 3, 2), ts("2026-03-03T03:45Z")),
        "22:45 EST"
    );
    assert_eq!(
        next("2026-03-03T03:45:01Z", Brief::Evening),
        (date(2026, 3, 3), ts("2026-03-04T03:45Z"))
    );
}
