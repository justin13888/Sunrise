//! Fields and enum variants from a newer build survive this one.

use super::*;

/// protocol-versioning.md §7: "a v1 client receiving a v2 op preserves the
/// unknown keys verbatim IN STORAGE and re-serializes them on outbound
/// merges".
///
/// `tasks.extra` was hardcoded to NULL by all three task writers, so the
/// entity carried its unknown fields for exactly one transaction and the
/// materialized row forgot them. The next outbound op then re-emitted the
/// task WITHOUT them, and under entity-level LWW every replica converged on
/// the truncated value — a silent, permanent data loss caused by the older
/// device merely winning one conflict.
#[test]
fn unknown_entity_fields_survive_the_materialized_row() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);
    legacy_peer(&db);

    let created = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "from a newer client".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    // Stand in for an op written by a newer schema: put the future fields
    // on the entity and write it back through the normal update path.
    let mut task = read_task_t(&e, &db, created.entity);
    task.unknown = future_fields();
    let stream = sunrise_domain::KeyDomain::of_home(task.stream_id).bytes();
    log_and_fold(&e, &mut db, &InnerOp::TaskUpdate(task), &stream);

    let back = read_task_t(&e, &db, created.entity);
    assert_eq!(
        back.unknown,
        future_fields(),
        "the projection must not forget what it could not interpret"
    );

    // ...and the fields ride back out in the entity's canonical CBOR, so
    // an outbound op carries them to every peer.
    let re = sunrise_cbor::encode_canonical(&back).unwrap();
    let decoded: sunrise_domain::Task = sunrise_cbor::decode_canonical(&re).unwrap();
    assert_eq!(decoded.unknown, future_fields());
}

/// ADR-0045 §6: an `extra` blob this build cannot parse is kept, not emptied.
///
/// It reads as "no unknowns", which is what used to make the next write of
/// the row store `NULL` over it. Now the write keeps the bytes and says so;
/// a write whose entity carries unknown fields of its own still replaces them.
#[test]
fn an_unparseable_extra_blob_survives_an_edit_and_is_logged() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    // The blob is written to the row by hand, as an older binary left it, and
    // is in no op: a rebuild of the log does not reproduce it.
    crate::engine::rebuild::check::disable();
    let mut db = db_root(ROOT);
    legacy_peer(&db);
    let task = new_task(&e, &mut db, "holds bytes this build cannot read");
    // A float: canonical CBOR a newer build could write, and a value
    // `CborValue` refuses, so the whole map fails to decode here.
    let opaque: Vec<u8> = vec![0xa1, 0x61, b'x', 0xf9, 0x3c, 0x00];
    let extra = |db: &Db| -> Option<Vec<u8>> {
        db.conn()
            .query_row(
                "SELECT extra FROM tasks WHERE id = ?",
                [&task.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    db.conn()
        .execute(
            "UPDATE tasks SET extra = ? WHERE id = ?",
            rusqlite::params![&opaque, &task.bytes()[..]],
        )
        .unwrap();
    assert!(read_task_t(&e, &db, task).unknown.is_empty());

    let logged = events_emitted_by(|| {
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    });
    assert_eq!(
        extra(&db),
        Some(opaque),
        "the bytes are written back unchanged"
    );
    assert!(
        logged
            .iter()
            .any(|ev| ev == "core.storage.extra_kept_opaque"),
        "keeping them is announced; got {logged:?}"
    );

    // An entity with unknown fields of its own replaces them.
    let mut t = read_task_t(&e, &db, task);
    t.unknown = future_fields();
    let stream = sunrise_domain::KeyDomain::of_home(t.stream_id).bytes();
    log_and_fold(&e, &mut db, &InnerOp::TaskUpdate(t), &stream);
    assert_eq!(read_task_t(&e, &db, task).unknown, future_fields());
}

/// Every table's `extra` lookup names a real key column: a query that does
/// not prepare fails the write it guards, for every row of that table.
#[test]
fn every_extra_lookup_prepares_against_the_schema() {
    let db = db_root(ROOT);
    for table in ExtraTable::ALL {
        db.conn()
            .prepare(table.select_extra())
            .unwrap_or_else(|e| panic!("{table:?}: {e}"));
    }
}

/// The same guarantee, for the five entities that did NOT have it.
///
/// `tasks.extra`, `blocks.extra` and `attachments.extra` were the whole of
/// the implementation. `streams`, `contexts`, `routines`, `focus_sessions`
/// and `focus_session_ends` had no column at all, and their readers
/// hardcoded `Unknowns::new()` — so an older build that merely read and
/// re-saved one of them emitted a truncated full-state op, and entity-level
/// LWW (ADR-0014) carried the truncation to every replica. The client
/// advertises capability bit 34 `CLI_FORWARD_COMPAT` as a REQUIRED v1 bit
/// while doing this, which is what made it a contract violation rather
/// than a missing feature.
///
/// Each case asserts both halves: the projection remembers, and the
/// entity's canonical CBOR carries the fields back out to peers.
#[test]
fn unknown_stream_fields_survive_the_materialized_row() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);
    legacy_peer(&db);

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
    stream.unknown = future_fields();
    log_and_fold(&e, &mut db, &InnerOp::StreamUpdate(stream), &META_STREAM);

    let back = read_stream(db.conn(), created.entity.bytes())
        .unwrap()
        .expect("stream exists");
    assert_eq!(back.unknown, future_fields());

    let re = sunrise_cbor::encode_canonical(&back).unwrap();
    let decoded: sunrise_domain::Stream = sunrise_cbor::decode_canonical(&re).unwrap();
    assert_eq!(decoded.unknown, future_fields());
}

