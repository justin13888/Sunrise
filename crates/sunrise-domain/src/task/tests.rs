//! The task's validation, ordering warnings, Inbox predicate and states.

use super::*;
use sunrise_id::{EntityKind, EntityRef};

fn ref_t() -> EntityRef {
    EntityRef::new(EntityKind::Stream, [1u8; 16])
}

fn utc() -> TimeZone {
    TimeZone::UTC
}

fn zone(name: &str) -> TimeZone {
    TimeZone::get(name).expect("zone")
}

fn timed(planned_at: SunriseTime, hard_due_at: SunriseTime) -> Task {
    Task {
        id: EntityRef::new(EntityKind::Task, [7u8; 16]),
        created_at: Timestamp::UNIX_EPOCH,
        updated_at: Timestamp::UNIX_EPOCH,
        title: "x".into(),
        body: None,
        stream_id: None,
        contexts: BTreeSet::new(),
        state: TaskState::Todo,
        priority: None,
        energy: None,
        estimated_duration_s: None,
        planned_at: Some(planned_at),
        target_at: None,
        hard_due_at: Some(hard_due_at),
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
        cancel_reason: None,
        late_acknowledged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn warnings(t: &Task, tz: &TimeZone) -> Vec<OrderingWarning> {
    t.ordering_warnings(tz, &crate::DaySchedule::default())
}

/// A deadline "Friday" is missed at the END of Friday, so a plan for
/// 09:00 Friday is before it in every zone — on the storage index the
/// all-day value sat at 00:00 UTC and this was flagged everywhere.
#[test]
fn a_plan_inside_an_all_day_deadline_is_not_after_it() {
    let friday = jiff::civil::date(2026, 3, 6);
    let t = timed(
        SunriseTime::floating(friday.at(9, 0, 0, 0)),
        SunriseTime::all_day(friday),
    );
    for tz in [
        "UTC",
        "Pacific/Honolulu",
        "Pacific/Kiritimati",
        "Asia/Kolkata",
    ] {
        assert_eq!(warnings(&t, &zone(tz)), [], "{tz}");
    }
    // The day after is a warning, and only a warning (ADR-0047 §1).
    let late = timed(
        SunriseTime::floating(friday.tomorrow().unwrap().at(9, 0, 0, 0)),
        SunriseTime::all_day(friday),
    );
    assert_eq!(
        warnings(&late, &utc()),
        [OrderingWarning::PlannedAfterHardDue]
    );
    late.validate_invariants(&utc()).unwrap();
}

/// A zoned plan and a floating deadline are compared where the reader
/// is: 09:00 New York is before an 11:00 floating deadline read in New
/// York, and after it read in Kolkata, where 11:00 is the night before.
#[test]
fn mixed_kinds_are_compared_in_the_readers_zone() {
    let day = jiff::civil::date(2026, 6, 1);
    let t = timed(
        SunriseTime::zoned(day.at(9, 0, 0, 0), "America/New_York"),
        SunriseTime::floating(day.at(11, 0, 0, 0)),
    );
    assert_eq!(warnings(&t, &zone("America/New_York")), []);
    assert_eq!(
        warnings(&t, &zone("Asia/Kolkata")),
        [OrderingWarning::PlannedAfterHardDue]
    );
}

/// A soft deadline after the hard one is the other expected ordering,
/// and a target on the hard deadline's own day is not after it.
#[test]
fn a_target_after_the_hard_deadline_is_a_warning() {
    let day = jiff::civil::date(2026, 6, 1);
    let mut t = timed(
        SunriseTime::floating(day.at(9, 0, 0, 0)),
        SunriseTime::all_day(day),
    );
    t.target_at = Some(SunriseTime::all_day(day));
    assert_eq!(warnings(&t, &utc()), []);
    t.target_at = Some(SunriseTime::floating(day.at(23, 0, 0, 0)));
    assert_eq!(warnings(&t, &utc()), []);
    t.target_at = Some(SunriseTime::all_day(day.tomorrow().unwrap()));
    assert_eq!(warnings(&t, &utc()), [OrderingWarning::TargetAfterHardDue]);
    // With no hard deadline there is nothing to be after.
    t.hard_due_at = None;
    assert_eq!(warnings(&t, &utc()), []);
}

/// The draft and the invariants refuse no ordering at all.
#[test]
fn no_ordering_rejects_a_draft() {
    let now = Timestamp::from_millisecond(1_700_000_000_000).unwrap();
    let later = now + jiff::SignedDuration::from_hours(1);
    let d = TaskDraft {
        title: "x".into(),
        planned_at: Some(later.into()),
        target_at: Some(later.into()),
        hard_due_at: Some(now.into()),
        ..Default::default()
    };
    d.validate(&utc()).unwrap();
    assert_eq!(
        OrderingWarning::TargetAfterHardDue.as_str(),
        "target_after_hard_due"
    );
    assert_eq!(
        OrderingWarning::PlannedAfterHardDue.as_str(),
        "planned_after_hard_due"
    );
}

/// The Inbox predicate reads all three times.
#[test]
fn any_of_the_three_times_takes_a_task_out_of_the_inbox() {
    let mut t = timed(Timestamp::UNIX_EPOCH.into(), Timestamp::UNIX_EPOCH.into());
    t.planned_at = None;
    t.hard_due_at = None;
    assert!(t.in_inbox());
    t.target_at = Some(SunriseTime::all_day(jiff::civil::date(2026, 6, 1)));
    assert!(!t.in_inbox());
    assert!(t.has_deadline());
    t.target_at = None;
    t.hard_due_at = Some(Timestamp::UNIX_EPOCH.into());
    assert!(!t.in_inbox());
    t.hard_due_at = None;
    t.planned_at = Some(Timestamp::UNIX_EPOCH.into());
    assert!(!t.in_inbox());
    t.planned_at = None;
    t.archived = true;
    assert!(!t.in_inbox() && !t.is_open());
    t.archived = false;
    t.deleted = true;
    assert!(!t.is_open());
    t.deleted = false;
    t.state = TaskState::Done;
    assert!(!t.is_open());
    t.state = TaskState::from_raw("waiting");
    assert!(t.is_open(), "an unknown state reads as todo");
}

#[test]
fn task_state_transitions() {
    assert!(TaskState::Todo.can_transition_to(&TaskState::InProgress));
    assert!(TaskState::InProgress.can_transition_to(&TaskState::Done));
    assert!(TaskState::Done.can_transition_to(&TaskState::Todo));
    assert!(TaskState::Cancelled.can_transition_to(&TaskState::Todo));
}

#[test]
fn an_unknown_state_transitions_as_todo_and_can_be_restored() {
    let unknown = TaskState::from_raw("archived");
    // Out of it: as `todo` would.
    assert!(unknown.can_transition_to(&TaskState::InProgress));
    assert!(unknown.can_transition_to(&TaskState::Done));
    // Back into it, as an undo restoring it verbatim does.
    assert!(TaskState::Done.can_transition_to(&unknown));
    assert!(unknown.can_transition_to(&unknown.clone()));
}

#[test]
fn draft_validates_title() {
    let mut d = TaskDraft {
        title: "  do the thing  ".into(),
        stream_id: Some(ref_t()),
        ..Default::default()
    };
    d.validate(&utc()).unwrap();
    d.title = String::new();
    assert_eq!(d.validate(&utc()), Err(ValidationError::InvalidTitle));
}

/// A time kind this build cannot place is compared with nothing, on
/// either side.
#[test]
fn an_unplaceable_time_kind_never_warns() {
    let now = Timestamp::from_millisecond(1_700_000_000_000).unwrap();
    let unknown = crate::SunriseTime::Unknown {
        kind: "lunar".into(),
        raw: crate::Unknowns::new(),
    };
    for (planned_at, hard_due_at) in [(unknown.clone(), now.into()), (now.into(), unknown.clone())]
    {
        let mut t = timed(planned_at, hard_due_at);
        t.target_at = Some(unknown.clone());
        assert_eq!(warnings(&t, &utc()), []);
    }
}

#[test]
fn draft_rejects_priority_out_of_range() {
    let d = TaskDraft {
        title: "x".into(),
        priority: Some(6),
        ..Default::default()
    };
    assert!(matches!(
        d.validate(&utc()),
        Err(ValidationError::Field {
            constraint: "range_1_5",
            ..
        })
    ));
}
