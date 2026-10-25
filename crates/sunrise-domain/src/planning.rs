//! Which section of the Today view a task belongs in.
//!
//! `docs/08-features/planning-views.md` composes Today out of named sections
//! and fixes the overdue boundary exactly: a task is overdue **iff** `due_at`
//! falls before the start of today in the reader's zone, "regardless of the
//! wall-clock time within the day". That is a domain rule, not a layout
//! choice, and it is not obvious enough to survive being re-derived: the
//! tempting `due_at < now` reads a task due at 17:00 as overdue from 17:01,
//! which the spec explicitly rules out.
//!
//! It lives here for the same reason [`crate::phrase`] does. Two clients that
//! each decide what "overdue" means will disagree, and the user will be right
//! to call the disagreement a bug.
//!
//! Deliberately **not** a query: the ordering *within* a section is the core's
//! (`Query::Today` sorts by `scheduled_at`, else `due_at`, resolved in the
//! reader's zone), and this only
//! says which bucket a row lands in. Classify, then stable-partition, and both
//! facts stay owned by whoever owns them.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::day_schedule::{planner_date, planner_day_of, DaySchedule};
use crate::lateness::LatenessState;
use crate::task::Task;
use crate::time::SunriseTime;

/// A section of the Today view, in the order the spec lists them.
///
/// The derived [`Ord`] is that order — sections sort the way they are
/// declared, so a client that sorts its groups gets the documented layout
/// without restating it. Blocks (§1) and promote-from-Inbox (§4) are not here:
/// neither is a property of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TodaySection {
    /// The user said they would do this today (§2). Also covers a task
    /// scheduled *before* today and still open: an intention that has slipped
    /// is still an intention, and the spec reserves "overdue" for deadlines.
    Scheduled,
    /// Due today, with no scheduled time of its own (§3).
    Due,
    /// The deadline passed before today began (§5). Folded by default, with a
    /// count badge.
    Overdue,
}

impl TodaySection {
    /// The section heading, as the spec words it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Scheduled => "Scheduled",
            Self::Due => "Due today",
            Self::Overdue => "Overdue",
        }
    }
}

/// Which section a Today row belongs in.
///
/// `tz` is the reading device's zone, which is what resolves the kinds of
/// [`SunriseTime`] that carry none of their own — an all-day deadline is
/// "today" on the day it names, wherever it is read.
///
/// Total by construction. A task carrying neither time cannot reach Today at
/// all (the query selects on one or the other being set), and classifies as
/// [`TodaySection::Due`] rather than panicking if one ever does.
///
/// Days are civil days here; [`today_section_in`] reads them as planner days
/// of a day schedule.
#[must_use]
pub fn today_section(
    scheduled_at: Option<&SunriseTime>,
    due_at: Option<&SunriseTime>,
    now_ms: u64,
    tz: &TimeZone,
) -> TodaySection {
    today_section_in(scheduled_at, due_at, now_ms, tz, &DaySchedule::default())
}

/// [`today_section`] with "today" the planner day of `schedule` that holds
/// `now_ms` (`docs/02-domain/day-schedule.md` §Planner day): at 00:30, for
/// someone who sleeps at 01:00, a task due "yesterday" is not overdue yet.
#[must_use]
pub fn today_section_in(
    scheduled_at: Option<&SunriseTime>,
    due_at: Option<&SunriseTime>,
    now_ms: u64,
    tz: &TimeZone,
    schedule: &DaySchedule,
) -> TodaySection {
    if due_at.is_some_and(|d| is_overdue_in(d, now_ms, tz, schedule)) {
        TodaySection::Overdue
    } else if scheduled_at.is_some() {
        TodaySection::Scheduled
    } else {
        TodaySection::Due
    }
}

/// `due_at < start_of_today_local`, compared as whole civil days.
///
/// Comparing dates rather than instants is the whole point: subtracting a
/// day's worth of milliseconds from `now` would make "overdue" depend on the
/// time of day, and days are not all the same length across a DST boundary
/// anyway.
#[must_use]
pub fn is_overdue(due_at: &SunriseTime, now_ms: u64, tz: &TimeZone) -> bool {
    is_overdue_in(due_at, now_ms, tz, &DaySchedule::default())
}

