//! The two dedicated digest views: the morning summary and the end-of-day plan.

use crate::day_schedule::{starts_at, DaySchedule};
use crate::task::{Task, TaskState};
use crate::time::SunriseTime;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::Serialize;

/// The morning notification's dedicated view
/// (issue #9): what got done, and what still needs a decision.
#[derive(Debug, Clone, Serialize)]
pub struct MorningSummary {
    /// Start of the previous civil day in the device's zone — the "since" the
    /// issue asks for.
    pub since: Timestamp,
    /// Start of today, so a client can split the completed list into
    /// "yesterday" and "already today" without a second query.
    pub today_start: Timestamp,
    /// Completed at or after `since`, newest completion first.
    pub completed: Vec<Task>,
    /// Live open tasks sitting unfiled in the Inbox: the triage queue.
    pub to_triage: Vec<Task>,
    /// Live open tasks landing before the end of today, **overdue included**,
    /// earliest first.
    pub due_today: Vec<Task>,
}

/// The end-of-day notification's dedicated view (issue #9): what is still open
/// today, and what the coming week holds.
#[derive(Debug, Clone, Serialize)]
pub struct EndOfDayPlan {
    /// Start of today in the device's zone.
    pub day_start: Timestamp,
    /// Start of tomorrow — the exclusive end of "today".
    pub day_end: Timestamp,
    /// Exclusive end of the seven civil days that follow today.
    pub week_end: Timestamp,
    /// Still open and landing at or before `day_end`, **overdue included**:
    /// the whole point of the view is deciding what to do about them.
    pub still_open: Vec<Task>,
    /// Open and landing in `[day_end, week_end)`.
    pub week_ahead: Vec<Task>,
    /// Open, live, and carrying neither a scheduled time nor a deadline — the
    /// candidates for "plan the coming week".
    pub unscheduled: Vec<Task>,
}

/// Build the morning summary from every live task and the civil bounds the
/// caller resolved. The triage queue is the Inbox view ([`Task::in_inbox`]).
///
/// `tz` is the reader's zone: every task time is resolved in it before it is
/// compared with the bounds (`docs/10-cross-cutting/time.md` §2).
#[must_use]
pub fn build_morning_summary(
    tasks: &[Task],
    since: Timestamp,
    today_start: Timestamp,
    today_end: Timestamp,
    tz: &TimeZone,
    schedule: &DaySchedule,
) -> MorningSummary {
    let mut completed: Vec<Task> = tasks
        .iter()
        .filter(|t| {
            !t.deleted
                && t.state == TaskState::Done
                && completion_ms(t, tz) >= since.as_millisecond()
        })
        .cloned()
        .collect();
    completed.sort_by(|a, b| {
        completion_ms(b, tz)
            .cmp(&completion_ms(a, tz))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut to_triage: Vec<Task> = tasks.iter().filter(|t| t.in_inbox()).cloned().collect();
    to_triage.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut due_today: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && lands_in(t, i64::MIN, today_end.as_millisecond(), tz, schedule))
        .cloned()
        .collect();
    due_today.sort_by(|a, b| plan_order(a, b, tz, schedule));

    MorningSummary {
        since,
        today_start,
        completed,
        to_triage,
        due_today,
    }
}

/// Build the end-of-day plan from every live task and the civil bounds the
/// caller resolved, resolving every task time in the reader's zone `tz`.
#[must_use]
pub fn build_end_of_day_plan(
    tasks: &[Task],
    day_start: Timestamp,
    day_end: Timestamp,
    week_end: Timestamp,
    tz: &TimeZone,
    schedule: &DaySchedule,
) -> EndOfDayPlan {
    let mut still_open: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && lands_in(t, i64::MIN, day_end.as_millisecond(), tz, schedule))
        .cloned()
        .collect();
    still_open.sort_by(|a, b| plan_order(a, b, tz, schedule));

    let mut week_ahead: Vec<Task> = tasks
        .iter()
        .filter(|t| {
            is_open(t)
                && lands_in(
                    t,
                    day_end.as_millisecond(),
                    week_end.as_millisecond(),
                    tz,
                    schedule,
                )
        })
        .cloned()
        .collect();
    week_ahead.sort_by(|a, b| plan_order(a, b, tz, schedule));

    let mut unscheduled: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && t.planned_at.is_none() && !t.has_deadline())
        .cloned()
        .collect();
    unscheduled.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    EndOfDayPlan {
        day_start,
        day_end,
        week_end,
        still_open,
        week_ahead,
        unscheduled,
    }
}

