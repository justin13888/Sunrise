//! Streams: their commands, order, counts and the fields they carry.

use super::*;

#[test]
fn create_stream_round_trips() {
    let mut db = db();
    let e = engine();
    let s = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                color: Some(StreamColor::Sky),
                ..Default::default()
            }),
        )
        .unwrap();
    let st = match e.query(&db, Query::EntityById(s.entity)).unwrap() {
        QueryResult::Stream(s) => *s,
        _ => panic!(),
    };
    assert_eq!(st.archived, false);
    // Name and color must round-trip through storage (regression: these
    // used to come back as "" / Slate).
    assert_eq!(st.name, "Work");
    assert_eq!(st.color, StreamColor::Sky);
}

#[test]
fn update_stream_persists_name_and_color() {
    let mut db = db();
    let e = engine();
    let s = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                color: Some(StreamColor::Sky),
                ..Default::default()
            }),
        )
        .unwrap();
    let patch = StreamPatch {
        name: Some("Personal".into()),
        color: Some(StreamColor::Emerald),
        ..Default::default()
    };
    e.apply(
        &mut db,
        Command::UpdateStream {
            id: s.entity,
            patch,
        },
    )
    .unwrap();
    let st = match e.query(&db, Query::EntityById(s.entity)).unwrap() {
        QueryResult::Stream(s) => *s,
        _ => panic!(),
    };
    assert_eq!(st.name, "Personal");
    assert_eq!(st.color, StreamColor::Emerald);
}

