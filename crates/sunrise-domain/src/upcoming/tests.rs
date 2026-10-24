//! The Upcoming fold: civil days across DST and zone changes, kinds kept, and
//! a deterministic no-date lane.

use super::*;
use crate::inbox::inbox_stream_ref;
use crate::unknown::Unknowns;
use jiff::civil::date;
use std::collections::BTreeSet;
use sunrise_id::EntityKind;

const NEW_YORK: &str = "America/New_York";
const TOKYO: &str = "Asia/Tokyo";
const HOUR_MS: i64 = 3_600_000;

fn tz(name: &str) -> TimeZone {
    TimeZone::get(name).unwrap()
}

fn stream(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Stream, [n; 16])
}

fn task(n: u8, title: &str) -> Task {
    let at = Timestamp::UNIX_EPOCH;
    Task {
        id: EntityRef::new(EntityKind::Task, [n; 16]),
        created_at: at,
        updated_at: at,
        title: title.into(),
        body: None,
        stream_id: stream(1),
        contexts: BTreeSet::new(),
        state: TaskState::Todo,
        priority: None,
        energy: None,
        estimated_duration_s: None,
        scheduled_at: None,
        due_at: None,
        scheduling_constraints: Vec::new(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: None,
        routine_occurrence: None,
        reminder_lead_s: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn planned(n: u8, title: &str, at: SunriseTime) -> Task {
    Task {
        scheduled_at: Some(at),
        ..task(n, title)
    }
}

fn due(n: u8, title: &str, at: SunriseTime) -> Task {
    Task {
        due_at: Some(at),
        ..task(n, title)
    }
}

fn block(n: u8, starts_at: SunriseTime, ends_at: SunriseTime) -> Block {
    let at = Timestamp::UNIX_EPOCH;
    Block {
        id: EntityRef::new(EntityKind::Block, [n; 16]),
        created_at: at,
        updated_at: at,
        stream_id: stream(1),
        starts_at,
        ends_at,
        title: Some(format!("block {n}")),
        title_track_task: false,
        tasks: BTreeSet::new(),
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn same(b: &Block) -> &Block {
    b
}

fn view(
    tasks: &[Task],
    blocks: &[Block],
    today: Date,
    span_days: u32,
    zone: &TimeZone,
) -> Upcoming<Block> {
    build_upcoming(UpcomingInput {
        tasks,
        blocks,
        block_of: same,
        stream_order: &[stream(1), stream(2), stream(3)],
        no_stream: inbox_stream_ref(),
        contexts: &[],
        today,
        span_days,
        tz: zone,
    })
    .unwrap()
}

/// The titles planned and due on `d`, in the view's order.
fn on(v: &Upcoming<Block>, d: Date) -> (Vec<String>, Vec<String>) {
    let day = v
        .days
        .iter()
        .find(|x| x.date == d)
        .unwrap_or_else(|| panic!("{d} is not in the view"));
    let titles = |ts: &[Task]| ts.iter().map(|t| t.title.clone()).collect::<Vec<_>>();
    (titles(&day.planned), titles(&day.due))
}

fn day_hours(v: &Upcoming<Block>, d: Date) -> i64 {
    let day = v.days.iter().find(|x| x.date == d).unwrap();
    (day.end.as_millisecond() - day.start.as_millisecond()) / HOUR_MS
}

// ---- span ----

#[test]
fn each_span_is_that_many_contiguous_civil_days_from_today() {
    let ny = tz(NEW_YORK);
    let today = date(2026, 3, 5);
    for span in UPCOMING_SPANS {
        let v = view(&[], &[], today, span, &ny);
        assert_eq!(v.days.len(), span as usize);
        assert_eq!(v.span_days, span);
        assert_eq!(v.days[0].date, today);
        for pair in v.days.windows(2) {
            assert_eq!(pair[0].date.tomorrow().unwrap(), pair[1].date);
            assert_eq!(pair[0].end, pair[1].start, "the days tile the span");
        }
    }
}

#[test]
fn a_span_the_view_does_not_offer_is_refused() {
    let ny = tz(NEW_YORK);
    for span in [0, 1, 6, 8, 15, 29, 31, u32::MAX] {
        assert_eq!(
            upcoming_days(date(2026, 3, 5), span, &ny).unwrap_err(),
            UpcomingError::Span(span)
        );
    }
}

#[test]
fn the_span_holds_today_to_its_last_day_and_nothing_either_side() {
    let ny = tz(NEW_YORK);
    let today = date(2026, 3, 5);
    let at = |d: Date| SunriseTime::floating(d.at(12, 0, 0, 0));
    let tasks = [
        planned(1, "yesterday", at(date(2026, 3, 4))),
        planned(2, "today", at(today)),
        planned(3, "last day", at(date(2026, 3, 11))),
        planned(4, "day after", at(date(2026, 3, 12))),
    ];
    let v = view(&tasks, &[], today, 7, &ny);
    let all: Vec<String> = v
        .days
        .iter()
        .flat_map(|d| d.planned.iter().map(|t| t.title.clone()))
        .collect();
    assert_eq!(all, ["today", "last day"]);
    assert!(
        v.no_date.is_empty(),
        "a dated task is not in the no-date lane"
    );
}

// ---- DST ----

/// New York springs forward on 2026-03-08 and falls back on 2026-11-01. Those
/// days are 23 and 25 hours, and what is planned on them stays on them.
#[test]
fn a_dst_day_is_23_or_25_hours_and_keeps_its_tasks() {
    let ny = tz(NEW_YORK);
    let spring = date(2026, 3, 8);
    let tasks = [
        // 02:30 does not exist on the spring day; it resolves into the same day.
        planned(
            1,
            "in the gap",
            SunriseTime::floating(spring.at(2, 30, 0, 0)),
        ),
        planned(2, "late", SunriseTime::floating(spring.at(23, 30, 0, 0))),
        planned(
            3,
            "next morning",
            SunriseTime::floating(date(2026, 3, 9).at(0, 30, 0, 0)),
        ),
    ];
    let v = view(&tasks, &[], date(2026, 3, 5), 7, &ny);
    assert_eq!(day_hours(&v, date(2026, 3, 7)), 24);
    assert_eq!(day_hours(&v, spring), 23);
    assert_eq!(on(&v, spring).0, ["in the gap", "late"]);
    assert_eq!(on(&v, date(2026, 3, 9)).0, ["next morning"]);

    let fall = date(2026, 11, 1);
    let tasks = [
        planned(
            4,
            "second 01:30",
            SunriseTime::floating(fall.at(1, 30, 0, 0)),
        ),
        planned(
            5,
            "before midnight",
            SunriseTime::floating(fall.at(23, 59, 0, 0)),
        ),
    ];
    let v = view(&tasks, &[], date(2026, 10, 28), 7, &ny);
    assert_eq!(day_hours(&v, fall), 25);
    assert_eq!(on(&v, fall).0, ["second 01:30", "before midnight"]);
}

// ---- zones ----

/// Moving from New York to Tokyo mid-span: the floating plan stays on its
/// wall-clock day, the zoned and instant ones fall on whichever day their
/// instant is in Tokyo, and the all-day deadline stays on its date.
#[test]
fn a_zone_change_mid_span_moves_only_what_is_pinned() {
    let d = date(2026, 3, 10);
    let next = date(2026, 3, 11);
    let pinned = SunriseTime::zoned(d.at(21, 0, 0, 0), NEW_YORK);
    let instant = SunriseTime::instant(
        tz(NEW_YORK)
            .to_zoned(d.at(20, 0, 0, 0))
            .unwrap()
            .timestamp(),
    );
    let tasks = [
        planned(1, "floating", SunriseTime::floating(d.at(21, 0, 0, 0))),
        planned(2, "zoned", pinned),
        planned(3, "instant", instant),
        due(4, "all-day", SunriseTime::all_day(d)),
    ];
    let in_ny = view(&tasks, &[], date(2026, 3, 9), 7, &tz(NEW_YORK));
    assert_eq!(in_ny.days[0].date, date(2026, 3, 9));
    // The zoned and floating 21:00 are one instant here; `cmp_in`'s storage
    // tiebreak puts the floating one first.
    assert_eq!(on(&in_ny, d).0, ["instant", "floating", "zoned"]);
    assert_eq!(on(&in_ny, d).1, ["all-day"]);

    // In Tokyo it is already the 10th when New York is on the 9th.
    let in_tokyo = view(&tasks, &[], date(2026, 3, 10), 7, &tz(TOKYO));
    assert_eq!(on(&in_tokyo, d).0, ["floating"]);
    assert_eq!(on(&in_tokyo, d).1, ["all-day"]);
    assert_eq!(on(&in_tokyo, next).0, ["instant", "zoned"]);
    // Nothing was rewritten: each time keeps its kind.
    let kinds: Vec<&str> = tasks
        .iter()
        .filter_map(|t| t.scheduled_at.as_ref().map(SunriseTime::kind_str))
        .collect();
    assert_eq!(kinds, ["floating", "zoned", "instant"]);
}

/// A floating 09:00 and a Berlin 09:00 on the same date, read in New York:
/// both are on that day, the Berlin one first because it is 03:00 here.
#[test]
fn a_floating_and_a_zoned_task_on_the_same_civil_day() {
    let d = date(2026, 6, 2);
    let tasks = [
        planned(1, "floating 09:00", SunriseTime::floating(d.at(9, 0, 0, 0))),
        planned(
            2,
            "berlin 09:00",
            SunriseTime::zoned(d.at(9, 0, 0, 0), "Europe/Berlin"),
        ),
        planned(3, "all day", SunriseTime::all_day(d)),
    ];
    let v = view(&tasks, &[], date(2026, 6, 1), 7, &tz(NEW_YORK));
    assert_eq!(on(&v, d).0, ["berlin 09:00", "floating 09:00", "all day"]);
    // In Berlin the two are one instant, and `cmp_in`'s storage tiebreak
    // decides; either way the order is the same on every run.
    let v = view(&tasks, &[], date(2026, 6, 1), 7, &tz("Europe/Berlin"));
    assert_eq!(on(&v, d).0, ["berlin 09:00", "floating 09:00", "all day"]);
}

// ---- membership ----

#[test]
fn a_deadline_is_listed_on_its_day_unless_the_task_is_planned_for_it() {
    let ny = tz(NEW_YORK);
    let d = date(2026, 3, 10);
    let tasks = [
        Task {
            due_at: Some(SunriseTime::all_day(d)),
            ..planned(
                1,
                "planned and due",
                SunriseTime::floating(d.at(9, 0, 0, 0)),
            )
        },
        Task {
            due_at: Some(SunriseTime::all_day(d)),
            ..planned(
                2,
                "planned earlier",
                SunriseTime::floating(date(2026, 3, 9).at(9, 0, 0, 0)),
            )
        },
        Task {
            priority: Some(1),
            ..due(3, "due, urgent", SunriseTime::all_day(d))
        },
        due(4, "due", SunriseTime::all_day(d)),
    ];
    let v = view(&tasks, &[], date(2026, 3, 9), 7, &ny);
    assert_eq!(
        on(&v, d),
        (
            vec!["planned and due".into()],
            vec!["due, urgent".into(), "planned earlier".into(), "due".into(),]
        )
    );
    assert_eq!(on(&v, date(2026, 3, 9)).0, ["planned earlier"]);
    // A task with a deadline and no plan is also in the no-date lane.
    let lane: Vec<&str> = v.no_date[0]
        .tasks
        .iter()
        .map(|t| t.title.as_str())
        .collect();
    assert_eq!(lane, ["due, urgent", "due"]);
}

#[test]
fn closed_archived_and_deleted_tasks_are_not_upcoming_and_contexts_filter() {
    let ny = tz(NEW_YORK);
    let d = date(2026, 3, 10);
    let at = SunriseTime::floating(d.at(9, 0, 0, 0));
    let errands = EntityRef::new(EntityKind::Context, [7; 16]);
    let tasks = [
        Task {
            state: TaskState::Done,
            ..planned(1, "done", at.clone())
        },
        Task {
            state: TaskState::Cancelled,
            ..planned(2, "cancelled", at.clone())
        },
        Task {
            archived: true,
            ..planned(3, "archived", at.clone())
        },
        Task {
            deleted: true,
            ..planned(4, "deleted", at.clone())
        },
        Task {
            state: TaskState::InProgress,
            contexts: [errands].into_iter().collect(),
            ..planned(5, "errand", at.clone())
        },
        planned(6, "other", at),
    ];
    let v = view(&tasks, &[], date(2026, 3, 9), 7, &ny);
    assert_eq!(on(&v, d).0, ["errand", "other"]);
    let filtered = build_upcoming(UpcomingInput {
        tasks: &tasks,
        blocks: &[],
        block_of: same,
        stream_order: &[],
        no_stream: inbox_stream_ref(),
        contexts: &[errands],
        today: date(2026, 3, 9),
        span_days: 7,
        tz: &ny,
    })
    .unwrap();
    assert_eq!(on(&filtered, d).0, ["errand"]);
}

/// A block that runs past midnight is on both days, as on the grid; a
/// deleted one is on neither.
#[test]
fn blocks_are_on_every_day_they_overlap_in_start_order() {
    let ny = tz(NEW_YORK);
    let d = date(2026, 3, 10);
    let next = date(2026, 3, 11);
    let blocks = [
        block(
            1,
            SunriseTime::floating(d.at(23, 0, 0, 0)),
            SunriseTime::floating(next.at(1, 0, 0, 0)),
        ),
        block(
            2,
            SunriseTime::floating(d.at(9, 0, 0, 0)),
            SunriseTime::floating(d.at(10, 0, 0, 0)),
        ),
        Block {
            deleted: true,
            ..block(
                3,
                SunriseTime::floating(d.at(12, 0, 0, 0)),
                SunriseTime::floating(d.at(13, 0, 0, 0)),
            )
        },
        // Ends exactly at the start of the 11th: not on it.
        block(
            4,
            SunriseTime::floating(d.at(22, 0, 0, 0)),
            SunriseTime::floating(next.at(0, 0, 0, 0)),
        ),
    ];
    let v = view(&[], &blocks, date(2026, 3, 9), 7, &ny);
    let ids = |date: Date| -> Vec<u8> {
        v.days
            .iter()
            .find(|x| x.date == date)
            .unwrap()
            .blocks
            .iter()
            .map(|b| b.id.bytes()[0])
            .collect()
    };
    assert_eq!(ids(d), [2, 4, 1]);
    assert_eq!(ids(next), [1]);
}

// ---- the no-date lane ----

#[test]
fn the_no_date_lane_is_grouped_capped_and_ordered_the_same_whatever_the_input_order() {
    let ny = tz(NEW_YORK);
    let in_stream = |n: u8, s: EntityRef, p: Option<u8>, due_on: Option<Date>| Task {
        stream_id: s,
        priority: p,
        due_at: due_on.map(SunriseTime::all_day),
        ..task(n, &format!("t{n}"))
    };
    let mut tasks = vec![
        in_stream(10, inbox_stream_ref(), None, None),
        in_stream(11, stream(3), None, None),
        in_stream(20, stream(2), Some(3), None),
        in_stream(21, stream(2), Some(1), None),
        in_stream(22, stream(2), None, Some(date(2026, 4, 1))),
        in_stream(23, stream(2), None, None),
        in_stream(24, stream(2), Some(1), None),
        in_stream(25, stream(2), None, Some(date(2026, 3, 20))),
        in_stream(26, stream(2), None, None),
        // A Stream the stream list does not name sorts after those it does.
        in_stream(30, stream(9), None, None),
    ];
    let lane = |tasks: &[Task]| -> Vec<(EntityRef, bool, Vec<String>, u32)> {
        view(tasks, &[], date(2026, 3, 9), 7, &ny)
            .no_date
            .into_iter()
            .map(|g| {
                let titles = g.tasks.iter().map(|t| t.title.clone()).collect();
                (g.stream, g.no_stream, titles, g.total)
            })
            .collect()
    };
    let expected = vec![
        (
            stream(2),
            false,
            vec![
                "t25".into(),
                "t22".into(),
                "t21".into(),
                "t24".into(),
                "t20".into(),
            ],
            7,
        ),
        (stream(3), false, vec!["t11".into()], 1),
        (stream(9), false, vec!["t30".into()], 1),
        (inbox_stream_ref(), true, vec!["t10".into()], 1),
    ];
    assert_eq!(lane(&tasks), expected);
    tasks.reverse();
    assert_eq!(lane(&tasks), expected);
    tasks.rotate_left(3);
    assert_eq!(lane(&tasks), expected);
}
