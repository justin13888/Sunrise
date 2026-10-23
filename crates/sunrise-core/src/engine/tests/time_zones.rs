//! `docs/10-cross-cutting/time.md` in the engine (issue #336): Today is the
//! reader's civil day, a time-selecting query's index prefilter is widened and
//! then decided on the resolved value, and both hold at the UTC−10 and UTC+14
//! extremes, where the storage index is furthest from any reader.

use super::*;
use jiff::civil::date;

/// An engine whose clock reads `local` in `zone`.
fn engine_at(zone: &'static str, local: jiff::civil::DateTime) -> (Engine, u64) {
    let tz = jiff::tz::TimeZone::get(zone).unwrap();
    let now = u64::try_from(tz.to_zoned(local).unwrap().timestamp().as_millisecond()).unwrap();
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0x36; 32])));
    let e = Engine::from_clock(Arc::new(ZonedClock(now, zone)), Arc::new(SystemRng), kc);
    (e, now)
}

fn task(
    e: &Engine,
    db: &mut Db,
    title: &str,
    scheduled: Option<SunriseTime>,
    due: Option<SunriseTime>,
) {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            scheduled_at: scheduled,
            due_at: due,
            ..Default::default()
        }),
    )
    .unwrap();
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

/// UTC−10. The storage index puts tomorrow's all-day value and tomorrow's
/// 07:00 inside a rolling 24 hours of 08:00 today; the reader's civil day
/// does not contain them.
#[test]
fn today_is_the_readers_civil_day_in_honolulu() {
    let (e, now) = engine_at("Pacific/Honolulu", date(2026, 3, 4).at(8, 0, 0, 0));
    let mut db = db();
    let d = date(2026, 3, 4);
    let next = date(2026, 3, 5);
    task(
        &e,
        &mut db,
        "tonight",
        Some(SunriseTime::floating(d.at(22, 0, 0, 0))),
        None,
    );
    task(
        &e,
        &mut db,
        "due today",
        None,
        Some(SunriseTime::all_day(d)),
    );
    task(
        &e,
        &mut db,
        "overdue",
        None,
        Some(SunriseTime::all_day(date(2026, 3, 2))),
    );
    task(
        &e,
        &mut db,
        "tomorrow early",
        Some(SunriseTime::floating(next.at(7, 0, 0, 0))),
        None,
    );
    task(
        &e,
        &mut db,
        "due tomorrow",
        None,
        Some(SunriseTime::all_day(next)),
    );
    assert_eq!(today(&e, &db, now), ["overdue", "due today", "tonight"]);
}

/// UTC+14. 23:30 on the 4th is 09:30 UTC on the 4th, so the index already
/// reads the 5th's all-day value and a zoned 00:30 on the 5th as hours away
/// from now; neither is today here.
#[test]
fn today_is_the_readers_civil_day_in_kiritimati() {
    let zone = "Pacific/Kiritimati";
    let (e, now) = engine_at(zone, date(2026, 3, 4).at(23, 30, 0, 0));
    let mut db = db();
    let d = date(2026, 3, 4);
    let next = date(2026, 3, 5);
    task(
        &e,
        &mut db,
        "late",
        Some(SunriseTime::floating(d.at(23, 45, 0, 0))),
        None,
    );
    let an_hour_ago = ms_to_ts(i64::try_from(now).unwrap() - 3_600_000);
    task(&e, &mut db, "an hour ago", Some(an_hour_ago.into()), None);
    task(
        &e,
        &mut db,
        "due tomorrow",
        None,
        Some(SunriseTime::all_day(next)),
    );
    task(
        &e,
        &mut db,
        "just after midnight",
        Some(SunriseTime::zoned(next.at(0, 30, 0, 0), zone)),
        None,
    );
    assert_eq!(today(&e, &db, now), ["an hour ago", "late"]);
}

/// A day and a timed value at the same resolved instant: the day first
/// (`docs/10-cross-cutting/time.md` §2 rule 3).
#[test]
fn today_sorts_an_all_day_value_before_a_timed_one_at_the_same_instant() {
    let (e, now) = engine_at("Asia/Kolkata", date(2026, 3, 4).at(12, 0, 0, 0));
    let mut db = db();
    let d = date(2026, 3, 4);
    task(
        &e,
        &mut db,
        "midnight",
        Some(SunriseTime::floating(d.at(0, 0, 0, 0))),
        None,
    );
    task(&e, &mut db, "the day", Some(SunriseTime::all_day(d)), None);
    assert_eq!(today(&e, &db, now), ["the day", "midnight"]);
}

