//! `changed_task_fields` reports each tracked field under its own name.
//!
//! This lives in `tests/` rather than beside the function for two reasons.
//! It needs nothing private — `TRACKED_TASK_FIELDS` and `changed_task_fields`
//! are both public, and a test that exercises only the public surface is one
//! the next refactor cannot quietly invalidate. And `activity.rs` sits at its
//! package's file-length threshold, so a ninety-line table belongs outside it
//! (`.github/scripts/file-size-gate.py`).

use std::collections::BTreeSet;

use jiff::Timestamp;
use sunrise_domain::activity::{changed_task_fields, TRACKED_TASK_FIELDS};
use sunrise_domain::common::{Energy, NoteBody};
use sunrise_domain::constraint::{ConstraintSeverity, ScheduleConstraint, WeekdaySet};
use sunrise_domain::task::{Task, TaskState};
use sunrise_domain::time::SunriseTime;
use sunrise_domain::unknown::Unknowns;
use sunrise_id::{EntityKind, EntityRef};

const T0: u64 = 1_700_000_000_000;

fn ts(ms: u64) -> Timestamp {
    Timestamp::from_millisecond(i64::try_from(ms).unwrap()).unwrap()
}

fn eref(kind: EntityKind, n: u8) -> EntityRef {
    let mut raw = [0u8; 16];
    raw[15] = n;
    EntityRef::new(kind, raw)
}

fn task() -> Task {
    Task {
        reminder_lead_s: None,
        id: eref(EntityKind::Task, 1),
        created_at: ts(T0),
        updated_at: ts(T0),
        title: "Ship it".into(),
        body: None,
        stream_id: eref(EntityKind::Stream, 2),
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
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

/// One isolated edit: the tracked field's name, and a mutation that touches
/// that field and no other.
type FieldMutation = (&'static str, fn(&mut Task));

/// Each name in `TRACKED_TASK_FIELDS` is paired with the comparison that
/// actually observes that field.
///
/// `changed_task_fields` joins the name array to a parallel `[bool; N]` with
/// `zip`, which pairs by position. The array length is compiler-enforced; the
/// correspondence is not. Permuting two comparisons — or writing
/// `prev.due_at != next.due_at` where `scheduled_at`'s name sits — compiles
/// cleanly and makes the activity feed name the wrong edit.
///
/// So: change exactly one field, and require exactly that field's name back.
/// Fifteen mutations for fifteen names, which is the only shape that pins the
/// mapping rather than the count.
#[test]
fn each_tracked_field_is_reported_under_its_own_name() {
    let base = task();

    let mutations: Vec<FieldMutation> = vec![
        ("title", |t| t.title = "Ship it properly".into()),
        ("body", |t| t.body = Some(NoteBody(b"notes".to_vec()))),
        ("stream_id", |t| t.stream_id = eref(EntityKind::Stream, 9)),
        ("contexts", |t| {
            t.contexts.insert(eref(EntityKind::Context, 3));
        }),
        ("state", |t| t.state = TaskState::Done),
        ("priority", |t| t.priority = Some(2)),
        ("energy", |t| t.energy = Some(Energy::High)),
        ("estimated_duration_s", |t| {
            t.estimated_duration_s = Some(900);
        }),
        ("scheduled_at", |t| {
            t.scheduled_at = Some(SunriseTime::Instant {
                at: ts(T0 + 60_000),
            });
        }),
        ("due_at", |t| {
            t.due_at = Some(SunriseTime::Instant {
                at: ts(T0 + 120_000),
            });
        }),
        ("blocked_by", |t| {
            t.blocked_by.insert(eref(EntityKind::Task, 7));
        }),
        ("assignee", |t| {
            t.assignee = Some(eref(EntityKind::Person, 4));
        }),
        ("archived", |t| t.archived = true),
        ("deferred_count", |t| t.deferred_count = 1),
    ];

    for (name, mutate) in &mutations {
        let mut next = base.clone();
        mutate(&mut next);
        assert_eq!(
            changed_task_fields(&base, &next),
            vec![*name],
            "changing `{name}` must report `{name}` and nothing else"
        );
    }

    // `scheduling_constraints` is the fifteenth. It is driven separately
    // because it needs a real `ScheduleConstraint` rather than a scalar, and a
    // closure list of one shape reads worse than naming the exception.
    let mut next = base.clone();
    next.scheduling_constraints = vec![ScheduleConstraint {
        time_of_day: None,
        days_of_week: WeekdaySet::default(),
        date_range: None,
        severity: ConstraintSeverity::Hard,
    }];
    assert_eq!(
        changed_task_fields(&base, &next),
        vec!["scheduling_constraints"]
    );

    // The mutation list plus that one covers every tracked name, so a field
    // added to the constant without a case here fails the count rather than
    // passing unnoticed.
    assert_eq!(
        mutations.len() + 1,
        TRACKED_TASK_FIELDS.len(),
        "every tracked field needs a mutation that isolates it"
    );

    // A no-op edit reports nothing, so the assertions above are the
    // comparisons firing and not a function that always answers.
    assert!(changed_task_fields(&base, &base.clone()).is_empty());
}