#[test]
fn unknown_context_fields_survive_the_materialized_row() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);
    legacy_peer(&db);

    let created = e
        .apply(
            &mut db,
            Command::CreateContext(ContextDraft {
                name: "errands".into(),
                description: None,
            }),
        )
        .unwrap();

    let mut ctx = read_context(db.conn(), created.entity.bytes())
        .unwrap()
        .expect("context exists");
    ctx.unknown = future_fields();
    log_and_fold(&e, &mut db, &InnerOp::ContextUpdate(ctx), &META_STREAM);

    let back = read_context(db.conn(), created.entity.bytes())
        .unwrap()
        .expect("context exists");
    assert_eq!(back.unknown, future_fields());

    let re = sunrise_cbor::encode_canonical(&back).unwrap();
    let decoded: sunrise_domain::Context = sunrise_cbor::decode_canonical(&re).unwrap();
    assert_eq!(decoded.unknown, future_fields());
}

#[test]
fn unknown_routine_fields_survive_the_materialized_row() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);
    legacy_peer(&db);

    let stream = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Home".into(),
                description: None,
                color: None,
                parent_id: None,
                review_cadence: None,
                reminder_lead_s: None,
                icon: None,
                default_context: None,
            }),
        )
        .unwrap()
        .entity;
    let draft = routine_draft(
        stream,
        "FREQ=DAILY",
        T0 as i64,
        RoutineCatchupPolicy::Skip,
        Vec::new(),
    );
    let rid = e
        .apply(&mut db, Command::CreateRoutine(draft))
        .unwrap()
        .entity;

    let mut routine = read_routine(db.conn(), rid.bytes())
        .unwrap()
        .expect("routine exists");
    routine.unknown = future_fields();
    let routine = e.routine_op_form(&mut db, &routine).unwrap();
    log_and_fold(
        &e,
        &mut db,
        &InnerOp::RoutineUpdate(Box::new(routine)),
        &META_STREAM,
    );

    let back = read_routine(db.conn(), rid.bytes())
        .unwrap()
        .expect("routine exists");
    assert_eq!(back.unknown, future_fields());

    let re = sunrise_cbor::encode_canonical(&back).unwrap();
    let decoded: sunrise_domain::Routine = sunrise_cbor::decode_canonical(&re).unwrap();
    assert_eq!(decoded.unknown, future_fields());
}

/// The focus family is append-only (ADR-0013), so the unknown fields have
/// to arrive with the insert rather than a later update — which is exactly
/// how a newer peer's `focus.start` / `focus.end` op reaches this build.
#[test]
fn unknown_focus_fields_survive_the_materialized_row() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);

    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "focus target".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let stream = sunrise_domain::stream_key(read_task_t(&e, &db, task).stream_id);

    let session = EntityRef::new(EntityKind::FocusSession, [9u8; 16]);
    let start = FocusStart {
        id: session,
        task_id: task,
        stream_id: stream,
        started_at: sunrise_domain::epoch_ms::from_u64(T0),
        planned_ms: None,
        energy: None,
        kind: FocusKind::Work,
        chunk: None,
        unknown: future_fields(),
    };
    let end = FocusEnd {
        session_id: session,
        ended_at: sunrise_domain::epoch_ms::from_u64(T0 + 1_000),
        actual_focused_ms: 1_000,
        interruptions: Vec::new(),
        completed_task: false,
        unknown: future_fields(),
    };
    let lww = e.lww_stamp(2);
    db.with_tx(|tx| {
        insert_focus_start_row(tx, &start, &lww)?;
        insert_focus_end_row(tx, &end, &lww)
    })
    .unwrap();

    let sessions = read_focus_sessions(db.conn(), "", &[]).unwrap();
    let got = sessions
        .iter()
        .find(|s| s.start.id == session)
        .expect("session exists");
    assert_eq!(got.start.unknown, future_fields());
    assert_eq!(
        got.end.as_ref().expect("end exists").unknown,
        future_fields()
    );
}

