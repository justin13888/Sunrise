//! Optional streams, the private key domain, the Inbox view and what deleting
//! a stream does to what it held (ADR-0046, issue #332).

use super::field_merge::{pair, Pair};
use super::testutil::*;
use super::*;
use crate::engine::block::read_block;
use crate::engine::routine::read_routine;
use proptest::prelude::*;
use sunrise_domain::{Disposition, StreamId, PRIVATE_DOMAIN_BYTES};

// ---- helpers ----

fn stream(e: &Engine, db: &mut Db, name: &str) -> EntityRef {
    e.apply(
        db,
        Command::CreateStream(StreamDraft {
            name: name.into(),
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn child(e: &Engine, db: &mut Db, name: &str, parent: EntityRef) -> EntityRef {
    e.apply(
        db,
        Command::CreateStream(StreamDraft {
            name: name.into(),
            parent_id: Some(parent),
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn task_in(e: &Engine, db: &mut Db, title: &str, stream: Option<EntityRef>) -> EntityRef {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            stream_id: stream,
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn delete(e: &Engine, db: &mut Db, id: EntityRef, disposition: Disposition) -> CommandResult {
    e.apply(db, Command::DeleteStream { id, disposition })
        .unwrap()
}

fn home(db: &Db, task: EntityRef) -> Option<EntityRef> {
    read_task(db.conn(), task.bytes())
        .unwrap()
        .expect("projected")
        .stream_id
}

fn ids(r: QueryResult) -> Vec<EntityRef> {
    match r {
        QueryResult::StreamTasks(t) | QueryResult::Tasks(t) => {
            t.into_iter().map(|t| t.id).collect()
        }
        other => panic!("expected tasks, got {other:?}"),
    }
}

fn inbox(e: &Engine, db: &Db) -> Vec<EntityRef> {
    ids(e.query(db, Query::Inbox).unwrap())
}

fn stream_ids(e: &Engine, db: &Db) -> Vec<EntityRef> {
    match e.query(db, Query::StreamList).unwrap() {
        QueryResult::Streams(rows) => rows.into_iter().map(|r| r.id).collect(),
        other => panic!("expected streams, got {other:?}"),
    }
}

fn update(e: &Engine, db: &mut Db, id: EntityRef, patch: TaskPatch) {
    e.apply(db, Command::UpdateTask { id, patch }).unwrap();
}

/// The key domain `db` logged `entity`'s newest op under.
fn sealed_under(db: &Db, entity: EntityRef) -> [u8; 16] {
    db.conn()
        .query_row(
            "SELECT stream_id FROM ops WHERE target_id = ? ORDER BY rowid DESC LIMIT 1",
            params![&entity.bytes()[..]],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .map(|b| blob16(&b))
        .unwrap()
}

/// Every envelope `from` logged under its own device, in seq order.
fn own_envelopes(from: &Engine, db: &Db) -> Vec<Vec<u8>> {
    let device = from.keychain.device_id();
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY rowid")
        .unwrap();
    stmt.query_map(params![&device[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn deliver(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
    for env in own_envelopes(from, from_db) {
        to.apply_remote_all(to_db, &env).unwrap();
    }
}

fn sync(p: &mut Pair) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba);
}

/// [`pair`] with both devices advertising their features, so the vault
/// requires `core.field_ops` at its first write and every command writes
/// `Patch` (ADR-0065 §7); without, both write full-state ops (D9).
fn pair_in(patches: bool) -> Pair {
    let mut p = pair();
    if patches {
        assert!(p.ea.advertise_features(&mut p.dba).unwrap());
        assert!(p.eb.advertise_features(&mut p.dbb).unwrap());
        sync(&mut p);
    }
    p
}

/// Both replicas project the same rows, and each is the fold of its log.
fn converged(p: &Pair) {
    assert_eq!(tasks_projection(&p.dba), tasks_projection(&p.dbb));
    assert_eq!(stream_ids(&p.ea, &p.dba), stream_ids(&p.eb, &p.dbb));
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// No live task, block or routine of `db` reads as belonging to a stream
/// that is a tombstone (ADR-0046 §Consequences).
fn nothing_lives_in_a_deleted_stream(db: &Db) {
    for (table, column) in [
        ("tasks", "stream_id"),
        ("blocks", "stream_id"),
        ("routines", "stream_id"),
        ("streams", "parent_id"),
    ] {
        let key = if table == "streams" {
            "stream_id"
        } else {
            "id"
        };
        let stranded: i64 = db
            .conn()
            .query_row(
                &format!(
                    "SELECT count(*) FROM {table} t JOIN streams s ON s.stream_id = t.{column}
                     WHERE t.deleted = 0 AND s.deleted = 1 AND t.{key} IS NOT NULL"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stranded, 0,
            "a live row of `{table}` names a deleted stream"
        );
    }
}

// ---- the private key domain ----

/// A task with no stream is sealed under the private domain, projects with
/// no stream, and gives the domain no `streams` row: it is not a Stream. The
/// legacy Inbox reference, given as a stream, means the same.
#[test]
fn a_task_with_no_stream_is_sealed_under_the_private_domain() {
    let (e, mut db) = (engine(), db());
    let none = task_in(&e, &mut db, "personal", None);
    let spelled = task_in(&e, &mut db, "old client", Some(inbox_stream_ref()));
    for t in [none, spelled] {
        assert_eq!(home(&db, t), None);
        assert_eq!(sealed_under(&db, t), PRIVATE_DOMAIN_BYTES);
    }
    let raw: Option<Vec<u8>> = db
        .conn()
        .query_row(
            "SELECT stream_id FROM tasks WHERE id = ?",
            params![&none.bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(raw, None, "no stream is NULL in the row");
    let rows: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM streams WHERE stream_id = ?",
            params![&PRIVATE_DOMAIN_BYTES[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "the private domain has no stream row");
    assert!(stream_ids(&e, &db).is_empty(), "nor a stream list entry");
    // The stream-less set, which a manual order is kept within.
    let mut stray = ids(e
        .query(&db, Query::StreamTasks(inbox_stream_ref()))
        .unwrap());
    stray.sort();
    let mut want = vec![none, spelled];
    want.sort();
    assert_eq!(stray, want);
    e.assert_rebuild_identity(&db);
}

/// An op sealed under the private domain is opened only with that domain's
/// key: a device with real random keys that was never handed it defers the
/// op, and opens it, landing the task with no stream, once it holds the key.
#[test]
fn a_private_domain_op_seals_and_opens_with_the_domains_own_key() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1; 32], clock.clone());
    let eb = engine_random_keys(ROOT, [2; 32], clock);
    let (mut dba, mut dbb) = (db_root(ROOT), db_root(ROOT));
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    ea.ensure_base_epochs(&mut dba).unwrap();
    let t = task_in(&ea, &mut dba, "personal", None);
    let env: Vec<u8> = dba
        .conn()
        .query_row(
            "SELECT envelope FROM ops WHERE target_id = ? ORDER BY rowid DESC LIMIT 1",
            params![&t.bytes()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        sunrise_crypto::decode_envelope(&env).unwrap().stream_id,
        PRIVATE_DOMAIN_BYTES
    );
    assert!(ea
        .keychain
        .held_stream_keys()
        .contains_key(&PRIVATE_DOMAIN_BYTES));
    // B holds no key for the domain: the op waits rather than applies.
    let _ = eb.apply_remote_all(&mut dbb, &env);
    assert!(read_task(dbb.conn(), t.bytes()).unwrap().is_none());
    assert!(deferred_rows(&dbb) > 0, "it is held for its key");
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &PRIVATE_DOMAIN_BYTES);
    let epoch = sunrise_crypto::decode_envelope(&env).unwrap().epoch;
    eb.drain_deferred(&mut dbb, &PRIVATE_DOMAIN_BYTES, epoch)
        .unwrap();
    assert_eq!(home(&dbb, t), None);
    assert_eq!(inbox(&eb, &dbb), vec![t]);
}

/// A stream-scoped grant carries the keys of the Streams it names and of
/// nothing else: never the private domain, never the vault-meta domain,
/// however it is asked (ADR-0046 §2).
#[test]
fn a_stream_scoped_grant_never_carries_the_private_or_meta_domain() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_random_keys(ROOT, [1; 32], clock);
    let mut db = db_root(ROOT);
    e.ensure_base_epochs(&mut db).unwrap();
    let work = stream(&e, &mut db, "Work");
    let home_ = stream(&e, &mut db, "Home");
    task_in(&e, &mut db, "w", Some(work));
    task_in(&e, &mut db, "h", Some(home_));
    task_in(&e, &mut db, "p", None);
    let held = e.keychain.held_stream_keys();
    for domain in [
        PRIVATE_DOMAIN_BYTES,
        META_STREAM,
        *work.bytes(),
        *home_.bytes(),
    ] {
        assert!(held.contains_key(&domain), "{domain:?} is held");
    }
    assert!(StreamId::new(inbox_stream_ref()).is_none());
    assert!(StreamId::new(EntityRef::new(EntityKind::Stream, META_STREAM)).is_none());
    // Every id this device holds a key for, as a Stream id where it can be one.
    let every: Vec<StreamId> = held
        .keys()
        .filter_map(|id| StreamId::new(EntityRef::new(EntityKind::Stream, *id)))
        .collect();
    let grant = e.keychain.stream_grant_keys(&every);
    assert!(!grant.contains_key(&PRIVATE_DOMAIN_BYTES));
    assert!(!grant.contains_key(&META_STREAM));
    assert_eq!(
        grant.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([*work.bytes(), *home_.bytes()])
    );
    let one = e
        .keychain
        .stream_grant_keys(&[StreamId::new(work).unwrap()]);
    assert_eq!(one.keys().copied().collect::<Vec<_>>(), vec![*work.bytes()]);
    assert_eq!(one[work.bytes()], held[work.bytes()], "every epoch of it");
}

// ---- the Inbox view ----

/// A task is in the Inbox exactly while it has no stream, no plan, no
/// deadline and no `triaged_at`, and is open: giving it a stream or a date
/// takes it out without any flag being cleared, and taking those away puts
/// it back.
#[test]
fn the_inbox_is_a_view_nothing_has_to_clear() {
    let (e, mut db) = (engine(), db());
    let work = stream(&e, &mut db, "Work");
    let t = task_in(&e, &mut db, "capture", None);
    assert_eq!(inbox(&e, &db), vec![t]);
    let plan = SunriseTime::instant(ms_to_ts(NOW + DAY_MS as i64));
    update(
        &e,
        &mut db,
        t,
        TaskPatch {
            scheduled_at: Some(Some(plan.clone())),
            ..Default::default()
        },
    );
    assert!(inbox(&e, &db).is_empty(), "a plan takes it out");
    update(
        &e,
        &mut db,
        t,
        TaskPatch {
            scheduled_at: Some(None),
            due_at: Some(Some(plan)),
            ..Default::default()
        },
    );
    assert!(inbox(&e, &db).is_empty(), "so does a deadline");
    update(
        &e,
        &mut db,
        t,
        TaskPatch {
            due_at: Some(None),
            stream_id: Some(Some(work)),
            ..Default::default()
        },
    );
    assert!(inbox(&e, &db).is_empty(), "and a stream");
    update(
        &e,
        &mut db,
        t,
        TaskPatch {
            stream_id: Some(None),
            ..Default::default()
        },
    );
    assert_eq!(inbox(&e, &db), vec![t], "moved to no stream, it is back");
    e.apply(
        &mut db,
        Command::PromoteToStream {
            id: t,
            stream: work,
        },
    )
    .unwrap();
    assert_eq!(home(&db, t), Some(work));
    e.apply(
        &mut db,
        Command::PromoteToStream {
            id: t,
            stream: inbox_stream_ref(),
        },
    )
    .unwrap();
    assert_eq!(
        home(&db, t),
        None,
        "the legacy Inbox reference is no stream"
    );
    assert_eq!(sealed_under(&db, t), PRIVATE_DOMAIN_BYTES);
    e.apply(&mut db, Command::MarkTriaged { ids: vec![t] })
        .unwrap();
    assert!(inbox(&e, &db).is_empty(), "triaged, it leaves");
    assert!(read_task_t(&e, &db, t).triaged_at.is_some());
    e.apply(&mut db, Command::ReturnToInbox { ids: vec![t] })
        .unwrap();
    assert_eq!(inbox(&e, &db), vec![t]);
    e.apply(&mut db, Command::CompleteTask(t)).unwrap();
    assert!(inbox(&e, &db).is_empty(), "a done task is not in it");
    e.assert_rebuild_identity(&db);
}

/// `MarkTriaged` is all or nothing: one id it cannot act on refuses the
/// batch with nothing written, and a task already triaged writes nothing.
#[test]
fn mark_triaged_refuses_the_batch_whole() {
    let (e, mut db) = (engine(), db());
    let a = task_in(&e, &mut db, "a", None);
    let b = task_in(&e, &mut db, "b", None);
    let before = op_count(&db);
    let ghost = EntityRef::new(EntityKind::Task, [0x77; 16]);
    for bad in [vec![a, ghost], vec![a, a], vec![]] {
        assert!(
            e.apply(&mut db, Command::MarkTriaged { ids: bad.clone() })
                .is_err(),
            "{bad:?}"
        );
    }
    assert_eq!(op_count(&db), before, "nothing was written");
    e.apply(&mut db, Command::MarkTriaged { ids: vec![a, b] })
        .unwrap();
    assert!(inbox(&e, &db).is_empty());
    let after = op_count(&db);
    e.apply(&mut db, Command::MarkTriaged { ids: vec![a] })
        .unwrap();
    assert_eq!(
        op_count(&db),
        after,
        "an already triaged task is left alone"
    );
}

/// An older peer's Inbox op, sealed under the private domain with the
/// legacy Inbox reference as its stream (as every build before ADR-0046
/// writes one), lands with no stream and in the Inbox.
#[test]
fn an_old_peers_inbox_sentinel_op_lands_as_no_stream() {
    let mut p = pair_in(false);
    let t = task_in(&p.ea, &mut p.dba, "from an old build", None);
    let env = create_env_for(&p.dba, t.bytes());
    let inner = decode_inner_op(&p.ea.keychain.open_op(&env).unwrap()).unwrap();
    let InnerOp::TaskCreate(task) = &inner else {
        panic!("a full-state create, as a build before field ops writes");
    };
    // On the wire the absent stream is the sentinel an older build writes.
    let wire = ciborium::value::Value::serialized(task).unwrap();
    let stream_field = wire
        .as_map()
        .unwrap()
        .iter()
        .find(|(k, _)| k.as_text() == Some("stream_id"))
        .map(|(_, v)| v.clone());
    assert_eq!(
        stream_field,
        Some(ciborium::value::Value::Text(
            sunrise_domain::INBOX_STREAM_ID.into()
        ))
    );
    sync(&mut p);
    assert_eq!(home(&p.dbb, t), None);
    assert_eq!(inbox(&p.eb, &p.dbb), vec![t]);
    converged(&p);
}

// ---- deleting a stream ----

/// What a stream held, seeded on `e`: a child stream, a task, a block and a
/// routine whose template names it.
struct Held {
    stream: EntityRef,
    child: EntityRef,
    task: EntityRef,
    block: EntityRef,
    routine: EntityRef,
}

fn holding(e: &Engine, db: &mut Db, name: &str) -> Held {
    let stream = stream(e, db, name);
    let child = child(e, db, &format!("{name} child"), stream);
    let task = task_in(e, db, &format!("{name} task"), Some(stream));
    let mut draft = block_draft(9, 1, Some("focus"));
    draft.stream_id = Some(stream);
    let block = e.apply(db, Command::CreateBlock(draft)).unwrap().entity;
    let routine = e
        .apply(
            db,
            Command::CreateRoutine(routine_draft(
                stream,
                "FREQ=DAILY",
                NOW,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            )),
        )
        .unwrap()
        .entity;
    Held {
        stream,
        child,
        task,
        block,
        routine,
    }
}

fn block_home(db: &Db, id: EntityRef) -> Option<EntityRef> {
    read_block(db.conn(), id.bytes())
        .unwrap()
        .unwrap()
        .stream_id
}

fn routine_home(db: &Db, id: EntityRef) -> Option<EntityRef> {
    read_routine(db.conn(), id.bytes())
        .unwrap()
        .unwrap()
        .template
        .stream_id
}

fn stream_row(db: &Db, id: EntityRef) -> Stream {
    crate::engine::stream::read_stream(db.conn(), id.bytes())
        .unwrap()
        .expect("a row")
}

/// `Detach` sends every task, block and routine template the stream held to
/// no stream, makes its children top-level, and records no re-home on the
/// tombstone; nothing it held is deleted. In both write modes.
#[test]
fn deleting_a_stream_detaches_everything_it_held() {
    for patches in [false, true] {
        let mut p = pair_in(patches);
        let h = holding(&p.ea, &mut p.dba, "Old");
        delete(&p.ea, &mut p.dba, h.stream, Disposition::Detach);
        let db = &p.dba;
        assert_eq!(home(db, h.task), None, "{patches}");
        assert_eq!(block_home(db, h.block), None);
        assert_eq!(routine_home(db, h.routine), None);
        assert_eq!(stream_row(db, h.child).parent_id, None, "promoted");
        let gone = stream_row(db, h.stream);
        assert!(gone.deleted);
        assert_eq!(gone.rehomed_to, None);
        assert!(
            !read_task_t(&p.ea, db, h.task).deleted,
            "re-homed, not deleted"
        );
        assert!(
            !read_block(db.conn(), h.block.bytes())
                .unwrap()
                .unwrap()
                .deleted
        );
        assert!(
            !read_routine(db.conn(), h.routine.bytes())
                .unwrap()
                .unwrap()
                .deleted
        );
        assert_eq!(stream_ids(&p.ea, db), vec![h.child]);
        assert!(
            inbox(&p.ea, db).contains(&h.task),
            "detached, it asks for a home"
        );
        nothing_lives_in_a_deleted_stream(db);
        sync(&mut p);
        assert_eq!(home(&p.dbb, h.task), None);
        assert_eq!(block_home(&p.dbb, h.block), None);
        assert_eq!(routine_home(&p.dbb, h.routine), None);
        converged(&p);
    }
}

/// `MoveTo` sends them all to a live stream instead, sealing the moved task
/// and block under its domain, and the tombstone records it.
#[test]
fn deleting_a_stream_into_another_moves_everything_it_held() {
    for patches in [false, true] {
        let mut p = pair_in(patches);
        let h = holding(&p.ea, &mut p.dba, "Old");
        let into = stream(&p.ea, &mut p.dba, "New");
        delete(&p.ea, &mut p.dba, h.stream, Disposition::MoveTo(into));
        let db = &p.dba;
        assert_eq!(home(db, h.task), Some(into), "{patches}");
        assert_eq!(sealed_under(db, h.task), *into.bytes());
        assert_eq!(block_home(db, h.block), Some(into));
        assert_eq!(routine_home(db, h.routine), Some(into));
        assert_eq!(stream_row(db, h.child).parent_id, None);
        assert_eq!(stream_row(db, h.stream).rehomed_to, Some(into));
        nothing_lives_in_a_deleted_stream(db);
        sync(&mut p);
        converged(&p);
    }
}

/// A move into the stream itself, a deleted one, one that does not exist,
/// or the private domain is refused, with nothing written.
#[test]
fn a_stream_cannot_be_deleted_into_itself_or_a_tombstone() {
    let (e, mut db) = (engine(), db());
    let a = stream(&e, &mut db, "a");
    let b = stream(&e, &mut db, "b");
    delete(&e, &mut db, b, Disposition::Detach);
    let before = op_count(&db);
    for to in [a, b, EntityRef::new(EntityKind::Stream, [0x66; 16])] {
        let res = e.apply(
            &mut db,
            Command::DeleteStream {
                id: a,
                disposition: Disposition::MoveTo(to),
            },
        );
        assert!(matches!(res, Err(EngineError::Invalid(_))), "{to}: {res:?}");
    }
    assert_eq!(op_count(&db), before);
    assert!(!stream_row(&db, a).deleted);
}

/// The deletion is one group (ADR-0065 §6): a replica that holds some of its
/// members shows none of them, and shows all of them once the last arrives.
#[test]
fn a_stream_deletion_is_never_half_visible() {
    for patches in [false, true] {
        let mut p = pair_in(patches);
        let h = holding(&p.ea, &mut p.dba, "Old");
        sync(&mut p);
        let before = own_envelopes(&p.ea, &p.dba).len();
        delete(&p.ea, &mut p.dba, h.stream, Disposition::Detach);
        let envs = own_envelopes(&p.ea, &p.dba);
        let members = &envs[before..];
        assert!(members.len() >= 5, "{patches}: {}", members.len());
        // Everything but the last member: B shows the stream as it was.
        for env in &members[..members.len() - 1] {
            p.eb.apply_remote_all(&mut p.dbb, env).unwrap();
        }
        assert_eq!(home(&p.dbb, h.task), Some(h.stream), "{patches}");
        assert!(!stream_row(&p.dbb, h.stream).deleted);
        p.eb.apply_remote_all(&mut p.dbb, &members[members.len() - 1])
            .unwrap();
        assert_eq!(home(&p.dbb, h.task), None);
        assert!(stream_row(&p.dbb, h.stream).deleted);
        converged(&p);
    }
}

/// The convergence the issue asks for: A deletes a stream into another
/// while B, offline, edits one of its tasks and files a new one into it.
/// Both replicas end with every one of them in the stream A chose, B's edit
/// kept, and nothing in the deleted stream.
#[test]
fn a_task_written_into_a_deleted_stream_on_another_device_reads_as_rehomed() {
    for patches in [false, true] {
        let mut p = pair_in(patches);
        let old = stream(&p.ea, &mut p.dba, "Old");
        let into = stream(&p.ea, &mut p.dba, "New");
        let kept = task_in(&p.ea, &mut p.dba, "kept", Some(old));
        sync(&mut p);
        set_clock(&p.ca, T0 + 10);
        set_clock(&p.cb, T0 + 20);
        delete(&p.ea, &mut p.dba, old, Disposition::MoveTo(into));
        update(
            &p.eb,
            &mut p.dbb,
            kept,
            TaskPatch {
                title: Some("edited on B".into()),
                ..Default::default()
            },
        );
        let late = task_in(&p.eb, &mut p.dbb, "filed late", Some(old));
        // A block and a routine filed on B too: B holds each before A's
        // tombstone reaches it, so only the fold's re-projection moves them.
        let mut draft = block_draft(9, 1, Some("late block"));
        draft.stream_id = Some(old);
        let late_block =
            p.eb.apply(&mut p.dbb, Command::CreateBlock(draft))
                .unwrap()
                .entity;
        let late_routine =
            p.eb.apply(
                &mut p.dbb,
                Command::CreateRoutine(routine_draft(
                    old,
                    "FREQ=DAILY",
                    NOW,
                    RoutineCatchupPolicy::Skip,
                    Vec::new(),
                )),
            )
            .unwrap()
            .entity;
        sync(&mut p);
        for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
            assert_eq!(home(db, kept), Some(into), "{patches}");
            assert_eq!(home(db, late), Some(into), "{patches}");
            assert_eq!(block_home(db, late_block), Some(into), "{patches}");
            assert_eq!(routine_home(db, late_routine), Some(into), "{patches}");
            assert_eq!(read_task_t(e, db, kept).title, "edited on B");
            nothing_lives_in_a_deleted_stream(db);
        }
        converged(&p);
        // The next write of the late task seals it under the stream it reads
        // as being in.
        update(
            &p.eb,
            &mut p.dbb,
            late,
            TaskPatch {
                title: Some("filed late, renamed".into()),
                ..Default::default()
            },
        );
        assert_eq!(sealed_under(&p.dbb, late), *into.bytes());
    }
}

/// Two streams each deleted into the other, on two devices, resolve to no
/// stream on both; a chain of re-homes is followed to its live end, and a
/// detach at its end sends it to no stream.
#[test]
fn rehoming_chains_resolve_and_a_cycle_resolves_to_no_stream() {
    let mut p = pair_in(true);
    let s1 = stream(&p.ea, &mut p.dba, "one");
    let s2 = stream(&p.ea, &mut p.dba, "two");
    let s3 = stream(&p.ea, &mut p.dba, "three");
    let t1 = task_in(&p.ea, &mut p.dba, "in one", Some(s1));
    let t2 = task_in(&p.ea, &mut p.dba, "in two", Some(s2));
    sync(&mut p);
    delete(&p.ea, &mut p.dba, s1, Disposition::MoveTo(s2));
    delete(&p.eb, &mut p.dbb, s2, Disposition::MoveTo(s1));
    sync(&mut p);
    for db in [&p.dba, &p.dbb] {
        assert_eq!(home(db, t1), None);
        assert_eq!(home(db, t2), None);
        nothing_lives_in_a_deleted_stream(db);
    }
    converged(&p);

    // A chain: a task filed into `three` late, after `three` went into
    // `four` and `four` went to no stream.
    let s4 = stream(&p.ea, &mut p.dba, "four");
    sync(&mut p);
    let late = task_in(&p.eb, &mut p.dbb, "late into three", Some(s3));
    delete(&p.ea, &mut p.dba, s3, Disposition::MoveTo(s4));
    sync(&mut p);
    assert_eq!(home(&p.dba, late), Some(s4));
    delete(&p.ea, &mut p.dba, s4, Disposition::Detach);
    let later = task_in(&p.eb, &mut p.dbb, "later into three", Some(s3));
    sync(&mut p);
    for db in [&p.dba, &p.dbb] {
        assert_eq!(home(db, late), None);
        assert_eq!(home(db, later), None);
    }
    converged(&p);
}

/// A routine whose stream was deleted into another generates its next
/// occurrences there.
#[test]
fn routine_occurrences_land_where_their_stream_was_rehomed() {
    let (e, mut db) = (engine(), db());
    let h = holding(&e, &mut db, "Gym");
    let into = stream(&e, &mut db, "Health");
    delete(&e, &mut db, h.stream, Disposition::MoveTo(into));
    e.apply(
        &mut db,
        Command::MaterializeRoutines {
            now_ms: (NOW + 3 * DAY_MS) as u64,
        },
    )
    .unwrap();
    let occurrences: Vec<Task> = super::super::review::read_live_tasks(db.conn())
        .unwrap()
        .into_iter()
        .filter(|t| t.routine_id == Some(h.routine))
        .collect();
    assert!(!occurrences.is_empty());
    assert!(
        occurrences.iter().all(|t| t.stream_id == Some(into)),
        "{occurrences:?}"
    );
    e.assert_rebuild_identity(&db);
}

/// A stream named as a parent after its deletion, on another device, reads
/// as having none: a deletion promotes every child, late ones included.
#[test]
fn a_child_nested_under_a_deleted_stream_on_another_device_reads_as_top_level() {
    let mut p = pair_in(true);
    let parent = stream(&p.ea, &mut p.dba, "parent");
    sync(&mut p);
    delete(&p.ea, &mut p.dba, parent, Disposition::Detach);
    let late = child(&p.eb, &mut p.dbb, "late child", parent);
    sync(&mut p);
    for db in [&p.dba, &p.dbb] {
        assert_eq!(stream_row(db, late).parent_id, None);
    }
    converged(&p);
}

// ---- the rest of the reads ----

/// Search reads a stream-less task as in the Inbox for `is:inbox` while it
/// is untriaged, and `stream:Inbox` names the tasks of no stream.
#[test]
fn search_reads_no_stream_and_the_inbox_view() {
    let (e, mut db) = (engine(), db());
    let work = stream(&e, &mut db, "Work");
    let loose = task_in(&e, &mut db, "parcel loose", None);
    let triaged = task_in(&e, &mut db, "parcel triaged", None);
    let filed = task_in(&e, &mut db, "parcel filed", Some(work));
    let done = task_in(&e, &mut db, "parcel done", None);
    e.apply(&mut db, Command::CompleteTask(done)).unwrap();
    e.apply(&mut db, Command::MarkTriaged { ids: vec![triaged] })
        .unwrap();
    let hits = |q: &str| -> BTreeSet<EntityRef> {
        match e
            .query(
                &db,
                Query::SearchGrouped {
                    text: q.into(),
                    now_ms: NOW as u64,
                    prefix: false,
                    per_group: None,
                },
            )
            .unwrap()
        {
            QueryResult::Search(r) => r
                .groups
                .iter()
                .flat_map(|g| g.hits.iter())
                .filter(|h| h.entity.kind() == EntityKind::Task)
                .map(|h| h.entity)
                .collect(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(hits("parcel is:inbox"), BTreeSet::from([loose]));
    assert_eq!(
        hits("parcel stream:Inbox"),
        BTreeSet::from([loose, triaged, done])
    );
    // A closed task is neither in the Inbox nor unplanned work.
    assert_eq!(
        hits("parcel is:unplanned"),
        BTreeSet::from([loose, triaged, filed])
    );
}

/// A promote, as the item of a bulk command (an undo's), is the move it
/// makes: to the stream, or to no stream for the legacy Inbox id.
#[test]
fn a_promote_is_the_bulk_item_of_its_move() {
    use crate::commands::{BulkTaskAction, BulkTaskItem};
    let id = EntityRef::new(EntityKind::Task, [7; 16]);
    let to = |stream| match BulkTaskItem::of_command(&Command::PromoteToStream { id, stream })
        .expect("a task command")
        .action
    {
        BulkTaskAction::Update(p) => p.stream_id,
        other => panic!("{other:?}"),
    };
    let work = EntityRef::new(EntityKind::Stream, [3; 16]);
    assert_eq!(to(work), Some(Some(work)));
    assert_eq!(to(sunrise_domain::inbox_stream_ref()), Some(None));
}

/// `Actionable` narrows to one stream, to the tasks of no stream for the
/// legacy Inbox id, and spans both for none.
#[test]
fn actionable_narrows_to_a_stream_or_to_no_stream() {
    let (e, mut db) = (engine(), db());
    let work = stream(&e, &mut db, "Work");
    let filed = task_in(&e, &mut db, "filed", Some(work));
    let loose = task_in(&e, &mut db, "loose", None);
    let actionable = |stream: Option<EntityRef>| -> BTreeSet<EntityRef> {
        match e
            .query(&db, Query::Actionable { stream, limit: 100 })
            .unwrap()
        {
            QueryResult::Actionable(v) => v.into_iter().map(|r| r.task.id).collect(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(actionable(Some(work)), BTreeSet::from([filed]));
    assert_eq!(
        actionable(Some(sunrise_domain::inbox_stream_ref())),
        BTreeSet::from([loose])
    );
    assert_eq!(actionable(None), BTreeSet::from([filed, loose]));
}

/// A saved view's `no_stream` part keeps the tasks of no stream, ORed with
/// the streams it names; the legacy Inbox reference in `streams` means the
/// same.
#[test]
fn a_saved_views_no_stream_part_keeps_the_tasks_of_no_stream() {
    use sunrise_domain::saved_view::FilterReader;
    use sunrise_domain::ViewFilter;
    let (e, mut db) = (engine(), db());
    let work = stream(&e, &mut db, "Work");
    let home_ = stream(&e, &mut db, "Home");
    let loose = task_in(&e, &mut db, "loose", None);
    let loose = read_task_t(&e, &db, loose);
    let filed = task_in(&e, &mut db, "filed", Some(work));
    let filed = read_task_t(&e, &db, filed);
    let other = task_in(&e, &mut db, "other", Some(home_));
    let other = read_task_t(&e, &db, other);
    let reader = FilterReader {
        tz: jiff::tz::TimeZone::UTC,
        today: jiff::civil::date(2026, 1, 1),
    };
    let keeps = |f: &ViewFilter| -> Vec<bool> {
        [&loose, &filed, &other]
            .iter()
            .map(|t| f.matches(t, &reader, true))
            .collect()
    };
    let both = ViewFilter {
        streams: vec![work],
        no_stream: true,
        ..Default::default()
    };
    assert_eq!(keeps(&both), [true, true, false]);
    let legacy = ViewFilter {
        streams: vec![inbox_stream_ref()],
        ..Default::default()
    };
    assert_eq!(keeps(&legacy), [true, false, false]);
    assert_eq!(keeps(&ViewFilter::default()), [true, true, true]);
}

/// The upcoming view's no-date lane files a stream-less task in its "No
/// stream" group, and a stream-less task planned for today is on Today.
#[test]
fn today_and_upcoming_hold_tasks_of_no_stream() {
    let (e, mut db) = (engine(), db());
    let undated = task_in(&e, &mut db, "someday", None);
    let today = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "today".into(),
                scheduled_at: Some(SunriseTime::instant(ms_to_ts(NOW + 3_600_000))),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let on_today = ids(e
        .query(
            &db,
            Query::Today {
                now_ms: NOW as u64,
                contexts: Vec::new(),
            },
        )
        .unwrap());
    assert!(on_today.contains(&today), "{on_today:?}");
    let QueryResult::Upcoming(view) = e
        .query(
            &db,
            Query::Upcoming {
                now_ms: NOW as u64,
                span_days: Some(7),
                contexts: Vec::new(),
            },
        )
        .unwrap()
    else {
        panic!("upcoming");
    };
    let group = view
        .no_date
        .iter()
        .find(|g| g.no_stream)
        .expect("a No stream group");
    assert_eq!(group.stream, inbox_stream_ref());
    assert!(group.tasks.iter().any(|t| t.id == undated));
}

// ---- the invariant, under random commands on two replicas ----

#[derive(Debug, Clone)]
enum Step {
    /// Create stream `n` (by name) on replica `a`.
    Stream { a: bool },
    /// Create a task in stream `s` (an index into the streams made so far,
    /// or no stream) on replica `a`.
    Task { a: bool, s: Option<usize> },
    /// Move task `t` to stream `s` or no stream.
    Move { a: bool, t: usize, s: Option<usize> },
    /// Delete stream `s`, detaching or into stream `to`.
    Delete {
        a: bool,
        s: usize,
        to: Option<usize>,
    },
    /// Exchange everything.
    Sync,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        2 => any::<bool>().prop_map(|a| Step::Stream { a }),
        3 => (any::<bool>(), proptest::option::of(0usize..6)).prop_map(|(a, s)| Step::Task { a, s }),
        2 => (any::<bool>(), 0usize..12, proptest::option::of(0usize..6))
            .prop_map(|(a, t, s)| Step::Move { a, t, s }),
        2 => (any::<bool>(), 0usize..6, proptest::option::of(0usize..6))
            .prop_map(|(a, s, to)| Step::Delete { a, s, to }),
        1 => Just(Step::Sync),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/optional_stream.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// After any command sequence on two replicas, once they have exchanged
    /// everything, they hold the same tasks in the same streams, and no live
    /// task, block or routine reads as being in a deleted stream
    /// (ADR-0046 §Consequences).
    #[test]
    fn no_live_entity_reads_as_in_a_deleted_stream(
        patches in any::<bool>(),
        steps in proptest::collection::vec(step(), 1..24),
    ) {
        let mut p = pair_in(patches);
        let mut streams: Vec<EntityRef> = Vec::new();
        let mut tasks: Vec<EntityRef> = Vec::new();
        for (i, s) in steps.into_iter().enumerate() {
            let pick = |ix: Option<usize>, streams: &[EntityRef]| {
                ix.and_then(|k| (!streams.is_empty()).then(|| streams[k % streams.len()]))
            };
            match s {
                Step::Stream { a } => {
                    let (e, db) = if a { (&p.ea, &mut p.dba) } else { (&p.eb, &mut p.dbb) };
                    streams.push(stream(e, db, &format!("s{i}")));
                }
                Step::Task { a, s } => {
                    let home = pick(s, &streams);
                    let (e, db) = if a { (&p.ea, &mut p.dba) } else { (&p.eb, &mut p.dbb) };
                    tasks.push(task_in(e, db, &format!("t{i}"), home));
                }
                Step::Move { a, t, s } => {
                    if tasks.is_empty() {
                        continue;
                    }
                    let id = tasks[t % tasks.len()];
                    let home = pick(s, &streams);
                    let (e, db) = if a { (&p.ea, &mut p.dba) } else { (&p.eb, &mut p.dbb) };
                    // The task may not have reached this replica yet.
                    let _ = e.apply(db, Command::UpdateTask {
                        id,
                        patch: TaskPatch { stream_id: Some(home), ..Default::default() },
                    });
                }
                Step::Delete { a, s, to } => {
                    if streams.is_empty() {
                        continue;
                    }
                    let id = streams[s % streams.len()];
                    let disposition = pick(to, &streams)
                        .map_or(Disposition::Detach, Disposition::MoveTo);
                    let (e, db) = if a { (&p.ea, &mut p.dba) } else { (&p.eb, &mut p.dbb) };
                    // Refused when either stream is not live here: fine.
                    let _ = e.apply(db, Command::DeleteStream { id, disposition });
                }
                Step::Sync => sync(&mut p),
            }
            nothing_lives_in_a_deleted_stream(&p.dba);
            nothing_lives_in_a_deleted_stream(&p.dbb);
        }
        sync(&mut p);
        sync(&mut p);
        prop_assert_eq!(tasks_projection(&p.dba), tasks_projection(&p.dbb));
        nothing_lives_in_a_deleted_stream(&p.dba);
        nothing_lives_in_a_deleted_stream(&p.dbb);
        p.ea.assert_rebuild_identity(&p.dba);
        p.eb.assert_rebuild_identity(&p.dbb);
    }
}
