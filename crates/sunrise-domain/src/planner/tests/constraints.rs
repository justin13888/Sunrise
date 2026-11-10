//! A task's hard time-of-day window: where a drop, a ripple and an
//! unschedule may put it, and when a broken one refuses the diff.

use super::*;

fn hard_window(start: i8, end: i8) -> ScheduleConstraint {
    ScheduleConstraint {
        time_of_day: Some(TimeOfDayRange::new(
            time(start, 0, 0, 0),
            time(end, 0, 0, 0),
        )),
        days_of_week: crate::constraint::WeekdaySet::default(),
        date_range: None,
        severity: ConstraintSeverity::Hard,
        unknown: crate::unknown::Unknowns::default(),
    }
}

#[test]
fn a_drop_outside_a_hard_time_of_day_window_is_refused() {
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 30);
    t.constraints.push(hard_window(9, 12));
    i.tasks.push(t);
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 14, 0)),
        },
    )
    .unwrap();
    assert!(!diff.committable);
    assert!(diff
        .violations
        .iter()
        .any(|v| v.rule == PlanRule::OutsideTimeOfDay && v.severity == PlanSeverity::Blocking));
}

#[test]
fn a_rippled_task_with_a_hard_window_moves_only_inside_it() {
    // The task may only start 09:00–10:00; the subject takes 09:00–09:30.
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 30);
    t.constraints.push(hard_window(9, 10));
    i.tasks.push(t);
    i.blocks.push(pblock(1, at(D, 14, 0), at(D, 14, 30), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 9, 0),
        },
    )
    .unwrap();
    let after = slot_of(&mv(&diff, tid(1)).unwrap().after);
    assert_eq!(after, (ts(D, 9, 30), ts(D, 10, 0)), "{diff:#?}");
    assert!(diff.violations.is_empty());
}

#[test]
fn a_rippled_task_with_no_lawful_position_stays_where_it_is() {
    // The window 09:00–10:00 is all the subject's, and the no-time lane
    // breaks it too (an all-day plan is checked at the start of its day, as
    // an ordinary submit checks it), so the task stays and the overlap is
    // reported rather than a write the core would refuse.
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 60);
    t.constraints.push(hard_window(9, 10));
    i.tasks.push(t);
    i.blocks.push(pblock(1, at(D, 14, 0), at(D, 15, 0), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 9, 0),
        },
    )
    .unwrap();
    assert!(mv(&diff, tid(1)).is_none(), "{diff:#?}");
    assert!(diff.unscheduled.is_empty());
    assert!(diff.committable);
    assert!(diff
        .violations
        .iter()
        .any(|v| v.item == tid(1) && v.rule == PlanRule::Overlaps && v.introduced));
}

#[test]
fn a_rippled_task_that_would_miss_its_hard_deadline_is_unscheduled() {
    // Due 09:30, and the subject takes 07:00–10:00: every free slot is late.
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 30);
    t.hard_due = Some(ts(D, 9, 30));
    i.tasks.push(t);
    i.blocks.push(pblock(1, at(D, 14, 0), at(D, 17, 0), false));
    let diff = solve(
        &i,
        &PlanIntent::MoveBlock {
            block: bid(1),
            to_start: at(D, 7, 0),
        },
    )
    .unwrap();
    assert_eq!(diff.unscheduled.len(), 1, "{diff:#?}");
    assert_eq!(diff.unscheduled[0].item, tid(1));
    assert_eq!(diff.unscheduled[0].day, D);
    assert_eq!(
        diff.unscheduled[0].reason,
        UnscheduleReason::WouldViolateHard
    );
    assert!(diff.committable);
}

#[test]
fn unscheduling_onto_a_day_its_hard_window_excludes_is_refused() {
    let mut i = base_day();
    let mut t = ptask(1, Some(at(D, 9, 0)), 30);
    t.constraints.push(hard_window(9, 10));
    i.tasks.push(t);
    let diff = solve(
        &i,
        &PlanIntent::Unschedule {
            items: vec![tid(1)],
            keep: UnscheduleKeep::Day,
        },
    )
    .unwrap();
    assert!(!diff.committable, "{diff:#?}");
}

#[test]
fn a_hard_window_already_broken_elsewhere_does_not_refuse_an_unrelated_drop() {
    let mut i = base_day();
    // Task 1 may only start 09:00–12:00 and already sits at 14:00.
    let mut flagged = ptask(1, Some(at(D, 14, 0)), 30);
    flagged.constraints.push(hard_window(9, 12));
    i.tasks.push(flagged);
    i.tasks.push(ptask(2, Some(at(D, 9, 0)), 30));
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(2),
            to: SlotTarget::At(at(D, 16, 0)),
        },
    )
    .unwrap();
    let standing = diff
        .violations
        .iter()
        .find(|v| v.item == tid(1) && v.severity == PlanSeverity::Blocking)
        .expect("the broken window is still reported");
    assert!(!standing.introduced);
    assert!(diff.committable, "{diff:#?}");

    // Moving the flagged task itself to another slot outside its window
    // writes it, so the commit is answerable for it and refuses.
    let diff = solve(
        &i,
        &PlanIntent::MoveTask {
            task: tid(1),
            to: SlotTarget::At(at(D, 15, 0)),
        },
    )
    .unwrap();
    assert!(!diff.committable, "{diff:#?}");
}
