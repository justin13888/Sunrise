//! A task's three times and its lateness in the engine (ADR-0047, #334):
//! Today as ADR-0047 composes it, `Query::Lateness` and the
//! `stale_after_days` preference, a floating deadline read in two zones, a
//! zone change that moves a task's lateness, the soft deadline in Upcoming,
//! and defer moving the plan of each kind and never a deadline.

use super::*;
use crate::queries::{TaskLateness, TodayRow};
use jiff::civil::date;
use sunrise_domain::{Lateness, PrefTarget, PrefValue, TodayGroup, TodayRowDeadline};

/// A clock that can move, in a fixed zone.
#[derive(Debug)]
struct Moving(PLMutex<u64>, &'static str);

impl Clock for Moving {
    fn now_ms(&self) -> u64 {
        *self.0.lock()
    }
    fn timezone(&self) -> String {
        self.1.to_string()
    }
}

/// An engine whose clock reads `local` in `zone`, and that clock.
fn moving_engine_at(zone: &'static str, local: jiff::civil::DateTime) -> (Engine, Arc<Moving>) {
    let tz = jiff::tz::TimeZone::get(zone).unwrap();
    let now = u64::try_from(tz.to_zoned(local).unwrap().timestamp().as_millisecond()).unwrap();
    let clock = Arc::new(Moving(PLMutex::new(now), zone));
    let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes([0x36; 32])));
    crate::engine::rebuild::check::enable();
    let e = Engine::from_clock(clock.clone(), Arc::new(SystemRng), kc);
    (e, clock)
}