/// An enum value from a newer schema must be kept, not rejected: rejecting
/// one field's value rejects the whole op, and two replicas then diverge
/// permanently over one string. It reads as the safe fallback and is kept
/// verbatim (ADR-0045 §6).
#[test]
fn an_unknown_enum_variant_is_kept_instead_of_failing_the_op() {
    use ciborium::value::Value;

    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock);
    let mut db = db_root(ROOT);
    let created = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "t".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    // Rewrite the stored state string to something no build understands.
    db.conn()
        .execute(
            "UPDATE tasks SET state = 'delegated' WHERE id = ?",
            params![&created.entity.bytes()[..]],
        )
        .unwrap();

    let back = read_task_t(&e, &db, created.entity);
    assert_eq!(
        back.state.as_str(),
        "delegated",
        "the stored raw state is kept, not degraded"
    );
    assert_eq!(
        back.state.effective(),
        TaskState::Todo,
        "an unrecognised state must read as OPEN, never as done"
    );

    // The same rule on the wire: a Task encoded with an unknown state
    // decodes rather than failing.
    let mut v = ciborium::value::Value::serialized(&back).unwrap();
    if let Value::Map(entries) = &mut v {
        for (k, val) in entries.iter_mut() {
            if matches!(k, Value::Text(t) if t == "state") {
                *val = Value::Text("delegated".into());
            }
        }
    }
    let bytes = sunrise_cbor::encode_canonical(&v).unwrap();
    let decoded: sunrise_domain::Task =
        sunrise_cbor::decode_lenient(&bytes).expect("an unknown variant must not reject the op");
    assert_eq!(decoded.state, TaskState::from_raw("delegated"));
    assert_eq!(
        sunrise_cbor::encode_canonical(&decoded).unwrap(),
        bytes,
        "and it re-encodes byte for byte"
    );
}

