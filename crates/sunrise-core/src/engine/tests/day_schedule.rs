//! The planner day in the engine (#338, `docs/02-domain/day-schedule.md`):
//! Today, Upcoming and the briefs read the day schedule's bounds, and a zone
//! change moves those bounds without rewriting anything stored.

use super::*;
use jiff::civil::date;
use sunrise_domain::{PrefTarget, PrefValue};

/// The one vault root, so two engines read the same database as one device at
/// two moments.
const ROOT: [u8; 32] = [0x38; 32];

fn sleep_at_one(e: &Engine, db: &mut Db) {
    for day in ["MO", "TU", "WE", "TH", "FR", "SA", "SU"] {
        e.apply(
            db,
            Command::SetPreference {
                key: format!("day_schedule.weekday.{day}"),
                value: PrefValue::DayWindow {
                    wake: jiff::civil::time(7, 0, 0, 0),
                    sleep: jiff::civil::time(1, 0, 0, 0),
                },
                target: PrefTarget::Vault,
            },
        )
        .unwrap();
    }
}

fn task(
    e: &Engine,
    db: &mut Db,
    title: &str,
    scheduled: Option<SunriseTime>,
    due: Option<SunriseTime>,
) -> EntityRef {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            planned_at: scheduled,
            hard_due_at: due,
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn today(e: &Engine, db: &Db, now: u64) -> Vec<String> {
    match e
        .query(
            db,
            Query::Today {
                now_ms: now,
                contexts: vec![],
            },
        )
        .unwrap()
    {
        QueryResult::Tasks(v) => v.into_iter().map(|t| t.title).collect(),
        other => panic!("expected tasks, got {other:?}"),
    }
}

fn stored(db: &Db) -> Option<std::collections::BTreeMap<String, sunrise_domain::CborValue>> {
    crate::engine::preferences::read_preferences(
        db.conn(),
        sunrise_domain::preferences_ref().bytes(),
    )
    .unwrap()
    .map(|p| p.values)
}

fn ts(s: &str) -> jiff::Timestamp {
    s.parse().unwrap()
}

/// 00:30 on Tuesday, for someone who sleeps at 01:00, is still Monday: Today
/// holds Monday's evening and not Tuesday's morning, and an all-day deadline
/// on Tuesday is not here yet. At 08:00 Tuesday it is.
#[test]
fn today_is_the_planner_day_so_a_late_night_is_still_the_day_before() {
    let (late, late_now) = engine_at(ROOT, "America/New_York", date(2026, 3, 3).at(0, 30, 0, 0));
    let mut db = db_root(ROOT);
    sleep_at_one(&late, &mut db);
    task(
        &late,
        &mut db,
        "monday evening",
        Some(SunriseTime::floating(date(2026, 3, 2).at(22, 0, 0, 0))),
        None,
    );
    task(
        &late,
        &mut db,
        "tuesday morning",
        Some(SunriseTime::floating(date(2026, 3, 3).at(9, 0, 0, 0))),
        None,
    );
    task(
        &late,
        &mut db,
        "due tuesday",
        None,
        Some(SunriseTime::all_day(date(2026, 3, 3))),
    );
    task(
        &late,
        &mut db,
        "due monday",
        None,
        Some(SunriseTime::all_day(date(2026, 3, 2))),
    );

    let titles = today(&late, &db, late_now);
    assert!(titles.contains(&"monday evening".to_owned()), "{titles:?}");
    assert!(titles.contains(&"due monday".to_owned()), "{titles:?}");
    assert!(
        !titles.contains(&"tuesday morning".to_owned()),
        "{titles:?}"
    );
    assert!(!titles.contains(&"due tuesday".to_owned()), "{titles:?}");

    let QueryResult::DaySchedule(day) = late
        .query(&db, Query::DaySchedule { at_ms: late_now })
        .unwrap()
    else {
        panic!("not a planner day");
    };
    assert_eq!(day.date, date(2026, 3, 2));
    assert_eq!(day.start, ts("2026-03-02T12:00Z"), "Monday 07:00 EST");
    assert_eq!(day.end, ts("2026-03-03T12:00Z"), "Tuesday 07:00 EST");
    assert_eq!(
        day.plannable,
        Some((ts("2026-03-02T12:00Z"), ts("2026-03-03T06:00Z")))
    );

    let (morning, morning_now) =
        engine_at(ROOT, "America/New_York", date(2026, 3, 3).at(8, 0, 0, 0));
    let titles = today(&morning, &db, morning_now);
    for t in ["tuesday morning", "due tuesday", "due monday"] {
        assert!(titles.contains(&t.to_owned()), "{t}: {titles:?}");
    }
}

/// The morning brief's "since" and "today" are planner-day starts: a task
/// done at 00:30, before a 01:00 sleep, is yesterday's the next morning.
/// The evening plan's day and week end are planner-day bounds too.
#[test]
fn the_briefs_read_planner_day_bounds() {
    let (late, _) = engine_at(ROOT, "America/New_York", date(2026, 3, 3).at(0, 30, 0, 0));
    let mut db = db_root(ROOT);
    sleep_at_one(&late, &mut db);
    let done = task(&late, &mut db, "late one", None, None);
    late.apply(&mut db, Command::CompleteTask(done)).unwrap();

    let (morning, now) = engine_at(ROOT, "America/New_York", date(2026, 3, 3).at(8, 0, 0, 0));
    let QueryResult::MorningSummary(s) = morning
        .query(&db, Query::MorningSummary { now_ms: now })
        .unwrap()
    else {
        panic!("not a summary");
    };
    assert_eq!(s.since, ts("2026-03-02T12:00Z"), "Monday's planner day");
    assert_eq!(s.today_start, ts("2026-03-03T12:00Z"), "Tuesday 07:00");
    assert_eq!(s.completed.len(), 1);
    let at = s.completed[0]
        .completed_at
        .as_ref()
        .unwrap()
        .resolve_in(&morning.device_zone())
        .unwrap();
    assert!(at < s.today_start, "done at 00:30 is yesterday's");

    let QueryResult::EndOfDayPlan(p) = morning
        .query(&db, Query::EndOfDayPlan { now_ms: now })
        .unwrap()
    else {
        panic!("not a plan");
    };
    assert_eq!(p.day_start, ts("2026-03-03T12:00Z"));
    assert_eq!(p.day_end, ts("2026-03-04T12:00Z"));
    assert_eq!(
        p.week_end,
        ts("2026-03-11T11:00Z"),
        "Wednesday the 11th, 07:00 EDT"
    );
}

/// Upcoming starts on the planner today and lays its days out on the
/// schedule's boundaries.
#[test]
fn upcoming_days_are_planner_days() {
    let (late, now) = engine_at(ROOT, "America/New_York", date(2026, 3, 3).at(0, 30, 0, 0));
    let mut db = db_root(ROOT);
    sleep_at_one(&late, &mut db);
    let QueryResult::Upcoming(u) = late
        .query(
            &db,
            Query::Upcoming {
                now_ms: now,
                span_days: Some(7),
                contexts: vec![],
            },
        )
        .unwrap()
    else {
        panic!("not upcoming");
    };
    assert_eq!(u.today, date(2026, 3, 2));
    assert_eq!(u.days[0].start, ts("2026-03-02T12:00Z"));
    assert_eq!(u.days[0].end, u.days[1].start);
    // The night the clocks spring forward (01:00 to 07:00 Sunday) is
    // Saturday's, so Saturday's planner day is the 23-hour one, wake to wake.
    let saturday = u.days.iter().find(|d| d.date == date(2026, 3, 7)).unwrap();
    assert_eq!(saturday.start, ts("2026-03-07T12:00Z"), "07:00 EST");
    assert_eq!(saturday.end, ts("2026-03-08T11:00Z"), "07:00 EDT");
}

/// The traveller: the same instant read in another zone is another planner
/// day, and nothing stored moves. 00:30 in New York is Monday's; in Tokyo
/// it is Tuesday afternoon.
#[test]
fn a_zone_change_moves_the_planner_day_and_rewrites_nothing() {
    let (ny, now) = engine_at(ROOT, "America/New_York", date(2026, 3, 3).at(0, 30, 0, 0));
    let mut db = db_root(ROOT);
    sleep_at_one(&ny, &mut db);
    let before = stored(&db);
    let day = |e: &Engine| match e.query(&db, Query::DaySchedule { at_ms: now }).unwrap() {
        QueryResult::DaySchedule(d) => d.date,
        other => panic!("{other:?}"),
    };
    assert_eq!(day(&ny), date(2026, 3, 2));
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes(ROOT)));
    let tokyo = Engine::from_clock(
        Arc::new(ZonedClock(now, "Asia/Tokyo")),
        Arc::new(SystemRng),
        kc,
    );
    assert_eq!(day(&tokyo), date(2026, 3, 3));
    assert_eq!(stored(&db), before);
}

