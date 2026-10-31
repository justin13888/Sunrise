//! Focus sessions, their measurements and the completion they can carry.

use super::*;

// ---- auto-completion (issue #10) ----

/// The signal: a session closed with `completed_task: true` completes the
/// task it was opened on, and stamps `completed_at`.
#[test]
fn a_session_that_says_it_finished_the_task_completes_it() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let session = open_session(&e, &mut db, task);
    let res = end_session(&e, &mut db, session, true);

    let t = task_of(&e, &db, task);
    assert_eq!(t.state, TaskState::Done);
    assert!(t.completed_at.is_some(), "completed_at is stamped");
    assert_eq!(
        res.state,
        Some(TaskState::Done),
        "the caller learns the task moved"
    );
}

/// A session closed WITHOUT the flag changes nothing about the task. The
/// signal is the user's statement, not the session's existence.
#[test]
fn ending_a_session_without_the_flag_leaves_the_task_open() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let session = open_session(&e, &mut db, task);
    let res = end_session(&e, &mut db, session, false);

    let t = task_of(&e, &db, task);
    assert_eq!(t.state, TaskState::Todo);
    assert!(t.completed_at.is_none());
    assert_eq!(res.state, None);
}

/// A task the user cancelled is left alone: `cancelled -> done` is not a
/// legal transition, and a session must not overrule the user.
#[test]
fn a_cancelled_task_is_not_auto_completed() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let session = open_session(&e, &mut db, task);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                state: Some(TaskState::Cancelled),
                ..Default::default()
            },
        },
    )
    .unwrap();
    end_session(&e, &mut db, session, true);
    assert_eq!(task_of(&e, &db, task).state, TaskState::Cancelled);
}

/// An already-done task is a no-op: no second `task.update` op, so a client
/// that both completes the task and closes the session with the flag does
/// not write the completion twice.
#[test]
fn an_already_done_task_produces_no_second_op() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let session = open_session(&e, &mut db, task);
    e.apply(&mut db, Command::CompleteTask(task)).unwrap();
    let before = op_count(&db);
    end_session(&e, &mut db, session, true);
    assert_eq!(
        op_count(&db) - before,
        1,
        "only the focus.end op; the task was already done"
    );
}

/// A deleted task is a no-op rather than an error: a session closing over
/// a task that was tombstoned elsewhere must still close.
#[test]
fn a_deleted_task_is_not_resurrected() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let session = open_session(&e, &mut db, task);
    e.apply(&mut db, Command::DeleteTask(task)).unwrap();
    end_session(&e, &mut db, session, true);
    let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
    assert!(t.deleted);
    assert_eq!(t.state, TaskState::Todo);
}

/// The op the completion emits is an ordinary full-state `task.update`,
/// which is what makes it converge like any hand-edit.
#[test]
fn the_completion_is_an_ordinary_task_update_op() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let session = open_session(&e, &mut db, task);
    end_session(&e, &mut db, session, true);
    let env = env_for_kind(&db, task.bytes(), "task.update");
    assert!(!env.is_empty(), "a task.update op was emitted");
}

/// Convergence: the completion is derived ONCE, on the originating device.
/// A replica that applies only the `focus.end` op derives nothing — so the
/// task moves exactly when the `task.update` arrives, and never twice.
#[test]
fn a_replica_does_not_re_derive_the_completion_from_the_focus_op() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca);
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "Write the report");
    let session = open_session(&ea, &mut dba, task);
    eb.apply_remote(&mut dbb, &create_env_for(&dba, task.bytes()))
        .unwrap();
    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, session.bytes(), "focus.start"),
    )
    .unwrap();

    end_session(&ea, &mut dba, session, true);

    // Only the focus.end crosses. B sees the session close and the task
    // stay open: the derivation is not re-run here.
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, session.bytes(), "focus.end"))
        .unwrap();
    assert_eq!(
        read_task(dbb.conn(), task.bytes()).unwrap().unwrap().state,
        TaskState::Todo,
        "a focus.end op is not itself a completion"
    );

    // The derived task op is what moves it, through the ordinary LWW path.
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.update"))
        .unwrap();
    let a = read_task(dba.conn(), task.bytes()).unwrap().unwrap();
    let b = read_task(dbb.conn(), task.bytes()).unwrap().unwrap();
    assert_eq!(b.state, TaskState::Done);
    assert_eq!(a.completed_at, b.completed_at, "one instant, not two");
    assert_eq!(
        row_stamp(&dba, "tasks", "id", &task),
        row_stamp(&dbb, "tasks", "id", &task),
    );
}