/// ADR-0045 §6: a rule holding a `FREQ` and a `BYDAY` token this build does
/// not know is kept — including raw values the RFC 5545 text cannot hold —
/// generates no occurrences, and survives an unrelated edit byte for byte.
#[test]
fn a_routine_with_an_unknown_frequency_is_kept_and_generates_nothing() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let mut draft = routine_draft(
        stream_ref(5),
        "FREQ=WEEKLY",
        NOW + 3_600_000,
        RoutineCatchupPolicy::Skip,
        Vec::new(),
    );
    // Separators the text form splits on, inside the raw values.
    draft.rrule.freq = sunrise_domain::Frequency::from_raw("HOURLY;X=1");
    draft.rrule.by_day = vec![
        sunrise_domain::Weekday::Mo,
        sunrise_domain::Weekday::from_raw("A,B"),
    ];
    let rule = draft.rrule.clone();
    let rid = e
        .apply(&mut db, Command::CreateRoutine(draft))
        .unwrap()
        .entity;

    assert!(
        past_task_count(&db, rid) == 0 && live_task_ids(&db).is_empty(),
        "a rule this build cannot read materializes nothing"
    );
    let rows = match e.query(&db, Query::Routines).unwrap() {
        QueryResult::Routines(v) => v,
        _ => panic!(),
    };
    let stored = rows.iter().find(|r| r.id == rid).unwrap();
    assert_eq!(stored.rrule, rule, "the stored rule reads back verbatim");
    let row = sunrise_domain::routine_rows(
        std::slice::from_ref(stored),
        ms_to_ts(NOW),
        &jiff::tz::TimeZone::UTC,
    )
    .pop()
    .unwrap();
    assert!(
        row.rrule.starts_with("unrecognised schedule"),
        "{}",
        row.rrule
    );
    assert_eq!(row.next, None);

    e.apply(
        &mut db,
        Command::UpdateRoutine {
            scope: sunrise_domain::EditScope::All,
            id: rid,
            patch: RoutinePatch {
                paused: Some(true),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let env = env_for_kind(&db, rid.bytes(), "routine.update");
    match decode_inner_op(&e.keychain.open_op(&env).unwrap()).unwrap() {
        InnerOp::RoutineUpdate(r) => assert_eq!(r.rrule, rule, "carried through verbatim"),
        other => panic!("expected a routine.update, got {}", other.inner_kind()),
    }
}

/// The `Task` the most recent `task.update` op on `target` carries.
fn last_task_update(e: &Engine, db: &Db, target: EntityRef) -> Task {
    let env = env_for_kind(db, target.bytes(), "task.update");
    match decode_inner_op(&e.keychain.open_op(&env).unwrap()).unwrap() {
        InnerOp::TaskUpdate(t) => t,
        other => panic!("expected a task.update, got {}", other.inner_kind()),
    }
}

/// ADR-0045 §6: a value this build does not know is carried through every
/// write it did not make. A newer peer (A) sets a task state and an energy
/// this build's vocabulary lacks; B stores them, and B's own unrelated edit
/// re-emits them byte for byte — where it used to write `todo` and no energy
/// back to every replica.
#[test]
fn an_unknown_state_and_energy_survive_a_peers_unrelated_edit() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0 + 1))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "from a newer build");
    ea.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                state: Some(TaskState::from_raw("archived")),
                energy: Some(Some(sunrise_domain::Energy::from_raw("frantic"))),
                ..Default::default()
            },
        },
    )
    .unwrap();
    eb.apply_remote_all(&mut dbb, &create_env_for(&dba, task.bytes()))
        .unwrap();
    for env in update_envs_for(&dba, task.bytes()) {
        eb.apply_remote_all(&mut dbb, &env).unwrap();
    }

    // B's projection holds the raw values, not their fallbacks.
    let stored = task_of(&eb, &dbb, task);
    assert_eq!(stored.state.as_str(), "archived");
    assert_eq!(stored.energy.as_ref().map(|e| e.as_str()), Some("frantic"));
    // And reads them as the fallbacks where logic acts: the task is open.
    assert_eq!(stored.state.effective(), TaskState::Todo);

    // B renames it: the op B writes carries A's values through.
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("renamed by an older build".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let emitted = last_task_update(&eb, &dbb, task);
    assert_eq!(emitted.state, TaskState::from_raw("archived"));
    assert_eq!(
        emitted.energy,
        Some(sunrise_domain::Energy::from_raw("frantic"))
    );

    // A command that sets the field explicitly replaces it.
    eb.apply(&mut dbb, Command::CompleteTask(task)).unwrap();
    assert_eq!(task_of(&eb, &dbb, task).state, TaskState::Done);
}

/// ADR-0045 §6, one level down: a time kind this build does not know, and a
/// field a newer build added inside a nested constraint, both land, are
/// stored, and are re-emitted unchanged by this build's unrelated edit.
#[test]
fn an_unknown_time_kind_and_nested_fields_survive_a_peers_unrelated_edit() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0 + 1))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    let text = |s: &str| sunrise_domain::CborValue(ciborium::value::Value::Text(s.into()));
    let mut raw = Unknowns::new();
    raw.insert("phase".into(), text("waxing"));
    let lunar = SunriseTime::Unknown {
        kind: "lunar".into(),
        raw,
    };
    let mut constraint = sample_constraint();
    constraint.unknown.insert("place".into(), text("office"));
    if let Some(w) = constraint.time_of_day.as_mut() {
        w.unknown.insert("slack_m".into(), text("15"));
    }

    let task = new_task(&ea, &mut dba, "from a newer build");
    ea.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                planned_at: Some(Some(lunar.clone())),
                scheduling_constraints: Some(vec![constraint.clone()]),
                ..Default::default()
            },
        },
    )
    .unwrap();
    eb.apply_remote_all(&mut dbb, &create_env_for(&dba, task.bytes()))
        .unwrap();
    for env in update_envs_for(&dba, task.bytes()) {
        eb.apply_remote_all(&mut dbb, &env).unwrap();
    }

    // B's projection holds both, not an instant and a truncated constraint.
    let stored = task_of(&eb, &dbb, task);
    assert_eq!(stored.planned_at.as_ref(), Some(&lunar));
    assert_eq!(stored.scheduling_constraints, vec![constraint.clone()]);

    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("renamed by an older build".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let emitted = last_task_update(&eb, &dbb, task);
    assert_eq!(emitted.planned_at, Some(lunar));
    assert_eq!(emitted.scheduling_constraints, vec![constraint]);
}