/// Live and not finished. `cancelled` counts as finished: it is a decision the
/// user already made, and re-offering it for triage every morning would be
/// nagging rather than summarizing.
fn is_open(t: &Task) -> bool {
    // An unknown state reads as `todo`: open.
    !t.deleted && matches!(t.state.effective(), TaskState::Todo | TaskState::InProgress)
}

/// When a task was completed, resolved in `tz`; `i64::MIN` for none, or for a
/// kind this build cannot place.
fn completion_ms(t: &Task, tz: &TimeZone) -> i64 {
    t.completed_at
        .as_ref()
        .and_then(|c| c.resolve_in(tz))
        .map_or(i64::MIN, Timestamp::as_millisecond)
}

/// The instant a task "lands" on in `tz`, whose days are `schedule`'s
/// planner days: its plan, or, when it has none, the earlier of its two
/// deadlines. `None` for a task with neither, or with a kind this build
/// cannot place.
///
/// Each value resolves where it starts ([`starts_at`]): an `all_day` value
/// at the start of its planner day, `boundary(d)` (`time.md` §2 rule 1), so
/// with a sleep time past midnight a brief's planner-day window `[from_ms,
/// to_ms)` never counts an all-day value of the next date into the late
/// night of the day before.
fn lands_at(t: &Task, tz: &TimeZone, schedule: &DaySchedule) -> Option<i64> {
    let start = |v: Option<&SunriseTime>| v.and_then(|v| starts_at(schedule, v, tz));
    let at = if t.planned_at.is_some() {
        start(t.planned_at.as_ref())
    } else {
        match (start(t.target_at.as_ref()), start(t.hard_due_at.as_ref())) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    };
    at.map(Timestamp::as_millisecond)
}

fn lands_in(t: &Task, from_ms: i64, to_ms: i64, tz: &TimeZone, schedule: &DaySchedule) -> bool {
    lands_at(t, tz, schedule).is_some_and(|ms| ms >= from_ms && ms < to_ms)
}

