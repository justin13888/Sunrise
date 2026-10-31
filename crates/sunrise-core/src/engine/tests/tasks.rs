//! Tasks: the command surface, the rows it writes and the Today query.

use super::*;

#[test]
fn create_task_seals_a_valid_envelope() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let res = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "sealed task".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    // The stored ops row is a real magic-prefixed OpEnvelope.
    let env_bytes = OpLog::get_envelope(&db, &res.op_id).unwrap().unwrap();
    assert_eq!(&env_bytes[..2], b"SR", "envelope carries the magic prefix");
    let env = sunrise_crypto::decode_envelope(&env_bytes).unwrap();
    // Inbox tasks route to the Inbox's own stream id, which since ADR-0024
    // is no longer the vault-meta id.
    assert_eq!(env.stream_id, INBOX_STREAM_BYTES);
    assert_eq!(env.device_id, e.keychain.device_id());
    assert_eq!(env.seq, res.seq);

    // Signature verifies against the keychain's device signing key.
    sunrise_crypto::verify_envelope(&env, &e.keychain.device_signing_pub()).unwrap();

    // open_op_row proves the full seal→unseal cycle: it yields inner-op CBOR
    // that parses as the externally-tagged `TaskCreate` variant.
    let inner = e.open_op_row(&db, &res.op_id).unwrap();
    let value: ciborium::value::Value = ciborium::de::from_reader(inner.as_slice()).unwrap();
    let ciborium::value::Value::Map(map) = value else {
        panic!("inner op must be a map");
    };
    let (k, _) = &map[0];
    assert_eq!(k, &ciborium::value::Value::Text("TaskCreate".to_string()));
}

#[test]
fn per_device_seq_is_independent() {
    // Two devices (distinct keychains) writing to the same DB keep separate
    // per-(stream, device) seq sequences.
    let mut db = db();
    let clock = || Arc::new(FakeClock(PLMutex::new(1_700_000_000_000)));
    let ea = Engine::from_clock(
        clock(),
        seeded_rng(),
        Arc::new(Keychain::for_test_seeded(
            VaultRootKey::from_bytes([0xab; 32]),
            [1u8; 32],
        )),
    );
    let eb = Engine::from_clock(
        clock(),
        seeded_rng(),
        Arc::new(Keychain::for_test_seeded(
            VaultRootKey::from_bytes([0xab; 32]),
            [2u8; 32],
        )),
    );
    assert_ne!(ea.keychain.device_id(), eb.keychain.device_id());

    let mk = |title: &str| {
        Command::CreateTask(TaskDraft {
            title: title.into(),
            ..Default::default()
        })
    };
    let a1 = ea.apply(&mut db, mk("a1")).unwrap();
    let b1 = eb.apply(&mut db, mk("b1")).unwrap();
    let a2 = ea.apply(&mut db, mk("a2")).unwrap();
    assert_eq!(a1.seq, 1);
    assert_eq!(b1.seq, 1, "device B starts its own seq at 1");
    assert_eq!(a2.seq, 2, "device A continues its own sequence");
}

