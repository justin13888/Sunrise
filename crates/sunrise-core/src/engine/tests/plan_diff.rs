//! The planner's seam: a preview is solved from the vault, and a commit
//! writes exactly the previewed diff or refuses (`docs/08-features/planner.md`,
//! #343).

use super::*;
use crate::engine::PlanError;
use sunrise_domain::planner::{PlanIntent, PlanPosition, SlotTarget};

const NOW_MS: u64 = 1_700_000_000_000;

fn at(h: i8, m: i8) -> SunriseTime {
    SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(h, m, 0, 0))
}

fn planned_task(e: &Engine, db: &mut Db, title: &str, start: SunriseTime, min: u64) -> EntityRef {
    task_with(
        e,
        db,
        title,
        TaskDraft {
            planned_at: Some(start),
            estimated_duration_s: Some(min * 60),
            ..Default::default()
        },
    )
}

fn block(e: &Engine, db: &mut Db, hour: i8) -> EntityRef {
    e.apply(
        db,
        Command::CreateBlock(block_draft(hour, 1, Some("standup"))),
    )
    .unwrap()
    .entity
}

fn read_block_row(db: &Db, id: EntityRef) -> sunrise_domain::Block {
    super::super::block::read_block(db.conn(), id.bytes())
        .unwrap()
        .unwrap()
}

fn move_to_eleven(b: EntityRef) -> PlanIntent {
    PlanIntent::MoveBlock {
        block: b,
        to_start: at(11, 0),
    }
}

#[test]
fn a_committed_preview_writes_exactly_its_moves_and_previews_again_as_empty() {
    let e = engine();
    let mut db = db();
    let b = block(&e, &mut db, 9);
    let t = planned_task(&e, &mut db, "write", at(11, 0), 30);

    let diff = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();
    assert_eq!(diff.moves.len(), 2, "{diff:#?}");
    assert!(diff.committable);
    let moved = e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    assert_eq!(moved, 2);

    let written = read_block_row(&db, b);
    assert_eq!(written.starts_at, at(11, 0));
    assert_eq!(written.ends_at, at(12, 0));
    // The task kept its floating kind and took the nearest free slot.
    assert_eq!(task_of(&e, &db, t).planned_at, Some(at(10, 30)));

    let again = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();
    assert!(again.is_empty(), "{again:#?}");
}

