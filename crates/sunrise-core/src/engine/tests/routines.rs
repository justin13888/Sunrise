//! Routines: materialization, its determinism and the streak.

use super::*;

// ---- routine materialization tests ----

#[test]
fn create_routine_materializes_future_tasks_with_linkage() {
    let mut db = db();
    let e = engine();
    let stream = stream_ref(5);
    let c = sample_constraint();
    let draft = routine_draft(
        stream,
        "FREQ=DAILY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Skip,
        vec![c.clone()],
    );
    let res = e.apply(&mut db, Command::CreateRoutine(draft)).unwrap();
    let rid = res.entity;
    assert_eq!(rid.kind(), EntityKind::Routine);

    // Routine is queryable.
    let routines = match e.query(&db, Query::Routines).unwrap() {
        QueryResult::Routines(v) => v,
        _ => panic!(),
    };
    assert_eq!(routines.len(), 1);
    let routine = routines.into_iter().next().unwrap();
    assert_eq!(routine.id, rid);

    // Daily occurrences within the 14-day horizon are all materialized.
    let window = (ms_to_ts(NOW), ms_to_ts(NOW + 14 * DAY_MS));
    let occ = routine
        .occurrences_in(window, &jiff::tz::TimeZone::UTC)
        .unwrap();
    assert!(occ.len() >= 13, "daily horizon materializes ~14 days");

    // Each occurrence has a deterministic task with linkage + copied
    // constraints + scheduled_at == occurrence instant.
    for o in &occ {
        let tid = occurrence_task_id(&rid, &o.key);
        let t = match e.query(&db, Query::EntityById(tid)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!("task {tid} missing for occurrence {}", o.key),
        };
        assert_eq!(t.routine_id, Some(rid));
        assert_eq!(t.routine_occurrence, Some(o.at));
        // Planned at its civil key, of the anchor's kind: zoned in the
        // routine's zone, never an instant (ADR-0066 §1).
        assert_eq!(t.planned_at, Some(routine.occurrence_time(o.civil)));
        assert!(matches!(t.planned_at, Some(SunriseTime::Zoned { .. })));
        assert_eq!(t.scheduling_constraints, vec![c.clone()]);
    }
}

#[test]
fn rematerialize_is_idempotent() {
    let mut db = db();
    let e = engine();
    let draft = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Skip,
        Vec::new(),
    );
    e.apply(&mut db, Command::CreateRoutine(draft)).unwrap();
    let ids_before = live_task_ids(&db);
    let ops_before = op_count(&db);

    // Re-run materialization at the same clock: no new tasks, no new ops.
    e.apply(&mut db, Command::MaterializeRoutines { now_ms: NOW as u64 })
        .unwrap();
    assert_eq!(live_task_ids(&db), ids_before, "no duplicate tasks");
    assert_eq!(op_count(&db), ops_before, "no new ops on re-materialize");
}

