//! `Query::Upcoming` in the engine (#351): civil days in the reader's zone
//! across DST and a zone change, the span and its preference, and the
//! no-date lane, read through the index prefilter.

use super::*;
use jiff::civil::{date, Date};
use sunrise_domain::{PrefTarget, PrefValue};

type View = crate::queries::Upcoming;

const NEW_YORK: &str = "America/New_York";

/// An engine whose clock reads `local` in `zone`.
fn engine_at(zone: &'static str, local: jiff::civil::DateTime) -> (Engine, u64) {
    let tz = jiff::tz::TimeZone::get(zone).unwrap();
    let now = u64::try_from(tz.to_zoned(local).unwrap().timestamp().as_millisecond()).unwrap();
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0x51; 32])));
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
            planned_at: scheduled,
            hard_due_at: due,
            ..Default::default()
        }),
    )
    .unwrap();
}

fn upcoming(e: &Engine, db: &Db, now: u64, span_days: Option<u32>) -> View {
    match e
        .query(
            db,
            Query::Upcoming {
                now_ms: now,
                span_days,
                contexts: vec![],
            },
        )
        .unwrap()
    {
        QueryResult::Upcoming(v) => *v,
        other => panic!("expected Upcoming, got {other:?}"),
    }
}

fn planned_on(v: &View, d: Date) -> Vec<String> {
    v.days
        .iter()
        .find(|x| x.date == d)
        .unwrap_or_else(|| panic!("{d} is not in the view"))
        .planned
        .iter()
        .map(|t| t.title.clone())
        .collect()
}

/// New York springs forward on 2026-03-08: that day is 23 hours, and a
/// floating plan in the skipped hour, a block across midnight and an all-day
/// deadline are all on the days the reader sees them on.
#[test]
fn upcoming_is_civil_days_across_a_dst_change() {
    let (e, now) = engine_at(NEW_YORK, date(2026, 3, 5).at(10, 0, 0, 0));
    let mut db = db();
    let spring = date(2026, 3, 8);
    task(
        &e,
        &mut db,
        "in the gap",
        Some(SunriseTime::floating(spring.at(2, 30, 0, 0))),
        None,
    );
    task(
        &e,
        &mut db,
        "due sunday",
        None,
        Some(SunriseTime::all_day(spring)),
    );
    e.apply(
        &mut db,
        Command::CreateBlock(BlockDraft {
            starts_at: SunriseTime::floating(date(2026, 3, 7).at(23, 0, 0, 0)),
            ends_at: SunriseTime::floating(spring.at(1, 0, 0, 0)),
            ..block_draft(9, 1, Some("night shift"))
        }),
    )
    .unwrap();

    let v = upcoming(&e, &db, now, Some(7));
    assert_eq!(v.today, date(2026, 3, 5));
    assert_eq!(v.days.len(), 7);
    let hours: Vec<i64> = v
        .days
        .iter()
        .map(|d| (d.end.as_millisecond() - d.start.as_millisecond()) / 3_600_000)
        .collect();
    assert_eq!(hours, [24, 24, 24, 23, 24, 24, 24]);
    assert_eq!(planned_on(&v, spring), ["in the gap"]);
    let sunday = &v.days[3];
    assert_eq!(sunday.due.len(), 1);
    assert_eq!(sunday.due[0].title, "due sunday");
    let night = |d: usize| {
        v.days[d]
            .blocks
            .iter()
            .filter_map(|b| b.title.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(night(2), ["night shift"]);
    assert_eq!(night(3), ["night shift"]);
    assert!(night(4).is_empty());
}

/// Written in New York, read in Tokyo after a flight: the floating plan keeps
/// its wall-clock day, the zoned one moves to the day its instant is in
/// Tokyo, and nothing stored changed.
#[test]
fn a_zone_change_mid_span_moves_only_the_pinned_plan() {
    let (ny, now) = engine_at(NEW_YORK, date(2026, 3, 9).at(9, 0, 0, 0));
    let mut db = db();
    let d = date(2026, 3, 11);
    task(
        &ny,
        &mut db,
        "floating 21:00",
        Some(SunriseTime::floating(d.at(21, 0, 0, 0))),
        None,
    );
    task(
        &ny,
        &mut db,
        "new york 21:00",
        Some(SunriseTime::zoned(d.at(21, 0, 0, 0), NEW_YORK)),
        None,
    );
    let before = upcoming(&ny, &db, now, Some(7));
    assert_eq!(planned_on(&before, d), ["floating 21:00", "new york 21:00"]);

    // The same instant, read by a device now in Tokyo.
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0x51; 32])));
    let tokyo = Engine::from_clock(
        Arc::new(ZonedClock(now, "Asia/Tokyo")),
        Arc::new(SystemRng),
        kc,
    );
    let after = upcoming(&tokyo, &db, now, Some(7));
    assert_eq!(
        after.today,
        date(2026, 3, 9),
        "09:00 in New York is 22:00 in Tokyo"
    );
    assert_eq!(planned_on(&after, d), ["floating 21:00"]);
    assert_eq!(planned_on(&after, date(2026, 3, 12)), ["new york 21:00"]);
    let kinds: Vec<String> = after
        .days
        .iter()
        .flat_map(|day| day.planned.iter())
        .filter_map(|t| t.planned_at.as_ref().map(|s| s.kind_str().to_owned()))
        .collect();
    assert_eq!(kinds, ["floating", "zoned"]);
}