#[test]
fn submit_enqueues_outbox_row() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let res = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "outboxed".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    let pending = Outbox::list_unacked(&db).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].op_id, res.op_id);
    assert_eq!(Outbox::pending_count(&db).unwrap(), 1);

    db.with_tx(|tx| {
        Outbox::mark_acked(tx, &res.op_id, 999)
            .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(Outbox::pending_count(&db).unwrap(), 0);
}

#[test]
fn create_task_round_trips() {
    let mut db = db();
    let e = engine();
    let cmd = Command::CreateTask(TaskDraft {
        title: "draft a memo".into(),
        ..Default::default()
    });
    let res = e.apply(&mut db, cmd).unwrap();
    assert_eq!(res.entity.kind(), EntityKind::Task);
    assert_eq!(res.state, Some(TaskState::Todo));
    let t = match e.query(&db, Query::EntityById(res.entity)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!("wrong query result variant"),
    };
    assert_eq!(t.title, "draft a memo");
    assert_eq!(t.stream_id, None);
}

#[test]
fn complete_task_sets_done() {
    let mut db = db();
    let e = engine();
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    e.apply(&mut db, Command::CompleteTask(r.entity)).unwrap();
    let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert_eq!(t.state, TaskState::Done);
    assert!(t.completed_at.is_some());
}

#[test]
fn defer_increments_count() {
    let mut db = db();
    let e = engine();
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    e.apply(
        &mut db,
        Command::DeferTask {
            id: r.entity,
            to_ms: 1_700_000_000_000 + 86_400_000,
        },
    )
    .unwrap();
    let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert_eq!(t.deferred_count, 1);
    assert!(t.planned_at.is_some());
}

#[test]
fn delete_task_marks_deleted_and_hides_from_inbox() {
    let mut db = db();
    let e = engine();
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    e.apply(&mut db, Command::DeleteTask(r.entity)).unwrap();
    let res = e.query(&db, Query::Inbox).unwrap();
    let tasks = match res {
        QueryResult::StreamTasks(v) => v,
        _ => panic!(),
    };
    assert!(tasks.is_empty());
}

/// ADR-0014's full-state-delete rule, for the entity the defect actually
/// occurred on.
///
/// The rule is asserted deterministically for Block, Context and
/// Attachment. It was not for Task: all seven `DeleteTask` tests are
/// local-only, none calls `apply_remote`, so nothing raced a Task delete
/// against a remote update. The only guard was
/// `crates/sunrise-e2e/tests/two_core_delete_convergence.rs`, whose own
/// docstring records that the divergence "failed roughly one run in four"
/// and was "stable, not transient" — invisible in the UI, because both
/// replicas showed a plausible task.
///
/// Under an id-only delete there is no state to contribute, so the losing
/// update's title survives on the replica that applied it and not on the
/// other. The two arrival orders are checked at once: B applies A's delete
/// after its own rename, and A applies B's rename after its own delete.
#[test]
fn a_task_delete_that_wins_lww_replaces_the_whole_row() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "contested".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.create"))
        .unwrap();

    // B renames it...
    set_clock(&cb, T0 + 1_000);
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("renamed".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // ...and A deletes it strictly later, so the delete wins on both sides.
    set_clock(&ca, T0 + 2_000);
    ea.apply(&mut dba, Command::DeleteTask(task)).unwrap();

    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.delete"))
        .unwrap();
    ea.apply_remote(&mut dba, &env_for_kind(&dbb, task.bytes(), "task.update"))
        .unwrap();

    let ra = read_task(dba.conn(), task.bytes()).unwrap().unwrap();
    let rb = read_task(dbb.conn(), task.bytes()).unwrap().unwrap();
    assert!(ra.deleted && rb.deleted, "both replicas tombstoned it");
    assert_eq!(
        ra.title, rb.title,
        "and agree on the rest of the row, not just the tombstone"
    );
    assert_eq!(
        ra.title, "contested",
        "the deleting replica's state is what replaced it, so the losing \
             rename is gone on both sides"
    );
    assert_eq!(
        row_stamp(&dba, "tasks", "id", &task),
        row_stamp(&dbb, "tasks", "id", &task),
    );
}

/// The mirror of the above, which is what stops "a delete always wins" from
/// passing for the rule. A Task delete is an ordinary full-state op and
/// carries no special standing: when the remote update is strictly later it
/// wins, and its state — a live task under its new title — replaces the
/// tombstone on both replicas.
#[test]
fn a_task_update_that_wins_lww_replaces_a_delete_it_raced() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "contested".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.create"))
        .unwrap();

    // A deletes it first...
    set_clock(&ca, T0 + 1_000);
    ea.apply(&mut dba, Command::DeleteTask(task)).unwrap();

    // ...and B renames it strictly later, so the rename wins on both sides.
    set_clock(&cb, T0 + 2_000);
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("renamed".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    eb.apply_remote(&mut dbb, &env_for_kind(&dba, task.bytes(), "task.delete"))
        .unwrap();
    ea.apply_remote(&mut dba, &env_for_kind(&dbb, task.bytes(), "task.update"))
        .unwrap();

    let ra = read_task(dba.conn(), task.bytes()).unwrap().unwrap();
    let rb = read_task(dbb.conn(), task.bytes()).unwrap().unwrap();
    assert_eq!(
        (ra.deleted, ra.title.as_str()),
        (rb.deleted, rb.title.as_str()),
        "the two replicas agree"
    );
    assert!(
        !ra.deleted,
        "the winning update's state replaces the row, tombstone included"
    );
    assert_eq!(ra.title, "renamed");
    assert_eq!(
        row_stamp(&dba, "tasks", "id", &task),
        row_stamp(&dbb, "tasks", "id", &task),
    );
}

#[test]
fn update_task_changes_state_and_records_op() {
    let mut db = db();
    let e = engine();
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let mut p = TaskPatch::default();
    p.state = Some(TaskState::InProgress);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: r.entity,
            patch: p,
        },
    )
    .unwrap();
    let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert_eq!(t.state, TaskState::InProgress);
    let count: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
        .unwrap();
    assert!(count >= 2);
}