/// A new stream lands after the last existing one, and moving one rewrites
/// exactly that one row.
#[test]
fn a_reorder_moves_one_stream_and_touches_no_sibling() {
    let mut db = db();
    let e = engine();
    let mut ids = Vec::new();
    for name in ["one", "two", "three"] {
        ids.push(
            e.apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: name.into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity,
        );
    }
    let listed = |e: &Engine, db: &Db| -> Vec<(String, String)> {
        match e.query(db, Query::StreamList).unwrap() {
            QueryResult::Streams(rows) => rows
                .iter()
                .map(|r| (r.name.clone(), r.sort_order.clone()))
                .collect(),
            other => panic!("expected Streams, got {other:?}"),
        }
    };
    let before = listed(&e, &db);
    assert_eq!(
        before.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["one", "two", "three"],
        "a new stream defaults to the end of the list"
    );

    // Drag "three" to the front: its new key is the one between the start
    // of the list and "one".
    let key = sort_order::between(None, Some(&before[0].1)).unwrap();
    e.apply(
        &mut db,
        Command::UpdateStream {
            id: ids[2],
            patch: StreamPatch {
                sort_order: Some(key.clone()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    let after = listed(&e, &db);
    assert_eq!(
        after.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["three", "one", "two"]
    );
    // The whole point of the fractional index: the two rows that did not
    // move still hold the keys they held before the drag.
    assert_eq!(after[1].1, before[0].1, "\"one\" was rewritten");
    assert_eq!(after[2].1, before[1].1, "\"two\" was rewritten");
    assert_eq!(after[0].1, key);
}

/// A key the core cannot place is refused rather than stored. Without this
/// one bad write would be permanent: nothing sorts after `"a0"`, so every
/// later append would land above it forever.
#[test]
fn a_sort_key_outside_the_alphabet_is_rejected() {
    let mut db = db();
    let e = engine();
    let s = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    for bad in ["a0", "", "NA"] {
        let res = e.apply(
            &mut db,
            Command::UpdateStream {
                id: s,
                patch: StreamPatch {
                    sort_order: Some(bad.into()),
                    ..Default::default()
                },
            },
        );
        assert!(
            matches!(res, Err(EngineError::Validation(_))),
            "{bad:?} was accepted"
        );
    }
    let st = match e.query(&db, Query::EntityById(s)).unwrap() {
        QueryResult::Stream(s) => *s,
        other => panic!("expected Stream, got {other:?}"),
    };
    assert!(sort_order::is_valid(&st.sort_order));
}

/// The private key domain, once the Inbox sentinel stream, is not a Stream
/// (ADR-0046 §2): no stream command may delete, edit or nest under it.
#[test]
fn the_private_domain_is_not_a_stream_any_stream_command_takes() {
    let mut db = db();
    let e = engine();
    let private = inbox_stream_ref();
    let res = e.apply(
        &mut db,
        Command::DeleteStream {
            id: private,
            disposition: sunrise_domain::Disposition::Detach,
        },
    );
    assert!(matches!(res, Err(EngineError::ReservedStream)), "{res:?}");
    let res = e.apply(
        &mut db,
        Command::UpdateStream {
            id: private,
            patch: StreamPatch {
                name: Some("Mine".into()),
                ..Default::default()
            },
        },
    );
    assert!(matches!(res, Err(EngineError::ReservedStream)), "{res:?}");
    let res = e.apply(
        &mut db,
        Command::CreateStream(StreamDraft {
            name: "Child".into(),
            parent_id: Some(private),
            ..Default::default()
        }),
    );
    assert!(matches!(res, Err(EngineError::ReservedStream)), "{res:?}");
    let work = seed_stream(&mut db, "Work");
    let res = e.apply(
        &mut db,
        Command::DeleteStream {
            id: work,
            disposition: sunrise_domain::Disposition::MoveTo(private),
        },
    );
    assert!(
        matches!(res, Err(EngineError::ReservedStream)),
        "a move into it is spelled Detach: {res:?}"
    );
}

#[test]
fn stream_list_counts_open_tasks_and_ordering() {
    let mut db = db();
    let e = engine();

    // Two real streams. "Beta" is created first, and stays first: a new
    // stream is appended after the last existing key, so an untouched
    // vault lists streams in the order they were made. Name order was the
    // rule only while `sort_order` was hardcoded and every row tied.
    let beta = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Beta".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let alpha = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "alpha".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let gone = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Deleted".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(
        &mut db,
        Command::DeleteStream {
            id: gone,
            disposition: sunrise_domain::Disposition::Detach,
        },
    )
    .unwrap();

    // Inbox: 1 open + 1 done (done not counted).
    e.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "inbox open".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    let inbox_done = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "inbox done".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(&mut db, Command::CompleteTask(inbox_done)).unwrap();

    // alpha: 2 open (one todo, one in-progress), 1 cancelled, 1 deleted.
    for _ in 0..2 {
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "a open".into(),
                stream_id: Some(alpha),
                ..Default::default()
            }),
        )
        .unwrap();
    }
    let a_ip = db_last_task(&db, alpha);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: a_ip,
            patch: TaskPatch {
                state: Some(TaskState::InProgress),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let a_cancel = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "a cancel".into(),
                stream_id: Some(alpha),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: a_cancel,
            patch: TaskPatch {
                state: Some(TaskState::Cancelled),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let a_del = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "a del".into(),
                stream_id: Some(alpha),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(&mut db, Command::DeleteTask(a_del)).unwrap();

    // beta: 0 open tasks.
    let rows = match e.query(&db, Query::StreamList).unwrap() {
        QueryResult::Streams(v) => v,
        _ => panic!("wrong variant"),
    };

    // The Inbox is a view, not a stream (ADR-0046 §3): it has no row here,
    // and its count is the view's. The real (non-deleted) streams come in
    // fractional-index order, which for a vault nobody has reordered is
    // creation order. Deleted stream excluded entirely.
    let names: Vec<_> = rows.iter().map(|r| r.name.clone()).collect();
    assert_eq!(names, vec!["Beta", "alpha"]);
    assert!(!rows.iter().any(|r| r.id == inbox_stream_ref()));
    let QueryResult::StreamTasks(inbox) = e.query(&db, Query::Inbox).unwrap() else {
        panic!("wrong variant");
    };
    assert_eq!(inbox.len(), 1, "the Inbox view holds the one open task");

    // The keys really are ascending and really are keys — the sidebar
    // computes new positions from these, so a row arriving without one
    // would break the very next drag.
    let keys: Vec<_> = rows.iter().map(|r| r.sort_order.clone()).collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
    assert!(keys.iter().all(|k| sort_order::is_valid(k)));

    let alpha_row = rows.iter().find(|r| r.id == alpha).unwrap();
    assert_eq!(alpha_row.open_task_count, 2);
    let beta_row = rows.iter().find(|r| r.id == beta).unwrap();
    assert_eq!(beta_row.open_task_count, 0);
}

/// Two devices reorder the same list while apart. They converge — and they
/// converge on **one device's arrangement**, not on a merge of the two.
///
/// This is `docs/02-domain/streams.md` §Merge mapping made executable.
/// `sort_order` is one per-field LWW register per Stream (ADR-0044), so the
/// losing device's drag is discarded whole. If the field ever merges by
/// another CRDT type, this test is where the change shows up first.
#[test]
fn concurrent_stream_reorders_converge_on_one_arrangement_not_a_merge() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    // One stream, known to both.
    let s = ea
        .apply(
            &mut dba,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, s.bytes(), "stream.create"))
        .unwrap();

    // Both devices drag it somewhere, at the same instant, in ignorance of
    // each other. The tie breaks on device id.
    set_clock(&ca, T0 + 5_000);
    set_clock(&cb, T0 + 5_000);
    let move_to = |key: &str| Command::UpdateStream {
        id: s,
        patch: StreamPatch {
            sort_order: Some(key.into()),
            ..Default::default()
        },
    };
    ea.apply(&mut dba, move_to("B")).unwrap();
    eb.apply(&mut dbb, move_to("Y")).unwrap();

    let a_env = env_for_kind(&dba, s.bytes(), "stream.update");
    let b_env = env_for_kind(&dbb, s.bytes(), "stream.update");
    eb.apply_remote(&mut dbb, &a_env).unwrap();
    ea.apply_remote(&mut dba, &b_env).unwrap();

    let key_of = |e: &Engine, db: &Db| match e.query(db, Query::StreamList).unwrap() {
        QueryResult::Streams(rows) => rows
            .iter()
            .find(|r| r.id == s)
            .expect("the stream is still there")
            .sort_order
            .clone(),
        other => panic!("expected Streams, got {other:?}"),
    };
    let a = key_of(&ea, &dba);
    let b = key_of(&eb, &dbb);
    assert_eq!(a, b, "the two replicas disagree about the order");
    // Exactly one of the two drags survived. Not something between them:
    // there is no interleave to land on, which is the warning the spec
    // carries and the behaviour it describes.
    assert!(a == "B" || a == "Y", "invented a third position: {a}");
}

/// `Stream.icon` was `Option<&'static str>` with
/// `#[serde(skip_deserializing)]`, so it ALWAYS read back as `None`:
/// setting an icon survived exactly as long as the process that set it,
/// on every device including the one that set it. The projection had no
/// column for it either, so this asserts both halves.
#[test]
fn a_streams_icon_survives_being_written() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);

    let created = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                description: None,
                color: None,
                parent_id: None,
                review_cadence: None,
                reminder_lead_s: None,
                icon: None,
                default_context: None,
            }),
        )
        .unwrap();

    let mut stream = read_stream(db.conn(), created.entity.bytes())
        .unwrap()
        .expect("stream exists");
    stream.icon = Some("briefcase".into());
    log_and_fold(&e, &mut db, &InnerOp::StreamUpdate(stream), &META_STREAM);

    let back = read_stream(db.conn(), created.entity.bytes())
        .unwrap()
        .expect("stream exists");
    assert_eq!(back.icon.as_deref(), Some("briefcase"));

    // ...and it survives the wire, which the borrowed type made impossible.
    let bytes = sunrise_cbor::encode_canonical(&back).unwrap();
    let decoded: sunrise_domain::Stream = sunrise_cbor::decode_canonical(&bytes).unwrap();
    assert_eq!(decoded.icon.as_deref(), Some("briefcase"));
}

