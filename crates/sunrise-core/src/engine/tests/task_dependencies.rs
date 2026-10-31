//! Task dependencies: derived `blocked`, the reverse index and the cascade.

use super::*;

// ---- task dependencies: derived `blocked` + the reverse index ----

#[test]
fn blocked_by_survives_the_materialized_projection() {
    // Before migration 0008 the `tasks` table had no blocker columns at
    // all: `blocked_by` lived only inside the op-log inner op, so every
    // read came back empty and nothing could derive `blocked` from it.
    let mut db = db();
    let e = engine();
    let blocker = new_task(&e, &mut db, "buy paint");
    let dependent = new_task(&e, &mut db, "paint the fence");
    set_blockers(&e, &mut db, dependent, vec![blocker]).unwrap();

    let t = read_task_t(&e, &db, dependent);
    assert_eq!(
        t.blocked_by,
        BTreeSet::from([blocker]),
        "blocked_by round-trips through the projection"
    );

    // Replacing the set replaces the index rows, it does not accumulate.
    set_blockers(&e, &mut db, dependent, vec![]).unwrap();
    assert!(read_task_t(&e, &db, dependent).blocked_by.is_empty());
}

#[test]
fn completing_a_blocker_flips_its_dependents_to_actionable() {
    let mut db = db();
    let e = engine();
    let blocker = new_task(&e, &mut db, "buy paint");
    let dependent = new_task(&e, &mut db, "paint the fence");
    set_blockers(&e, &mut db, dependent, vec![blocker]).unwrap();

    let rows = actionable_rows(&e, &db);
    let dep = row_for(&rows, dependent);
    assert_eq!(dep.effective_state, EffectiveTaskState::Blocked);
    assert_eq!(dep.open_blockers, 1);
    let blk = row_for(&rows, blocker);
    assert_eq!(blk.effective_state, EffectiveTaskState::Todo);
    assert_eq!(blk.unblocks, 1, "reverse index: it holds one task back");

    // The whole point: no unblock op, no repair pass — completing the
    // blocker is enough.
    e.apply(&mut db, Command::CompleteTask(blocker)).unwrap();

    let rows = actionable_rows(&e, &db);
    let dep = row_for(&rows, dependent);
    assert_eq!(dep.effective_state, EffectiveTaskState::Todo);
    assert_eq!(dep.open_blockers, 0);
    assert!(
        !rows.iter().any(|r| r.task.id == blocker),
        "a done blocker drops out of the open list"
    );
    assert_eq!(
        read_task_t(&e, &db, dependent).blocked_by,
        BTreeSet::from([blocker]),
        "the edge itself is kept; only its effect went away"
    );
}