#[test]
fn two_engines_produce_identical_task_ids() {
    // Same routine content (same id) on two independent DBs => identical
    // deterministic task-id sets, even though op-ids differ (rng).
    fn fixed_routine() -> Routine {
        Routine {
            id: EntityRef::new(EntityKind::Routine, [0x33; 16]),
            created_at: ms_to_ts(NOW),
            updated_at: ms_to_ts(NOW),
            template: TaskTemplate {
                title: "R".into(),
                stream_id: Some(stream_ref(7)),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: Unknowns::new(),
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: ms_to_ts(NOW + 3_600_000).into(),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: 0,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            split_from: None,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn run() -> BTreeSet<[u8; 16]> {
        let mut db = db();
        let e = engine();
        let r = fixed_routine();
        fold_unlogged(
            &mut db,
            &InnerOp::RoutineCreate(Box::new(r.clone())),
            &e.lww_stamp(1),
            META_STREAM,
        );
        e.materialize_one_routine(&mut db, &r, NOW as u64).unwrap();
        live_task_ids(&db)
    }

    let a = run();
    let b = run();
    assert!(!a.is_empty());
    assert_eq!(a, b, "task-id sets converge across devices");
}

#[test]
fn catchup_policies_skip_queue_merge() {
    let start = NOW - (5 * DAY_MS + 3_600_000); // 6 missed daily occurrences
                                                // Skip: no past tasks.
    {
        let mut db = db();
        let e = engine();
        let d = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            start,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
        assert_eq!(past_task_count(&db, rid), 0, "skip drops missed");
    }
    // Queue: one task per missed occurrence.
    {
        let mut db = db();
        let e = engine();
        let d = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            start,
            RoutineCatchupPolicy::Queue,
            Vec::new(),
        );
        let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
        assert_eq!(past_task_count(&db, rid), 6, "queue keeps every missed");
    }
    // Merge: exactly one past task, titled with the catch-up suffix.
    {
        let mut db = db();
        let e = engine();
        let d = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            start,
            RoutineCatchupPolicy::Merge,
            Vec::new(),
        );
        let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
        assert_eq!(
            past_task_count(&db, rid),
            1,
            "merge collapses missed to one"
        );
        let title: String = db
            .conn()
            .query_row(
                "SELECT title FROM tasks
                     WHERE routine_id = ? AND deleted = 0 AND planned_at_ms <= ?",
                params![rid.bytes().to_vec(), NOW],
                |r| r.get(0),
            )
            .unwrap();
        assert!(title.contains("(x6 catch-up)"), "merge title: {title}");
    }
}

#[test]
fn skip_occurrence_prevents_task() {
    let mut db = db();
    let e = engine();
    let d = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Skip,
        Vec::new(),
    );
    let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
    let routine = read_routine(db.conn(), rid.bytes()).unwrap().unwrap();
    let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
    let occ = routine
        .occurrences_in(window, &jiff::tz::TimeZone::UTC)
        .unwrap();
    let target = occ[2].key.clone();
    let tid = occurrence_task_id(&rid, &target);

    // Task exists pre-skip.
    assert!(matches!(
        e.query(&db, Query::EntityById(tid)).unwrap(),
        QueryResult::Task(_)
    ));

    e.apply(
        &mut db,
        Command::SkipRoutineOccurrence {
            id: rid,
            occurrence_key: target.clone(),
        },
    )
    .unwrap();

    // The existing task is tombstoned...
    let t = match e.query(&db, Query::EntityById(tid)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert!(t.deleted, "skipped occurrence's task is tombstoned");

    // ...and re-materialization does not resurrect it.
    e.apply(&mut db, Command::MaterializeRoutines { now_ms: NOW as u64 })
        .unwrap();
    let t2 = match e.query(&db, Query::EntityById(tid)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert!(t2.deleted, "skip persists across materialize");
}

#[test]
fn update_routine_regenerates_future_but_keeps_started() {
    let mut db = db();
    let e = engine();
    let d = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Skip,
        Vec::new(),
    );
    let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
    let routine = read_routine(db.conn(), rid.bytes()).unwrap().unwrap();
    let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
    let daily = routine
        .occurrences_in(window, &jiff::tz::TimeZone::UTC)
        .unwrap();
    // occ[0] is the anchor weekday (kept by a weekly rule); occ[2] is a
    // different weekday (dropped); mark occ[3] in-progress so it survives.
    let keep_anchor = occurrence_task_id(&rid, &daily[0].key);
    let drop_id = occurrence_task_id(&rid, &daily[2].key);
    let started_id = occurrence_task_id(&rid, &daily[3].key);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: started_id,
            patch: TaskPatch {
                state: Some(TaskState::InProgress),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // Change to weekly (only the anchor weekday recurs).
    let patch = RoutinePatch {
        rrule: Some(RRule::parse("FREQ=WEEKLY").unwrap()),
        ..Default::default()
    };
    e.apply(
        &mut db,
        Command::UpdateRoutine {
            scope: sunrise_domain::EditScope::All,
            id: rid,
            patch,
        },
    )
    .unwrap();

    let deleted = |id: EntityRef| -> bool {
        match e.query(&db, Query::EntityById(id)).unwrap() {
            QueryResult::Task(t) => t.deleted,
            _ => panic!(),
        }
    };
    assert!(!deleted(keep_anchor), "anchor-weekday occurrence stays");
    assert!(deleted(drop_id), "off-cadence future task regenerated away");
    assert!(!deleted(started_id), "started task is preserved");
}

#[test]
fn deterministic_routine_task_converges() {
    // Both engines hold the SAME routine (same id) and each materializes the
    // same occurrence locally, producing a task with the SAME deterministic
    // id but different op-ids. Exchanging both create envelopes converges to
    // ONE identical task per DB.
    fn fixed_routine() -> Routine {
        Routine {
            id: EntityRef::new(EntityKind::Routine, [0x44; 16]),
            created_at: ms_to_ts(NOW),
            updated_at: ms_to_ts(NOW),
            template: TaskTemplate {
                title: "Stretch".into(),
                stream_id: Some(stream_ref(7)),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: Unknowns::new(),
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: ms_to_ts(NOW + 3_600_000).into(),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: 0,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            split_from: None,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca);
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let r = fixed_routine();
    // Each replica holds the routine as a row no op wrote.
    crate::engine::rebuild::check::disable();
    for (e, d) in [(&ea, &mut dba), (&eb, &mut dbb)] {
        fold_unlogged(
            d,
            &InnerOp::RoutineCreate(Box::new(r.clone())),
            &e.lww_stamp(1),
            META_STREAM,
        );
        e.materialize_one_routine(d, &r, NOW as u64).unwrap();
    }

    // Pick the first materialized occurrence's deterministic task id.
    let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
    let occ = r.occurrences_in(window, &jiff::tz::TimeZone::UTC).unwrap();
    let tid = occurrence_task_id(&r.id, &occ[0].key);

    // Exchange the two create envelopes both ways.
    let env_a = create_env_for(&dba, tid.bytes());
    let env_b = create_env_for(&dbb, tid.bytes());
    eb.apply_remote(&mut dbb, &env_a).unwrap();
    ea.apply_remote(&mut dba, &env_b).unwrap();

    // Exactly one task per DB, and identical content.
    for d in [&dba, &dbb] {
        let n: i64 = d
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE id = ?",
                params![tid.bytes().to_vec()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "one task row for the shared occurrence id");
    }
    assert_eq!(read_task_t(&ea, &dba, tid), read_task_t(&eb, &dbb, tid));
}

// ---- routine streak counter ----

#[test]
fn streak_survives_a_gap_inside_grace_and_resets_outside_it() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    let (rid, occ) = seed_routine(&e, &mut db);
    assert_eq!(routine_of(&e, &db, rid).streak_counter, 0);

    // 1. On time.
    set_clock(&clock, (occ[0].1 + 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();
    assert_eq!(routine_of(&e, &db, rid).streak_counter, 1);

    // 2. 23h late — inside the 24h default grace window, streak survives.
    set_clock(&clock, (occ[1].1 + 23 * 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[1].0)).unwrap();
    let r = routine_of(&e, &db, rid);
    assert_eq!(r.streak_counter, 2, "a gap inside grace keeps the streak");
    assert_eq!(r.forgivenesses_in_window, 0);

    // 3. 25h late — outside grace. Forgiveness absorbs the first miss.
    set_clock(&clock, (occ[2].1 + 25 * 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[2].0)).unwrap();
    let r = routine_of(&e, &db, rid);
    assert_eq!(r.streak_counter, 2, "forgiven, not advanced, not reset");
    assert_eq!(r.forgivenesses_in_window, 1);

    // 4. A second miss in the same 30-day window resets.
    set_clock(&clock, (occ[3].1 + 25 * 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[3].0)).unwrap();
    let r = routine_of(&e, &db, rid);
    assert_eq!(r.streak_counter, 0, "a gap outside grace resets the streak");
    assert_eq!(r.streak_started_at, None);
}

#[test]
fn re_completing_an_occurrence_never_moves_the_streak() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    let (rid, occ) = seed_routine(&e, &mut db);

    set_clock(&clock, (occ[0].1 + 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();
    let after_first = routine_of(&e, &db, rid);
    assert_eq!(after_first.streak_counter, 1);
    assert_eq!(after_first.streak_keys.len(), 1);
    let routine_ops_before = routine_op_count(&db);

    // Resurrect and re-complete, well outside grace. The idempotency key
    // makes it a no-op — and emits no routine op at all.
    set_clock(&clock, (occ[0].1 + 10 * DAY_MS) as u64);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: occ[0].0,
            patch: TaskPatch {
                state: Some(TaskState::Todo),
                ..Default::default()
            },
        },
    )
    .unwrap();
    e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();

    // The stored streak is as of the newest key, and the duplicate left it.
    let after = super::routine::read_routine(db.conn(), rid.bytes())
        .unwrap()
        .unwrap();
    assert_eq!(after.streak_counter, 1);
    assert_eq!(after.streak_keys, after_first.streak_keys);
    // A read applies the occurrences that lapsed since, ten days of them:
    // the first is forgiven and the second ends the run (ADR-0065 §5).
    assert_eq!(routine_of(&e, &db, rid).streak_counter, 0);
    assert_eq!(
        routine_op_count(&db),
        routine_ops_before,
        "a duplicate completion emits no routine op"
    );
}

#[test]
fn a_non_routine_task_completing_touches_no_routine() {
    let mut db = db();
    let e = engine();
    let id = new_task(&e, &mut db, "one-off");
    let before = routine_op_count(&db);
    e.apply(&mut db, Command::CompleteTask(id)).unwrap();
    assert_eq!(routine_op_count(&db), before);
}

#[test]
fn the_streak_advance_converges_to_the_other_replica() {
    // The completion emits BOTH a task.update and a routine.update in one
    // transaction, so a replica that never saw the completion still lands
    // on the same counter.
    let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let (rid, occ) = seed_routine(&ea, &mut dba);
    // B learns the routine and the occurrence task.
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
        .unwrap();
    eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
        .unwrap();

    // Both replicas move together. An hour of real skew between two peers
    // is outside the HLC drift window on purpose; this test is about
    // convergence, not about clock abuse.
    set_clock(&ca, (occ[0].1 + 3_600_000) as u64);
    set_clock(&cb, (occ[0].1 + 3_600_000) as u64);
    ea.apply(&mut dba, Command::CompleteTask(occ[0].0)).unwrap();
    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, occ[0].0.bytes(), "task.update"),
    )
    .unwrap();
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
        .unwrap();

    let ra = routine_of(&ea, &dba, rid);
    let rb = routine_of(&eb, &dbb, rid);
    assert_eq!(ra.streak_counter, 1);
    assert_eq!(rb.streak_counter, 1, "streak converged");
    assert_eq!(ra.streak_keys, rb.streak_keys);
    assert_eq!(ra.streak_started_at, rb.streak_started_at);
}
