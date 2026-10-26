//! The triage queue and its four actions in the engine (ADR-0047 §3–§4,
//! #335): the queue's order and Keep's return at the next transition, one
//! invalid id refusing a whole batch, what each action writes, a backdated
//! completion in the stats and the routine streak, the drop reason across
//! sync and through a peer that predates it, and nothing changing a task
//! without a command.

use super::*;
use crate::queries::TriageRow;
use sunrise_domain::{DeferTarget, Lateness, TriageAction};

fn clocked() -> (Engine, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    (engine_clocked(clock.clone()), clock)
}

fn add(
    e: &Engine,
    db: &mut Db,
    title: &str,
    target: Option<SunriseTime>,
    hard: Option<SunriseTime>,
) -> EntityRef {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            target_at: target,
            hard_due_at: hard,
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn queue(e: &Engine, db: &Db) -> Vec<TriageRow> {
    match e.query(db, Query::Triage).unwrap() {
        QueryResult::Triage(rows) => rows,
        other => panic!("expected Triage, got {other:?}"),
    }
}

fn titles(rows: &[TriageRow]) -> Vec<(String, Lateness)> {
    rows.iter()
        .map(|r| (r.task.title.clone(), r.lateness.state))
        .collect()
}

fn triage(
    e: &Engine,
    db: &mut Db,
    ids: &[EntityRef],
    action: TriageAction,
) -> Result<crate::commands::CommandResult, EngineError> {
    e.apply(
        db,
        Command::Triage {
            ids: ids.to_vec(),
            action,
        },
    )
}

fn at(ms: i64) -> SunriseTime {
    ms_to_ts(ms).into()
}

fn op_count(db: &Db) -> i64 {
    db.conn()
        .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
        .unwrap()
}

/// The queue holds what is late or stale, hard first, then soft, then
/// stale, each oldest first; Keep takes a task out, and a later threshold
/// brings it back by itself.
#[test]
fn the_queue_orders_and_keep_holds_until_the_next_transition() {
    let (e, clock) = clocked();
    let mut db = db();
    let hour = 3_600_000;
    let soft = add(
        &e,
        &mut db,
        "soft",
        Some(at(NOW + hour)),
        Some(at(NOW + 5 * DAY_MS)),
    );
    let hard_late = add(&e, &mut db, "hard late", None, Some(at(NOW + 2 * hour)));
    let hard_early = add(&e, &mut db, "hard early", None, Some(at(NOW + hour)));
    add(&e, &mut db, "on track", None, Some(at(NOW + 30 * DAY_MS)));
    assert!(queue(&e, &db).is_empty(), "nothing is late yet");
    set_clock(&clock, (NOW + 3 * hour) as u64);
    assert_eq!(
        titles(&queue(&e, &db)),
        [
            ("hard early".into(), Lateness::LateHard),
            ("hard late".into(), Lateness::LateHard),
            ("soft".into(), Lateness::LateSoft),
        ]
    );
    triage(&e, &mut db, &[soft, hard_early], TriageAction::Keep).unwrap();
    assert_eq!(
        titles(&queue(&e, &db)),
        [("hard late".into(), Lateness::LateHard)]
    );
    // The soft one's hard deadline passes: a new state, back in the queue.
    set_clock(&clock, (NOW + 5 * DAY_MS + hour) as u64);
    assert_eq!(
        titles(&queue(&e, &db)),
        [
            ("hard late".into(), Lateness::LateHard),
            ("soft".into(), Lateness::LateHard),
        ]
    );
    // A paused stream asks for nothing.
    let s = e
        .apply(
            &mut db,
            Command::CreateStream(sunrise_domain::StreamDraft {
                name: "Paused".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(
        &mut db,
        Command::PromoteToStream {
            id: hard_late,
            stream: s,
        },
    )
    .unwrap();
    e.apply(
        &mut db,
        Command::UpdateStream {
            id: s,
            patch: sunrise_domain::StreamPatch {
                paused: Some(true),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        titles(&queue(&e, &db)),
        [("soft".into(), Lateness::LateHard)]
    );
    e.assert_rebuild_identity(&db);
}

/// A batch is all or nothing: one unknown, deleted or repeated id, an empty
/// batch, or one task the action cannot apply to refuses it, and nothing is
/// written.
#[test]
fn one_invalid_id_refuses_the_whole_batch() {
    let (e, clock) = clocked();
    let mut db = db();
    let a = add(&e, &mut db, "a", None, None);
    let b = add(&e, &mut db, "b", None, None);
    let gone = add(&e, &mut db, "gone", None, None);
    e.apply(&mut db, Command::DeleteTask(gone)).unwrap();
    set_clock(&clock, (NOW + DAY_MS) as u64);
    let ops = op_count(&db);
    let unknown = EntityRef::new(EntityKind::Task, [0x99; 16]);
    let drop = || TriageAction::Drop {
        reason: Some("not mine".into()),
    };
    assert!(matches!(
        triage(&e, &mut db, &[a, unknown, b], drop()),
        Err(EngineError::NotFound(_))
    ));
    assert!(matches!(
        triage(&e, &mut db, &[a, gone], drop()),
        Err(EngineError::Invalid(_))
    ));
    assert!(matches!(
        triage(&e, &mut db, &[a, a], drop()),
        Err(EngineError::Invalid(_))
    ));
    assert!(matches!(
        triage(&e, &mut db, &[], drop()),
        Err(EngineError::Invalid(_))
    ));
    // A completion in the future for one of them refuses both.
    let tomorrow = SunriseTime::all_day(jiff::civil::date(2023, 11, 17));
    assert!(matches!(
        triage(
            &e,
            &mut db,
            &[a, b],
            TriageAction::AlreadyDone {
                completed_at: tomorrow
            }
        ),
        Err(EngineError::Invalid(_))
    ));
    let long = "x".repeat(513);
    assert!(triage(&e, &mut db, &[a], TriageAction::Drop { reason: Some(long) }).is_err());
    assert_eq!(op_count(&db), ops, "nothing written");
    for id in [a, b] {
        assert_eq!(read_task_t(&e, &db, id).state, TaskState::Todo);
    }
    // The batch with only good ids is one commit of both.
    triage(&e, &mut db, &[a, b], drop()).unwrap();
    assert_eq!(op_count(&db), ops + 2);
}

/// What each action writes.
#[test]
fn each_action_writes_what_its_row_says() {
    let (e, clock) = clocked();
    let mut db = db();
    let target = at(NOW + DAY_MS);
    let hard = SunriseTime::all_day(jiff::civil::date(2023, 11, 30));
    let t = add(&e, &mut db, "t", Some(target.clone()), Some(hard.clone()));
    set_clock(&clock, (NOW + 2 * DAY_MS) as u64);

    // Defer to a time keeps its kind, counts, and never moves a deadline.
    let floating = SunriseTime::floating(jiff::civil::date(2023, 11, 20).at(9, 0, 0, 0));
    triage(
        &e,
        &mut db,
        &[t],
        TriageAction::Defer {
            to: DeferTarget::At(floating.clone()),
        },
    )
    .unwrap();
    let r = read_task_t(&e, &db, t);
    assert_eq!(r.planned_at, Some(floating));
    assert_eq!((r.target_at, r.hard_due_at), (Some(target), Some(hard)));
    assert_eq!(r.deferred_count, 1);
    // The next free slot, until the planner: the next planner day.
    triage(
        &e,
        &mut db,
        &[t],
        TriageAction::Defer {
            to: DeferTarget::NextFreeSlot,
        },
    )
    .unwrap();
    let r = read_task_t(&e, &db, t);
    // NOW + 2 days is 2023-11-16 in the tests' UTC.
    assert_eq!(
        r.planned_at,
        Some(SunriseTime::all_day(jiff::civil::date(2023, 11, 17)))
    );
    assert_eq!(r.deferred_count, 2);

    // Drop with a reason; reopening clears the reason.
    triage(
        &e,
        &mut db,
        &[t],
        TriageAction::Drop {
            reason: Some("  superseded  ".into()),
        },
    )
    .unwrap();
    let r = read_task_t(&e, &db, t);
    assert_eq!(r.state, TaskState::Cancelled);
    assert_eq!(r.drop_reason(), Some("superseded"));
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: t,
            patch: TaskPatch {
                state: Some(TaskState::Todo),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(read_task_t(&e, &db, t).cancel_reason, None);

    // Already done, backdated to an instant and then to a date.
    let yesterday_noon = NOW + DAY_MS + 12 * 3_600_000;
    triage(
        &e,
        &mut db,
        &[t],
        TriageAction::AlreadyDone {
            completed_at: at(yesterday_noon),
        },
    )
    .unwrap();
    let r = read_task_t(&e, &db, t);
    assert_eq!(r.state, TaskState::Done);
    assert_eq!(r.completed_at, Some(at(yesterday_noon)));
    let day = SunriseTime::all_day(jiff::civil::date(2023, 11, 15));
    triage(
        &e,
        &mut db,
        &[t],
        TriageAction::AlreadyDone {
            completed_at: day.clone(),
        },
    )
    .unwrap();
    assert_eq!(read_task_t(&e, &db, t).completed_at, Some(day));
    // Never before it existed.
    assert!(triage(
        &e,
        &mut db,
        &[t],
        TriageAction::AlreadyDone {
            completed_at: at(NOW - 1)
        }
    )
    .is_err());

    // Keep stamps now.
    let k = add(&e, &mut db, "k", None, None);
    triage(&e, &mut db, &[k], TriageAction::Keep).unwrap();
    assert_eq!(
        read_task_t(&e, &db, k).late_acknowledged_at,
        Some(ms_to_ts(NOW + 2 * DAY_MS))
    );
    e.assert_rebuild_identity(&db);
}

/// A backdated completion counts where the work was done: in the morning
/// summary's "since yesterday", and in the routine streak, inside the grace
/// window a completion at the click would have missed.
#[test]
fn a_backdated_completion_feeds_the_stats_and_the_streak() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    let (rid, occ) = seed_routine(&e, &mut db);
    // On time: the first occurrence.
    set_clock(&clock, (occ[0].1 + 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();
    assert_eq!(routine_of(&e, &db, rid).streak_counter, 1);
    // The second is clicked done two days late, but was done an hour after
    // it fell due: the streak counts it.
    set_clock(&clock, (occ[1].1 + 48 * 3_600_000) as u64);
    triage(
        &e,
        &mut db,
        &[occ[1].0],
        TriageAction::AlreadyDone {
            completed_at: at(occ[1].1 + 3_600_000),
        },
    )
    .unwrap();
    assert_eq!(
        routine_of(&e, &db, rid).streak_counter,
        2,
        "the backdated moment is inside the grace window"
    );
    // A completion stamped at the click, as `CompleteTask` would, misses.
    let (e2, clock2) = clocked();
    let mut db2 = super::testutil::db();
    let (rid2, occ2) = seed_routine(&e2, &mut db2);
    set_clock(&clock2, (occ2[0].1 + 3_600_000) as u64);
    e2.apply(&mut db2, Command::CompleteTask(occ2[0].0))
        .unwrap();
    set_clock(&clock2, (occ2[1].1 + 48 * 3_600_000) as u64);
    e2.apply(&mut db2, Command::CompleteTask(occ2[1].0))
        .unwrap();
    assert_eq!(routine_of(&e2, &db2, rid2).streak_counter, 1);

    // The stats read the completion as written.
    let (e3, clock3) = clocked();
    let mut db3 = super::testutil::db();
    let t = add(&e3, &mut db3, "done yesterday", None, None);
    set_clock(&clock3, (NOW + 2 * DAY_MS) as u64);
    let yesterday = SunriseTime::all_day(jiff::civil::date(2023, 11, 15));
    triage(
        &e3,
        &mut db3,
        &[t],
        TriageAction::AlreadyDone {
            completed_at: yesterday,
        },
    )
    .unwrap();
    let QueryResult::MorningSummary(s) = e3
        .query(
            &db3,
            Query::MorningSummary {
                now_ms: (NOW + 2 * DAY_MS) as u64,
            },
        )
        .unwrap()
    else {
        panic!("summary");
    };
    assert_eq!(
        s.completed
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        ["done yesterday"]
    );
    assert!(s.completed[0]
        .completed_at
        .as_ref()
        .is_some_and(SunriseTime::is_all_day));
}

/// No code path but a user command changes a task's state, plan,
/// completion or acknowledgement (ADR-0047 §4): every read, the passage of
/// time, a zone change, materialization, compaction and a rebuild leave a
/// late task exactly as it was and write no task op.
#[test]
fn nothing_changes_a_task_without_a_command() {
    let (e, clock) = clocked();
    let mut db = db();
    let late = add(&e, &mut db, "late", None, Some(at(NOW + 3_600_000)));
    let stale = add(&e, &mut db, "stale", None, None);
    let before: Vec<Task> = [late, stale].map(|id| read_task_t(&e, &db, id)).into();
    let task_ops = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM ops WHERE inner_kind LIKE 'task.%'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    let ops = task_ops(&db);
    for days in [1, 15, 40] {
        let now = (NOW + days * DAY_MS) as u64;
        set_clock(&clock, now);
        for q in [
            Query::Triage,
            Query::TodayView {
                now_ms: now,
                contexts: vec![],
            },
            Query::Today {
                now_ms: now,
                contexts: vec![],
            },
            Query::Lateness {
                ids: vec![late, stale],
            },
            Query::MorningSummary { now_ms: now },
            Query::EndOfDayPlan { now_ms: now },
        ] {
            e.query(&db, q).unwrap();
        }
        let ny = jiff::tz::TimeZone::get("America/New_York").unwrap();
        e.time_zone_impact(&db, &jiff::tz::TimeZone::UTC, &ny, now)
            .unwrap();
        e.apply(&mut db, Command::MaterializeRoutines { now_ms: now })
            .unwrap();
        e.compact_op_log(&mut db, &super::compaction::policy())
            .unwrap();
    }
    e.rebuild_projection(&mut db).unwrap();
    assert_eq!(task_ops(&db), ops, "no task op was written");
    let after: Vec<Task> = [late, stale].map(|id| read_task_t(&e, &db, id)).into();
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(
            (
                &b.state,
                &b.planned_at,
                &b.completed_at,
                &b.late_acknowledged_at
            ),
            (
                &a.state,
                &a.planned_at,
                &a.completed_at,
                &a.late_acknowledged_at
            )
        );
    }
    assert!(queue(&e, &db).len() == 2, "both still need a decision");
}

/// Every op `from`'s own device logged, in the order it logged them.
fn own_ops(from: &Engine, from_db: &Db) -> Vec<Vec<u8>> {
    let device = from.keychain.device_id();
    let mut stmt = from_db
        .conn()
        .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY rowid")
        .unwrap();
    stmt.query_map(params![&device[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// A drop reason reaches a second device, and a peer that predates it (a
/// build that holds it as an unknown field) writes it back unchanged in the
/// full-state op of an edit it makes, so the reason survives the round trip
/// (ADR-0045 §6, #322).
#[test]
fn a_drop_reason_syncs_and_a_peer_that_predates_it_keeps_it() {
    const ROOT: [u8; 32] = [0x35; 32];
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    let t = add(&ea, &mut dba, "renew passport", None, None);
    set_clock(&ca, T0 + 1_000);
    triage(
        &ea,
        &mut dba,
        &[t],
        TriageAction::Drop {
            reason: Some("moved abroad".into()),
        },
    )
    .unwrap();
    for env in own_ops(&ea, &dba) {
        eb.apply_remote(&mut dbb, &env).unwrap();
    }
    let on_b = read_task_t(&eb, &dbb, t);
    assert_eq!(on_b.state, TaskState::Cancelled);
    assert_eq!(on_b.drop_reason(), Some("moved abroad"));

    // B, as a build before `DOC_SCHEMA_V` 21 would, holds the reason as an
    // unknown field, and retitles the task with a full-state op carrying it
    // back as it read it.
    let mut older = on_b.clone();
    let reason = older.cancel_reason.take().unwrap();
    older.unknown.insert(
        "cancel_reason".into(),
        sunrise_cbor::CborValue(ciborium::value::Value::Text(reason)),
    );
    older.title = "renew passport (abroad)".into();
    set_clock(&cb, T0 + 2_000);
    let (env, _) = super::field_merge::emit_patch(
        &eb,
        &mut dbb,
        &sunrise_domain::PRIVATE_DOMAIN_BYTES,
        &InnerOp::TaskUpdate(older),
    );
    ea.apply_remote(&mut dba, &env).unwrap();
    let on_a = read_task_t(&ea, &dba, t);
    assert_eq!(on_a.title, "renew passport (abroad)");
    assert_eq!(
        on_a.drop_reason(),
        Some("moved abroad"),
        "the reason survives"
    );
    assert!(on_a.unknown.is_empty(), "and is read as the field it is");
    ea.assert_rebuild_identity(&dba);
}

/// A stream is paused while `paused` is set and its expiry, if any, is
/// still ahead: a task in a live stream, or in one whose pause has run out,
/// is in the queue; one whose pause runs until later is not. A cancel of a
/// task already cancelled keeps its reason.
#[test]
fn a_pause_holds_until_its_expiry_and_a_re_cancel_keeps_the_reason() {
    let (e, clock) = clocked();
    let mut db = db();
    let stream = |e: &Engine, db: &mut Db, name: &str| {
        e.apply(
            db,
            Command::CreateStream(sunrise_domain::StreamDraft {
                name: name.into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity
    };
    let live = stream(&e, &mut db, "Live");
    let until_later = stream(&e, &mut db, "Later");
    let ran_out = stream(&e, &mut db, "Ran out");
    for (s, until) in [(until_later, NOW + 10 * DAY_MS), (ran_out, NOW + DAY_MS)] {
        e.apply(
            &mut db,
            Command::UpdateStream {
                id: s,
                patch: sunrise_domain::StreamPatch {
                    paused: Some(true),
                    paused_until: Some(Some(at(until))),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    }
    for (title, s) in [
        ("in live", live),
        ("in later", until_later),
        ("in ran out", ran_out),
    ] {
        let t = add(&e, &mut db, title, None, Some(at(NOW + 3_600_000)));
        e.apply(&mut db, Command::PromoteToStream { id: t, stream: s })
            .unwrap();
    }
    set_clock(&clock, (NOW + 2 * DAY_MS) as u64);
    let mut queued: Vec<String> = queue(&e, &db).into_iter().map(|r| r.task.title).collect();
    queued.sort();
    assert_eq!(queued, ["in live", "in ran out"]);
    // At the expiry itself the pause is over.
    set_clock(&clock, (NOW + 10 * DAY_MS) as u64);
    assert_eq!(queue(&e, &db).len(), 3);
    set_clock(&clock, (NOW + 10 * DAY_MS - 1) as u64);
    assert_eq!(queue(&e, &db).len(), 2);

    let t = add(&e, &mut db, "dropped", None, None);
    triage(
        &e,
        &mut db,
        &[t],
        TriageAction::Drop {
            reason: Some("why".into()),
        },
    )
    .unwrap();
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: t,
            patch: TaskPatch {
                state: Some(TaskState::Cancelled),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(read_task_t(&e, &db, t).drop_reason(), Some("why"));
}

// ---- one refusal, wherever it falls ----

/// How the one bad id in a batch is bad.
#[derive(Debug, Clone, Copy)]
enum Bad {
    /// No task by that id: refused while checking.
    Unknown,
    /// A deleted task: refused while checking.
    Deleted,
    /// A task the action's write cannot apply to, refused while planning:
    /// a done task cannot be dropped (`plan_update`), and a task whose hard
    /// constraint the defer target breaks cannot be deferred there
    /// (`plan_defer`).
    Unplannable,
}

/// A defer to `OUTSIDE_WINDOW_MS`, which `sample_constraint`'s hard 09:00 to
/// 17:00 window refuses.
fn defer_outside_the_window() -> TriageAction {
    TriageAction::Defer {
        to: DeferTarget::At(ms_to_ts(OUTSIDE_WINDOW_MS).into()),
    }
}

/// The task `bad` names for `action`, created in `db`.
fn bad_id(e: &Engine, db: &mut Db, bad: Bad, defer: bool) -> EntityRef {
    match bad {
        Bad::Unknown => EntityRef::new(EntityKind::Task, [0x99; 16]),
        Bad::Deleted => {
            let id = add(e, db, "gone", None, None);
            e.apply(db, Command::DeleteTask(id)).unwrap();
            id
        }
        Bad::Unplannable if defer => {
            e.apply(
                db,
                Command::CreateTask(TaskDraft {
                    title: "nine to five".into(),
                    scheduling_constraints: vec![sample_constraint()],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity
        }
        Bad::Unplannable => {
            let id = add(e, db, "done already", None, None);
            e.apply(db, Command::CompleteTask(id)).unwrap();
            id
        }
    }
}

/// Builds the batch: `good` fresh tasks with the bad one at `at`, and
/// returns it with every task id it holds that the vault has.
fn batch_with_one_bad(
    e: &Engine,
    db: &mut Db,
    good: usize,
    at: usize,
    bad: Bad,
    defer: bool,
) -> (Vec<EntityRef>, Vec<EntityRef>, EntityRef) {
    let mut ids: Vec<EntityRef> = (0..good)
        .map(|i| add(e, db, &format!("good {i}"), None, None))
        .collect();
    let wrong = bad_id(e, db, bad, defer);
    let held: Vec<EntityRef> = ids
        .iter()
        .copied()
        .chain((!matches!(bad, Bad::Unknown)).then_some(wrong))
        .collect();
    ids.insert(at.min(good), wrong);
    (ids, held, wrong)
}

/// The stored row of each of `ids`, a tombstoned one included.
fn rows(db: &Db, ids: &[EntityRef]) -> Vec<Task> {
    ids.iter()
        .map(|id| read_task(db.conn(), id.bytes()).unwrap().expect("held"))
        .collect()
}

/// A batch whose last id fails in planning, after every id before it was
/// checked and planned, still writes nothing: the refusal of a done task's
/// drop and of a defer into a hard constraint's gap each refuse the whole
/// batch.
#[test]
fn a_later_id_refused_in_planning_refuses_the_batch() {
    for defer in [false, true] {
        let (e, _clock) = clocked();
        let mut db = db();
        let (ids, held, wrong) = batch_with_one_bad(&e, &mut db, 3, 3, Bad::Unplannable, defer);
        assert_eq!(ids.last(), Some(&wrong), "the bad id is planned last");
        let ops = op_count(&db);
        let before: Vec<Task> = rows(&db, &held);
        let action = if defer {
            defer_outside_the_window()
        } else {
            TriageAction::Drop { reason: None }
        };
        let err = triage(&e, &mut db, &ids, action).unwrap_err();
        if defer {
            assert!(
                matches!(
                    err,
                    EngineError::Validation(
                        sunrise_domain::ValidationError::HardScheduleConstraint
                    )
                ),
                "{err:?}"
            );
        } else {
            assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
        }
        assert_eq!(op_count(&db), ops, "nothing written (defer: {defer})");
        let after: Vec<Task> = rows(&db, &held);
        assert_eq!(before, after, "no task row moved (defer: {defer})");
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        cases: 32,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/triage.txt",
            ),
        )),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// One bad id anywhere in a batch of good ones, refused while checking
    /// or while planning, refuses the batch: the op count and every task
    /// row are as they were. The same batch without it is one write of
    /// each task.
    #[test]
    fn one_bad_id_at_any_position_writes_nothing(
        good in 1usize..5,
        at in 0usize..5,
        bad in proptest::sample::select(vec![Bad::Unknown, Bad::Deleted, Bad::Unplannable]),
        defer in proptest::bool::ANY,
    ) {
        begin_case();
        let (e, _clock) = clocked();
        let mut db = db();
        let (ids, held, wrong) = batch_with_one_bad(&e, &mut db, good, at, bad, defer);
        let ops = op_count(&db);
        let before: Vec<Task> = rows(&db, &held);
        let action = || if defer {
            defer_outside_the_window()
        } else {
            TriageAction::Drop { reason: Some("not mine".into()) }
        };
        proptest::prop_assert!(triage(&e, &mut db, &ids, action()).is_err());
        proptest::prop_assert_eq!(op_count(&db), ops);
        let after: Vec<Task> = rows(&db, &held);
        proptest::prop_assert_eq!(before, after);

        let rest: Vec<EntityRef> = ids.into_iter().filter(|id| *id != wrong).collect();
        triage(&e, &mut db, &rest, action()).unwrap();
        proptest::prop_assert_eq!(op_count(&db), ops + i64::try_from(good).unwrap());
    }
}