/// [`is_overdue`] over the planner days of `schedule`: the deadline's planner
/// day is before the one `now_ms` is in. With an empty schedule the two agree.
#[must_use]
pub fn is_overdue_in(
    due_at: &SunriseTime,
    now_ms: u64,
    tz: &TimeZone,
    schedule: &DaySchedule,
) -> bool {
    let Ok(Ok(now)) = i64::try_from(now_ms).map(Timestamp::from_millisecond) else {
        // An unreadable clock is not evidence that anything is late.
        return false;
    };
    let Ok(today) = planner_date(schedule, now, tz) else {
        return false;
    };
    // A deadline with no instant in `tz` is not evidence of lateness either
    // (`docs/11-adr/0066-times-survive-travel.md` §3).
    planner_day_of(schedule, due_at, tz).is_some_and(|due| due < today)
}

/// A group of the Today view as ADR-0047 composes it
/// (`docs/08-features/planning-views.md` §Today), in the order it lists them.
///
/// The successor of [`TodaySection`], which stays for the clients that still
/// classify on two times: this one reads all three times and the task's
/// [`LatenessState`], so "late" means a missed deadline, soft or hard, from
/// the moment it is missed, and a slipped plan is carried over, never late.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodayGroup {
    /// `planned_at` falls on today.
    Planned,
    /// `planned_at` fell on an earlier planner day, and the task is not
    /// late. Carried into Today without any write; nothing rolls the plan
    /// forward.
    CarriedOver,
    /// `hard_due_at` or `target_at` falls on today, and the task is not
    /// planned for today. [`TodayRowDeadline`] says which.
    DueToday,
    /// `LateHard` or `LateSoft`, whatever its plan says. Folded by default.
    Late,
}

impl TodayGroup {
    /// The stable spelling: `planned`, `carried_over`, `due_today`, `late`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::CarriedOver => "carried_over",
            Self::DueToday => "due_today",
            Self::Late => "late",
        }
    }
}

/// Which deadline puts a [`TodayGroup::DueToday`] row in Today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodayRowDeadline {
    /// The hard deadline falls today (it wins when both do).
    Hard,
    /// Only the soft deadline falls today.
    Target,
}