/// `Stream.description` was accepted, carried in the op, and never stored.
///
/// `create_stream` copied it onto the entity and `update_stream` applied
/// the patch, so it reached every peer — and `read_stream` hardcoded
/// `description: None`, so no replica could materialize it. The second
/// half is what made it destructive rather than merely absent: the next
/// `UpdateStream` read `None` and re-emitted `description: null`, and
/// under entity-level LWW that erased the value on every replica that
/// still had it. The seam exposed the field the whole time, so a client
/// could set it, watch it vanish, and be right.
///
/// `default_context` had no column, no patch field and no draft field.
/// `icon` had a column that round-tripped and no way to reach it: the only
/// writer in the tree was a test issuing direct SQL, which is why the gap
/// outlived the test that was supposed to cover it. This one goes through
/// the command surface for exactly that reason.
#[test]
fn stream_description_icon_and_default_context_survive_the_command_surface() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);

    let ctx = e
        .apply(
            &mut db,
            Command::CreateContext(ContextDraft {
                name: "deep-work".into(),
                description: None,
            }),
        )
        .unwrap()
        .entity;

    let created = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                description: Some(sunrise_domain::NoteBody(b"the big rewrite".to_vec())),
                color: None,
                parent_id: None,
                review_cadence: None,
                reminder_lead_s: None,
                icon: Some("briefcase".into()),
                default_context: Some(ctx),
            }),
        )
        .unwrap()
        .entity;

    let got = read_stream(db.conn(), created.bytes()).unwrap().unwrap();
    assert_eq!(
        got.description.as_ref().map(|b| b.0.clone()),
        Some(b"the big rewrite".to_vec()),
        "a description set at create must reach the projection"
    );
    assert_eq!(got.icon.as_deref(), Some("briefcase"));
    assert_eq!(got.default_context, Some(ctx));

    // The erasure. An unrelated edit re-emits the whole entity, so if the
    // projection had forgotten the description this update would carry
    // `null` and win on every replica.
    e.apply(
        &mut db,
        Command::UpdateStream {
            id: created,
            patch: StreamPatch {
                name: Some("Work (2026)".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    let after = read_stream(db.conn(), created.bytes()).unwrap().unwrap();
    assert_eq!(after.name, "Work (2026)");
    assert_eq!(
        after.description.as_ref().map(|b| b.0.clone()),
        Some(b"the big rewrite".to_vec()),
        "an unrelated edit must not erase the description"
    );
    assert_eq!(after.icon.as_deref(), Some("briefcase"));
    assert_eq!(after.default_context, Some(ctx));

    // And each is settable and clearable through the patch.
    e.apply(
        &mut db,
        Command::UpdateStream {
            id: created,
            patch: StreamPatch {
                icon: Some(Some("rocket".into())),
                default_context: Some(None),
                description: Some(None),
                ..Default::default()
            },
        },
    )
    .unwrap();

    let cleared = read_stream(db.conn(), created.bytes()).unwrap().unwrap();
    assert_eq!(cleared.icon.as_deref(), Some("rocket"));
    assert_eq!(cleared.default_context, None);
    assert_eq!(cleared.description, None);
}