#[test]
fn a_changed_input_refuses_the_commit_as_stale_and_writes_nothing() {
    let e = engine();
    let mut db = db();
    let b = block(&e, &mut db, 9);
    let t = planned_task(&e, &mut db, "write", at(11, 0), 30);
    let diff = e.plan_preview(&db, &move_to_eleven(b), NOW_MS).unwrap();

    e.apply(
        &mut db,
        Command::UpdateTask {
            id: t,
            patch: TaskPatch {
                estimated_duration_s: Some(Some(45 * 60)),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let ops = op_count(&db);
    assert!(matches!(
        e.plan_apply(&mut db, &diff, NOW_MS),
        Err(PlanError::Stale)
    ));
    assert_eq!(op_count(&db), ops, "a stale commit wrote nothing");
    assert_eq!(read_block_row(&db, b).starts_at, at(9, 0));
}

#[test]
fn a_bulk_defer_counts_a_deferral_on_each_task_and_moves_nothing_else() {
    let e = engine();
    let mut db = db();
    let a = planned_task(&e, &mut db, "a", at(9, 0), 30);
    let c = planned_task(&e, &mut db, "c", at(10, 0), 30);
    let to = jiff::civil::date(2026, 3, 5);
    let intent = PlanIntent::BulkDefer {
        tasks: vec![a, c],
        to,
        place: false,
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert_eq!(e.plan_apply(&mut db, &diff, NOW_MS).unwrap(), 2);
    for id in [a, c] {
        let task = task_of(&e, &db, id);
        assert_eq!(task.planned_at, Some(SunriseTime::all_day(to)));
        assert_eq!(task.deferred_count, 1);
        assert_eq!(task.hard_due_at, None);
    }
}

#[test]
fn a_task_moved_to_a_day_is_committed_date_only_and_not_deferred() {
    let e = engine();
    let mut db = db();
    let t = planned_task(&e, &mut db, "write", at(9, 0), 30);
    let to = jiff::civil::date(2026, 3, 6);
    let intent = PlanIntent::MoveTask {
        task: t,
        to: SlotTarget::Day(to),
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert_eq!(diff.moves.len(), 1, "{diff:#?}");
    assert_eq!(diff.moves[0].after, PlanPosition::Day(to));
    assert!(diff.committable);
    assert_eq!(e.plan_apply(&mut db, &diff, NOW_MS).unwrap(), 1);
    let task = task_of(&e, &db, t);
    assert_eq!(task.planned_at, Some(SunriseTime::all_day(to)));
    // Only a bulk defer counts a deferral.
    assert_eq!(task.deferred_count, 0);
    assert!(e.plan_preview(&db, &intent, NOW_MS).unwrap().is_empty());
}

#[test]
fn a_placing_bulk_defer_writes_a_slot_on_the_day_and_counts_a_deferral() {
    let e = engine();
    let mut db = db();
    let a = planned_task(&e, &mut db, "a", at(9, 0), 30);
    let c = planned_task(&e, &mut db, "c", at(10, 0), 30);
    let to = jiff::civil::date(2026, 3, 5);
    let intent = PlanIntent::BulkDefer {
        tasks: vec![a, c],
        to,
        place: true,
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert_eq!(diff.moves.len(), 2, "{diff:#?}");
    assert!(diff.unscheduled.is_empty(), "{diff:#?}");
    assert_eq!(e.plan_apply(&mut db, &diff, NOW_MS).unwrap(), 2);
    let utc = jiff::tz::TimeZone::UTC;
    let mut starts = Vec::new();
    for id in [a, c] {
        let moved = diff.moves.iter().find(|m| m.item == id).unwrap();
        let PlanPosition::Slot { start, .. } = &moved.after else {
            panic!("{diff:#?}");
        };
        let task = task_of(&e, &db, id);
        // Exactly the previewed slot, written through the deferral.
        assert_eq!(task.planned_at.as_ref(), Some(start));
        assert_eq!(task.deferred_count, 1);
        let instant = start.resolve_in(&utc).unwrap();
        assert_eq!(instant.to_zoned(utc.clone()).date(), to);
        starts.push(instant);
    }
    // In the order named: `c` takes the next free slot after `a`'s.
    assert!(starts[0] + jiff::SignedDuration::from_mins(30) <= starts[1]);
}

#[test]
fn a_drop_outside_a_hard_window_is_refused_at_commit() {
    let e = engine();
    let mut db = db();
    let t = task_with(
        &e,
        &mut db,
        "mornings only",
        TaskDraft {
            planned_at: Some(at(9, 0)),
            scheduling_constraints: vec![ScheduleConstraint {
                time_of_day: Some(sunrise_domain::TimeOfDayRange::new(
                    jiff::civil::time(8, 0, 0, 0),
                    jiff::civil::time(12, 0, 0, 0),
                )),
                days_of_week: sunrise_domain::WeekdaySet::default(),
                date_range: None,
                severity: sunrise_domain::ConstraintSeverity::Hard,
                unknown: Unknowns::default(),
            }],
            ..Default::default()
        },
    );
    let intent = PlanIntent::MoveTask {
        task: t,
        to: SlotTarget::At(at(15, 0)),
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    assert!(!diff.committable);
    assert!(matches!(
        e.plan_apply(&mut db, &diff, NOW_MS),
        Err(PlanError::NotCommittable)
    ));
    assert_eq!(task_of(&e, &db, t).planned_at, Some(at(9, 0)));
}

#[test]
fn an_auto_place_slots_unplanned_tasks_into_the_first_free_time() {
    let e = engine();
    let mut db = db();
    block(&e, &mut db, 0);
    let t = task_with(&e, &mut db, "loose", TaskDraft::default());
    let intent = PlanIntent::AutoPlace {
        tasks: vec![t],
        day: jiff::civil::date(2026, 3, 4),
        not_before: None,
    };
    let diff = e.plan_preview(&db, &intent, NOW_MS).unwrap();
    let PlanPosition::Slot { start, .. } = &diff.moves[0].after else {
        panic!("{diff:#?}");
    };
    // The reader's zone is UTC; the block holds midnight to 01:00.
    assert_eq!(
        start.resolve_in(&jiff::tz::TimeZone::UTC),
        at(1, 0).resolve_in(&jiff::tz::TimeZone::UTC)
    );
    e.plan_apply(&mut db, &diff, NOW_MS).unwrap();
    assert!(task_of(&e, &db, t).planned_at.is_some());
}

#[test]
fn an_intent_naming_nothing_is_invalid() {
    let e = engine();
    let db = db();
    let ghost = EntityRef::new(EntityKind::Task, [7; 16]);
    let intent = PlanIntent::MoveTask {
        task: ghost,
        to: SlotTarget::At(at(9, 0)),
    };
    assert!(matches!(
        e.plan_preview(&db, &intent, NOW_MS),
        Err(PlanError::Invalid(_))
    ));
}
