//! The queue's membership and order, Keep's suppression until the next
//! lateness transition, the backdated-completion rules and the drop reason.

use super::*;
use crate::lateness::{lateness, Lateness};
use crate::task::{TaskDraft, TaskState};
use jiff::civil::date;
use sunrise_id::{EntityKind, EntityRef};

fn ts(s: &str) -> Timestamp {
    s.parse().expect(s)
}

fn task(n: u8, created: &str) -> Task {
    Task::from_draft(
        EntityRef::new(EntityKind::Task, [n; 16]),
        ts(created),
        TaskDraft {
            title: format!("t{n}"),
            ..Default::default()
        },
    )
}

fn read(t: &Task, now: &str) -> LatenessState {
    lateness(t, ts(now), &TimeZone::UTC, &DaySchedule::default(), 14)
}

/// A task is in the queue while it is late or stale and not kept since its
/// state began; a Keep made before the state began does not hide it; a
/// paused stream and a closed task are never in it.
#[test]
fn membership_and_keep() {
    let mut t = task(1, "2026-06-01T00:00:00Z");
    t.target_at = Some(SunriseTime::all_day(date(2026, 6, 2)));
    t.hard_due_at = Some(SunriseTime::all_day(date(2026, 6, 9)));
    let on_track = read(&t, "2026-06-02T12:00:00Z");
    assert!(!in_triage(&t, &on_track, false), "on track");
    let soft = read(&t, "2026-06-04T12:00:00Z");
    assert_eq!(soft.state, Lateness::LateSoft);
    assert!(in_triage(&t, &soft, false));
    assert!(
        !in_triage(&t, &soft, true),
        "a paused stream asks for nothing"
    );
    // Kept on the 4th: out of the queue while still LateSoft.
    t.late_acknowledged_at = Some(ts("2026-06-04T12:00:00Z"));
    assert!(!in_triage(&t, &soft, false));
    // Kept exactly when the state began counts as kept.
    t.late_acknowledged_at = soft.since;
    assert!(!in_triage(&t, &soft, false));
    // Kept before the soft deadline was missed: the miss is new.
    t.late_acknowledged_at = Some(ts("2026-06-02T12:00:00Z"));
    assert!(in_triage(&t, &soft, false));
    // Kept during LateSoft, it comes back by itself as LateHard.
    t.late_acknowledged_at = Some(ts("2026-06-04T12:00:00Z"));
    let hard = read(&t, "2026-06-10T12:00:00Z");
    assert_eq!(hard.state, Lateness::LateHard);
    assert!(
        in_triage(&t, &hard, false),
        "a new state begins after the Keep"
    );
    t.state = TaskState::Done;
    assert!(!in_triage(&t, &read(&t, "2026-06-10T12:00:00Z"), false));
    assert!(
        !in_triage(&t, &hard, false),
        "a closed task is never queued"
    );
}

/// A Keep of a stale task is a touch: it is stale again only after another
/// `stale_after_days` untouched.
#[test]
fn a_kept_stale_task_returns_after_another_stretch() {
    let mut t = task(2, "2026-06-01T00:00:00Z");
    let stale = read(&t, "2026-06-15T12:00:00Z");
    assert_eq!(stale.state, Lateness::Stale);
    assert!(in_triage(&t, &stale, false));
    let kept_at = ts("2026-06-15T12:00:00Z");
    t.late_acknowledged_at = Some(kept_at);
    t.updated_at = kept_at;
    assert!(!in_triage(&t, &read(&t, "2026-06-20T00:00:00Z"), false));
    let again = read(&t, "2026-06-29T00:00:00Z");
    assert_eq!(again.state, Lateness::Stale);
    assert!(again.since > Some(kept_at));
    assert!(in_triage(&t, &again, false));
}