/// An unknown state counts as open in the SQL that lists and counts open
/// work, as `todo` does in Rust.
#[test]
fn an_unknown_state_counts_as_open_in_the_inbox() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "open, in a state this build lacks");
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                state: Some(TaskState::from_raw("archived")),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let QueryResult::StreamTasks(inbox) = e.query(&db, Query::Inbox).unwrap() else {
        panic!("wrong variant");
    };
    assert_eq!(inbox.len(), 1, "the Inbox view counts it as open");
}

/// A stream's own open count (the non-Inbox rows of the stream list) counts
/// an unknown state as open.
#[test]
fn an_unknown_state_counts_as_open_in_a_streams_own_count() {
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
    let task = task_with(
        &e,
        &mut db,
        "in a state this build lacks",
        TaskDraft {
            stream_id: Some(s),
            ..Default::default()
        },
    );
    set_unknown_state(&e, &mut db, task, "archived");
    let rows = match e.query(&db, Query::StreamList).unwrap() {
        QueryResult::Streams(r) => r,
        _ => panic!(),
    };
    let row = rows.iter().find(|r| r.id == s).unwrap();
    assert_eq!(row.open_task_count, 1, "the stream counts it as open");
}

/// The actionable scan reads an unknown state as open in all three places it
/// filters on state: the task itself, a blocker, and a dependent.
#[test]
fn the_actionable_scan_reads_an_unknown_state_as_open() {
    let mut db = db();
    let e = engine();
    let blocker = new_task(&e, &mut db, "blocker");
    let dependent = new_task(&e, &mut db, "dependent");
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: dependent,
            patch: TaskPatch {
                blocked_by: Some(vec![blocker]),
                ..Default::default()
            },
        },
    )
    .unwrap();
    set_unknown_state(&e, &mut db, blocker, "archived");
    set_unknown_state(&e, &mut db, dependent, "archived");

    let rows = actionable_rows(&e, &db);
    let b = row_for(&rows, blocker);
    assert_eq!(
        b.unblocks, 1,
        "the unknown-state dependent is waiting on it"
    );
    assert_eq!(b.open_blockers, 0);
    let d = row_for(&rows, dependent);
    assert_eq!(
        d.open_blockers, 1,
        "the unknown-state blocker is still open"
    );
    assert_eq!(d.effective_state, EffectiveTaskState::Blocked);
}