fn block(
    e: &Engine,
    db: &mut Db,
    title: &str,
    from: jiff::civil::DateTime,
    to: jiff::civil::DateTime,
) {
    e.apply(
        db,
        Command::CreateBlock(BlockDraft {
            starts_at: SunriseTime::floating(from),
            ends_at: SunriseTime::floating(to),
            ..block_draft(9, 1, Some(title))
        }),
    )
    .unwrap();
}

fn titles(rows: Vec<BlockRow>) -> Vec<String> {
    rows.into_iter().filter_map(|r| r.title).collect()
}

/// The day grid reads a widened index window and keeps what resolves into
/// the reader's day. At UTC−10 a 07:00 floating block indexes at 07:00 UTC,
/// three hours before the local day begins on the timeline.
#[test]
fn the_day_grid_keeps_a_morning_block_west_of_utc() {
    let (e, now) = engine_at("Pacific/Honolulu", date(2026, 3, 4).at(12, 0, 0, 0));
    let mut db = db();
    let d = date(2026, 3, 4);
    block(&e, &mut db, "breakfast", d.at(7, 0, 0, 0), d.at(8, 0, 0, 0));
    block(
        &e,
        &mut db,
        "yesterday",
        date(2026, 3, 3).at(7, 0, 0, 0),
        date(2026, 3, 3).at(8, 0, 0, 0),
    );
    assert_eq!(titles(day_rows(&e, &db, now)), ["breakfast"]);
}

/// And at UTC+14 yesterday evening's floating block indexes inside today's
/// window on the timeline; it is not on today's grid.
#[test]
fn the_day_grid_drops_yesterday_evening_east_of_utc() {
    let (e, now) = engine_at("Pacific/Kiritimati", date(2026, 3, 4).at(12, 0, 0, 0));
    let mut db = db();
    let d = date(2026, 3, 4);
    block(
        &e,
        &mut db,
        "dinner yesterday",
        date(2026, 3, 3).at(20, 0, 0, 0),
        date(2026, 3, 3).at(21, 0, 0, 0),
    );
    block(&e, &mut db, "lunch", d.at(12, 0, 0, 0), d.at(13, 0, 0, 0));
    assert_eq!(titles(day_rows(&e, &db, now)), ["lunch"]);
}

/// Validation runs in the reader's zone: a zoned 09:00 New York plan with a
/// floating 11:00 deadline is consistent for a reader in New York and not
/// for one in Kolkata.
#[test]
fn due_before_scheduled_is_decided_in_the_readers_zone() {
    let d = date(2026, 6, 1);
    let draft = TaskDraft {
        title: "t".into(),
        scheduled_at: Some(SunriseTime::zoned(d.at(9, 0, 0, 0), "America/New_York")),
        due_at: Some(SunriseTime::floating(d.at(11, 0, 0, 0))),
        ..Default::default()
    };
    let (ny, _) = engine_at("America/New_York", d.at(8, 0, 0, 0));
    ny.apply(&mut db(), Command::CreateTask(draft.clone()))
        .unwrap();
    let (kolkata, _) = engine_at("Asia/Kolkata", d.at(8, 0, 0, 0));
    let err = kolkata
        .apply(&mut db(), Command::CreateTask(draft))
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::Validation(ValidationError::DueBeforeScheduled)
        ),
        "{err:?}"
    );
}