#[test]
fn the_order_is_hard_then_soft_then_stale_oldest_first_then_id() {
    let s = |state, since: &str| LatenessState {
        state,
        since: Some(ts(since)),
    };
    let rows = [
        (
            task(5, "2026-01-01T00:00:00Z"),
            s(Lateness::Stale, "2026-01-02T00:00:00Z"),
        ),
        (
            task(4, "2026-01-01T00:00:00Z"),
            s(Lateness::LateSoft, "2026-01-03T00:00:00Z"),
        ),
        (
            task(3, "2026-01-01T00:00:00Z"),
            s(Lateness::LateHard, "2026-01-05T00:00:00Z"),
        ),
        (
            task(2, "2026-01-01T00:00:00Z"),
            s(Lateness::LateHard, "2026-01-04T00:00:00Z"),
        ),
        (
            task(1, "2026-01-01T00:00:00Z"),
            s(Lateness::LateHard, "2026-01-04T00:00:00Z"),
        ),
    ];
    let mut order: Vec<&(Task, LatenessState)> = rows.iter().collect();
    order.sort_by(|a, b| triage_order((&a.0, &a.1), (&b.0, &b.1)));
    let titles: Vec<&str> = order.iter().map(|r| r.0.title.as_str()).collect();
    assert_eq!(titles, ["t1", "t2", "t3", "t4", "t5"]);
}

/// A backdated completion is an instant or a date, never after now and
/// never before the task existed, read on the reader's planner days.
#[test]
fn a_backdated_completion_is_checked() {
    let t = task(1, "2026-06-01T10:00:00Z");
    let now = ts("2026-06-05T08:00:00Z");
    let utc = TimeZone::UTC;
    let none = DaySchedule::default();
    let check = |v: SunriseTime| check_completion(&t, &v, now, &utc, &none);
    assert_eq!(check(ts("2026-06-04T09:00:00Z").into()), Ok(()));
    assert_eq!(check(now.into()), Ok(()));
    assert_eq!(
        check(ts("2026-06-05T08:00:01Z").into()),
        Err(CompletionError::Future)
    );
    assert_eq!(
        check(ts("2026-06-01T09:59:59Z").into()),
        Err(CompletionError::BeforeCreated)
    );
    assert_eq!(check(ts("2026-06-01T10:00:00Z").into()), Ok(()));
    assert_eq!(
        check(SunriseTime::all_day(date(2026, 6, 5))),
        Ok(()),
        "today"
    );
    assert_eq!(
        check(SunriseTime::all_day(date(2026, 6, 1))),
        Ok(()),
        "the day it was made"
    );
    assert_eq!(
        check(SunriseTime::all_day(date(2026, 6, 6))),
        Err(CompletionError::Future)
    );
    assert_eq!(
        check(SunriseTime::all_day(date(2026, 5, 31))),
        Err(CompletionError::BeforeCreated)
    );
    assert_eq!(
        check(SunriseTime::floating(date(2026, 6, 4).at(9, 0, 0, 0))),
        Err(CompletionError::Kind)
    );
    // The reader's day decides a date: 23:00 on the 4th in Honolulu is the
    // 5th in UTC, so "the 5th" is in the future there.
    let honolulu = TimeZone::get("Pacific/Honolulu").unwrap();
    assert_eq!(
        check_completion(
            &t,
            &SunriseTime::all_day(date(2026, 6, 5)),
            now,
            &honolulu,
            &none
        ),
        Err(CompletionError::Future)
    );
}

#[test]
fn a_drop_reason_is_trimmed_bounded_and_shown_only_while_cancelled() {
    assert_eq!(cancel_reason(None), Ok(None));
    assert_eq!(cancel_reason(Some("   ")), Ok(None));
    assert_eq!(
        cancel_reason(Some("  not mine  ")),
        Ok(Some("not mine".to_owned()))
    );
    let long = "é".repeat(MAX_CANCEL_REASON_LEN);
    assert_eq!(cancel_reason(Some(&long)), Ok(Some(long.clone())));
    assert_eq!(
        cancel_reason(Some(&format!("{long}x"))),
        Err(ValidationError::Field {
            field: "task.cancel_reason",
            constraint: "max_512"
        })
    );
    let mut t = task(1, "2026-06-01T00:00:00Z");
    t.cancel_reason = Some("not mine".into());
    assert_eq!(t.drop_reason(), None, "a reason on an open task is ignored");
    t.state = TaskState::Cancelled;
    assert_eq!(t.drop_reason(), Some("not mine"));
}
