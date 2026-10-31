//! The activity feed and the timelines read back from the op log.

use super::*;

#[test]
fn an_activity_feed_reports_the_order_things_actually_happened_in() {
    // `fold_activity` is a state machine over successive full-state
    // snapshots, so the order it reads ops in *is* the history it reports.
    // Ordering by `op_id` sorted same-millisecond ops by the random low
    // bits of a ULID — and a defer-then-complete pair read backwards does
    // not come out merely reordered, it fabricates a **reopen** that never
    // happened.
    //
    // The clock is fixed, so every op here shares `ts_ms` and the tie is
    // always taken. Twelve independent tasks make the old behaviour's
    // chance of passing (1/6)^12.
    let mut db = db();
    let e = engine();
    for i in 0..12 {
        let id = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: format!("task {i}"),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(
            &mut db,
            Command::DeferTask {
                id,
                to_ms: 1_700_000_000_000 + 86_400_000,
            },
        )
        .unwrap();
        e.apply(&mut db, Command::CompleteTask(id)).unwrap();

        let events = match e
            .query(
                &db,
                Query::ActivityTimeline {
                    entity: id,
                    limit: 50,
                },
            )
            .unwrap()
        {
            QueryResult::Activity(rows) => rows,
            other => panic!("expected Activity, got {other:?}"),
        };
        // Newest first.
        let verbs: Vec<&str> = events.iter().map(|ev| ev.kind.verb()).collect();
        assert_eq!(
            verbs,
            vec!["task.completed", "task.deferred", "task.created"],
            "task {i}: the feed must not invent a transition"
        );
    }
}

// -----------------------------------------------------------------------
// Reviews and stats (`docs/08-features/reviews-and-stats.md`)
//
// These go through the *real* pipeline — every fact the folds see has been
// sealed into an envelope, written to the op log, and decrypted back out —
// so they prove the op log really is a usable source of truth for history,
// not just that the pure folds add up.
// -----------------------------------------------------------------------

#[test]
fn activity_timeline_reads_the_op_log_back_through_the_envelope_seal() {
    let (e, mut db, clock) = review_fixture();
    let task = review_task(&e, &mut db, "write the RFC", None);

    set_clock(&clock, REVIEW_MON + REVIEW_DAY);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("write the RFC properly".into()),
                priority: Some(Some(2)),
                ..Default::default()
            },
        },
    )
    .unwrap();

    set_clock(&clock, REVIEW_MON + 2 * REVIEW_DAY);
    e.apply(
        &mut db,
        Command::DeferTask {
            id: task,
            to_ms: REVIEW_MON + 5 * REVIEW_DAY,
        },
    )
    .unwrap();

    set_clock(&clock, REVIEW_MON + 3 * REVIEW_DAY);
    e.apply(&mut db, Command::CompleteTask(task)).unwrap();

    // Newest first, one event per user action, and the field edit is
    // summarized rather than expanded.
    let feed = timeline(&e, &db, task);
    let verbs: Vec<&str> = feed.iter().map(|f| f.kind.verb()).collect();
    assert_eq!(
        verbs,
        vec![
            "task.completed",
            "task.deferred",
            "task.updated",
            "task.created"
        ]
    );
    assert_eq!(
        feed[2].kind,
        ActivityKind::TaskUpdated { fields: 2 },
        "title + priority, and nothing else"
    );
    assert_eq!(feed[0].at_ms, REVIEW_MON + 3 * REVIEW_DAY);
    assert_eq!(
        feed[0].device,
        e.keychain().device_id(),
        "the feed says which device did it"
    );
}

#[test]
fn a_focus_session_shows_up_on_the_tasks_timeline() {
    let (e, mut db, clock) = review_fixture();
    let task = review_task(&e, &mut db, "deep work", None);

    set_clock(&clock, REVIEW_MON + 3_600_000);
    let session = e
        .apply(
            &mut db,
            Command::StartFocus(FocusStartDraft {
                task_id: task,
                kind: FocusKind::Work,
                length: SessionLength::OnePomodoro,
                energy: None,
            }),
        )
        .unwrap()
        .entity;
    set_clock(&clock, REVIEW_MON + 3_600_000 + POMODORO_MS);
    e.apply(
        &mut db,
        Command::EndFocus {
            session,
            actual_focused_ms: Some(POMODORO_MS),
            completed_task: false,
        },
    )
    .unwrap();

    let feed = timeline(&e, &db, task);
    assert_eq!(
        feed.iter().map(|f| f.kind.verb()).collect::<Vec<_>>(),
        vec!["focus.ended", "focus.started", "task.created"],
    );
    assert_eq!(
        feed[0].kind,
        ActivityKind::FocusEnded {
            session,
            focused_ms: POMODORO_MS,
            completed_task: false,
        }
    );
}

#[test]
fn a_streams_timeline_covers_the_tasks_that_live_in_it() {
    let (e, mut db, clock) = review_fixture();
    let stream = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    set_clock(&clock, REVIEW_MON + 1_000);
    review_task(&e, &mut db, "in the stream", Some(stream));
    review_task(&e, &mut db, "also in the stream", Some(stream));
    review_task(&e, &mut db, "in the inbox", None);

    let feed = timeline(&e, &db, stream);
    assert_eq!(
        feed.iter()
            .filter(|f| f.kind == ActivityKind::TaskCreated)
            .count(),
        2,
        "only the stream's own tasks: {feed:#?}"
    );
    assert!(feed.iter().any(|f| f.kind == ActivityKind::StreamCreated));
}

#[test]
fn a_routine_generated_task_never_appears_as_user_activity() {
    let (e, mut db, _clock) = review_fixture();
    let stream = stream_ref(7);
    let rid = e
        .apply(
            &mut db,
            Command::CreateRoutine(routine_draft(
                stream,
                "FREQ=DAILY",
                REVIEW_MON as i64,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            )),
        )
        .unwrap()
        .entity;
    // Materialization produced tasks; none of them are user activity.
    let generated: Vec<[u8; 16]> = live_task_ids(&db).into_iter().collect();
    assert!(!generated.is_empty(), "the routine materialized something");
    for raw in generated {
        let feed = timeline(&e, &db, EntityRef::new(EntityKind::Task, raw));
        assert!(
            !feed.iter().any(|f| f.kind == ActivityKind::TaskCreated),
            "routine generation is not user activity"
        );
    }
    assert_eq!(rid.kind(), EntityKind::Routine);
}

#[test]
fn activity_timeline_rejects_an_entity_that_has_no_feed() {
    let (e, db, _clock) = review_fixture();
    let err = e
        .query(
            &db,
            Query::ActivityTimeline {
                entity: EntityRef::new(EntityKind::Context, [1u8; 16]),
                limit: 10,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "got {err:?}");
}
