//! Table tests for [`lateness`] (ADR-0047 §2, `docs/10-cross-cutting/time.md`
//! §8): every state, the midnight boundary, a DST day each way, a floating
//! deadline read in two zones, the date line, a sleep past midnight, and the
//! tasks lateness is never evaluated for.

use super::*;
use crate::day_schedule::DayWindow;
use crate::task::TaskState;
use crate::time::SunriseTime;
use crate::unknown::Unknowns;
use jiff::civil::{date, time, DateTime};
use std::collections::BTreeSet;
use sunrise_id::{EntityKind, EntityRef};

fn tz(name: &str) -> TimeZone {
    TimeZone::get(name).expect(name)
}

fn ts(s: &str) -> Timestamp {
    s.parse().expect(s)
}

/// `civil` in `zone`, as an instant.
fn at(zone: &str, civil: DateTime) -> Timestamp {
    tz(zone).to_zoned(civil).expect("civil").timestamp()
}

/// An open task touched at `touched`, with the given deadlines.
fn task(hard: Option<SunriseTime>, target: Option<SunriseTime>, touched: Timestamp) -> Task {
    Task {
        id: EntityRef::new(EntityKind::Task, [3u8; 16]),
        created_at: touched,
        updated_at: touched,
        title: "t".into(),
        body: None,
        stream_id: None,
        contexts: BTreeSet::new(),
        state: TaskState::Todo,
        priority: None,
        energy: None,
        estimated_duration_s: None,
        planned_at: None,
        target_at: target,
        hard_due_at: hard,
        scheduling_constraints: Vec::new(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: None,
        routine_occurrence: None,
        reminder_lead_s: None,
        sort_order: None,
        triaged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn late(state: Lateness, since: Timestamp) -> LatenessState {
    LatenessState {
        state,
        since: Some(since),
    }
}

const ON: LatenessState = LatenessState::ON_TRACK;

/// One row: a task, a reader, a moment, and the answer.
struct Row {
    name: &'static str,
    task: Task,
    zone: &'static str,
    schedule: DaySchedule,
    now: Timestamp,
    stale_after_days: u16,
    want: LatenessState,
}

fn row(
    name: &'static str,
    task: Task,
    zone: &'static str,
    now: Timestamp,
    want: LatenessState,
) -> Row {
    Row {
        name,
        task,
        zone,
        schedule: DaySchedule::default(),
        now,
        stale_after_days: DEFAULT_STALE_AFTER_DAYS,
        want,
    }
}

fn check(rows: Vec<Row>) {
    for r in rows {
        let got = lateness(&r.task, r.now, &tz(r.zone), &r.schedule, r.stale_after_days);
        assert_eq!(got, r.want, "{}", r.name);
    }
}

#[test]
fn every_state_and_the_first_match() {
    let touched = ts("2026-06-01T00:00:00Z");
    let due: SunriseTime = ts("2026-06-05T17:00:00Z").into();
    let target: SunriseTime = ts("2026-06-03T17:00:00Z").into();
    check(vec![
        row(
            "a fresh task with no deadline is on track",
            task(None, None, touched),
            "UTC",
            ts("2026-06-02T00:00:00Z"),
            ON,
        ),
        row(
            "before both deadlines",
            task(Some(due.clone()), Some(target.clone()), touched),
            "UTC",
            ts("2026-06-03T16:59:59.999Z"),
            ON,
        ),
        row(
            "at the soft deadline, to the millisecond",
            task(Some(due.clone()), Some(target.clone()), touched),
            "UTC",
            ts("2026-06-03T17:00:00Z"),
            late(Lateness::LateSoft, ts("2026-06-03T17:00:00Z")),
        ),
        row(
            "past both: the hard deadline wins",
            task(Some(due.clone()), Some(target.clone()), touched),
            "UTC",
            ts("2026-06-05T17:00:00Z"),
            late(Lateness::LateHard, ts("2026-06-05T17:00:00Z")),
        ),
        row(
            "a hard deadline alone",
            task(Some(due.clone()), None, touched),
            "UTC",
            ts("2026-06-06T00:00:00Z"),
            late(Lateness::LateHard, ts("2026-06-05T17:00:00Z")),
        ),
        row(
            "a target alone, before it",
            task(None, Some(target.clone()), touched),
            "UTC",
            ts("2026-06-02T00:00:00Z"),
            ON,
        ),
        row(
            "untouched for fourteen planner days, no deadline",
            task(None, None, touched),
            "UTC",
            ts("2026-06-15T00:00:00Z"),
            late(Lateness::Stale, ts("2026-06-15T00:00:00Z")),
        ),
        row(
            "thirteen days is not yet stale",
            task(None, None, touched),
            "UTC",
            ts("2026-06-14T23:59:59Z"),
            ON,
        ),
        row(
            "a deadline in the future keeps an untouched task off the stale clock",
            task(Some(ts("2026-12-01T00:00:00Z").into()), None, touched),
            "UTC",
            ts("2026-07-01T00:00:00Z"),
            ON,
        ),
    ]);
}

/// The ADR's `stale_after_days = 0` disables staleness, and a short setting
/// turns a task stale sooner.
#[test]
fn stale_after_days_is_read_as_given() {
    let touched = ts("2026-06-01T12:00:00Z");
    let t = task(None, None, touched);
    let now = ts("2027-06-01T12:00:00Z");
    let zone = tz("UTC");
    let none = DaySchedule::default();
    assert_eq!(lateness(&t, now, &zone, &none, 0), ON, "0 disables");
    assert_eq!(
        lateness(&t, ts("2026-06-02T00:00:00Z"), &zone, &none, 1),
        late(Lateness::Stale, ts("2026-06-02T00:00:00Z")),
        "one planner day after the touch, from its start"
    );
    assert_eq!(
        lateness(&t, ts("2026-06-01T23:59:59Z"), &zone, &none, 1),
        ON
    );
}

/// An all-day deadline is missed at the END of its day: on time through
/// 23:59:59.999 on the day it names, late from the next midnight, in the
/// reader's zone.
#[test]
fn the_midnight_boundary() {
    let monday = date(2026, 3, 2);
    let t = task(
        Some(SunriseTime::all_day(monday)),
        None,
        ts("2026-03-01T00:00:00Z"),
    );
    let ny = "America/New_York";
    let midnight = at(ny, date(2026, 3, 3).at(0, 0, 0, 0));
    check(vec![
        row(
            "the last moment of Monday in New York",
            t.clone(),
            ny,
            midnight - jiff::SignedDuration::from_millis(1),
            ON,
        ),
        row(
            "Tuesday 00:00 in New York",
            t.clone(),
            ny,
            midnight,
            late(Lateness::LateHard, midnight),
        ),
        row(
            "the same instant is still Monday in Honolulu",
            t,
            "Pacific/Honolulu",
            midnight,
            ON,
        ),
    ]);
}

/// A DST day is one planner day: the spring-forward day is 23 hours long and
/// the fall-back day 25, and an all-day deadline on either is missed at the
/// next civil midnight, never 24 hours after the last one. The stale clock
/// counts civil days across both.
#[test]
fn a_dst_day_each_way() {
    let ny = "America/New_York";
    let spring = date(2026, 3, 8);
    let fall = date(2026, 11, 1);
    let spring_end = at(ny, date(2026, 3, 9).at(0, 0, 0, 0));
    let fall_end = at(ny, date(2026, 11, 2).at(0, 0, 0, 0));
    assert_eq!(spring_end, ts("2026-03-09T04:00:00Z"), "EDT after the gap");
    assert_eq!(fall_end, ts("2026-11-02T05:00:00Z"), "EST after the fold");
    let t_spring = task(
        Some(SunriseTime::all_day(spring)),
        None,
        ts("2026-03-01T12:00:00Z"),
    );
    let t_fall = task(
        None,
        Some(SunriseTime::all_day(fall)),
        ts("2026-10-25T12:00:00Z"),
    );
    // 24 hours after the spring day's start is already the next day; 24
    // hours after the fall day's start is still the fall day.
    let spring_start = at(ny, spring.at(0, 0, 0, 0));
    let fall_start = at(ny, fall.at(0, 0, 0, 0));
    check(vec![
        row(
            "23:59 on the spring-forward day",
            t_spring.clone(),
            ny,
            at(ny, spring.at(23, 59, 0, 0)),
            ON,
        ),
        row(
            "24 hours after the spring day began is the next day",
            t_spring.clone(),
            ny,
            spring_start + jiff::SignedDuration::from_hours(24),
            late(Lateness::LateHard, spring_end),
        ),
        row(
            "24 hours after the fall day began is still the fall day",
            t_fall.clone(),
            ny,
            fall_start + jiff::SignedDuration::from_hours(24),
            ON,
        ),
        row(
            "the next midnight after the fold",
            t_fall,
            ny,
            fall_end,
            late(Lateness::LateSoft, fall_end),
        ),
    ]);
    // Touched at noon on 1 March, stale after seven planner days: from the
    // start of 8 March, the spring-forward day, which begins at 00:00 EST.
    let undated = task(None, None, ts("2026-03-01T17:00:00Z"));
    let zone = tz(ny);
    let none = DaySchedule::default();
    assert_eq!(
        lateness(
            &undated,
            spring_start - jiff::SignedDuration::from_millis(1),
            &zone,
            &none,
            7
        ),
        ON
    );
    assert_eq!(
        lateness(&undated, spring_start, &zone, &none, 7),
        late(Lateness::Stale, ts("2026-03-08T05:00:00Z"))
    );
}

/// A floating deadline carries no zone: it is missed at its wall clock
/// wherever the reader is, so two readers at one instant disagree, and both
/// are right.
#[test]
fn a_floating_deadline_read_in_two_zones() {
    let target = SunriseTime::floating(date(2026, 6, 1).at(9, 0, 0, 0));
    let t = task(None, Some(target), ts("2026-05-30T00:00:00Z"));
    let now = ts("2026-06-01T10:00:00Z");
    check(vec![
        row(
            "10:00 in UTC is past 09:00",
            t.clone(),
            "UTC",
            now,
            late(Lateness::LateSoft, ts("2026-06-01T09:00:00Z")),
        ),
        row(
            "06:00 in New York is before it",
            t.clone(),
            "America/New_York",
            now,
            ON,
        ),
        row(
            "15:30 in Kolkata is past it, since 09:00 there",
            t,
            "Asia/Kolkata",
            now,
            late(Lateness::LateSoft, ts("2026-06-01T03:30:00Z")),
        ),
    ]);
}

/// A zoned deadline is one instant for every reader.
#[test]
fn a_zoned_deadline_is_the_same_for_every_reader() {
    let due = SunriseTime::zoned(date(2026, 6, 1).at(17, 0, 0, 0), "Europe/Paris");
    let t = task(Some(due), None, ts("2026-05-30T00:00:00Z"));
    let now = ts("2026-06-01T15:00:00Z");
    for zone in ["UTC", "America/New_York", "Asia/Tokyo"] {
        assert_eq!(
            lateness(&t, now, &tz(zone), &DaySchedule::default(), 14),
            late(Lateness::LateHard, ts("2026-06-01T15:00:00Z")),
            "{zone}"
        );
    }
}

/// `Pacific/Apia` and `Pacific/Pago_Pago` share a longitude and are a day
/// apart: at one instant an all-day deadline is late in one and on track in
/// the other (`time.md` §8).
#[test]
fn the_date_line() {
    let t = task(
        Some(SunriseTime::all_day(date(2026, 6, 1))),
        None,
        ts("2026-05-30T00:00:00Z"),
    );
    let now = ts("2026-06-01T12:00:00Z");
    check(vec![
        row(
            "01:00 on the 2nd in Apia",
            t.clone(),
            "Pacific/Apia",
            now,
            late(Lateness::LateHard, ts("2026-06-01T11:00:00Z")),
        ),
        row(
            "01:00 on the 1st in Pago Pago",
            t,
            "Pacific/Pago_Pago",
            now,
            ON,
        ),
    ]);
}

/// With a sleep time past midnight, Friday's planner day runs until the
/// later of its 01:00 sleep and Saturday's 07:00 wake (`day-schedule.md`
/// §Planner day: the night belongs to the day that ended with it). An
/// all-day Friday deadline is therefore not late at 00:30 Saturday, nor at
/// 06:59, and is missed at 07:00; a touch at 00:30 counts on Friday's planner
/// day for staleness.
#[test]
fn a_sleep_past_midnight_extends_the_day() {
    let daily = DaySchedule {
        weekday: [Some(DayWindow::new(time(7, 0, 0, 0), time(1, 0, 0, 0)).expect("window")); 7],
        ..DaySchedule::default()
    };
    let friday = date(2026, 3, 6);
    let t = task(
        Some(SunriseTime::all_day(friday)),
        None,
        ts("2026-03-01T12:00:00Z"),
    );
    let at_0030 = ts("2026-03-07T00:30:00Z");
    let at_0700 = ts("2026-03-07T07:00:00Z");
    let zone = tz("UTC");
    assert_eq!(lateness(&t, at_0030, &zone, &daily, 14), ON);
    assert_eq!(
        lateness(&t, ts("2026-03-07T06:59:59Z"), &zone, &daily, 14),
        ON
    );
    assert_eq!(
        lateness(&t, at_0700, &zone, &daily, 14),
        late(Lateness::LateHard, at_0700)
    );
    // With no schedule the same deadline is missed at civil midnight.
    assert_eq!(
        lateness(&t, at_0030, &zone, &DaySchedule::default(), 14),
        late(Lateness::LateHard, ts("2026-03-07T00:00:00Z"))
    );
    // Touched at 00:30 Saturday: Friday's planner day. One day stale is
    // Saturday's planner day, which begins at 07:00 (wake, after the 01:00
    // sleep).
    let undated = task(None, None, at_0030);
    assert_eq!(
        lateness(&undated, ts("2026-03-07T06:59:00Z"), &zone, &daily, 1),
        ON
    );
    assert_eq!(
        lateness(&undated, ts("2026-03-07T07:00:00Z"), &zone, &daily, 1),
        late(Lateness::Stale, ts("2026-03-07T07:00:00Z"))
    );
}

/// Lateness is evaluated for open tasks only.
#[test]
fn a_closed_archived_or_deleted_task_is_never_late() {
    let missed = task(
        Some(ts("2026-01-01T00:00:00Z").into()),
        None,
        ts("2025-01-01T00:00:00Z"),
    );
    let now = ts("2026-06-01T00:00:00Z");
    let zone = tz("UTC");
    let none = DaySchedule::default();
    assert_eq!(
        lateness(&missed, now, &zone, &none, 14).state,
        Lateness::LateHard
    );
    for change in [
        |t: &mut Task| t.state = TaskState::Done,
        |t: &mut Task| t.state = TaskState::Cancelled,
        |t: &mut Task| t.archived = true,
        |t: &mut Task| t.deleted = true,
    ] {
        let mut t = missed.clone();
        change(&mut t);
        assert_eq!(lateness(&t, now, &zone, &none, 14), ON);
    }
    let mut unknown = missed;
    unknown.state = TaskState::from_raw("waiting");
    assert_eq!(
        lateness(&unknown, now, &zone, &none, 14).state,
        Lateness::LateHard,
        "an unknown state is open, as todo"
    );
}

/// A deadline this build cannot place is never missed, and still counts as
/// a deadline, so the task is not stale either: an invented answer is worse
/// than none (ADR-0066 §3).
#[test]
fn an_unplaceable_deadline_is_never_missed() {
    let unknown = SunriseTime::Unknown {
        kind: "lunar".into(),
        raw: Unknowns::new(),
    };
    let t = task(
        Some(unknown.clone()),
        Some(unknown),
        ts("2020-01-01T00:00:00Z"),
    );
    assert_eq!(
        lateness(
            &t,
            ts("2026-01-01T00:00:00Z"),
            &tz("UTC"),
            &DaySchedule::default(),
            14
        ),
        ON
    );
}

#[test]
fn the_states_spell_and_order_as_the_queue_reads_them() {
    for l in Lateness::ALL {
        assert_eq!(Lateness::parse(l.as_str()), Some(l));
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(json, format!("\"{}\"", l.as_str()));
    }
    assert_eq!(Lateness::parse("late"), None);
    let mut all = [
        Lateness::OnTrack,
        Lateness::Stale,
        Lateness::LateSoft,
        Lateness::LateHard,
    ];
    all.sort_unstable();
    assert_eq!(all, Lateness::ALL);
    assert!(Lateness::LateHard.is_late() && Lateness::LateSoft.is_late());
    assert!(!Lateness::Stale.is_late() && !Lateness::OnTrack.is_late());
    assert!(Lateness::Stale.needs_decision() && !Lateness::OnTrack.needs_decision());
}

/// The deadline and start resolution the states read, against the civil
/// rules they reduce to with no schedule.
#[test]
fn deadline_and_start_reduce_to_the_civil_rules_with_no_schedule() {
    let none = DaySchedule::default();
    for zone in [
        "UTC",
        "America/New_York",
        "Pacific/Kiritimati",
        "Asia/Kolkata",
    ] {
        let z = tz(zone);
        for v in [
            SunriseTime::all_day(date(2026, 3, 8)),
            SunriseTime::floating(date(2026, 3, 8).at(2, 30, 0, 0)),
            SunriseTime::zoned(date(2026, 3, 8).at(9, 0, 0, 0), "Europe/Paris"),
            SunriseTime::instant(ts("2026-03-08T12:00:00Z")),
        ] {
            assert_eq!(deadline_at(&none, &v, &z), v.due_in(&z), "{zone} {v}");
            assert_eq!(
                crate::day_schedule::starts_at(&none, &v, &z),
                v.resolve_in(&z),
                "{zone} {v}"
            );
        }
    }
}