/// Editing a routine's rule regenerates an untouched future task in an
/// unknown state as it does a `todo` one, and keeps an in-progress one.
#[test]
fn update_routine_regenerates_a_future_task_in_an_unknown_state() {
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
    // occ[2] falls on a weekday a weekly rule drops.
    let drop_id = occurrence_task_id(&rid, &daily[2].key);
    set_unknown_state(&e, &mut db, drop_id, "snoozed");

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

    let t = match e.query(&db, Query::EntityById(drop_id)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert!(
        t.deleted,
        "an unknown state is regenerated away like `todo`"
    );
}

/// A focus session of a kind this build does not know counts as work: it
/// advances the next session's chunk and the planner's prior-session count.
#[test]
fn an_unknown_focus_kind_counts_as_a_work_session() {
    let mut db = db();
    let (e, clock) = focus_engine(0);
    let task = task_with(
        &e,
        &mut db,
        "75 minutes of work",
        TaskDraft {
            estimated_duration_s: Some(75 * 60),
            ..Default::default()
        },
    );
    let s = e
        .apply(
            &mut db,
            Command::StartFocus(FocusStartDraft {
                task_id: task,
                kind: FocusKind::from_raw("deep_work"),
                length: SessionLength::OnePomodoro,
                energy: None,
            }),
        )
        .unwrap()
        .entity;
    set_clock(&clock, POMODORO_MS);
    e.apply(
        &mut db,
        Command::EndFocus {
            session: s,
            actual_focused_ms: None,
            completed_task: false,
        },
    )
    .unwrap();

    let rows = plan(&e, &db, None);
    let row = rows.iter().find(|r| r.task.id == task).unwrap();
    assert_eq!(row.prior_sessions, 1, "the planner counts it as work");

    set_clock(&clock, POMODORO_MS + 1);
    let next = start_work(&e, &mut db, task, SessionLength::SizedToEstimate);
    let row = sessions_for(&e, &db, task)
        .into_iter()
        .find(|r| r.session.start.id == next)
        .unwrap();
    assert_eq!(
        row.session.start.chunk,
        Some(Chunk::new(2, 3)),
        "the next sitting counts it as a prior work session"
    );
}

/// Storage keeps a stream's unknown colour and cadence verbatim through a
/// rename, where it used to read them back as `slate` and `weekly`.
#[test]
fn an_unknown_stream_color_and_cadence_survive_a_rename() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let s = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                color: Some(StreamColor::from_raw("teal")),
                review_cadence: Some(StreamReviewCadence::from_raw("quarterly")),
                ..Default::default()
            }),
        )
        .unwrap();
    e.apply(
        &mut db,
        Command::UpdateStream {
            id: s.entity,
            patch: StreamPatch {
                name: Some("Personal".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    let st = match e.query(&db, Query::EntityById(s.entity)).unwrap() {
        QueryResult::Stream(s) => *s,
        _ => panic!(),
    };
    assert_eq!(st.color.as_str(), "teal");
    assert_eq!(st.review_cadence.as_str(), "quarterly");
    let listed = match e.query(&db, Query::StreamList).unwrap() {
        QueryResult::Streams(r) => r,
        _ => panic!(),
    };
    assert!(listed.iter().any(|r| r.color.as_str() == "teal"));

    let env = env_for_kind(&db, s.entity.bytes(), "stream.update");
    match decode_inner_op(&e.keychain.open_op(&env).unwrap()).unwrap() {
        InnerOp::StreamUpdate(emitted) => {
            assert_eq!(emitted.color, StreamColor::from_raw("teal"));
            assert_eq!(
                emitted.review_cadence,
                StreamReviewCadence::from_raw("quarterly")
            );
        }
        other => panic!("expected a stream.update, got {}", other.inner_kind()),
    }
}

/// ADR-0045 §6: a catch-up policy this build does not know reads as `skip`,
/// its fallback. Six missed daily occurrences materialize no backlog (`queue`
/// would make six, `merge` one), and the future ones are still generated.
#[test]
fn an_unknown_catchup_policy_materializes_no_backlog() {
    let mut db = db();
    let e = engine();
    let d = routine_draft(
        stream_ref(5),
        "FREQ=DAILY",
        NOW - (5 * DAY_MS + 3_600_000),
        RoutineCatchupPolicy::from_raw("backfill_weekends"),
        Vec::new(),
    );
    let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
    assert_eq!(past_task_count(&db, rid), 0, "an unknown policy skips");
    assert!(
        !live_task_ids(&db).is_empty(),
        "future occurrences are still materialized"
    );
    assert_eq!(
        routine_of(&e, &db, rid).catchup_policy.as_str(),
        "backfill_weekends",
        "the raw policy is kept"
    );
}

/// Skipping an occurrence tombstones its untouched task when that task is in
/// a state this build does not know, as it does a `todo` one.
#[test]
fn skipping_an_occurrence_tombstones_its_task_in_an_unknown_state() {
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
    let key = routine
        .occurrences_in(window, &jiff::tz::TimeZone::UTC)
        .unwrap()[2]
        .key
        .clone();
    let tid = occurrence_task_id(&rid, &key);
    set_unknown_state(&e, &mut db, tid, "snoozed");

    e.apply(
        &mut db,
        Command::SkipRoutineOccurrence {
            id: rid,
            occurrence_key: key,
        },
    )
    .unwrap();
    let t = match e.query(&db, Query::EntityById(tid)).unwrap() {
        QueryResult::Task(t) => *t,
        _ => panic!(),
    };
    assert!(t.deleted, "an unknown state is tombstoned like `todo`");
}

/// A scheduled task in a state this build does not know is open, so it still
/// produces a reminder.
#[test]
fn a_task_in_an_unknown_state_still_produces_a_reminder() {
    let (e, mut db) = engine_at_ms(NOTIFY_NOON);
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "Call the bank".into(),
                planned_at: Some(SunriseTime::instant(ms_to_ts(NOTIFY_NOON + 3_600_000))),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    set_unknown_state(&e, &mut db, task, "snoozed");
    let out = reminders(
        &e,
        &db,
        NOTIFY_NOON,
        NOTIFY_NOON + 86_400_000,
        ReminderSettings::default(),
    );
    assert_eq!(
        out.iter().map(|r| r.entity).collect::<Vec<_>>(),
        vec![task],
        "an unknown state reads as `todo`"
    );
}