/// A Stream and a Context list their tasks in the order the reader sees them
/// land (`docs/10-cross-cutting/time.md` §2 rule 3), not in index order. In
/// Los Angeles a floating 09:00 is 17:00Z, after New York's 09:00 (14:00Z),
/// though the index, which anchors the floating value in UTC, puts it first.
/// A task with no time leads, as it did.
#[test]
fn stream_and_context_lists_order_in_the_readers_zone() {
    let (e, _) = engine_at("America/Los_Angeles", date(2026, 3, 4).at(8, 0, 0, 0));
    let mut db = db();
    let ctx = e
        .apply(
            &mut db,
            Command::CreateContext(sunrise_domain::ContextDraft {
                name: "errands".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let d = date(2026, 3, 4);
    for (title, at) in [
        ("floats", Some(SunriseTime::floating(d.at(9, 0, 0, 0)))),
        (
            "pinned",
            Some(SunriseTime::zoned(d.at(9, 0, 0, 0), "America/New_York")),
        ),
        ("unplanned", None),
    ] {
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: title.into(),
                scheduled_at: at,
                contexts: vec![ctx],
                ..Default::default()
            }),
        )
        .unwrap();
    }
    let titles = |q: Query| match e.query(&db, q).unwrap() {
        QueryResult::StreamTasks(v) => v.into_iter().map(|t| t.title).collect::<Vec<_>>(),
        other => panic!("expected tasks, got {other:?}"),
    };
    assert_eq!(titles(Query::Inbox), ["unplanned", "pinned", "floats"]);
    assert_eq!(
        titles(Query::ContextTasks(ctx)),
        ["unplanned", "pinned", "floats"]
    );
}

// ---------------------------------------------------------------------------
// Travel (`docs/10-cross-cutting/time.md` §8, ADR-0066): a zone change
// rewrites nothing stored and recomputes everything derived.
// ---------------------------------------------------------------------------

/// An engine reading `zone` at `now`, whose reader's zone a test reports as a
/// client does on a zone change.
fn traveller(zone: &'static str, now: u64) -> (Engine, Arc<crate::config::ReportedZoneClock>) {
    let clock = Arc::new(crate::config::ReportedZoneClock::new(Arc::new(ZonedClock(
        now, zone,
    ))));
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0x36; 32])));
    let e = Engine::from_clock(clock.clone(), Arc::new(SystemRng), kc);
    (e, clock)
}

fn ms(at: &str) -> u64 {
    u64::try_from(at.parse::<jiff::Timestamp>().unwrap().as_millisecond()).unwrap()
}

fn tz(name: &str) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(name).unwrap()
}

