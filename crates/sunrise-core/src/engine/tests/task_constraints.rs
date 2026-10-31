//! A task's scheduling constraints and what they gate.

use super::*;

#[test]
fn create_task_with_constraints_round_trips() {
    let mut db = db();
    let e = engine();
    let c = sample_constraint();
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "with constraint".into(),
                scheduling_constraints: vec![c.clone()],
                ..Default::default()
            }),
        )
        .unwrap();
    let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert_eq!(t.scheduling_constraints, vec![c]);
}

#[test]
fn update_task_replaces_whole_constraint_list() {
    let mut db = db();
    let e = engine();
    let c1 = sample_constraint();
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                scheduling_constraints: vec![c1],
                ..Default::default()
            }),
        )
        .unwrap();
    // Replace with a different, single-element list.
    let c2 = ScheduleConstraint {
        time_of_day: None,
        days_of_week: sunrise_domain::WeekdaySet::from_days([sunrise_domain::Weekday::Sa]),
        date_range: None,
        severity: sunrise_domain::ConstraintSeverity::Soft,
        unknown: Unknowns::new(),
    };
    let patch = TaskPatch {
        scheduling_constraints: Some(vec![c2.clone()]),
        ..Default::default()
    };
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: r.entity,
            patch,
        },
    )
    .unwrap();
    let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert_eq!(t.scheduling_constraints, vec![c2]);

    // Clearing to empty persists as NULL / empty list.
    let clear = TaskPatch {
        scheduling_constraints: Some(vec![]),
        ..Default::default()
    };
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: r.entity,
            patch: clear,
        },
    )
    .unwrap();
    let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert!(t.scheduling_constraints.is_empty());
}

#[test]
fn a_deadline_patched_before_the_plan_is_kept_and_warned_about() {
    let mut db = db();
    let e = engine();
    let now = 1_700_000_000_000i64;
    let scheduled = ms_to_ts(now + 2 * 3_600_000);
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                planned_at: Some(scheduled.into()),
                ..Default::default()
            }),
        )
        .unwrap();
    // A patch that sets ONLY the hard deadline, earlier than the plan. The
    // ordering is expected, not enforced (ADR-0047 §1): the write stands,
    // and the result says which ordering it breaks.
    let earlier = ms_to_ts(now + 3_600_000);
    let patch = TaskPatch {
        hard_due_at: Some(Some(earlier.into())),
        ..Default::default()
    };
    let res = e
        .apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch,
            },
        )
        .expect("an ordering never rejects");
    assert_eq!(
        res.ordering_warnings,
        [sunrise_domain::OrderingWarning::PlannedAfterHardDue]
    );
    let QueryResult::Task(t) = e.query(&db, Query::EntityById(r.entity)).unwrap() else {
        panic!("task");
    };
    assert_eq!(t.hard_due_at, Some(earlier.into()));
    // A soft deadline after the hard one is the other ordering.
    let res = e
        .apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch: TaskPatch {
                    target_at: Some(Some(scheduled.into())),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    assert_eq!(
        res.ordering_warnings,
        [
            sunrise_domain::OrderingWarning::TargetAfterHardDue,
            sunrise_domain::OrderingWarning::PlannedAfterHardDue
        ]
    );
    // An edit that moves none of the three times re-raises nothing.
    let res = e
        .apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch: TaskPatch {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    assert!(res.ordering_warnings.is_empty());
}

#[test]
fn invalid_constraint_list_rejected_on_create_and_update() {
    let mut db = db();
    let e = engine();
    // An empty (no-dimension) constraint is invalid.
    let bad = ScheduleConstraint {
        time_of_day: None,
        days_of_week: sunrise_domain::WeekdaySet::new(),
        date_range: None,
        severity: sunrise_domain::ConstraintSeverity::Hard,
        unknown: Unknowns::new(),
    };
    let create = e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "x".into(),
            scheduling_constraints: vec![bad.clone()],
            ..Default::default()
        }),
    );
    assert!(matches!(create, Err(EngineError::Validation(_))));

    // A valid task, then an update that introduces the invalid list.
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "ok".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let patch = TaskPatch {
        scheduling_constraints: Some(vec![bad]),
        ..Default::default()
    };
    let update = e.apply(
        &mut db,
        Command::UpdateTask {
            id: r.entity,
            patch,
        },
    );
    assert!(matches!(update, Err(EngineError::Validation(_))));
}

