//! The planner's ranking and the session each row would open.

use super::*;

// ---- the planner ----

#[test]
fn planner_prefers_the_high_leverage_actionable_task_at_matching_energy() {
    let mut db = db();
    let (e, _clock) = focus_engine(0);
    // Two actionable High-energy tasks. `hub` releases two dependents;
    // `chore` releases nothing.
    let hub = task_with(
        &e,
        &mut db,
        "build the thing",
        TaskDraft {
            energy: Some(Energy::High),
            ..Default::default()
        },
    );
    let chore = task_with(
        &e,
        &mut db,
        "tidy the desk",
        TaskDraft {
            energy: Some(Energy::High),
            ..Default::default()
        },
    );
    for title in ["deploy", "qa"] {
        let dep = task_with(&e, &mut db, title, TaskDraft::default());
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: dep,
                patch: TaskPatch {
                    blocked_by: Some(vec![hub]),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    }

    let rows = plan(&e, &db, Some(Energy::High));
    assert_eq!(rows[0].task.id, hub, "leverage picks the hub: {rows:#?}");
    assert_eq!(rows[0].unblocks, 2);
    assert_eq!(rows[0].energy_fit, sunrise_domain::EnergyFit::Exact);
    assert!(rows.iter().any(|r| r.task.id == chore));

    // The two blocked dependents never appear: the planner is not a dead
    // end.
    assert_eq!(
        rows.len(),
        2,
        "only the two actionable tasks are proposed: {rows:#?}"
    );
}

#[test]
fn planner_matches_energy_before_leverage() {
    let mut db = db();
    let (e, _clock) = focus_engine(0);
    let deep = task_with(
        &e,
        &mut db,
        "rewrite the parser",
        TaskDraft {
            energy: Some(Energy::High),
            ..Default::default()
        },
    );
    let shallow = task_with(
        &e,
        &mut db,
        "file receipts",
        TaskDraft {
            energy: Some(Energy::Low),
            ..Default::default()
        },
    );
    // Give the deep task real leverage so only the energy budget can
    // demote it.
    let dep = task_with(&e, &mut db, "downstream", TaskDraft::default());
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: dep,
            patch: TaskPatch {
                blocked_by: Some(vec![deep]),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // Depleted: the low-energy task wins despite zero leverage.
    let rows = plan(&e, &db, Some(Energy::Low));
    assert_eq!(rows[0].task.id, shallow, "{rows:#?}");
    // Fresh: leverage takes over again.
    let rows = plan(&e, &db, Some(Energy::High));
    assert_eq!(rows[0].task.id, deep, "{rows:#?}");
    // No declared budget: energy carries no signal, leverage decides.
    let rows = plan(&e, &db, None);
    assert_eq!(rows[0].task.id, deep, "{rows:#?}");
}

#[test]
fn planner_rows_carry_the_session_it_would_open() {
    let mut db = db();
    let (e, _clock) = focus_engine(0);
    let task = task_with(
        &e,
        &mut db,
        "big one",
        TaskDraft {
            estimated_duration_s: Some(75 * 60),
            ..Default::default()
        },
    );
    let rows = plan(&e, &db, None);
    let row = rows.iter().find(|r| r.task.id == task).unwrap();
    assert_eq!(row.prior_sessions, 0);
    assert_eq!(row.suggested.planned_ms, Some(POMODORO_MS));
    assert_eq!(row.suggested.chunk, Some(Chunk::new(1, 3)));

    // After one sitting the proposal advances the chunk index.
    let s = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
    e.apply(
        &mut db,
        Command::EndFocus {
            session: s,
            actual_focused_ms: Some(POMODORO_MS),
            completed_task: false,
        },
    )
    .unwrap();
    let rows = plan(&e, &db, None);
    let row = rows.iter().find(|r| r.task.id == task).unwrap();
    assert_eq!(row.prior_sessions, 1);
    assert_eq!(row.suggested.chunk, Some(Chunk::new(2, 3)));
}