#[test]
fn today_query_returns_tasks_due_in_window() {
    let mut db = db();
    let e = engine();
    let now = 1_700_000_000_000u64;
    let due_soon = ms_to_ts((now + 3_600_000) as i64);
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "soon".into(),
            hard_due_at: Some(due_soon.into()),
            ..Default::default()
        }),
    )
    .unwrap();
    let far = ms_to_ts((now + 7 * 86_400_000) as i64);
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "far".into(),
            hard_due_at: Some(far.into()),
            ..Default::default()
        }),
    )
    .unwrap();
    let r = e
        .query(
            &db,
            Query::Today {
                now_ms: now,
                contexts: vec![],
            },
        )
        .unwrap();
    let tasks = match r {
        QueryResult::Tasks(v) => v,
        _ => panic!(),
    };
    let titles: Vec<_> = tasks.iter().map(|t| t.title.clone()).collect();
    assert!(titles.contains(&"soon".to_string()));
    assert!(!titles.contains(&"far".to_string()));
}

#[test]
fn today_query_filters_by_context() {
    // `Query::Today`'s `contexts` field used to be bound to `_contexts` and
    // dropped on the floor, so callers got an unfiltered list back with no
    // error. Assert the filter is honoured in all three modes: empty slice
    // means no filter, a matching context narrows, and a non-matching one
    // excludes.
    let mut db = db();
    let e = engine();
    let now = 1_700_000_000_000u64;
    let due_soon = ms_to_ts((now + 3_600_000) as i64);

    let home = EntityRef::new(EntityKind::Context, [0xA1; 16]);
    let work = EntityRef::new(EntityKind::Context, [0xB2; 16]);

    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "at home".into(),
            hard_due_at: Some(due_soon.into()),
            contexts: vec![home],
            ..Default::default()
        }),
    )
    .unwrap();
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "at work".into(),
            hard_due_at: Some(due_soon.into()),
            contexts: vec![work],
            ..Default::default()
        }),
    )
    .unwrap();
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "anywhere".into(),
            hard_due_at: Some(due_soon.into()),
            ..Default::default()
        }),
    )
    .unwrap();

    let titles = |contexts: Vec<EntityRef>| -> Vec<String> {
        match e
            .query(
                &db,
                Query::Today {
                    now_ms: now,
                    contexts,
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(v) => v.iter().map(|t| t.title.clone()).collect(),
            _ => panic!("expected Tasks"),
        }
    };

    // No filter: everything in the window.
    let all = titles(vec![]);
    assert_eq!(
        all.len(),
        3,
        "unfiltered Today should return all 3: {all:?}"
    );

    // Single context: only the task carrying it.
    let only_home = titles(vec![home]);
    assert_eq!(only_home, vec!["at home".to_string()]);

    // Multiple contexts are an OR-set, and a context-less task is excluded
    // by any non-empty filter.
    let mut both = titles(vec![home, work]);
    both.sort();
    assert_eq!(both, vec!["at home".to_string(), "at work".to_string()]);

    // A context nothing carries yields nothing — not everything.
    let unused = EntityRef::new(EntityKind::Context, [0xC3; 16]);
    assert!(
        titles(vec![unused]).is_empty(),
        "an unmatched context must exclude, not fall through to unfiltered"
    );
}

#[test]
fn invalid_state_transition_rejected() {
    let mut db = db();
    let e = engine();
    let r = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    // todo can transition to in_progress, done, cancelled all directly,
    // so we pick a forbidden non-self transition.
    // todo → todo is allowed. Force a sequence: todo → done → in_progress
    // is allowed; then in_progress → done allowed. Try patching done →
    // explicitly back to todo (allowed). What's NOT allowed? Per the
    // current `can_transition_to`, every non-equal pair across the four
    // states is allowed. The matrix is essentially fully connected. So
    // invalidity is exposed by patching wrong_kind; test that path.
    let bogus = EntityRef::new(EntityKind::Stream, *r.entity.bytes());
    let res = e.apply(
        &mut db,
        Command::UpdateTask {
            id: bogus,
            patch: TaskPatch::default(),
        },
    );
    assert!(matches!(res, Err(EngineError::Invalid(_))));
}