// ---- scheduling-constraint enforcement ----

#[test]
fn scheduling_against_a_hard_constraint_is_rejected_on_create() {
    let mut db = db();
    let e = engine();
    let err = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "deploy at 10pm".into(),
                planned_at: Some(ms_to_ts(OUTSIDE_WINDOW_MS).into()),
                scheduling_constraints: vec![sample_constraint()],
                ..Default::default()
            }),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::Validation(sunrise_domain::ValidationError::HardScheduleConstraint)
        ),
        "got {err:?}"
    );
    // Nothing was written.
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);

    // The same constraint at an instant inside the window passes.
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "deploy at noon".into(),
            planned_at: Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
            scheduling_constraints: vec![sample_constraint()],
            ..Default::default()
        }),
    )
    .unwrap();
}

#[test]
fn a_soft_violation_is_surfaced_rather_than_blocking() {
    let mut db = db();
    let e = engine();
    let soft = soft_9_to_5();
    let res = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "deploy at 10pm".into(),
                planned_at: Some(ms_to_ts(OUTSIDE_WINDOW_MS).into()),
                scheduling_constraints: vec![soft.clone()],
                ..Default::default()
            }),
        )
        .unwrap();
    assert_eq!(
        res.soft_violations,
        vec![soft],
        "a soft violation never blocks, but it must not vanish either"
    );
    // Inside the window there is nothing to report.
    let res = e
        .apply(
            &mut db,
            Command::UpdateTask {
                id: res.entity,
                patch: TaskPatch {
                    planned_at: Some(Some(ms_to_ts(INSIDE_WINDOW_MS).into())),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    assert!(res.soft_violations.is_empty());
}

#[test]
fn rescheduling_and_deferring_clear_the_same_hard_gate() {
    let mut db = db();
    let e = engine();
    let id = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "deploy".into(),
                planned_at: Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
                scheduling_constraints: vec![sample_constraint()],
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;

    // Moving it out of the window is rejected...
    assert!(matches!(
        e.apply(
            &mut db,
            Command::UpdateTask {
                id,
                patch: TaskPatch {
                    planned_at: Some(Some(ms_to_ts(OUTSIDE_WINDOW_MS).into())),
                    ..Default::default()
                },
            },
        ),
        Err(EngineError::Validation(
            sunrise_domain::ValidationError::HardScheduleConstraint
        ))
    ));
    // ... and so is deferring into it, since deferring *is* scheduling.
    assert!(matches!(
        e.apply(
            &mut db,
            Command::DeferTask {
                id,
                to_ms: OUTSIDE_WINDOW_MS as u64,
            },
        ),
        Err(EngineError::Validation(
            sunrise_domain::ValidationError::HardScheduleConstraint
        ))
    ));
    assert_eq!(
        read_task_t(&e, &db, id).planned_at,
        Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
        "both rejections left the schedule untouched"
    );
}

#[test]
fn an_unrelated_edit_is_not_re_gated_by_constraints() {
    // The rule is "fails validation when the user *schedules* against it",
    // not "every write to a task that already sits outside its window is
    // rejected" — otherwise a task could become uneditable.
    let mut db = db();
    let e = engine();
    let id = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "deploy".into(),
                planned_at: Some(ms_to_ts(INSIDE_WINDOW_MS).into()),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    // Attach a hard constraint the current schedule already violates by
    // moving the schedule and the list in one go is rejected; here we
    // instead rename a task whose stored list is violated.
    db.conn()
        .execute(
            "UPDATE tasks SET planned_at_ms = ?, scheduling_constraints = ? WHERE id = ?",
            params![
                OUTSIDE_WINDOW_MS,
                encode_constraints(&[sample_constraint()]).unwrap(),
                id.bytes().to_vec()
            ],
        )
        .unwrap();
    e.apply(
        &mut db,
        Command::UpdateTask {
            id,
            patch: TaskPatch {
                title: Some("deploy (renamed)".into()),
                ..Default::default()
            },
        },
    )
    .expect("a rename must not be blocked by an already-violated window");
    assert!(e.apply(&mut db, Command::CompleteTask(id)).is_ok());
}
