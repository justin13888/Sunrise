//! Defer, snooze, schedule and the Today capture default, across the two
//! DST changes of 2026 in Toronto. The Apple app's answer to each defer was
//! `now + N × 86,400,000`, and each test that crosses a change says what that
//! would have got wrong.

use super::*;
use crate::civil::reader_zone;
use jiff::civil::date;
use sunrise_domain::{TodaySection, Unknowns};
use sunrise_id::EntityKind;

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;

fn toronto() -> TimeZone {
    reader_zone("America/Toronto").unwrap()
}

/// 2026-03-07T17:00:00Z: Saturday noon in Toronto, the day before
/// spring-forward.
fn sat_noon() -> Timestamp {
    Timestamp::from_millisecond(1_772_902_800_000).unwrap()
}

fn ms(value: &SunriseTime) -> i64 {
    value.resolve_in(&toronto()).unwrap().as_millisecond()
}

#[test]
fn an_unplanned_task_deferred_a_day_keeps_the_wall_clock_and_follows_the_user() {
    let to = deferred_to(None, 1, sat_noon(), &toronto()).unwrap();
    assert_eq!(to, SunriseTime::floating(date(2026, 3, 8).at(12, 0, 0, 0)));
    let now = sat_noon().as_millisecond();
    assert_eq!(
        ms(&to) - now,
        DAY_MS - HOUR_MS,
        "the spring-forward day is 23 hours"
    );
    assert_ne!(ms(&to), now + DAY_MS, "the Apple app's answer was 13:00");
}

#[test]
fn an_instant_plan_moves_by_civil_days_across_both_changes() {
    let tz = toronto();
    let plan = SunriseTime::instant(sat_noon());
    let to = deferred_to(Some(&plan), 1, sat_noon(), &tz).unwrap();
    assert!(matches!(to, SunriseTime::Instant { .. }), "{to:?}");
    assert_eq!(ms(&to) - sat_noon().as_millisecond(), DAY_MS - HOUR_MS);

    // Saturday 2026-10-31 12:00 EDT; the clocks fall back that night.
    let fall = Timestamp::from_millisecond(1_793_462_400_000).unwrap();
    let to = deferred_to(Some(&SunriseTime::instant(fall)), 1, fall, &tz).unwrap();
    assert_eq!(ms(&to) - fall.as_millisecond(), DAY_MS + HOUR_MS);
    assert_eq!(
        to.resolve_in(&tz).unwrap().to_zoned(tz.clone()).datetime(),
        date(2026, 11, 1).at(12, 0, 0, 0)
    );
}

/// "Defer to tomorrow" on a task planned for yesterday morning means
/// tomorrow morning: the date counts from today, the time is the plan's.
#[test]
fn a_late_plan_is_deferred_from_today_keeping_its_kind_and_time() {
    let tz = toronto();
    let friday_nine = SunriseTime::floating(date(2026, 3, 6).at(9, 0, 0, 0));
    assert_eq!(
        deferred_to(Some(&friday_nine), 1, sat_noon(), &tz).unwrap(),
        SunriseTime::floating(date(2026, 3, 8).at(9, 0, 0, 0))
    );
    let berlin = SunriseTime::zoned(date(2026, 3, 6).at(9, 0, 0, 0), "Europe/Berlin");
    assert_eq!(
        deferred_to(Some(&berlin), 7, sat_noon(), &tz).unwrap(),
        SunriseTime::zoned(date(2026, 3, 14).at(9, 0, 0, 0), "Europe/Berlin")
    );
    let day = SunriseTime::all_day(date(2026, 3, 1));
    assert_eq!(
        deferred_to(Some(&day), 1, sat_noon(), &tz).unwrap(),
        SunriseTime::all_day(date(2026, 3, 8))
    );
}