/// `docs/08-features/focus-mode.md`: "a focus session that completes a
/// routine occurrence increments the routine streak" — and the advance
/// converges, because it rides in the same transaction as its completion.
#[test]
fn completing_a_routine_occurrence_through_focus_advances_the_streak() {
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
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
        .unwrap();
    eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
        .unwrap();

    // Both replicas move together: an hour of real skew between peers is
    // outside the HLC drift window on purpose, and this test is about the
    // streak, not about clock abuse.
    set_clock(&ca, (occ[0].1 + 3_600_000) as u64);
    set_clock(&cb, (occ[0].1 + 3_600_000) as u64);
    let session = open_session(&ea, &mut dba, occ[0].0);
    end_session(&ea, &mut dba, session, true);
    assert_eq!(routine_of(&ea, &dba, rid).streak_counter, 1);

    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, occ[0].0.bytes(), "task.update"),
    )
    .unwrap();
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
        .unwrap();
    assert_eq!(routine_of(&eb, &dbb, rid).streak_counter, 1);
    assert_eq!(
        row_stamp(&dba, "routines", "id", &rid),
        row_stamp(&dbb, "routines", "id", &rid),
    );
}

// ---- focus sessions (ADR-0013) ----

#[test]
fn start_focus_mints_an_fcs_entity_and_never_touches_the_task() {
    let mut db = db();
    let (e, _clock) = focus_engine(1_000);
    let task = task_with(&e, &mut db, "write the ADR", TaskDraft::default());
    let before = read_task(db.conn(), task.bytes()).unwrap().unwrap();

    let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
    assert_eq!(
        session.kind(),
        EntityKind::FocusSession,
        "a session is its own entity, not a field on the Task"
    );
    assert!(session.to_str().starts_with("fcs_"));

    // The Task is byte-identical: focusing is not an edit.
    let after = read_task(db.conn(), task.bytes()).unwrap().unwrap();
    assert_eq!(before, after);

    // The op is a `focus.start` routed under the task's stream.
    let kind: String = db
        .conn()
        .query_row(
            "SELECT inner_kind FROM ops WHERE target_id = ?",
            params![&session.bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "focus.start");
}

#[test]
fn a_dangling_start_reads_as_running_and_elapsed_comes_from_the_clock() {
    let mut db = db();
    let (e, clock) = focus_engine(10_000);
    let task = task_with(&e, &mut db, "long haul", TaskDraft::default());
    let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);

    // No `end` op has been written — and that is a *valid* state, the one
    // the app dying mid-session leaves behind.
    let rows = running(&e, &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session.start.id, session);
    assert!(rows[0].running);
    assert_eq!(rows[0].focused_ms, 0);

    // Elapsed is derived: nothing was written, only the clock moved.
    set_clock(&clock, 10_000 + 90_000);
    let rows = running(&e, &db);
    assert_eq!(rows[0].focused_ms, 90_000);
    set_clock(&clock, 10_000 + 300_000);
    assert_eq!(running(&e, &db)[0].focused_ms, 300_000);

    // And the row itself stores no ticking value to disagree with.
    let stored: Option<i64> = db
        .conn()
        .query_row(
            "SELECT actual_focused_ms FROM focus_session_ends WHERE session_id = ?",
            params![&session.bytes()[..]],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(stored, None, "nothing is frozen until the end op");
}

#[test]
fn end_focus_freezes_the_measurement_and_the_session_is_then_immutable() {
    let mut db = db();
    let (e, clock) = focus_engine(1_000);
    let task = task_with(&e, &mut db, "ship it", TaskDraft::default());
    let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);

    set_clock(&clock, 1_000 + POMODORO_MS);
    e.apply(
        &mut db,
        Command::EndFocus {
            session,
            actual_focused_ms: None,
            completed_task: true,
        },
    )
    .unwrap();

    assert!(running(&e, &db).is_empty(), "the session is closed");
    let rows = sessions_for(&e, &db, task);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].running);
    assert_eq!(rows[0].focused_ms, POMODORO_MS);

    // The clock moving on no longer changes the answer.
    set_clock(&clock, u64::MAX);
    assert_eq!(sessions_for(&e, &db, task)[0].focused_ms, POMODORO_MS);

    // Closing twice is rejected: an ended session is immutable, which is
    // the whole reason start and end are separate append-only records.
    let err = e
        .apply(
            &mut db,
            Command::EndFocus {
                session,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
}

#[test]
fn end_focus_accepts_a_caller_supplied_actual_smaller_than_elapsed() {
    let mut db = db();
    let (e, clock) = focus_engine(0);
    let task = task_with(&e, &mut db, "paused a lot", TaskDraft::default());
    let session = start_work(&e, &mut db, task, SessionLength::UntilDone);
    set_clock(&clock, 60 * 60 * 1000);
    e.apply(
        &mut db,
        Command::EndFocus {
            session,
            actual_focused_ms: Some(20 * 60 * 1000),
            completed_task: false,
        },
    )
    .unwrap();
    let rows = sessions_for(&e, &db, task);
    // Elapsed wall time is an hour; focused time is what the client
    // measured.
    assert_eq!(rows[0].session.elapsed_ms(0), 60 * 60 * 1000);
    assert_eq!(rows[0].focused_ms, 20 * 60 * 1000);
}

#[test]
fn ending_an_unknown_session_is_not_found() {
    let mut db = db();
    let (e, _clock) = focus_engine(0);
    let ghost = EntityRef::new(EntityKind::FocusSession, [0x5A; 16]);
    let err = e
        .apply(
            &mut db,
            Command::EndFocus {
                session: ghost,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::NotFound(_)), "{err:?}");
    // And a non-session id is rejected on the kind, not on lookup.
    let err = e
        .apply(
            &mut db,
            Command::EndFocus {
                session: EntityRef::new(EntityKind::Task, [1; 16]),
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
}

#[test]
fn interruptions_accumulate_and_ride_the_end_op() {
    let mut db = db();
    let (e, clock) = focus_engine(1_000);
    let task = task_with(&e, &mut db, "deep work", TaskDraft::default());
    let session = start_work(&e, &mut db, task, SessionLength::OnePomodoro);

    set_clock(&clock, 2_000);
    e.apply(
        &mut db,
        Command::LogInterruption {
            session,
            reason: InterruptionReason::Meeting,
        },
    )
    .unwrap();
    set_clock(&clock, 3_000);
    e.apply(
        &mut db,
        Command::LogInterruption {
            session,
            reason: InterruptionReason::SelfInterrupt,
        },
    )
    .unwrap();

    // Visible on the running session, before any end op exists.
    let rows = running(&e, &db);
    assert_eq!(rows[0].session.interruptions.len(), 2);

    set_clock(&clock, 4_000);
    e.apply(
        &mut db,
        Command::EndFocus {
            session,
            actual_focused_ms: None,
            completed_task: false,
        },
    )
    .unwrap();
    let rows = sessions_for(&e, &db, task);
    assert_eq!(
        rows[0].session.end.as_ref().unwrap().interruptions.len(),
        2,
        "the end op carries the interruptions the ending device knew about"
    );

    let s = stats(&e, &db, 4_000);
    assert_eq!(s.interruptions, 2);
    assert_eq!(s.top_interruptions.len(), 2);
}

#[test]
fn a_break_session_is_sized_by_the_cycle_not_the_estimate() {
    let mut db = db();
    let (e, clock) = focus_engine(0);
    let task = task_with(
        &e,
        &mut db,
        "cycle",
        TaskDraft {
            estimated_duration_s: Some(10 * 60 * 60),
            ..Default::default()
        },
    );
    // Four completed work sessions, then a break: the long one.
    for i in 0..4u64 {
        set_clock(&clock, i * 1_000_000);
        let s = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
        set_clock(&clock, i * 1_000_000 + POMODORO_MS);
        e.apply(
            &mut db,
            Command::EndFocus {
                session: s,
                actual_focused_ms: None,
                completed_task: false,
            },
        )
        .unwrap();
    }
    set_clock(&clock, 9_000_000);
    let brk = e
        .apply(
            &mut db,
            Command::StartFocus(FocusStartDraft {
                task_id: task,
                kind: FocusKind::Break,
                length: SessionLength::SizedToEstimate,
                energy: None,
            }),
        )
        .unwrap()
        .entity;
    let row = running(&e, &db)
        .into_iter()
        .find(|r| r.session.start.id == brk)
        .unwrap();
    assert_eq!(row.session.start.kind, FocusKind::Break);
    assert_eq!(
        row.session.start.planned_ms,
        Some(sunrise_domain::LONG_BREAK_MS),
        "the fourth cycle earns the long break"
    );
    assert_eq!(row.session.start.chunk, None);
}

#[test]
fn a_long_estimate_chunks_and_the_index_advances_with_prior_sessions() {
    let mut db = db();
    let (e, clock) = focus_engine(0);
    let task = task_with(
        &e,
        &mut db,
        "90 minutes of work",
        TaskDraft {
            estimated_duration_s: Some(90 * 60),
            ..Default::default()
        },
    );
    let first = start_work(&e, &mut db, task, SessionLength::SizedToEstimate);
    let row = sessions_for(&e, &db, task)
        .into_iter()
        .find(|r| r.session.start.id == first)
        .unwrap();
    assert_eq!(row.session.start.chunk, Some(Chunk::new(1, 4)));
    assert_eq!(row.session.start.planned_ms, Some(POMODORO_MS));

    set_clock(&clock, POMODORO_MS);
    e.apply(
        &mut db,
        Command::EndFocus {
            session: first,
            actual_focused_ms: None,
            completed_task: false,
        },
    )
    .unwrap();
    set_clock(&clock, POMODORO_MS + 1);
    let second = start_work(&e, &mut db, task, SessionLength::SizedToEstimate);
    let row = sessions_for(&e, &db, task)
        .into_iter()
        .find(|r| r.session.start.id == second)
        .unwrap();
    assert_eq!(
        row.session.start.chunk,
        Some(Chunk::new(2, 4)),
        "the second sitting reads chunk 2 of 4"
    );
}

// ---- calibration + cascade ----

#[test]
fn focus_stats_calibrate_from_recorded_actuals() {
    let mut db = db();
    let (e, clock) = focus_engine(0);
    // A 30-minute estimate that actually took 51 minutes => 1.7x.
    let task = task_with(
        &e,
        &mut db,
        "estimate me",
        TaskDraft {
            estimated_duration_s: Some(30 * 60),
            energy: Some(Energy::High),
            ..Default::default()
        },
    );
    let s = start_work(&e, &mut db, task, SessionLength::SizedToEstimate);
    set_clock(&clock, 51 * 60 * 1000);
    e.apply(
        &mut db,
        Command::EndFocus {
            session: s,
            actual_focused_ms: None,
            completed_task: true,
        },
    )
    .unwrap();

    let st = stats(&e, &db, 51 * 60 * 1000);
    assert_eq!(st.sessions, 1);
    assert_eq!(st.running, 0);
    assert_eq!(st.total_focused_ms, 51 * 60 * 1000);
    let c = st.overall.expect("calibrates");
    assert!((c.factor - 1.7).abs() < 1e-9, "factor was {}", c.factor);
    // Bucketed per Stream and per energy too.
    assert_eq!(st.per_stream.len(), 1);
    assert!((st.per_stream[0].calibration.unwrap().factor - 1.7).abs() < 1e-9);
    assert_eq!(st.per_energy[0].energy, Some(Energy::High));
}

#[test]
fn focus_stats_filter_by_stream_and_since() {
    let mut db = db();
    let (e, clock) = focus_engine(1_000);
    let task = task_with(&e, &mut db, "inbox work", TaskDraft::default());
    let early = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
    set_clock(&clock, 2_000);
    e.apply(
        &mut db,
        Command::EndFocus {
            session: early,
            actual_focused_ms: None,
            completed_task: false,
        },
    )
    .unwrap();
    set_clock(&clock, 50_000);
    let late = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
    set_clock(&clock, 60_000);
    e.apply(
        &mut db,
        Command::EndFocus {
            session: late,
            actual_focused_ms: None,
            completed_task: false,
        },
    )
    .unwrap();

    let all = stats(&e, &db, 60_000);
    assert_eq!(all.sessions, 2);
    let recent = match e
        .query(
            &db,
            Query::FocusStats {
                stream: None,
                since_ms: Some(10_000),
                now_ms: 60_000,
            },
        )
        .unwrap()
    {
        QueryResult::FocusStats(s) => *s,
        other => panic!("{other:?}"),
    };
    assert_eq!(recent.sessions, 1);
    assert_eq!(recent.total_focused_ms, 10_000);
}

#[test]
fn two_concurrent_sessions_on_one_task_both_survive_and_aggregate() {
    // The single-replica shape of the ADR's convergence property: two
    // sessions minted independently are two rows, not one contended
    // register, and their focused time sums.
    let mut db = db();
    let (e, clock) = focus_engine(0);
    let task = task_with(
        &e,
        &mut db,
        "shared work",
        TaskDraft {
            estimated_duration_s: Some(20 * 60),
            ..Default::default()
        },
    );
    let a = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
    let b = start_work(&e, &mut db, task, SessionLength::OnePomodoro);
    assert_ne!(a, b, "concurrent starts mint distinct ids");
    assert_eq!(running(&e, &db).len(), 2);

    set_clock(&clock, 600_000);
    e.apply(
        &mut db,
        Command::EndFocus {
            session: a,
            actual_focused_ms: Some(600_000),
            completed_task: false,
        },
    )
    .unwrap();
    e.apply(
        &mut db,
        Command::EndFocus {
            session: b,
            actual_focused_ms: Some(600_000),
            completed_task: true,
        },
    )
    .unwrap();

    let st = stats(&e, &db, 600_000);
    assert_eq!(st.sessions, 2, "both survive");
    assert_eq!(st.total_focused_ms, 1_200_000, "and both count");
    // 20 minutes of focus against a 20-minute estimate: bang on.
    let c = st.overall.expect("calibrates");
    assert!((c.factor - 1.0).abs() < 1e-9, "factor was {}", c.factor);
    assert_eq!(c.samples, 1, "one task, two sessions");
}

#[test]
fn focus_ops_are_append_only_on_the_remote_path() {
    // Two replicas of one vault. The `end` op is delivered BEFORE the
    // `start` it belongs to — the arrival order that a single mutable row
    // would silently drop.
    let clock_a = Arc::new(FakeClock(PLMutex::new(1_000)));
    let clock_b = Arc::new(FakeClock(PLMutex::new(1_000)));
    let ea = engine_seeded([0x77; 32], [1; 32], clock_a.clone());
    let eb = engine_seeded([0x77; 32], [2; 32], clock_b.clone());
    let mut da = db_root([0x77; 32]);
    let mut dbb = db_root([0x77; 32]);
    // Mutual trust so envelopes verify.
    trust(&ea, &mut da, &eb);
    trust(&eb, &mut dbb, &ea);

    let task = task_with(&ea, &mut da, "cross-device", TaskDraft::default());
    let task_env = create_env_for(&da, task.bytes());
    eb.apply_remote(&mut dbb, &task_env).unwrap();

    let session = start_work(&ea, &mut da, task, SessionLength::OnePomodoro);
    set_clock(&clock_a, 1_000 + 60_000);
    ea.apply(
        &mut da,
        Command::EndFocus {
            session,
            actual_focused_ms: None,
            completed_task: true,
        },
    )
    .unwrap();

    let env_of = |kind: &str| -> Vec<u8> {
        let op_id: Vec<u8> = da
            .conn()
            .query_row(
                "SELECT op_id FROM ops WHERE target_id = ? AND inner_kind = ?",
                params![&session.bytes()[..], kind],
                |r| r.get(0),
            )
            .unwrap();
        let mut id = [0u8; 16];
        id.copy_from_slice(&op_id);
        env_bytes(&da, &id)
    };
    let start_env = env_of("focus.start");
    let end_env = env_of("focus.end");

    // Out of order on purpose, and each delivered twice.
    for env in [&end_env, &end_env, &start_env, &start_env] {
        eb.apply_remote(&mut dbb, env).unwrap();
    }

    set_clock(&clock_b, 9_999_999);
    let rows = sessions_for(&eb, &dbb, task);
    assert_eq!(rows.len(), 1, "re-delivery did not duplicate the session");
    assert!(!rows[0].running, "the end that arrived first still applied");
    assert_eq!(
        rows[0].focused_ms, 60_000,
        "the frozen value crossed intact"
    );
    assert!(rows[0].session.end.as_ref().unwrap().completed_task);
}

/// A session that says it finished its task completes a task in a state this
/// build does not know, as it would a `todo` one.
#[test]
fn a_finished_session_completes_a_task_in_an_unknown_state() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let session = open_session(&e, &mut db, task);
    set_unknown_state(&e, &mut db, task, "snoozed");
    let res = end_session(&e, &mut db, session, true);

    assert_eq!(task_of(&e, &db, task).state, TaskState::Done);
    assert_eq!(res.state, Some(TaskState::Done));
}
