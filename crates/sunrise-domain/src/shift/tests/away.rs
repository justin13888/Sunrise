//! "While you were away": what accrued during a shift (ADR-0073 §5).

use super::*;

fn task_at(b: u8, created: Timestamp) -> Task {
    Task::from_draft(
        eref(EntityKind::Task, b),
        created,
        TaskDraft {
            title: format!("task {b}"),
            ..TaskDraft::default()
        },
    )
}

#[test]
fn while_you_were_away_lists_missed_occurrences_deadlines_and_captures() {
    let (mut unchanged, shifts, _) = tokyo_trip();
    let trip = shifts[0].1.clone();
    let root = unchanged.series_root();
    // The 10th was completed on time, the 11th done late, the 12th dropped.
    unchanged.streak_keys = vec!["2026-06-10T09:00".into()];
    let mut done = task_at(0x11, utc(2026, 6, 1, 0, 0));
    done.id = occurrence_task_id(&root, "2026-06-11T09:00");
    done.routine_id = Some(unchanged.id);
    done.state = TaskState::Done;
    let mut dropped = task_at(0x12, utc(2026, 6, 1, 0, 0));
    dropped.id = occurrence_task_id(&root, "2026-06-12T09:00");
    dropped.routine_id = Some(unchanged.id);
    dropped.state = TaskState::Cancelled;
    // The 13th materialized and is still open; the 14th never did.
    let mut open_occ = task_at(0x13, utc(2026, 6, 11, 0, 0));
    open_occ.id = occurrence_task_id(&root, "2026-06-13T09:00");
    open_occ.routine_id = Some(unchanged.id);

    let mut paused = daily("UTC", date(2026, 6, 1).at(7, 0, 0, 0), false);
    paused.id = eref(EntityKind::Routine, 2);

    let mut due = task_at(0x21, utc(2026, 6, 1, 0, 0));
    due.hard_due_at = Some(SunriseTime::instant(utc(2026, 6, 12, 12, 0)));
    let mut due_done = task_at(0x22, utc(2026, 6, 1, 0, 0));
    due_done.hard_due_at = Some(SunriseTime::instant(utc(2026, 6, 12, 12, 0)));
    due_done.state = TaskState::Done;
    let mut due_later = task_at(0x23, utc(2026, 6, 1, 0, 0));
    due_later.hard_due_at = Some(SunriseTime::instant(utc(2026, 6, 18, 12, 0)));
    let mut due_first = task_at(0x24, utc(2026, 6, 1, 0, 0));
    due_first.hard_due_at = Some(SunriseTime::instant(utc(2026, 6, 10, 1, 0)));

    let captured_late = task_at(0x31, utc(2026, 6, 13, 0, 0));
    let captured = task_at(0x32, utc(2026, 6, 11, 0, 0));
    let before = task_at(0x33, utc(2026, 6, 9, 0, 0));
    let mut deleted = task_at(0x34, utc(2026, 6, 11, 0, 0));
    deleted.deleted = true;

    let routines = [
        (unchanged.clone(), ShiftPolicy::Unchanged),
        (paused, ShiftPolicy::Pause),
    ];
    let tasks = [
        done,
        dropped,
        open_occ,
        due,
        due_done,
        due_later,
        due_first,
        captured_late,
        captured,
        before,
        deleted,
    ];
    let now = utc(2026, 6, 20, 0, 0);
    let s = away_summary(&trip, now, &routines, &tasks, &TimeZone::UTC).unwrap();
    assert_eq!(s.window, Some((trip.starts_at, trip.ends_at.unwrap())));
    assert_eq!(
        s.missed,
        vec![
            MissedOccurrence {
                routine: unchanged.id,
                key: "2026-06-13T09:00".into(),
                at: utc(2026, 6, 13, 16, 0),
            },
            MissedOccurrence {
                routine: unchanged.id,
                key: "2026-06-14T09:00".into(),
                at: utc(2026, 6, 14, 16, 0),
            },
        ]
    );
    assert_eq!(
        s.deadlines_passed,
        vec![eref(EntityKind::Task, 0x24), eref(EntityKind::Task, 0x21)]
    );
    assert_eq!(
        s.captured,
        vec![eref(EntityKind::Task, 0x32), eref(EntityKind::Task, 0x31)]
    );

    // Read while the shift still runs, the window ends at the read.
    let mut running = trip.clone();
    running.ends_at = None;
    let at = utc(2026, 6, 12, 0, 0);
    let s = away_summary(&running, at, &routines, &tasks, &TimeZone::UTC).unwrap();
    assert_eq!(s.window, Some((trip.starts_at, at)));
    assert!(s.missed.is_empty(), "the 10th is keyed and the 11th done");
    assert_eq!(s.captured, vec![eref(EntityKind::Task, 0x32)]);

    // A shift that has not started yet has nothing to summarise.
    let s = away_summary(
        &trip,
        utc(2026, 6, 9, 0, 0),
        &routines,
        &tasks,
        &TimeZone::UTC,
    )
    .unwrap();
    assert_eq!(s, AwaySummary::default());
}