/// A wall-clock time that the spring-forward gap skips is stored as meant
/// and resolves forward, by the one rule `time.md` §3 states.
#[test]
fn a_plan_deferred_into_the_gap_keeps_its_wall_clock() {
    let tz = toronto();
    let half_two = SunriseTime::floating(date(2026, 3, 7).at(2, 30, 0, 0));
    let to = deferred_to(Some(&half_two), 1, sat_noon(), &tz).unwrap();
    assert_eq!(to, SunriseTime::floating(date(2026, 3, 8).at(2, 30, 0, 0)));
    assert_eq!(
        to.resolve_in(&tz).unwrap().to_zoned(tz.clone()).datetime(),
        date(2026, 3, 8).at(3, 30, 0, 0)
    );
}

#[test]
fn an_unknown_plan_is_replaced_as_an_unplanned_one() {
    let unknown = SunriseTime::Unknown {
        kind: "lunar".into(),
        raw: Unknowns::default(),
    };
    assert_eq!(
        deferred_to(Some(&unknown), 1, sat_noon(), &toronto()).unwrap(),
        SunriseTime::floating(date(2026, 3, 8).at(12, 0, 0, 0))
    );
}

#[test]
fn the_defer_command_carries_the_kind() {
    let id = EntityRef::new(EntityKind::Task, [3; 16]);
    let plan = SunriseTime::all_day(date(2026, 3, 7));
    match defer_command(id, Some(&plan), 2, sat_noon(), &toronto()).unwrap() {
        Command::DeferTaskTo { id: got, to } => {
            assert_eq!(got, id);
            assert_eq!(to, SunriseTime::all_day(date(2026, 3, 9)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_snooze_is_an_hour_or_a_civil_day() {
    let tz = toronto();
    let hour = snoozed_to(None, SnoozeSpan::OneHour, sat_noon(), &tz).unwrap();
    assert_eq!(ms(&hour), sat_noon().as_millisecond() + HOUR_MS);
    assert!(matches!(hour, SunriseTime::Instant { .. }));

    let plan = SunriseTime::floating(date(2026, 3, 7).at(12, 0, 0, 0));
    assert_eq!(
        snoozed_to(Some(&plan), SnoozeSpan::Tomorrow, sat_noon(), &tz).unwrap(),
        SunriseTime::floating(date(2026, 3, 8).at(12, 0, 0, 0))
    );
    assert_eq!(
        snoozed_to(Some(&plan), SnoozeSpan::NextWeek, sat_noon(), &tz).unwrap(),
        SunriseTime::floating(date(2026, 3, 14).at(12, 0, 0, 0))
    );
}

#[test]
fn a_scheduled_time_is_floating_unless_a_kind_is_asked_for() {
    let tz = toronto();
    let now = sat_noon().as_millisecond();
    assert_eq!(
        scheduled_at(now, None, &tz).unwrap(),
        SunriseTime::floating(date(2026, 3, 7).at(12, 0, 0, 0))
    );
    assert_eq!(
        scheduled_at(now, Some(TimeKind::Instant), &tz).unwrap(),
        SunriseTime::instant(sat_noon())
    );
    assert_eq!(
        scheduled_on_day(1, sat_noon(), &tz).unwrap(),
        SunriseTime::all_day(date(2026, 3, 8))
    );
    assert_eq!(
        scheduled_on_day(7, sat_noon(), &tz).unwrap(),
        SunriseTime::all_day(date(2026, 3, 14))
    );
}

/// A capture into Today is dated today, and Today then lists it.
#[test]
fn a_today_capture_is_planned_for_today_all_day() {
    let tz = toronto();
    // 23:30 on Saturday in Toronto is already Sunday in UTC.
    let late = Timestamp::from_millisecond(1_772_944_200_000).unwrap();
    let plan = today_capture_plan(late, &tz);
    assert_eq!(plan, SunriseTime::all_day(date(2026, 3, 7)));
    let now_ms = u64::try_from(late.as_millisecond()).unwrap();
    assert_eq!(
        crate::vocab::today_section(Some(&plan), None, now_ms, "America/Toronto"),
        TodaySection::Scheduled
    );
}