#[test]
fn a_cancelled_or_deleted_blocker_also_releases_its_dependents() {
    let mut db = db();
    let e = engine();
    let cancelled = new_task(&e, &mut db, "wait for legal");
    let deleted = new_task(&e, &mut db, "chase invoice");
    let dependent = new_task(&e, &mut db, "ship it");
    set_blockers(&e, &mut db, dependent, vec![cancelled, deleted]).unwrap();
    assert_eq!(
        row_for(&actionable_rows(&e, &db), dependent).open_blockers,
        2
    );

    e.apply(
        &mut db,
        Command::UpdateTask {
            id: cancelled,
            patch: TaskPatch {
                state: Some(TaskState::Cancelled),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        row_for(&actionable_rows(&e, &db), dependent).open_blockers,
        1
    );

    // A tombstoned blocker will never reach `done`; it must not block
    // forever either.
    e.apply(&mut db, Command::DeleteTask(deleted)).unwrap();
    let dep = row_for(&actionable_rows(&e, &db), dependent);
    assert_eq!(dep.open_blockers, 0);
    assert_eq!(dep.effective_state, EffectiveTaskState::Todo);
}

#[test]
fn actionable_ranks_by_how_much_each_task_unblocks() {
    // The read path a Focus Planner wants: actionable first, then by the
    // size of the reverse edge set.
    let mut db = db();
    let e = engine();
    let hub = new_task(&e, &mut db, "unblock everything");
    let minor = new_task(&e, &mut db, "unblock one thing");
    let leaf = new_task(&e, &mut db, "unblock nothing");
    let d1 = new_task(&e, &mut db, "d1");
    let d2 = new_task(&e, &mut db, "d2");
    let d3 = new_task(&e, &mut db, "d3");
    for d in [d1, d2] {
        set_blockers(&e, &mut db, d, vec![hub]).unwrap();
    }
    set_blockers(&e, &mut db, d3, vec![hub, minor]).unwrap();

    let rows = actionable_rows(&e, &db);
    let order: Vec<EntityRef> = rows.iter().map(|r| r.task.id).collect();
    assert_eq!(
        &order[..3],
        &[hub, minor, leaf][..],
        "actionable first, most-unblocking first: {order:?}"
    );
    assert_eq!(row_for(&rows, hub).unblocks, 3);
    assert_eq!(row_for(&rows, minor).unblocks, 1);
    assert_eq!(row_for(&rows, leaf).unblocks, 0);
    // The three blocked ones sort after every actionable one.
    for id in [d1, d2, d3] {
        assert_eq!(
            row_for(&rows, id).effective_state,
            EffectiveTaskState::Blocked
        );
    }
    assert!(
        order.iter().position(|x| *x == d1).unwrap() > 2,
        "blocked tasks sink below actionable ones"
    );
}

#[test]
fn self_blocking_and_cycles_are_rejected() {
    let mut db = db();
    let e = engine();
    let a = new_task(&e, &mut db, "a");
    let b = new_task(&e, &mut db, "b");
    let c = new_task(&e, &mut db, "c");

    assert!(matches!(
        set_blockers(&e, &mut db, a, vec![a]),
        Err(EngineError::Validation(
            sunrise_domain::ValidationError::BlockedByCycle
        ))
    ));

    // a <- b <- c, then closing the loop c <- a.
    set_blockers(&e, &mut db, b, vec![a]).unwrap();
    set_blockers(&e, &mut db, c, vec![b]).unwrap();
    assert!(matches!(
        set_blockers(&e, &mut db, a, vec![c]),
        Err(EngineError::Validation(
            sunrise_domain::ValidationError::BlockedByCycle
        ))
    ));
    // The rejected edge left nothing behind.
    assert!(read_task_t(&e, &db, a).blocked_by.is_empty());
}

// ---- out-of-order dependency ops still converge ----

#[test]
fn a_dependency_op_that_overtakes_its_blocker_still_converges() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let blocker = new_task(&ea, &mut dba, "buy paint");
    let dependent = new_task(&ea, &mut dba, "paint the fence");
    // Each op needs a strictly later stamp than the row it supersedes, or
    // entity-level LWW discards it as a tie.
    set_clock(&ca, T0 + 1_000);
    set_blockers(&ea, &mut dba, dependent, vec![blocker]).unwrap();

    // Deliver the dependent's ops FIRST — B has never heard of the blocker.
    eb.apply_remote(&mut dbb, &create_env_for(&dba, dependent.bytes()))
        .unwrap();
    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, dependent.bytes(), "task.update"),
    )
    .unwrap();

    let rows = actionable_rows(&eb, &dbb);
    let dep = row_for(&rows, dependent);
    assert_eq!(
        dep.effective_state,
        EffectiveTaskState::Blocked,
        "an unknown blocker counts as open, so the task is not flashed as actionable"
    );
    assert_eq!(dep.open_blockers, 1);
    assert!(
        !rows.iter().any(|r| r.task.id == blocker),
        "the blocker itself does not exist on B yet"
    );

    // The blocker's create finally lands, still open.
    eb.apply_remote(&mut dbb, &create_env_for(&dba, blocker.bytes()))
        .unwrap();
    assert_eq!(
        row_for(&actionable_rows(&eb, &dbb), dependent).open_blockers,
        1
    );
    assert_eq!(row_for(&actionable_rows(&eb, &dbb), blocker).unblocks, 1);

    // A completes it; B applies that op and the dependent frees itself.
    set_clock(&ca, T0 + 2_000);
    ea.apply(&mut dba, Command::CompleteTask(blocker)).unwrap();
    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, blocker.bytes(), "task.update"),
    )
    .unwrap();
    let dep = row_for(&actionable_rows(&eb, &dbb), dependent);
    assert_eq!(dep.effective_state, EffectiveTaskState::Todo);
    assert_eq!(dep.open_blockers, 0);

    // Both replicas hold the identical dependency index, and re-delivering
    // every op in reverse changes nothing.
    assert_eq!(blocker_edges(&dba), blocker_edges(&dbb));
    for env in all_envelopes(&dba).iter().rev() {
        eb.apply_remote(&mut dbb, env).unwrap();
    }
    assert_eq!(blocker_edges(&dba), blocker_edges(&dbb));
    assert_eq!(tasks_projection(&dba), tasks_projection(&dbb));
}

#[test]
fn unblock_cascade_reports_what_a_completion_released() {
    let mut db = db();
    let (e, _clock) = focus_engine(0);
    let build = task_with(&e, &mut db, "build", TaskDraft::default());
    let docs = task_with(&e, &mut db, "docs", TaskDraft::default());
    let deploy = task_with(&e, &mut db, "deploy", TaskDraft::default());
    let qa = task_with(&e, &mut db, "qa", TaskDraft::default());
    for (dep, blockers) in [(deploy, vec![build]), (qa, vec![build, docs])] {
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: dep,
                patch: TaskPatch {
                    blocked_by: Some(blockers),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    }

    // Mid-session completion of `build`.
    e.apply(&mut db, Command::CompleteTask(build)).unwrap();
    let cascade = match e.query(&db, Query::UnblockCascade(build)).unwrap() {
        QueryResult::UnblockCascade(c) => *c,
        other => panic!("{other:?}"),
    };
    assert_eq!(cascade.completed, build);
    assert_eq!(cascade.released, vec![deploy], "deploy is free now");
    assert_eq!(cascade.still_blocked, vec![qa], "qa still waits on docs");

    // Finishing `docs` releases `qa` too, with no repair pass in between.
    e.apply(&mut db, Command::CompleteTask(docs)).unwrap();
    let cascade = match e.query(&db, Query::UnblockCascade(docs)).unwrap() {
        QueryResult::UnblockCascade(c) => *c,
        other => panic!("{other:?}"),
    };
    assert_eq!(cascade.released, vec![qa]);
    assert!(cascade.still_blocked.is_empty());
}