// ---- a DST day is still a day ----

/// An engine in New York whose clock reads the instant `at`.
fn ny_at(at: &str) -> (Engine, u64) {
    let now = u64::try_from(ts(at).as_millisecond()).unwrap();
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes(ROOT)));
    let e = Engine::from_clock(
        Arc::new(ZonedClock(now, "America/New_York")),
        Arc::new(SystemRng),
        kc,
    );
    (e, now)
}

/// A task completed at the instant `at`, written by a device whose clock
/// reads it.
fn done_at(db: &mut Db, title: &str, at: &str) {
    let (e, _) = ny_at(at);
    let id = task(&e, db, title, None, None);
    e.apply(db, Command::CompleteTask(id)).unwrap();
}

fn briefs(e: &Engine, db: &Db, now: u64) -> (MorningSummary, EndOfDayPlan) {
    let QueryResult::MorningSummary(s) =
        e.query(db, Query::MorningSummary { now_ms: now }).unwrap()
    else {
        panic!("not a summary");
    };
    let QueryResult::EndOfDayPlan(p) = e.query(db, Query::EndOfDayPlan { now_ms: now }).unwrap()
    else {
        panic!("not a plan");
    };
    (*s, *p)
}

fn titles_of(tasks: &[Task]) -> Vec<&str> {
    let mut v: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
    v.sort_unstable();
    v
}