/// Earliest first, then most urgent, then by id so two devices agree.
fn plan_order(a: &Task, b: &Task, tz: &TimeZone, schedule: &DaySchedule) -> std::cmp::Ordering {
    lands_at(a, tz, schedule)
        .unwrap_or(i64::MAX)
        .cmp(&lands_at(b, tz, schedule).unwrap_or(i64::MAX))
        .then_with(|| {
            a.priority
                .unwrap_or(u8::MAX)
                .cmp(&b.priority.unwrap_or(u8::MAX))
        })
        .then_with(|| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{ts, DAY_MS};
    use super::*;
    use crate::unknown::Unknowns;
    use jiff::civil;
    use std::collections::BTreeSet;
    use sunrise_id::{EntityKind, EntityRef};

    fn task(n: u8) -> Task {
        Task {
            id: EntityRef::new(EntityKind::Task, [n; 16]),
            created_at: ts(0),
            updated_at: ts(0),
            title: format!("task {n}"),
            body: None,
            stream_id: Some(EntityRef::new(EntityKind::Stream, [9u8; 16])),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            planned_at: None,
            target_at: None,
            hard_due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            hard_constraint_violated: false,
            reminder_lead_s: None,
            sort_order: None,
            triaged_at: None,
            cancel_reason: None,
            late_acknowledged_at: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    // ---- the two views ----

    #[test]
    fn the_morning_summary_splits_done_from_to_triage() {
        let day = 1_772_064_000_000i64; // a civil midnight, UTC
        let since = day - DAY_MS;

        let mut done_yesterday = task(1);
        done_yesterday.state = TaskState::Done;
        done_yesterday.completed_at = Some(SunriseTime::instant(ts(since + 3_600_000)));

        let mut done_last_week = task(2);
        done_last_week.state = TaskState::Done;
        done_last_week.completed_at = Some(SunriseTime::instant(ts(since - 5 * DAY_MS)));

        let mut inbox_item = task(3);
        inbox_item.stream_id = None;

        let mut due_today = task(4);
        due_today.hard_due_at = Some(SunriseTime::instant(ts(day + 3_600_000)));

        let filed_and_later = {
            let mut t = task(5);
            t.planned_at = Some(SunriseTime::instant(ts(day + 10 * DAY_MS)));
            t
        };

        let s = build_morning_summary(
            &[
                done_yesterday.clone(),
                done_last_week,
                inbox_item.clone(),
                due_today.clone(),
                filed_and_later,
            ],
            ts(since),
            ts(day),
            ts(day + DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );
        assert_eq!(
            s.completed.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![done_yesterday.id],
            "only completions since the previous calendar date"
        );
        assert_eq!(
            s.to_triage.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![inbox_item.id],
            "triage is the unfiled Inbox"
        );
        assert_eq!(
            s.due_today.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![due_today.id]
        );
    }

    /// An overdue task belongs on both views: the morning one so it is not
    /// forgotten, the end-of-day one so a decision gets made about it.
    #[test]
    fn an_overdue_task_shows_up_as_still_open() {
        let day = 1_772_064_000_000i64;
        let mut overdue = task(1);
        overdue.hard_due_at = Some(SunriseTime::instant(ts(day - 3 * DAY_MS)));

        let plan = build_end_of_day_plan(
            &[overdue.clone()],
            ts(day),
            ts(day + DAY_MS),
            ts(day + 8 * DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );
        assert_eq!(
            plan.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![overdue.id]
        );

        let summary = build_morning_summary(
            &[overdue.clone()],
            ts(day - DAY_MS),
            ts(day),
            ts(day + DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );
        assert_eq!(
            summary.due_today.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![overdue.id]
        );
    }

    #[test]
    fn the_end_of_day_plan_separates_today_the_week_and_the_unscheduled() {
        let day = 1_772_064_000_000i64;
        let mut today = task(1);
        today.planned_at = Some(SunriseTime::instant(ts(day + 3_600_000)));
        let mut later_this_week = task(2);
        later_this_week.planned_at = Some(SunriseTime::instant(ts(day + 3 * DAY_MS)));
        let mut next_month = task(3);
        next_month.planned_at = Some(SunriseTime::instant(ts(day + 40 * DAY_MS)));
        let floating = task(4);
        let mut done = task(5);
        done.state = TaskState::Done;

        let plan = build_end_of_day_plan(
            &[
                today.clone(),
                later_this_week.clone(),
                next_month,
                floating.clone(),
                done,
            ],
            ts(day),
            ts(day + DAY_MS),
            ts(day + 8 * DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );
        assert_eq!(
            plan.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![today.id]
        );
        assert_eq!(
            plan.week_ahead.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![later_this_week.id]
        );
        assert_eq!(
            plan.unscheduled.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![floating.id],
            "a done task is not a planning candidate"
        );
    }

    /// `plan_order` is three keys deep — "earliest first, then most urgent,
    /// then by id so two devices agree" — and every other test of the two
    /// views produces a single-element list, which pins none of them.
    ///
    /// This fixture ties deliberately: four tasks, three of them landing on
    /// the same instant, two of those also sharing a priority. So the expected
    /// order can only come out right if all three keys are applied, in order.
    fn tied_fixture(landing_ms: i64) -> [Task; 4] {
        // Earliest, and *least* urgent — so it can only lead on key one.
        let mut earliest = task(9);
        earliest.planned_at = Some(SunriseTime::instant(ts(landing_ms - 1)));
        earliest.priority = None;

        // The three below all land together. `urgent` can only lead on key
        // two; `tie_a` and `tie_b` are separated by key three alone.
        let mut tie_b = task(2);
        tie_b.planned_at = Some(SunriseTime::instant(ts(landing_ms)));
        tie_b.priority = Some(5);

        let mut urgent = task(3);
        urgent.planned_at = Some(SunriseTime::instant(ts(landing_ms)));
        urgent.priority = Some(1);

        let mut tie_a = task(1);
        tie_a.planned_at = Some(SunriseTime::instant(ts(landing_ms)));
        tie_a.priority = Some(5);

        [earliest, tie_b, urgent, tie_a]
    }

    /// The order the three keys, applied in order, must produce: the earliest
    /// (id 9) despite having no priority at all, then the urgent one, then the
    /// two that tie on both keys, in id order.
    fn expected_plan_order(tasks: &[Task; 4]) -> Vec<EntityRef> {
        vec![tasks[0].id, tasks[2].id, tasks[3].id, tasks[1].id]
    }

    #[test]
    fn the_end_of_day_plan_orders_by_landing_then_urgency_then_id() {
        let day = 1_772_064_000_000i64;
        let today = tied_fixture(day + 3_600_000);
        let next_week = tied_fixture(day + 3 * DAY_MS);

        let mut tasks = today.to_vec();
        tasks.extend(next_week.iter().map(|t| {
            // Distinct ids across the two buckets, same shape within each.
            let mut t = t.clone();
            t.id = EntityRef::new(EntityKind::Task, [t.id.bytes()[0] + 10; 16]);
            t
        }));
        let plan = build_end_of_day_plan(
            &tasks,
            ts(day),
            ts(day + DAY_MS),
            ts(day + 8 * DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );

        assert_eq!(
            plan.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            expected_plan_order(&today),
            "earliest first, then most urgent, then by id"
        );
        let expected_week: Vec<EntityRef> = expected_plan_order(&next_week)
            .into_iter()
            .map(|id| EntityRef::new(EntityKind::Task, [id.bytes()[0] + 10; 16]))
            .collect();
        assert_eq!(
            plan.week_ahead.iter().map(|t| t.id).collect::<Vec<_>>(),
            expected_week,
            "the week-ahead bucket sorts by the same three keys"
        );
    }

    #[test]
    fn the_morning_summary_orders_due_today_by_landing_then_urgency_then_id() {
        let day = 1_772_064_000_000i64;
        let tasks = tied_fixture(day + 3_600_000);
        let s = build_morning_summary(
            &tasks,
            ts(day - DAY_MS),
            ts(day),
            ts(day + DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );
        assert_eq!(
            s.due_today.iter().map(|t| t.id).collect::<Vec<_>>(),
            expected_plan_order(&tasks),
            "the morning view uses the same comparator as the evening one"
        );
    }

    /// ADR-0045 §6: a state this build does not know reads as `todo`, its
    /// fallback, so the end-of-day plan keeps it open in every bucket.
    #[test]
    fn the_end_of_day_plan_reads_an_unknown_state_as_open() {
        let day = 1_772_064_000_000i64;
        let mut today = task(1);
        today.state = TaskState::from_raw("snoozed");
        today.planned_at = Some(SunriseTime::instant(ts(day + 3_600_000)));
        let mut floating = task(2);
        floating.state = TaskState::from_raw("snoozed");

        let plan = build_end_of_day_plan(
            &[today.clone(), floating.clone()],
            ts(day),
            ts(day + DAY_MS),
            ts(day + 8 * DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );
        assert_eq!(
            plan.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![today.id]
        );
        assert_eq!(
            plan.unscheduled.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![floating.id]
        );
    }

    /// With a sleep time past midnight, Friday's planner day runs to
    /// Saturday's 07:00 wake. An all-day value of Saturday lands at the start
    /// of Saturday's planner day, not at civil midnight inside Friday's late
    /// night, so the evening plan does not count it as still open today
    /// (#574's leftover, `time.md` §2 rule 1). A target alone lands too, and
    /// a task with only a target is not "unscheduled".
    #[test]
    fn an_all_day_value_lands_at_the_start_of_its_planner_day() {
        use crate::day_schedule::{day_bounds, DayWindow};
        let window = DayWindow::new(civil::time(7, 0, 0, 0), civil::time(1, 0, 0, 0)).unwrap();
        let schedule = DaySchedule {
            weekday: [Some(window); 7],
            ..DaySchedule::default()
        };
        let tz = TimeZone::UTC;
        let friday = civil::date(2026, 3, 6);
        let (start, end) = day_bounds(&schedule, friday, &tz).unwrap();
        let week_end = end + jiff::SignedDuration::from_hours(24 * 7);
        let mut saturday = task(1);
        saturday.hard_due_at = Some(SunriseTime::all_day(friday.tomorrow().unwrap()));
        let mut planned_friday = task(2);
        planned_friday.planned_at = Some(SunriseTime::all_day(friday));
        let mut target_friday = task(3);
        target_friday.target_at = Some(SunriseTime::floating(friday.at(18, 0, 0, 0)));
        let tasks = [
            saturday.clone(),
            planned_friday.clone(),
            target_friday.clone(),
        ];
        let plan = build_end_of_day_plan(&tasks, start, end, week_end, &tz, &schedule);
        let ids = |v: &[Task]| v.iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(&plan.still_open), [planned_friday.id, target_friday.id]);
        assert_eq!(ids(&plan.week_ahead), [saturday.id]);
        assert!(plan.unscheduled.is_empty());
        // Read in civil days, the same Saturday value is on Saturday too.
        let civil_end = day_bounds(&DaySchedule::default(), friday, &tz).unwrap().1;
        let civil_plan = build_end_of_day_plan(
            &tasks,
            start,
            civil_end,
            week_end,
            &tz,
            &DaySchedule::default(),
        );
        assert_eq!(ids(&civil_plan.week_ahead), [saturday.id]);
    }

    #[test]
    fn a_cancelled_task_is_not_offered_for_triage() {
        let day = 1_772_064_000_000i64;
        let mut cancelled = task(1);
        cancelled.stream_id = None;
        cancelled.state = TaskState::Cancelled;
        let s = build_morning_summary(
            &[cancelled],
            ts(day - DAY_MS),
            ts(day),
            ts(day + DAY_MS),
            &TimeZone::UTC,
            &DaySchedule::default(),
        );
        assert!(s.to_triage.is_empty());
    }
}