/// The span is the one asked for, else the device's preference, and one the
/// view does not offer is refused rather than clamped.
#[test]
fn the_span_comes_from_the_query_then_the_device_preference() {
    let (e, now) = engine_at(NEW_YORK, date(2026, 3, 5).at(10, 0, 0, 0));
    let mut db = db();
    assert_eq!(upcoming(&e, &db, now, None).days.len(), 7);
    assert_eq!(upcoming(&e, &db, now, Some(30)).days.len(), 30);
    e.apply(
        &mut db,
        Command::SetPreference {
            key: "views.upcoming.span_days".into(),
            value: PrefValue::Uint(14),
            target: PrefTarget::Device,
        },
    )
    .unwrap();
    assert_eq!(upcoming(&e, &db, now, None).days.len(), 14);
    assert_eq!(upcoming(&e, &db, now, Some(7)).days.len(), 7);
    for bad in [0, 8, 31] {
        let err = e
            .query(
                &db,
                Query::Upcoming {
                    now_ms: now,
                    span_days: Some(bad),
                    contexts: vec![],
                },
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "{bad}: {err:?}");
    }
}

/// UTC+14, late in the evening: the storage index puts the last day's
/// all-day plan and the day after's early plan within a day of each other;
/// the prefilter reads both and the span keeps only the first.
#[test]
fn the_last_day_is_decided_on_the_resolved_value_at_utc_plus_14() {
    let zone = "Pacific/Kiritimati";
    let (e, now) = engine_at(zone, date(2026, 3, 4).at(23, 30, 0, 0));
    let mut db = db();
    let last = date(2026, 3, 10);
    task(
        &e,
        &mut db,
        "last day",
        Some(SunriseTime::all_day(last)),
        None,
    );
    task(
        &e,
        &mut db,
        "the day after",
        Some(SunriseTime::floating(date(2026, 3, 11).at(0, 30, 0, 0))),
        None,
    );
    task(
        &e,
        &mut db,
        "yesterday",
        Some(SunriseTime::floating(date(2026, 3, 3).at(23, 30, 0, 0))),
        None,
    );
    let v = upcoming(&e, &db, now, Some(7));
    let all: Vec<String> = v
        .days
        .iter()
        .flat_map(|d| d.planned.iter().map(|t| t.title.clone()))
        .collect();
    assert_eq!(all, ["last day"]);
    assert!(v.no_date.is_empty());
}

/// Undated open tasks are in the no-date lane, the Inbox's as "No stream",
/// after the named Streams.
#[test]
fn undated_tasks_are_in_the_no_date_lane_with_the_inbox_last() {
    let (e, now) = engine_at(NEW_YORK, date(2026, 3, 5).at(10, 0, 0, 0));
    let mut db = db();
    let work = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    task(&e, &mut db, "untriaged", None, None);
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "write the brief".into(),
            stream_id: Some(work),
            ..Default::default()
        }),
    )
    .unwrap();
    let v = upcoming(&e, &db, now, None);
    let lane: Vec<(bool, Vec<String>, u32)> = v
        .no_date
        .iter()
        .map(|g| {
            (
                g.no_stream,
                g.tasks.iter().map(|t| t.title.clone()).collect(),
                g.total,
            )
        })
        .collect();
    assert_eq!(
        lane,
        [
            (false, vec!["write the brief".to_owned()], 1),
            (true, vec!["untriaged".to_owned()], 1),
        ]
    );
    assert_eq!(v.no_date[0].stream, work);
}