/// The night New York springs forward (Sunday 8 March 2026), with no day
/// schedule: Sunday is a 23-hour day, and the briefs bound it by its civil
/// midnights, never by 24 hours. Monday's "since" is Sunday's midnight, so
/// a completion an hour before it is not news; Sunday's brief lists what
/// lands before Monday's midnight, and not what lands in the hour after it
/// that a 24-hour day would have reached.
#[test]
fn the_briefs_bound_a_spring_forward_day_by_its_midnights() {
    let mut db = db_root(ROOT);
    done_at(&mut db, "saturday 23:30", "2026-03-08T04:30Z");
    done_at(&mut db, "sunday 00:30", "2026-03-08T05:30Z");
    let (sunday, now) = ny_at("2026-03-08T16:00Z");
    for (title, at) in [
        ("sunday 23:30", "2026-03-09T03:30Z"),
        ("monday 00:30", "2026-03-09T04:30Z"),
    ] {
        task(
            &sunday,
            &mut db,
            title,
            Some(SunriseTime::instant(ts(at))),
            None,
        );
    }

    let (s, p) = briefs(&sunday, &db, now);
    assert_eq!(s.since, ts("2026-03-07T05:00Z"), "Saturday 00:00 EST");
    assert_eq!(s.today_start, ts("2026-03-08T05:00Z"), "Sunday 00:00 EST");
    assert_eq!(titles_of(&s.due_today), ["sunday 23:30"]);
    assert_eq!(p.day_start, ts("2026-03-08T05:00Z"));
    assert_eq!(p.day_end, ts("2026-03-09T04:00Z"), "Monday 00:00 EDT");
    assert_eq!(p.week_end, ts("2026-03-16T04:00Z"), "the 16th, 00:00 EDT");

    let (monday, now) = ny_at("2026-03-09T12:00Z");
    let (s, _) = briefs(&monday, &db, now);
    assert_eq!(s.since, ts("2026-03-08T05:00Z"), "Sunday 00:00 EST");
    assert_eq!(s.today_start, ts("2026-03-09T04:00Z"), "Monday 00:00 EDT");
    assert_eq!(titles_of(&s.completed), ["sunday 00:30"]);
}

/// The night New York falls back (Sunday 1 November 2026): Sunday is a
/// 25-hour day. Monday's "since" reaches the whole of it, so a completion
/// in its first half hour is news; Sunday's brief lists what lands in its
/// last hour, which a 24-hour day would have left out.
#[test]
fn the_briefs_bound_a_fall_back_day_by_its_midnights() {
    let mut db = db_root(ROOT);
    done_at(&mut db, "saturday 23:30", "2026-11-01T03:30Z");
    done_at(&mut db, "sunday 00:30", "2026-11-01T04:30Z");
    let (sunday, now) = ny_at("2026-11-01T17:00Z");
    for (title, at) in [
        ("sunday 23:30", "2026-11-02T04:30Z"),
        ("monday 00:30", "2026-11-02T05:30Z"),
    ] {
        task(
            &sunday,
            &mut db,
            title,
            Some(SunriseTime::instant(ts(at))),
            None,
        );
    }

    let (s, p) = briefs(&sunday, &db, now);
    assert_eq!(s.today_start, ts("2026-11-01T04:00Z"), "Sunday 00:00 EDT");
    assert_eq!(titles_of(&s.due_today), ["sunday 23:30"]);
    assert_eq!(p.day_start, ts("2026-11-01T04:00Z"));
    assert_eq!(p.day_end, ts("2026-11-02T05:00Z"), "Monday 00:00 EST");
    assert_eq!(p.week_end, ts("2026-11-09T05:00Z"), "the 9th, 00:00 EST");

    let (monday, now) = ny_at("2026-11-02T13:00Z");
    let (s, _) = briefs(&monday, &db, now);
    assert_eq!(s.since, ts("2026-11-01T04:00Z"), "Sunday 00:00 EDT");
    assert_eq!(s.today_start, ts("2026-11-02T05:00Z"), "Monday 00:00 EST");
    assert_eq!(titles_of(&s.completed), ["sunday 00:30"]);
}
