//! Contexts: their commands, their tasks and their convergence.

use super::*;

// ---- contexts ----

#[test]
fn created_context_is_listed_and_readable_by_id() {
    let mut db = db();
    let e = engine();
    let id = e
        .apply(
            &mut db,
            Command::CreateContext(ContextDraft {
                name: "  Errands ".into(),
                description: Some("out of the house".into()),
            }),
        )
        .unwrap()
        .entity;
    assert_eq!(id.kind(), EntityKind::Context);

    let rows = context_rows(&e, &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].name, "Errands", "the name is stored trimmed");
    assert_eq!(rows[0].description.as_deref(), Some("out of the house"));
    assert_eq!(rows[0].task_count, 0);
    assert!(!rows[0].archived);

    match e.query(&db, Query::EntityById(id)).unwrap() {
        QueryResult::Context(c) => assert_eq!(c.name, "Errands"),
        other => panic!("expected Context, got {other:?}"),
    }
}

#[test]
fn duplicate_and_invalid_context_names_are_rejected() {
    let mut db = db();
    let e = engine();
    new_context(&e, &mut db, "errands");

    // Same name, different case + padding: still the same handle to a user
    // typing `@errands`, so it must not create a second context.
    for dup in ["errands", "  ERRANDS  ", "Errands"] {
        let err = e
            .apply(
                &mut db,
                Command::CreateContext(ContextDraft {
                    name: dup.into(),
                    ..Default::default()
                }),
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Invalid(_)), "{dup}: {err:?}");
    }

    // Blank and over-length names never reach the store.
    for bad in [String::from("   "), "x".repeat(65)] {
        let err = e
            .apply(
                &mut db,
                Command::CreateContext(ContextDraft {
                    name: bad.clone(),
                    ..Default::default()
                }),
            )
            .unwrap_err();
        assert!(matches!(err, EngineError::Validation(_)), "{err:?}");
    }

    // A rename onto an existing name is refused the same way...
    let other = new_context(&e, &mut db, "home");
    let err = e
        .apply(
            &mut db,
            Command::UpdateContext {
                id: other,
                patch: ContextPatch {
                    name: Some("ERRANDS".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
    // ...but renaming a context to its own name is not a clash.
    e.apply(
        &mut db,
        Command::UpdateContext {
            id: other,
            patch: ContextPatch {
                name: Some("Home".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    assert_eq!(context_rows(&e, &db).len(), 2, "exactly two survived");
}

#[test]
fn reserved_prefix_names_are_accepted_verbatim() {
    // The spec calls `waiting-on:` / `energy:` conventions, not enforced
    // types — the store must not police them.
    let mut db = db();
    let e = engine();
    for n in ["waiting-on:carlos", "energy:high", "energy:whatever"] {
        new_context(&e, &mut db, n);
    }
    let names: Vec<String> = context_rows(&e, &db).into_iter().map(|r| r.name).collect();
    assert_eq!(
        names,
        vec!["energy:high", "energy:whatever", "waiting-on:carlos"]
    );
}

#[test]
fn deleting_a_context_removes_it_from_every_task_that_carries_it() {
    let mut db = db();
    let e = engine();
    let errands = new_context(&e, &mut db, "errands");
    let home = new_context(&e, &mut db, "home");

    let mk = |e: &Engine, db: &mut Db, title: &str, ctxs: Vec<EntityRef>| {
        e.apply(
            db,
            Command::CreateTask(TaskDraft {
                title: title.into(),
                contexts: ctxs,
                ..Default::default()
            }),
        )
        .unwrap()
        .entity
    };
    let both = mk(&e, &mut db, "buy milk", vec![errands, home]);
    let only_errands = mk(&e, &mut db, "post parcel", vec![errands]);
    let untagged = mk(&e, &mut db, "think", vec![]);

    let by_name = |rows: Vec<ContextRow>, n: &str| {
        rows.into_iter().find(|r| r.name == n).expect("row present")
    };
    assert_eq!(by_name(context_rows(&e, &db), "errands").task_count, 2);

    e.apply(&mut db, Command::DeleteContext(errands)).unwrap();

    // Gone from the list...
    let rows = context_rows(&e, &db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "home");
    // ...and gone from every task that carried it, while the *other*
    // context on the same task is untouched.
    assert_eq!(task_context_ids(&db, both), BTreeSet::from([*home.bytes()]));
    assert!(task_context_ids(&db, only_errands).is_empty());
    assert!(task_context_ids(&db, untagged).is_empty());
    assert_eq!(
        read_task_t(&e, &db, both).contexts,
        BTreeSet::from([home]),
        "the task projection agrees"
    );
    assert_eq!(by_name(context_rows(&e, &db), "home").task_count, 1);

    // The Today context filter no longer matches the purged context.
    match e
        .query(
            &db,
            Query::Today {
                now_ms: 1_700_000_000_000,
                contexts: vec![errands],
            },
        )
        .unwrap()
    {
        QueryResult::Tasks(t) => assert!(t.is_empty(), "no task still carries it"),
        other => panic!("expected Tasks, got {other:?}"),
    }
}

#[test]
fn context_tasks_lists_across_streams_and_survives_a_purge() {
    // A Context cuts across Streams, so its listing must too — that is
    // the whole difference between a Context and a Stream, and a client
    // cannot offer "stand in @errands" without it.
    let mut db = db();
    let e = engine();
    let errands = new_context(&e, &mut db, "errands");
    let work = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let inbox_task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "post a letter".into(),
                contexts: vec![errands],
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let work_task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "collect the parcel".into(),
                stream_id: Some(work),
                contexts: vec![errands],
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "untagged".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    let listed = |e: &Engine, db: &Db| match e.query(db, Query::ContextTasks(errands)).unwrap() {
        QueryResult::StreamTasks(t) => t.into_iter().map(|t| t.id).collect::<BTreeSet<_>>(),
        other => panic!("expected StreamTasks, got {other:?}"),
    };
    assert_eq!(listed(&e, &db), BTreeSet::from([inbox_task, work_task]));

    // Completed work stays listed: hiding it would make the picker's
    // count disagree with the list it opens.
    e.apply(&mut db, Command::CompleteTask(inbox_task)).unwrap();
    assert_eq!(listed(&e, &db), BTreeSet::from([inbox_task, work_task]));

    // Deleting a task drops it; deleting the Context empties the listing,
    // because the purge removes the tag from every task.
    e.apply(&mut db, Command::DeleteTask(inbox_task)).unwrap();
    assert_eq!(listed(&e, &db), BTreeSet::from([work_task]));
    e.apply(&mut db, Command::DeleteContext(errands)).unwrap();
    assert!(listed(&e, &db).is_empty());
}

#[test]
fn archiving_a_context_keeps_it_on_its_tasks() {
    // Per the spec, archiving only hides a Context from the picker.
    let mut db = db();
    let e = engine();
    let ctx = new_context(&e, &mut db, "errands");
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "buy milk".into(),
                contexts: vec![ctx],
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;

    e.apply(
        &mut db,
        Command::UpdateContext {
            id: ctx,
            patch: ContextPatch {
                archived: Some(true),
                ..Default::default()
            },
        },
    )
    .unwrap();

    let rows = context_rows(&e, &db);
    assert_eq!(rows.len(), 1, "archived contexts are still listed");
    assert!(rows[0].archived);
    assert_eq!(rows[0].task_count, 1);
    assert_eq!(read_task_t(&e, &db, task).contexts, BTreeSet::from([ctx]));
}

#[test]
fn context_commands_reject_a_non_context_id() {
    let mut db = db();
    let e = engine();
    let err = e
        .apply(&mut db, Command::DeleteContext(stream_ref(3)))
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
}

/// `PromoteToStream` takes two ids and checks the kind of both. The command
/// appeared nowhere in this module's tests; its only coverage was a CLI
/// happy path, which passes ids the CLI itself resolved and so never hands
/// it a mismatched pair. The crate already has this exact test for three
/// sibling families (Context, Block, Attachment).
///
/// What this pins is the command's observable contract, not the two
/// `require_kind` calls in `promote_task` itself: `update_task`, which it
/// delegates to, repeats both checks — `require_kind(id, Task)` on entry
/// and `require_kind(s, Stream)` on `patch.stream_id` — and returns the
/// same `Invalid` variant with the same message. Deleting either call from
/// `promote_task` alone changes nothing any caller can see. Deleting both
/// copies of either check does, and is what fails this test.
#[test]
fn promote_to_stream_rejects_either_id_of_the_wrong_kind() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "promote me");
    let stream = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Ops".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;

    // A Stream id where the Task belongs.
    let err = e
        .apply(&mut db, Command::PromoteToStream { id: stream, stream })
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");

    // A Task id where the Stream belongs.
    let err = e
        .apply(
            &mut db,
            Command::PromoteToStream {
                id: task,
                stream: task,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");

    // A Context id in the Stream slot: the kind is checked, not merely
    // "is it not a Task".
    let ctx = new_context(&e, &mut db, "errand");
    let err = e
        .apply(
            &mut db,
            Command::PromoteToStream {
                id: task,
                stream: ctx,
            },
        )
        .unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");

    // And the well-kinded pair still works, so the rejections above are the
    // guards and not a broken fixture.
    e.apply(&mut db, Command::PromoteToStream { id: task, stream })
        .unwrap();
    assert_eq!(read_task_t(&e, &db, task).stream_id, Some(stream));
}

#[test]
fn context_ops_seal_their_own_inner_op_variants() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let create = e
        .apply(
            &mut db,
            Command::CreateContext(ContextDraft {
                name: "errands".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    assert_eq!(inner_op_variant(&e, &db, &create.op_id), "ContextCreate");

    let upd = e
        .apply(
            &mut db,
            Command::UpdateContext {
                id: create.entity,
                patch: ContextPatch {
                    description: Some(Some("errand list".into())),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    assert_eq!(inner_op_variant(&e, &db, &upd.op_id), "ContextUpdate");

    let del = e
        .apply(&mut db, Command::DeleteContext(create.entity))
        .unwrap();
    assert_eq!(inner_op_variant(&e, &db, &del.op_id), "ContextDelete");
}

#[test]
fn remote_context_create_and_delete_converge() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    // A creates a context and tags a task with it; B receives both.
    let ctx = new_context(&ea, &mut dba, "errands");
    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "buy milk".into(),
                contexts: vec![ctx],
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;

    let create_env = env_for_kind(&dba, ctx.bytes(), "context.create");
    assert!(matches!(
        eb.apply_remote(&mut dbb, &create_env).unwrap(),
        Some(DomainEvent::Created(r)) if r == ctx
    ));
    eb.apply_remote(&mut dbb, &create_env_for(&dba, task.bytes()))
        .unwrap();

    let rows = context_rows(&eb, &dbb);
    assert_eq!(rows.len(), 1, "the context reached B");
    assert_eq!(rows[0].name, "errands");
    assert_eq!(rows[0].task_count, 1, "and B sees the membership");
    // Re-delivery is a no-op.
    assert!(eb.apply_remote(&mut dbb, &create_env).unwrap().is_none());

    // A renames it; B converges under LWW.
    set_clock(&ca, T0 + 1_000);
    ea.apply(
        &mut dba,
        Command::UpdateContext {
            id: ctx,
            patch: ContextPatch {
                name: Some("errands & chores".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let upd_env = env_for_kind(&dba, ctx.bytes(), "context.update");
    assert!(matches!(
        eb.apply_remote(&mut dbb, &upd_env).unwrap(),
        Some(DomainEvent::Updated(r)) if r == ctx
    ));
    assert_eq!(context_rows(&eb, &dbb)[0].name, "errands & chores");

    // A deletes it: B must both drop the context AND strip it from the task.
    set_clock(&ca, T0 + 2_000);
    ea.apply(&mut dba, Command::DeleteContext(ctx)).unwrap();
    let del_env = env_for_kind(&dba, ctx.bytes(), "context.delete");
    assert!(matches!(
        eb.apply_remote(&mut dbb, &del_env).unwrap(),
        Some(DomainEvent::Deleted(r)) if r == ctx
    ));
    assert!(context_rows(&eb, &dbb).is_empty(), "gone from B's list");
    assert!(
        task_context_ids(&dbb, task).is_empty(),
        "and stripped from B's copy of the task, without an extra task op"
    );
    assert_eq!(
        task_context_ids(&dba, task),
        task_context_ids(&dbb, task),
        "both replicas agree"
    );
}

/// A `ContextDelete` that overtakes its own `ContextCreate` materializes
/// the tombstone anyway.
///
/// This was the last delete op still missing its insert-when-absent branch.
/// The failure was worse than a plain dropped delete: the membership purge
/// is keyed on the id alone and ran regardless, so B stripped the context
/// off its tasks and then let the older create land the Context *alive* —
/// a live context nothing is tagged with, on a replica whose peer has no
/// such context at all, and no later op to reconcile them.
#[test]
fn a_context_delete_that_overtakes_its_create_still_lands_as_a_tombstone() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let ctx = new_context(&ea, &mut dba, "errands");
    set_clock(&ca, T0 + 1_000);
    ea.apply(&mut dba, Command::DeleteContext(ctx)).unwrap();

    // Delete first, create second.
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, ctx.bytes(), "context.delete"))
        .unwrap();
    let rb = read_context(dbb.conn(), ctx.bytes())
        .unwrap()
        .expect("the delete materialized a row of its own");
    assert!(rb.deleted);
    assert_eq!(rb.name, "errands", "and it carries the full state");

    eb.apply_remote(&mut dbb, &env_for_kind(&dba, ctx.bytes(), "context.create"))
        .unwrap();
    assert!(
        read_context(dbb.conn(), ctx.bytes())
            .unwrap()
            .unwrap()
            .deleted,
        "the older create loses LWW and does not resurrect it"
    );
    assert!(
        context_rows(&eb, &dbb).is_empty(),
        "and it stays out of the context list"
    );
    assert_eq!(
        row_stamp(&dba, "contexts", "id", &ctx),
        row_stamp(&dbb, "contexts", "id", &ctx),
        "both replicas agree on the winning stamp"
    );
}

#[test]
fn concurrent_context_renames_converge_to_one_name() {
    // Both replicas rename the same context at the same instant; the tie is
    // broken by device id, and both must land on the same winner.
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let ctx = new_context(&ea, &mut dba, "errands");
    let create_env = env_for_kind(&dba, ctx.bytes(), "context.create");
    eb.apply_remote(&mut dbb, &create_env).unwrap();

    set_clock(&ca, T0 + 5_000);
    set_clock(&cb, T0 + 5_000);
    let rename = |name: &str| Command::UpdateContext {
        id: ctx,
        patch: ContextPatch {
            name: Some(name.into()),
            ..Default::default()
        },
    };
    ea.apply(&mut dba, rename("from A")).unwrap();
    eb.apply(&mut dbb, rename("from B")).unwrap();

    let a_env = env_for_kind(&dba, ctx.bytes(), "context.update");
    let b_env = env_for_kind(&dbb, ctx.bytes(), "context.update");
    eb.apply_remote(&mut dbb, &a_env).unwrap();
    ea.apply_remote(&mut dba, &b_env).unwrap();

    let name_a = context_rows(&ea, &dba)[0].name.clone();
    let name_b = context_rows(&eb, &dbb)[0].name.clone();
    assert_eq!(name_a, name_b, "both replicas picked the same winner");
    assert!(name_a == "from A" || name_a == "from B", "{name_a}");
}
