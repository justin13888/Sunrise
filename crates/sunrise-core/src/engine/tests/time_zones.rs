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