/// Where `task` sits in the Today view of a reader whose planner today is
/// `today`, in `tz`, with the day schedule `schedule`, given the task's
/// `lateness` for that reader; `None` when it is not in Today at all.
///
/// First match wins: late, planned today, planned earlier, a deadline today.
/// A task planned for a later day and with no deadline today is not in
/// Today, and neither is a closed one (lateness is `OnTrack` for it, and the
/// core reads only open tasks). A time this build cannot place is on no day.
#[must_use]
pub fn today_group(
    task: &Task,
    lateness: &LatenessState,
    today: Date,
    tz: &TimeZone,
    schedule: &DaySchedule,
) -> Option<(TodayGroup, Option<TodayRowDeadline>)> {
    if lateness.state.is_late() {
        return Some((TodayGroup::Late, None));
    }
    let day = |v: Option<&SunriseTime>| v.and_then(|v| planner_day_of(schedule, v, tz));
    match day(task.planned_at.as_ref()) {
        Some(d) if d == today => return Some((TodayGroup::Planned, None)),
        Some(d) if d < today => return Some((TodayGroup::CarriedOver, None)),
        _ => {}
    }
    if day(task.hard_due_at.as_ref()) == Some(today) {
        return Some((TodayGroup::DueToday, Some(TodayRowDeadline::Hard)));
    }
    if day(task.target_at.as_ref()) == Some(today) {
        return Some((TodayGroup::DueToday, Some(TodayRowDeadline::Target)));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil;

    /// 2026-03-10T17:00 in New York, as epoch ms.
    fn now_ny() -> u64 {
        let tz = TimeZone::get("America/New_York").expect("tz");
        u64::try_from(
            tz.to_zoned(civil::date(2026, 3, 10).at(17, 0, 0, 0))
                .expect("civil")
                .timestamp()
                .as_millisecond(),
        )
        .expect("positive")
    }

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").expect("tz")
    }

    #[test]
    fn a_deadline_later_today_is_not_overdue_even_after_it_passes() {
        // 09:00 today, read at 17:00 today. The spec is explicit that this is
        // a Today task, not an overdue one.
        let due = SunriseTime::zoned(civil::date(2026, 3, 10).at(9, 0, 0, 0), "America/New_York");
        assert!(!is_overdue(&due, now_ny(), &ny()));
        assert_eq!(
            today_section(None, Some(&due), now_ny(), &ny()),
            TodaySection::Due
        );
    }

    #[test]
    fn a_deadline_yesterday_is_overdue() {
        let due = SunriseTime::zoned(civil::date(2026, 3, 9).at(23, 59, 0, 0), "America/New_York");
        assert!(is_overdue(&due, now_ny(), &ny()));
        assert_eq!(
            today_section(None, Some(&due), now_ny(), &ny()),
            TodaySection::Overdue
        );
    }

    #[test]
    fn an_all_day_deadline_is_due_on_the_day_it_names() {
        let today = SunriseTime::all_day(civil::date(2026, 3, 10));
        let yesterday = SunriseTime::all_day(civil::date(2026, 3, 9));
        assert!(!is_overdue(&today, now_ny(), &ny()));
        assert!(is_overdue(&yesterday, now_ny(), &ny()));
    }

    #[test]
    fn the_reader_s_zone_decides_for_a_floating_deadline() {
        // A floating deadline carries no zone, so the reader supplies one and
        // the answer legitimately differs between two readers. 22:00 on the
        // 10th is still ahead in New York, where it is 17:00 on the 10th; the
        // same wall clock read in Tokyo — where that instant is already 06:00
        // on the 11th — is yesterday, and overdue.
        let late_today = SunriseTime::floating(civil::date(2026, 3, 10).at(22, 0, 0, 0));
        assert!(!is_overdue(&late_today, now_ny(), &ny()));
        let tokyo = TimeZone::get("Asia/Tokyo").expect("tz");
        assert!(is_overdue(&late_today, now_ny(), &tokyo));
    }

    #[test]
    fn an_overdue_deadline_outranks_a_scheduled_time() {
        // A task can be both scheduled for today and overdue; the folded
        // Overdue section is where the user needs to see it.
        let sched =
            SunriseTime::zoned(civil::date(2026, 3, 10).at(15, 0, 0, 0), "America/New_York");
        let due = SunriseTime::all_day(civil::date(2026, 3, 1));
        assert_eq!(
            today_section(Some(&sched), Some(&due), now_ny(), &ny()),
            TodaySection::Overdue
        );
    }

    #[test]
    fn a_scheduled_task_that_slipped_is_still_scheduled_not_overdue() {
        // Scheduled last week, no deadline. The spec's overdue boundary reads
        // `due_at` only.
        let sched = SunriseTime::all_day(civil::date(2026, 3, 2));
        assert_eq!(
            today_section(Some(&sched), None, now_ny(), &ny()),
            TodaySection::Scheduled
        );
    }

    #[test]
    fn the_sections_sort_the_way_the_spec_lists_them() {
        let mut all = [
            TodaySection::Overdue,
            TodaySection::Due,
            TodaySection::Scheduled,
        ];
        all.sort_unstable();
        assert_eq!(
            all,
            [
                TodaySection::Scheduled,
                TodaySection::Due,
                TodaySection::Overdue
            ]
        );
    }

    #[test]
    fn a_row_with_neither_time_classifies_rather_than_panicking() {
        assert_eq!(
            today_section(None, None, now_ny(), &ny()),
            TodaySection::Due
        );
    }

    #[test]
    fn an_unreadable_clock_does_not_make_everything_late() {
        let due = SunriseTime::all_day(civil::date(1999, 1, 1));
        assert!(!is_overdue(&due, u64::MAX, &ny()));
    }

    fn task(
        planned: Option<SunriseTime>,
        target: Option<SunriseTime>,
        hard: Option<SunriseTime>,
    ) -> Task {
        Task {
            id: sunrise_id::EntityRef::new(sunrise_id::EntityKind::Task, [1u8; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::from_millisecond(i64::try_from(now_ny()).unwrap()).unwrap(),
            title: "t".into(),
            body: None,
            stream_id: None,
            contexts: std::collections::BTreeSet::new(),
            state: crate::TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            planned_at: planned,
            target_at: target,
            hard_due_at: hard,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: std::collections::BTreeSet::new(),
            blocked_by: std::collections::BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            reminder_lead_s: None,
            sort_order: None,
            triaged_at: None,
            cancel_reason: None,
            late_acknowledged_at: None,
            archived: false,
            deleted: false,
            unknown: crate::Unknowns::new(),
        }
    }

    /// The ADR-0047 composition, row by row: late wins over every plan, a
    /// plan today is planned, an earlier one is carried over (a slipped plan
    /// is not lateness), and a deadline today, hard over soft, is due today.
    #[test]
    fn the_today_groups_in_their_order_of_precedence() {
        use crate::lateness::{lateness, Lateness};
        let now = Timestamp::from_millisecond(i64::try_from(now_ny()).unwrap()).unwrap();
        let today = civil::date(2026, 3, 10);
        let none = DaySchedule::default();
        let at = |h: i8| SunriseTime::zoned(today.at(h, 0, 0, 0), "America/New_York");
        let yesterday = SunriseTime::all_day(civil::date(2026, 3, 9));
        let tomorrow = SunriseTime::all_day(civil::date(2026, 3, 11));
        let group = |t: &Task| {
            let l = lateness(t, now, &ny(), &none, 14);
            today_group(t, &l, today, &ny(), &none)
        };
        let late = task(Some(at(9)), None, Some(at(16)));
        assert_eq!(
            lateness(&late, now, &ny(), &none, 14).state,
            Lateness::LateHard
        );
        let rows = [
            (
                "a missed hard deadline today",
                late,
                Some((TodayGroup::Late, None)),
            ),
            (
                "a missed target, planned earlier",
                task(Some(yesterday.clone()), Some(yesterday.clone()), None),
                Some((TodayGroup::Late, None)),
            ),
            (
                "planned later today",
                task(Some(at(20)), None, Some(tomorrow.clone())),
                Some((TodayGroup::Planned, None)),
            ),
            (
                "planned yesterday, no deadline",
                task(Some(yesterday.clone()), None, None),
                Some((TodayGroup::CarriedOver, None)),
            ),
            (
                "hard deadline tonight, planned tomorrow",
                task(Some(tomorrow.clone()), Some(at(21)), Some(at(22))),
                Some((TodayGroup::DueToday, Some(TodayRowDeadline::Hard))),
            ),
            (
                "only the target falls today",
                task(
                    None,
                    Some(SunriseTime::all_day(today)),
                    Some(tomorrow.clone()),
                ),
                Some((TodayGroup::DueToday, Some(TodayRowDeadline::Target))),
            ),
            (
                "planned tomorrow, due tomorrow",
                task(Some(tomorrow.clone()), None, Some(tomorrow)),
                None,
            ),
            ("nothing at all", task(None, None, None), None),
        ];
        for (name, t, want) in rows {
            assert_eq!(group(&t), want, "{name}");
        }
        let mut groups = [
            TodayGroup::Late,
            TodayGroup::DueToday,
            TodayGroup::CarriedOver,
            TodayGroup::Planned,
        ];
        groups.sort_unstable();
        assert_eq!(
            groups.map(TodayGroup::as_str),
            ["planned", "carried_over", "due_today", "late"]
        );
    }
}