fn add(
    e: &Engine,
    db: &mut Db,
    title: &str,
    planned: Option<SunriseTime>,
    target: Option<SunriseTime>,
    hard: Option<SunriseTime>,
) -> EntityRef {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            planned_at: planned,
            target_at: target,
            hard_due_at: hard,
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn today_view(e: &Engine, db: &Db) -> Vec<TodayRow> {
    match e
        .query(
            db,
            Query::TodayView {
                now_ms: e.clock.now_ms(),
                contexts: vec![],
            },
        )
        .unwrap()
    {
        QueryResult::TodayView(rows) => rows,
        other => panic!("expected TodayView, got {other:?}"),
    }
}

fn lateness_of(e: &Engine, db: &Db, id: EntityRef) -> sunrise_domain::LatenessState {
    match e.query(db, Query::Lateness { ids: vec![id] }).unwrap() {
        QueryResult::Lateness(rows) => {
            let [TaskLateness { task, lateness }] = rows.as_slice() else {
                panic!("one row, got {rows:?}");
            };
            assert_eq!(*task, id);
            *lateness
        }
        other => panic!("expected Lateness, got {other:?}"),
    }
}

fn floating(d: jiff::civil::Date, h: i8) -> SunriseTime {
    SunriseTime::floating(d.at(h, 0, 0, 0))
}

/// Today in New York at 15:00 on Tuesday 10 March 2026, every group, each
/// row with its lateness: a missed hard deadline earlier today is late from
/// the moment it passed, a target alone puts a task in Today, a slipped plan
/// is carried over and never late, and a task planned for tomorrow with no
/// deadline today is not in Today.
#[test]
fn today_view_groups_by_the_three_times_and_reads_each_lateness() {
    let d = date(2026, 3, 10);
    let (e, _) = moving_engine_at("America/New_York", d.at(15, 0, 0, 0));
    let mut db = db();
    let yesterday = SunriseTime::all_day(date(2026, 3, 9));
    let tomorrow = SunriseTime::all_day(date(2026, 3, 11));
    add(&e, &mut db, "planned", Some(floating(d, 17)), None, None);
    add(&e, &mut db, "slipped", Some(yesterday.clone()), None, None);
    add(
        &e,
        &mut db,
        "target today",
        None,
        Some(SunriseTime::all_day(d)),
        None,
    );
    add(
        &e,
        &mut db,
        "hard tonight",
        Some(tomorrow.clone()),
        None,
        Some(floating(d, 22)),
    );
    let missed_hard = add(
        &e,
        &mut db,
        "missed at nine",
        None,
        None,
        Some(floating(d, 9)),
    );
    let missed_soft = add(&e, &mut db, "missed target", None, Some(yesterday), None);
    add(&e, &mut db, "tomorrow", Some(tomorrow), None, None);
    let rows = today_view(&e, &db);
    let shape: Vec<(&str, TodayGroup, Option<TodayRowDeadline>, Lateness)> = rows
        .iter()
        .map(|r| (r.task.title.as_str(), r.group, r.deadline, r.lateness.state))
        .collect();
    assert_eq!(
        shape,
        [
            ("planned", TodayGroup::Planned, None, Lateness::OnTrack),
            ("slipped", TodayGroup::CarriedOver, None, Lateness::OnTrack),
            (
                "target today",
                TodayGroup::DueToday,
                Some(TodayRowDeadline::Target),
                Lateness::OnTrack
            ),
            (
                "hard tonight",
                TodayGroup::DueToday,
                Some(TodayRowDeadline::Hard),
                Lateness::OnTrack
            ),
            ("missed at nine", TodayGroup::Late, None, Lateness::LateHard),
            ("missed target", TodayGroup::Late, None, Lateness::LateSoft),
        ]
    );
    let tz = jiff::tz::TimeZone::get("America/New_York").unwrap();
    assert_eq!(
        lateness_of(&e, &db, missed_hard).since,
        Some(tz.to_zoned(d.at(9, 0, 0, 0)).unwrap().timestamp())
    );
    assert_eq!(
        lateness_of(&e, &db, missed_soft).since,
        Some(tz.to_zoned(d.at(0, 0, 0, 0)).unwrap().timestamp()),
        "an all-day target is missed at the end of its day"
    );
    // Query::Today reads the same set, the target-only task included.
    let QueryResult::Tasks(flat) = e
        .query(
            &db,
            Query::Today {
                now_ms: e.clock.now_ms(),
                contexts: vec![],
            },
        )
        .unwrap()
    else {
        panic!("tasks");
    };
    assert_eq!(flat.len(), rows.len());
}

/// `Query::Lateness` reads the `stale_after_days` preference: fourteen
/// planner days by default, the value set, and never with `0`. A task this
/// replica does not hold is refused.
#[test]
fn lateness_reads_the_stale_after_days_preference() {
    let start = date(2026, 6, 1);
    let (e, clock) = moving_engine_at("UTC", start.at(12, 0, 0, 0));
    let mut db = db();
    let id = add(&e, &mut db, "undated", None, None, None);
    let at = |d: jiff::civil::Date| {
        *clock.0.lock() = u64::try_from(
            d.at(0, 0, 0, 0)
                .to_zoned(jiff::tz::TimeZone::UTC)
                .unwrap()
                .timestamp()
                .as_millisecond(),
        )
        .unwrap();
    };
    at(date(2026, 6, 14));
    assert_eq!(lateness_of(&e, &db, id).state, Lateness::OnTrack);
    at(date(2026, 6, 15));
    let stale = lateness_of(&e, &db, id);
    assert_eq!(stale.state, Lateness::Stale);
    assert_eq!(
        stale.since.map(|s| s.as_millisecond()),
        Some(i64::try_from(e.clock.now_ms()).unwrap())
    );
    for (days, want) in [
        (30, Lateness::OnTrack),
        (0, Lateness::OnTrack),
        (7, Lateness::Stale),
    ] {
        e.apply(
            &mut db,
            Command::SetPreference {
                key: "stale_after_days".into(),
                value: PrefValue::Uint(days),
                target: PrefTarget::Vault,
            },
        )
        .unwrap();
        assert_eq!(lateness_of(&e, &db, id).state, want, "{days}");
    }
    let unknown = EntityRef::new(EntityKind::Task, [0x77; 16]);
    assert!(matches!(
        e.query(&db, Query::Lateness { ids: vec![unknown] }),
        Err(EngineError::NotFound(_))
    ));
}

/// One vault, one instant, two readers: a floating 09:00 target is missed
/// for a reader in UTC at 10:00 and still ahead for one in New York at
/// 06:00. Each answer is the reader's, and nothing stored differs.
#[test]
fn a_floating_target_is_late_in_one_zone_and_on_track_in_another() {
    let d = date(2026, 6, 1);
    let (utc, _) = moving_engine_at("UTC", d.at(10, 0, 0, 0));
    let (ny, _) = moving_engine_at("America/New_York", d.at(6, 0, 0, 0));
    assert_eq!(utc.clock.now_ms(), ny.clock.now_ms(), "one instant");
    let mut db = db();
    let id = add(
        &utc,
        &mut db,
        "send report",
        None,
        Some(floating(d, 9)),
        None,
    );
    assert_eq!(lateness_of(&utc, &db, id).state, Lateness::LateSoft);
    assert_eq!(lateness_of(&ny, &db, id).state, Lateness::OnTrack);
    let groups =
        |e: &Engine| -> Vec<TodayGroup> { today_view(e, &db).iter().map(|r| r.group).collect() };
    assert_eq!(groups(&utc), [TodayGroup::Late]);
    assert_eq!(groups(&ny), [TodayGroup::DueToday]);
}

/// A zone change counts a task whose lateness moved, even though no time it
/// holds is floating: an all-day hard deadline missed in Kolkata and not yet
/// in New York.
#[test]
fn a_zone_change_counts_a_task_whose_lateness_moved() {
    let d = date(2026, 6, 1);
    let (e, _) = moving_engine_at("UTC", d.at(20, 0, 0, 0));
    let mut db = db();
    add(
        &e,
        &mut db,
        "all-day",
        None,
        Some(SunriseTime::all_day(d)),
        None,
    );
    add(
        &e,
        &mut db,
        "pinned",
        None,
        None,
        Some(SunriseTime::zoned(
            date(2026, 6, 9).at(9, 0, 0, 0),
            "Europe/Paris",
        )),
    );
    let ny = jiff::tz::TimeZone::get("America/New_York").unwrap();
    let kolkata = jiff::tz::TimeZone::get("Asia/Kolkata").unwrap();
    let (affected, _) = e
        .time_zone_impact(&db, &ny, &kolkata, e.clock.now_ms())
        .unwrap();
    assert_eq!(
        affected, 1,
        "the all-day target, and not the pinned deadline"
    );

    // Each of the three times counts alone: a far-off floating hard
    // deadline moves without its lateness changing, and so does a plan.
    for (title, planned, hard) in [
        ("far hard", None, Some(floating(date(2026, 9, 1), 9))),
        ("far plan", Some(floating(date(2026, 9, 1), 9)), None),
    ] {
        let mut db = super::support::db();
        add(&e, &mut db, title, planned, None, hard);
        let (affected, _) = e
            .time_zone_impact(&db, &ny, &kolkata, e.clock.now_ms())
            .unwrap();
        assert_eq!(affected, 1, "{title} alone");
    }
}

/// Staleness counts planner days in the reader's zone, so a zone change can
/// move it with no time stored on the task at all: touched at 22:00 UTC and
/// read three hours later, an undated task is a day old in UTC and none in
/// New York.
#[test]
fn a_zone_change_counts_a_task_whose_staleness_moved() {
    let (e, clock) = moving_engine_at("UTC", date(2026, 6, 1).at(22, 0, 0, 0));
    let mut db = db();
    e.apply(
        &mut db,
        Command::SetPreference {
            key: "stale_after_days".into(),
            value: PrefValue::Uint(1),
            target: PrefTarget::Vault,
        },
    )
    .unwrap();
    add(&e, &mut db, "undated", None, None, None);
    *clock.0.lock() += 3 * 3_600_000;
    let utc = jiff::tz::TimeZone::UTC;
    let ny = jiff::tz::TimeZone::get("America/New_York").unwrap();
    let (affected, _) = e
        .time_zone_impact(&db, &utc, &ny, e.clock.now_ms())
        .unwrap();
    assert_eq!(affected, 1);
    let (unmoved, _) = e
        .time_zone_impact(&db, &utc, &utc, e.clock.now_ms())
        .unwrap();
    assert_eq!(unmoved, 0);
}

/// Upcoming lists a soft deadline on its day, apart from the hard ones.
#[test]
fn upcoming_lists_a_target_on_its_day() {
    let d = date(2026, 3, 9);
    let (e, _) = moving_engine_at("UTC", d.at(8, 0, 0, 0));
    let mut db = db();
    let friday = date(2026, 3, 13);
    add(
        &e,
        &mut db,
        "aim for friday",
        None,
        Some(SunriseTime::all_day(friday)),
        None,
    );
    let QueryResult::Upcoming(view) = e
        .query(
            &db,
            Query::Upcoming {
                now_ms: e.clock.now_ms(),
                span_days: Some(7),
                contexts: vec![],
            },
        )
        .unwrap()
    else {
        panic!("upcoming");
    };
    let day = view.days.iter().find(|x| x.date == friday).unwrap();
    assert_eq!(
        day.targets
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        ["aim for friday"]
    );
    assert!(day.due.is_empty() && day.planned.is_empty());
}

/// Defer moves the plan, of every kind, and never a deadline: a zoned plan
/// stays zoned in its zone, and the target and the hard deadline are what
/// they were (ADR-0047 §1).
#[test]
fn defer_moves_a_zoned_plan_and_never_a_deadline() {
    let d = date(2026, 3, 6);
    let (e, _) = moving_engine_at("America/New_York", d.at(8, 0, 0, 0));
    let mut db = db();
    let plan = SunriseTime::zoned(d.at(9, 0, 0, 0), "Europe/Paris");
    let target = SunriseTime::all_day(date(2026, 3, 10));
    let hard = SunriseTime::floating(date(2026, 3, 12).at(17, 0, 0, 0));
    let id = add(
        &e,
        &mut db,
        "call Paris",
        Some(plan.clone()),
        Some(target.clone()),
        Some(hard.clone()),
    );
    let to = sunrise_domain::shift_civil_days(&plan, 2, &e.device_zone()).unwrap();
    let r = e
        .apply(&mut db, Command::DeferTaskTo { id, to: to.clone() })
        .unwrap();
    assert!(r.ordering_warnings.is_empty());
    let QueryResult::Task(t) = e.query(&db, Query::EntityById(id)).unwrap() else {
        panic!("task");
    };
    assert_eq!(
        t.planned_at,
        Some(SunriseTime::zoned(
            date(2026, 3, 8).at(9, 0, 0, 0),
            "Europe/Paris"
        ))
    );
    assert_eq!(t.target_at, Some(target));
    assert_eq!(t.hard_due_at, Some(hard));
    assert_eq!(t.deferred_count, 1);
}

// ---- derived at read time, in the order asked ----

/// `Query::Lateness` of several tasks answers each, in the order asked,
/// whatever that order is.
#[test]
fn lateness_of_several_tasks_answers_in_the_order_asked() {
    let day = date(2026, 6, 1);
    let (e, _) = moving_engine_at("UTC", day.at(12, 0, 0, 0));
    let mut db = db();
    let soft = add(&e, &mut db, "a", None, Some(floating(day, 9)), None);
    let quiet = add(&e, &mut db, "b", None, None, None);
    let hard = add(&e, &mut db, "c", None, None, Some(floating(day, 10)));
    for asked in [
        vec![hard, soft, quiet],
        vec![quiet, hard, soft],
        vec![soft, quiet, hard],
    ] {
        let QueryResult::Lateness(rows) = e
            .query(&db, Query::Lateness { ids: asked.clone() })
            .unwrap()
        else {
            panic!("expected Lateness");
        };
        let got: Vec<EntityRef> = rows.iter().map(|r| r.task).collect();
        assert_eq!(got, asked);
        let state = |id| rows.iter().find(|r| r.task == id).unwrap().lateness.state;
        assert_eq!(
            (state(soft), state(quiet), state(hard)),
            (Lateness::LateSoft, Lateness::OnTrack, Lateness::LateHard)
        );
    }
}

/// Zones the lateness property reads in: one with no DST, two that move
/// their clocks (one of them by half an hour), and one far east.
const ZONES: [&str; 4] = [
    "UTC",
    "America/New_York",
    "Australia/Lord_Howe",
    "Asia/Tokyo",
];

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        cases: 48,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/lateness.txt",
            ),
        )),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// Lateness is derived purely at read time: as now moves forward with
    /// no edit in between, every task's state only moves forward (on track,
    /// then stale or late soft, then late hard), reading twice at one
    /// instant agrees, the rows come back in the order asked, and no read
    /// writes an op.
    #[test]
    fn lateness_only_moves_forward_and_no_read_writes(
        zone in proptest::sample::select(ZONES.to_vec()),
        stale_after in 0u64..21,
        steps in proptest::collection::vec(0i64..(3 * 24), 1..12),
        order in proptest::strategy::Strategy::prop_shuffle(
            proptest::strategy::Just(vec![0usize, 1, 2, 3, 4]),
        ),
    ) {
        let start = date(2026, 3, 1);
        let (e, clock) = moving_engine_at(zone, start.at(12, 0, 0, 0));
        let mut db = db();
        e.apply(
            &mut db,
            Command::SetPreference {
                key: "stale_after_days".into(),
                value: PrefValue::Uint(stale_after),
                target: PrefTarget::Vault,
            },
        )
        .unwrap();
        let day = |n: i8| start.checked_add(jiff::Span::new().days(n)).unwrap();
        let tasks = [
            add(&e, &mut db, "undated", None, None, None),
            add(&e, &mut db, "target", None, Some(floating(day(3), 9)), None),
            add(&e, &mut db, "hard", None, None, Some(SunriseTime::all_day(day(5)))),
            add(
                &e,
                &mut db,
                "both",
                None,
                Some(SunriseTime::zoned(day(2).at(18, 0, 0, 0), "Europe/Paris")),
                Some(floating(day(6), 8)),
            ),
            add(&e, &mut db, "planned", Some(floating(day(1), 9)), None, None),
        ];
        let asked: Vec<EntityRef> = order.iter().map(|i| tasks[*i]).collect();
        let ops = op_count(&db);
        let read = |e: &Engine, db: &Db| match e
            .query(db, Query::Lateness { ids: asked.clone() })
            .unwrap()
        {
            QueryResult::Lateness(rows) => rows,
            other => panic!("expected Lateness, got {other:?}"),
        };
        let mut last: Option<Vec<TaskLateness>> = None;
        for hours in steps {
            *clock.0.lock() += u64::try_from(hours).unwrap() * 3_600_000;
            let rows = read(&e, &db);
            proptest::prop_assert_eq!(&rows, &read(&e, &db), "a second read agrees");
            let got: Vec<EntityRef> = rows.iter().map(|r| r.task).collect();
            proptest::prop_assert_eq!(&got, &asked, "in the order asked");
            let now = i64::try_from(*clock.0.lock()).unwrap();
            for r in &rows {
                let since = r.lateness.since.map(|s| s.as_millisecond());
                proptest::prop_assert_eq!(
                    since.is_some(),
                    r.lateness.state != Lateness::OnTrack,
                    "{:?}", r
                );
                proptest::prop_assert!(since.is_none_or(|s| s <= now), "{:?}", r);
            }
            if let Some(prev) = &last {
                for (was, is) in prev.iter().zip(&rows) {
                    proptest::prop_assert!(
                        is.lateness.state <= was.lateness.state,
                        "{:?} moved back from {:?} to {:?}",
                        is.task, was.lateness.state, is.lateness.state
                    );
                }
            }
            last = Some(rows);
        }
        proptest::prop_assert_eq!(op_count(&db), ops, "no read wrote an op");
    }
}