/// [`task`], returning the new task's id.
fn task_id(
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
            scheduled_at: scheduled,
            due_at: due,
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

/// New York to Kolkata at 20:00 EDT, which is 05:30 tomorrow in Kolkata. The
/// floating 09:00 task joins Today at 09:00 IST, the zoned 08:00 New York one
/// keeps its instant, now on Kolkata's yesterday, an all-day deadline on the
/// New York date becomes late, and not one stored task changes.
#[test]
fn new_york_to_kolkata_recomputes_today_and_lateness_and_rewrites_nothing() {
    let now = ms("2026-06-02T00:00:00Z");
    let (e, clock) = traveller("America/New_York", now);
    let mut db = db();
    let (d, next) = (date(2026, 6, 1), date(2026, 6, 2));
    let floating = SunriseTime::floating(next.at(9, 0, 0, 0));
    let pinned = SunriseTime::zoned(d.at(8, 0, 0, 0), "America/New_York");
    let ids = [
        task_id(&e, &mut db, "floating nine", Some(floating.clone()), None),
        task_id(&e, &mut db, "pinned morning", Some(pinned.clone()), None),
        task_id(
            &e,
            &mut db,
            "due today",
            None,
            Some(SunriseTime::all_day(d)),
        ),
    ];
    let stored = |e: &Engine, db: &Db| ids.map(|id| task_of(e, db, id));
    let before = stored(&e, &db);
    assert_eq!(today(&e, &db, now), ["due today", "pinned morning"]);

    clock.report("Asia/Kolkata");
    // The pinned task stays as a slipped plan from yesterday, Kolkata's
    // yesterday; the floating one is today's.
    assert_eq!(
        today(&e, &db, now),
        ["due today", "pinned morning", "floating nine"]
    );
    assert!(sunrise_domain::is_overdue(
        &SunriseTime::all_day(d),
        now,
        &e.device_zone()
    ));
    assert_eq!(stored(&e, &db), before, "a zone change is not an edit");
    assert_eq!(
        floating.resolve_in(&e.device_zone()),
        Some(ms("2026-06-02T03:30:00Z")).map(|m| ms_to_ts(m as i64))
    );
    assert_eq!(
        pinned.resolve_in(&e.device_zone()),
        pinned.resolve_in(&tz("America/New_York"))
    );
    // The floating plan moved and the deadline's day did; the zoned one
    // moved neither.
    let (affected, _) = e
        .time_zone_impact(&db, &tz("America/New_York"), &tz("Asia/Kolkata"), now)
        .unwrap();
    assert_eq!(affected, 2);
}

/// A floating 09:00 reminder moves to 09:00 in the new zone; a zoned 09:00
/// New York one keeps its instant. Each keeps its identity, so a client
/// replaces the pending notification instead of keeping a stale one.
#[test]
fn a_zone_change_replans_the_floating_reminder_and_keeps_the_zoned_one() {
    let now = ms("2026-06-01T12:00:00Z");
    let (e, clock) = traveller("America/New_York", now);
    let mut db = db();
    let nine = date(2026, 6, 2).at(9, 0, 0, 0);
    let floating = task_id(
        &e,
        &mut db,
        "stretch",
        Some(SunriseTime::floating(nine)),
        None,
    );
    let pinned = task_id(
        &e,
        &mut db,
        "standup",
        Some(SunriseTime::zoned(nine, "America/New_York")),
        None,
    );
    let horizon = now as i64 + 3 * 86_400_000;
    let plan = |e: &Engine| reminders(e, &db, now as i64, horizon, ReminderSettings::default());
    let fire = |plan: &[sunrise_domain::ReminderIntent], id: EntityRef| {
        plan.iter().find(|i| i.entity == id).unwrap().clone()
    };

    let in_ny = plan(&e);
    clock.report("Europe/Berlin");
    let in_berlin = plan(&e);

    let at =
        |r: &sunrise_domain::ReminderIntent, zone: &str| r.fire_at.to_zoned(tz(zone)).datetime();
    assert_eq!(at(&fire(&in_ny, floating), "America/New_York"), nine);
    assert_eq!(at(&fire(&in_berlin, floating), "Europe/Berlin"), nine);
    assert_eq!(
        fire(&in_ny, pinned).fire_at,
        fire(&in_berlin, pinned).fire_at
    );
    assert_eq!(
        fire(&in_berlin, pinned).fire_at,
        ms_to_ts(ms("2026-06-02T13:00:00Z") as i64)
    );
    for id in [floating, pinned] {
        assert_eq!(fire(&in_ny, id).identity(), fire(&in_berlin, id).identity());
    }
}

/// A reminder whose time has no instant is not scheduled at a stand-in: the
/// plan reports it, with the reason.
#[test]
fn a_reminder_with_no_instant_is_reported_by_the_plan() {
    let now = ms("2026-06-01T12:00:00Z");
    let (e, _) = traveller("UTC", now);
    let mut db = db();
    // A kind a newer build wrote, with no `at` to place it by.
    let lunar = SunriseTime::Unknown {
        kind: "lunar".into(),
        raw: Unknowns::new(),
    };
    let id = task_id(&e, &mut db, "someday", Some(lunar), None);
    let plan = e
        .reminder_plan(&db, now, now + 86_400_000, &ReminderSettings::default())
        .unwrap();
    assert!(plan.intents.is_empty());
    assert_eq!(plan.unscheduled.len(), 1);
    assert_eq!(plan.unscheduled[0].entity, id);
    assert!(matches!(
        plan.unscheduled[0].reason,
        sunrise_domain::Unresolvable::UnknownKind { .. }
    ));
}

/// A name the tzdb cannot resolve is not UTC: an engine that has resolved a
/// zone keeps reading in it, and only one that never has reads in UTC and
/// says so (`docs/11-adr/0066-times-survive-travel.md` §3).
#[test]
fn an_unresolvable_zone_keeps_the_last_one_that_resolved() {
    let now = ms("2026-06-01T20:00:00Z");
    let (e, clock) = traveller("Mars/Olympus_Mons", now);
    assert_eq!(e.device_zone(), jiff::tz::TimeZone::UTC);
    assert!(e.zone_unknown());

    clock.report("Asia/Kolkata");
    assert_eq!(e.device_zone(), tz("Asia/Kolkata"));
    assert!(!e.zone_unknown());

    clock.report("Mars/Olympus_Mons");
    assert_eq!(e.device_zone(), tz("Asia/Kolkata"), "not UTC");
    assert!(!e.zone_unknown());
    // Today is still Kolkata's: 01:30 on the 2nd there, the 1st in UTC.
    let mut db = db();
    task(
        &e,
        &mut db,
        "kolkata today",
        Some(SunriseTime::all_day(date(2026, 6, 2))),
        None,
    );
    assert_eq!(today(&e, &db, now), ["kolkata today"]);
}

/// Defer keeps the kind it is given: a floating "tomorrow 09:00" moved a
/// civil day is "the day after, 09:00", still floating, and on New York's
/// spring-forward night a floating 02:30 is stored as 02:30 and fires at
/// 03:30 EDT.
#[test]
fn defer_keeps_the_kind_and_the_wall_clock_time() {
    let ny = "America/New_York";
    let now = ms("2026-03-06T15:00:00Z");
    let (e, _) = traveller(ny, now);
    let mut db = db();
    let was = SunriseTime::floating(date(2026, 3, 7).at(2, 30, 0, 0));
    let id = task_id(&e, &mut db, "night shift", Some(was.clone()), None);
    let to = sunrise_domain::shift_civil_days(&was, 1, &e.device_zone()).unwrap();
    e.apply(&mut db, Command::DeferTaskTo { id, to: to.clone() })
        .unwrap();
    let t = task_of(&e, &db, id);
    assert_eq!(
        t.scheduled_at,
        Some(SunriseTime::floating(date(2026, 3, 8).at(2, 30, 0, 0)))
    );
    assert_eq!(t.deferred_count, 1);
    assert_eq!(
        to.resolve_in(&tz(ny)).unwrap().to_zoned(tz(ny)).datetime(),
        date(2026, 3, 8).at(3, 30, 0, 0)
    );

    // An all-day plan moves its date.
    let day = SunriseTime::all_day(date(2026, 3, 7));
    let id = task_id(&e, &mut db, "errand", Some(day.clone()), None);
    let to = sunrise_domain::shift_civil_days(&day, 2, &e.device_zone()).unwrap();
    e.apply(&mut db, Command::DeferTaskTo { id, to }).unwrap();
    assert_eq!(
        task_of(&e, &db, id).scheduled_at,
        Some(SunriseTime::all_day(date(2026, 3, 9)))
    );
}

/// The epoch-millisecond `DeferTask` still works, writing the instant it
/// names.
#[test]
fn the_millisecond_defer_still_writes_an_instant() {
    let now = ms("2026-03-06T15:00:00Z");
    let (e, _) = traveller("UTC", now);
    let mut db = db();
    let id = task_id(&e, &mut db, "t", None, None);
    let to_ms = ms("2026-03-08T09:00:00Z");
    e.apply(&mut db, Command::DeferTask { id, to_ms }).unwrap();
    let t = task_of(&e, &db, id);
    assert_eq!(
        t.scheduled_at,
        Some(SunriseTime::instant(ms_to_ts(to_ms as i64)))
    );
    assert_eq!(t.deferred_count, 1);
}

/// Defer passes the same validation as an edit of `scheduled_at`: past an
/// all-day deadline it is refused, in the reader's zone, and nothing is
/// written.
#[test]
fn defer_past_the_deadline_is_refused_like_an_edit() {
    let now = ms("2026-06-01T12:00:00Z");
    let (e, _) = traveller("Asia/Kolkata", now);
    let mut db = db();
    let due = SunriseTime::all_day(date(2026, 6, 2));
    let id = task_id(&e, &mut db, "taxes", None, Some(due));
    let ops = op_count(&db);
    let late = SunriseTime::floating(date(2026, 6, 3).at(9, 0, 0, 0));
    let err = e
        .apply(
            &mut db,
            Command::DeferTaskTo {
                id,
                to: late.clone(),
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::Validation(ValidationError::DueBeforeScheduled)
        ),
        "{err:?}"
    );
    let edit = e
        .apply(
            &mut db,
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    scheduled_at: Some(Some(late)),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
    assert_eq!(format!("{edit:?}"), format!("{err:?}"));
    assert_eq!(op_count(&db), ops, "nothing written");
    assert_eq!(task_of(&e, &db, id).deferred_count, 0);
}
