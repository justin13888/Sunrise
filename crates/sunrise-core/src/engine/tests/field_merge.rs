//! Per-field merge (ADR-0044, issue #319): `Patch` ops received from peers,
//! merged field by field with the full-state ops every build still writes.
//!
//! This build never emits a `Patch` from a command (ADR-0044 §9), so the
//! tests write them the way a peer's build would: seal one into the log with
//! [`emit_patch`], which also folds it into the writer's own state, and hand
//! its envelope to another replica.

use super::testutil::*;
use super::*;
use crate::engine::merge::{check_patch, merge_op, OpRef, PatchProblem};
use ciborium::value::Value;
use proptest::prelude::*;
use std::collections::BTreeMap;

// ---- building patches ----

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn op(kind: &str, v: Value) -> Value {
    Value::Map(vec![(text(kind), v)])
}

fn set(v: Value) -> Value {
    op("set", v)
}

fn add(elements: Vec<Value>) -> Value {
    op("add", Value::Array(elements))
}

fn remove(element: Value, tags: &[OpRef]) -> Value {
    op(
        "remove",
        Value::Array(vec![Value::Array(vec![
            element,
            Value::Array(tags.iter().map(|t| t.to_value()).collect()),
        ])]),
    )
}

fn inc(n: i64) -> Value {
    op("inc", Value::Integer(n.into()))
}

fn eref(r: EntityRef) -> Value {
    Value::serialized(&r).unwrap()
}

fn patch(target: EntityRef, create: bool, fields: Vec<(&str, Value)>) -> InnerOp {
    InnerOp::Patch(Box::new(crate::inner_op::PatchPayload {
        target,
        create,
        origin: None,
        fields: fields
            .into_iter()
            .map(|(k, v)| (k.to_owned(), sunrise_domain::CborValue(v)))
            .collect(),
        unknown: Unknowns::new(),
    }))
}

/// Seal `op` into `sender`'s log under `stream`, fold it into `sender`'s own
/// state as the receive path would, and return its envelope and op-ref.
fn emit_patch(sender: &Engine, db: &mut Db, stream: &[u8; 16], op: &InnerOp) -> (Vec<u8>, OpRef) {
    let device = sender.keychain.device_id();
    let bytes = encode_inner_op(op).unwrap();
    let (op_id, seq) = db
        .with_tx(|tx| {
            let seq = sender.next_seq_tx(tx, stream)?;
            let op_id = remote_op_id(stream, &device, seq);
            let lww = sender.lww_stamp(seq);
            sender.ops_insert(
                tx,
                &op_id,
                stream,
                seq,
                lww.hlc,
                &bytes,
                op.inner_kind(),
                op.target_kind(),
                Some(op.target_ref().bytes()),
                Some(T0),
                None,
                T0,
                &[],
            )?;
            merge_op(tx, op, &lww, *stream)?;
            Ok((op_id, seq))
        })
        .unwrap();
    (
        env_bytes(db, &op_id),
        OpRef {
            stream: *stream,
            device,
            seq,
        },
    )
}

/// The op-ref of the newest op `db` logged for `target`.
fn last_op_ref(db: &Db, target: &EntityRef) -> OpRef {
    db.conn()
        .query_row(
            "SELECT stream_id, device_id, seq FROM ops WHERE target_id = ?
             ORDER BY rowid DESC LIMIT 1",
            params![&target.bytes()[..]],
            |r| {
                Ok(OpRef {
                    stream: blob16(&r.get::<_, Vec<u8>>(0)?),
                    device: blob16(&r.get::<_, Vec<u8>>(1)?),
                    seq: u64::try_from(r.get::<_, i64>(2)?).unwrap(),
                })
            },
        )
        .unwrap()
}

/// Two replicas of one vault that trust each other, on clocks of their own.
struct Pair {
    ea: Engine,
    eb: Engine,
    ca: Arc<FakeClock>,
    cb: Arc<FakeClock>,
    dba: Db,
    dbb: Db,
}

fn pair() -> Pair {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    Pair {
        ea,
        eb,
        ca,
        cb,
        dba,
        dbb,
    }
}

/// A task created on A with a full-state op and delivered to B.
fn shared_task(p: &mut Pair, draft: TaskDraft) -> EntityRef {
    let task =
        p.ea.apply(&mut p.dba, Command::CreateTask(draft))
            .unwrap()
            .entity;
    p.eb.apply_remote(&mut p.dbb, &create_env_for(&p.dba, task.bytes()))
        .unwrap();
    task
}

fn task_at(db: &Db, id: EntityRef) -> Task {
    read_task(db.conn(), id.bytes())
        .unwrap()
        .expect("projected")
}

fn inbox() -> [u8; 16] {
    sunrise_domain::INBOX_STREAM_BYTES
}

/// Both replicas hold the same projection of `task`, row stamp included.
fn assert_converged(p: &Pair, task: EntityRef) -> Task {
    let a = task_at(&p.dba, task);
    let b = task_at(&p.dbb, task);
    assert_eq!(a, b, "the two replicas project the same task");
    assert_eq!(
        row_stamp(&p.dba, "tasks", "id", &task),
        row_stamp(&p.dbb, "tasks", "id", &task),
    );
    a
}

// ---- the four field types ----

/// The loss #319 names first: A renames the task while B sets its priority.
/// Under full-state last-writer-wins one edit replaced the other. Each is now
/// a write to its own register, and both survive on both replicas.
#[test]
fn concurrent_patches_to_different_fields_both_survive() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 1_000);
    let (rename, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("title", set(text("renamed")))]),
    );
    set_clock(&p.cb, T0 + 2_000);
    let (priority, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(
            task,
            false,
            vec![("priority", set(Value::Integer(4.into())))],
        ),
    );
    p.ea.apply_remote(&mut p.dba, &priority).unwrap();
    p.eb.apply_remote(&mut p.dbb, &rename).unwrap();

    let merged = assert_converged(&p, task);
    assert_eq!(merged.title, "renamed");
    assert_eq!(merged.priority, Some(4));
}

/// Two devices each add a context: an OR-set keeps both, where the old merge
/// kept one.
#[test]
fn concurrent_adds_to_a_set_both_survive() {
    let mut p = pair();
    let home = new_context(&p.ea, &mut p.dba, "home");
    let work = new_context(&p.ea, &mut p.dba, "work");
    for c in [home, work] {
        p.eb.apply_remote(
            &mut p.dbb,
            &env_for_kind(&p.dba, c.bytes(), "context.create"),
        )
        .unwrap();
    }
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    let (a_adds, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("contexts", add(vec![eref(home)]))]),
    );
    let (b_adds, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(task, false, vec![("contexts", add(vec![eref(work)]))]),
    );
    p.ea.apply_remote(&mut p.dba, &b_adds).unwrap();
    p.eb.apply_remote(&mut p.dbb, &a_adds).unwrap();

    let merged = assert_converged(&p, task);
    assert_eq!(merged.contexts, BTreeSet::from([home, work]));
}

/// Observed-remove, add-wins. B removes the context it saw A add; A adds the
/// same context again concurrently. The new add carries a tag B never
/// observed, so it survives the remove on both replicas. A remove of the tag
/// that was observed does take effect.
#[test]
fn a_remove_takes_only_the_adds_its_writer_observed() {
    let mut p = pair();
    let home = new_context(&p.ea, &mut p.dba, "home");
    p.eb.apply_remote(
        &mut p.dbb,
        &env_for_kind(&p.dba, home.bytes(), "context.create"),
    )
    .unwrap();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            contexts: vec![home],
            ..Default::default()
        },
    );
    // The create added `home` under the create's own tag.
    let create_tag = last_op_ref(&p.dba, &task);

    let (b_removes, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(
            task,
            false,
            vec![("contexts", remove(eref(home), &[create_tag]))],
        ),
    );
    assert!(
        task_at(&p.dbb, task).contexts.is_empty(),
        "the observed add is removed where it was removed"
    );
    let (a_readds, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("contexts", add(vec![eref(home)]))]),
    );
    p.ea.apply_remote(&mut p.dba, &b_removes).unwrap();
    p.eb.apply_remote(&mut p.dbb, &a_readds).unwrap();

    let merged = assert_converged(&p, task);
    assert_eq!(merged.contexts, BTreeSet::from([home]), "add wins");
}

/// Two devices each defer the task once: a PN-counter counts two, where the
/// old merge counted one.
#[test]
fn concurrent_increments_both_count() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    let (a_defers, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("deferred_count", inc(1))]),
    );
    let (b_defers, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(task, false, vec![("deferred_count", inc(1))]),
    );
    p.ea.apply_remote(&mut p.dba, &b_defers).unwrap();
    p.eb.apply_remote(&mut p.dbb, &a_defers).unwrap();
    // A re-delivery is the same op, counted once.
    p.eb.apply_remote(&mut p.dbb, &a_defers).unwrap();

    assert_eq!(assert_converged(&p, task).deferred_count, 2);
}

/// A map field this build does not know: each key is its own register, so
/// two devices writing different keys both survive, and a null tombstones a
/// key. The merged map is kept with the task's unknown fields.
#[test]
fn concurrent_writes_to_different_map_keys_both_survive() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    let map = |k: &str, v: Value| op("map", Value::Map(vec![(text(k), set(v))]));
    let (a_writes, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("x_labels", map("a", text("1")))]),
    );
    let (b_writes, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(task, false, vec![("x_labels", map("b", text("2")))]),
    );
    p.ea.apply_remote(&mut p.dba, &b_writes).unwrap();
    p.eb.apply_remote(&mut p.dbb, &a_writes).unwrap();
    let merged = assert_converged(&p, task);
    assert_eq!(
        merged.unknown.get("x_labels").map(|v| v.get().clone()),
        Some(Value::Map(vec![
            (text("a"), text("1")),
            (text("b"), text("2"))
        ]))
    );

    set_clock(&p.ca, T0 + 5_000);
    let (a_clears, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("x_labels", map("a", Value::Null))]),
    );
    p.eb.apply_remote(&mut p.dbb, &a_clears).unwrap();
    let merged = assert_converged(&p, task);
    assert_eq!(
        merged.unknown.get("x_labels").map(|v| v.get().clone()),
        Some(Value::Map(vec![(text("b"), text("2"))]))
    );
}

// ---- legacy full-state ops ----

/// A full-state op is a write to every field it carries at its own stamp
/// (ADR-0044 §7). One stamped after a `Patch` replaces the patched field too,
/// exactly as entity-level last-writer-wins did, and both replicas agree
/// whichever order they saw the two in.
#[test]
fn a_later_full_state_op_writes_every_field_it_carries() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 1_000);
    let (rename, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("title", set(text("patched")))]),
    );
    set_clock(&p.cb, T0 + 2_000);
    p.eb.apply(
        &mut p.dbb,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                priority: Some(Some(2)),
                ..Default::default()
            },
        },
    )
    .unwrap();
    p.eb.apply_remote(&mut p.dbb, &rename).unwrap();
    p.ea.apply_remote(
        &mut p.dba,
        &env_for_kind(&p.dbb, task.bytes(), "task.update"),
    )
    .unwrap();

    let merged = assert_converged(&p, task);
    assert_eq!(
        (merged.title.as_str(), merged.priority),
        ("draft", Some(2)),
        "the later full-state op carried the old title, and it wins the title too"
    );
}

/// A history made only of full-state ops projects exactly as entity-level
/// last-writer-wins did: the newest op's whole state, sets included.
#[test]
fn full_state_ops_alone_reproduce_entity_level_lww() {
    let mut p = pair();
    let home = new_context(&p.ea, &mut p.dba, "home");
    let work = new_context(&p.ea, &mut p.dba, "work");
    for c in [home, work] {
        p.eb.apply_remote(
            &mut p.dbb,
            &env_for_kind(&p.dba, c.bytes(), "context.create"),
        )
        .unwrap();
    }
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            contexts: vec![home],
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 1_000);
    p.ea.apply(
        &mut p.dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                contexts: Some(vec![work]),
                title: Some("a".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    set_clock(&p.cb, T0 + 2_000);
    let b_update =
        p.eb.apply(
            &mut p.dbb,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    priority: Some(Some(5)),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    p.eb.apply_remote(
        &mut p.dbb,
        &env_for_kind(&p.dba, task.bytes(), "task.update"),
    )
    .unwrap();
    p.ea.apply_remote(&mut p.dba, &env_bytes(&p.dbb, &b_update.op_id))
        .unwrap();

    let merged = assert_converged(&p, task);
    assert_eq!(merged.title, "t", "B's later op carried the title it had");
    assert_eq!(merged.contexts, BTreeSet::from([home]), "and its set");
    assert_eq!(merged.priority, Some(5));
}

fn orset_adds(db: &Db, id: EntityRef) -> i64 {
    db.conn()
        .query_row(
            "SELECT count(*) FROM merge_orset_adds WHERE entity_id = ?",
            params![&id.bytes()[..]],
            |r| r.get(0),
        )
        .unwrap()
}

/// Each full-state op re-adds every element it carries under its own tag,
/// and every add below the newest one reads as removed. Those adds are
/// deleted when the floor rises, and a redelivered older op adds none, so a
/// set written only by full-state ops holds one add per element however many
/// ops wrote it, in any delivery order.
#[test]
fn full_state_ops_keep_one_add_per_element() {
    let mut p = pair();
    let home = new_context(&p.ea, &mut p.dba, "home");
    let work = new_context(&p.ea, &mut p.dba, "work");
    for c in [home, work] {
        p.eb.apply_remote(
            &mut p.dbb,
            &env_for_kind(&p.dba, c.bytes(), "context.create"),
        )
        .unwrap();
    }
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            contexts: vec![home, work],
            ..Default::default()
        },
    );
    let mut updates = Vec::new();
    for i in 1..=5_u64 {
        set_clock(&p.ca, T0 + i * 1_000);
        let res =
            p.ea.apply(
                &mut p.dba,
                Command::UpdateTask {
                    id: task,
                    patch: TaskPatch {
                        title: Some(format!("t{i}")),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        updates.push(env_bytes(&p.dba, &res.op_id));
    }
    // The newest first, then every older one late.
    let newest = updates.pop().unwrap();
    p.eb.apply_remote(&mut p.dbb, &newest).unwrap();
    for env in &updates {
        p.eb.apply_remote(&mut p.dbb, env).unwrap();
    }
    let merged = task_at(&p.dbb, task);
    assert_eq!(merged.title, "t5");
    assert_eq!(merged.contexts, BTreeSet::from([home, work]));
    assert_eq!(orset_adds(&p.dbb, task), 2, "one add per element");
}

/// The legacy floor and a register compare the same full stamp. Two
/// full-state ops of one device with an equal `(hlc, seq)` in two streams are
/// told apart by the stream alone, and the one with the greater stream is both
/// the floor and every register's value, whichever arrives last.
#[test]
fn an_equal_clock_in_two_streams_is_decided_by_the_stream() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    let base = task_at(&p.dba, task);
    let lww = crate::engine::lww::LwwStamp {
        hlc: sunrise_cbor::hlc::Hlc::at(T0 + 10_000),
        device: [0x09; 16],
        seq: 7,
    };
    let lesser = (
        InnerOp::TaskUpdate(Task {
            title: "lesser stream".into(),
            ..base.clone()
        }),
        [0x01; 16],
    );
    let greater = (
        InnerOp::TaskUpdate(Task {
            title: "greater stream".into(),
            ..base
        }),
        [0x02; 16],
    );
    for (db, order) in [
        (&mut p.dba, [&greater, &lesser]),
        (&mut p.dbb, [&lesser, &greater]),
    ] {
        db.with_tx(|tx| {
            for (op, stream) in order {
                merge_op(tx, op, &lww, *stream)?;
            }
            Ok(())
        })
        .unwrap();
    }
    let merged = assert_converged(&p, task);
    assert_eq!(merged.title, "greater stream");
}

/// A routine's occurrence create is `generated` (ADR-0044 §3) on every
/// replica: on the one that generated it, whose merge folds the row the
/// command wrote, as on a peer that replayed the op.
#[test]
fn a_generated_occurrence_is_generated_on_every_replica() {
    let mut p = pair();
    p.ea.apply(
        &mut p.dba,
        Command::CreateRoutine(routine_draft(
            inbox_stream_ref(),
            "FREQ=DAILY",
            i64::try_from(T0).unwrap() + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        )),
    )
    .unwrap();
    let occurrence: Vec<u8> = p
        .dba
        .conn()
        .query_row(
            "SELECT target_id FROM ops WHERE inner_kind = 'task.create' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let task = EntityRef::new(EntityKind::Task, blob16(&occurrence));
    assert!(task_at(&p.dba, task).routine_id.is_some());
    p.eb.apply_remote(&mut p.dbb, &create_env_for(&p.dba, task.bytes()))
        .unwrap();
    // Any op on the task makes the generating replica fold its row.
    set_clock(&p.ca, T0 + 1_000);
    let (edit, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(
            task,
            false,
            vec![("priority", set(Value::Integer(1.into())))],
        ),
    );
    p.eb.apply_remote(&mut p.dbb, &edit).unwrap();
    let origin = |db: &Db, field: &str| -> String {
        db.conn()
            .query_row(
                "SELECT origin FROM merge_registers WHERE entity_id = ? AND field = ?",
                params![&task.bytes()[..], field],
                |r| r.get(0),
            )
            .unwrap()
    };
    for db in [&p.dba, &p.dbb] {
        assert_eq!(origin(db, "title"), "generated");
        assert_eq!(origin(db, "priority"), "user");
    }
}

/// An entity the vault held before migration 0033 has a row and no field
/// state. The first `Patch` to reach it seeds the state from the row, so the
/// fields the patch does not name keep the values the row held.
#[test]
fn a_patch_to_an_entity_with_no_field_state_keeps_its_other_fields() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "kept".into(),
            priority: Some(3),
            ..Default::default()
        },
    );
    // What a vault that took migration 0033 looks like: rows, no state.
    for table in [
        "merge_entities",
        "merge_registers",
        "merge_orset_adds",
        "merge_orset_removes",
        "merge_counter_deltas",
        "merge_map_entries",
    ] {
        p.dbb
            .conn()
            .execute(&format!("DELETE FROM {table}"), [])
            .unwrap();
    }
    let (energy, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("deferred_count", inc(1))]),
    );
    p.eb.apply_remote(&mut p.dbb, &energy).unwrap();

    let merged = assert_converged(&p, task);
    assert_eq!(merged.title, "kept");
    assert_eq!(merged.priority, Some(3));
    assert_eq!(merged.deferred_count, 1);
}

// ---- create, delete, and visibility ----

/// An edit that arrives after a delete is kept inside the tombstoned entity
/// and does not resurrect it (ADR-0044 §5). A restore shows it.
#[test]
fn an_edit_after_a_delete_stays_inside_the_tombstone() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 1_000);
    let (delete, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("deleted", set(Value::Bool(true)))]),
    );
    set_clock(&p.cb, T0 + 2_000);
    let (edit, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(task, false, vec![("title", set(text("edited later")))]),
    );
    p.ea.apply_remote(&mut p.dba, &edit).unwrap();
    p.eb.apply_remote(&mut p.dbb, &delete).unwrap();
    let merged = assert_converged(&p, task);
    assert!(merged.deleted, "a later edit does not undo the delete");
    assert_eq!(merged.title, "edited later", "and it is not lost");

    set_clock(&p.ca, T0 + 3_000);
    let (restore, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, false, vec![("deleted", set(Value::Bool(false)))]),
    );
    p.eb.apply_remote(&mut p.dbb, &restore).unwrap();
    let merged = assert_converged(&p, task);
    assert!(!merged.deleted);
    assert_eq!(merged.title, "edited later");
}

/// A write that arrives before its entity's create is merged and waits: the
/// entity is not projected until a create is applied (ADR-0044 §4), and then
/// it shows both.
#[test]
fn a_write_before_the_create_waits_for_it() {
    let mut p = pair();
    let task = EntityRef::new(EntityKind::Task, [0x77; 16]);
    let (create, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(task, true, vec![("title", set(text("made by a patch")))]),
    );
    set_clock(&p.ca, T0 + 1_000);
    let (edit, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(
            task,
            false,
            vec![("priority", set(Value::Integer(1.into())))],
        ),
    );
    p.eb.apply_remote(&mut p.dbb, &edit).unwrap();
    assert!(
        read_task(p.dbb.conn(), task.bytes()).unwrap().is_none(),
        "not visible before its create"
    );
    p.eb.apply_remote(&mut p.dbb, &create).unwrap();
    let merged = assert_converged(&p, task);
    assert_eq!(merged.title, "made by a patch");
    assert_eq!(merged.priority, Some(1));
    assert_eq!(
        merged.state,
        TaskState::Todo,
        "an omitted field reads its default"
    );
    assert_eq!(
        merged.created_at.as_millisecond(),
        i64::try_from(T0).unwrap(),
        "created_at is the create's own time"
    );
    assert_eq!(
        merged.updated_at.as_millisecond(),
        i64::try_from(T0 + 1_000).unwrap(),
        "updated_at is the newest op's time"
    );
}

/// A tombstoned context is not shown on any task, and the task's set keeps it
/// (ADR-0044 §5). A restore re-projects every task whose set still holds the
/// context, so its membership shows again on both replicas.
#[test]
fn a_restored_context_shows_again_on_its_tasks() {
    let mut p = pair();
    let home = new_context(&p.ea, &mut p.dba, "home");
    p.eb.apply_remote(
        &mut p.dbb,
        &env_for_kind(&p.dba, home.bytes(), "context.create"),
    )
    .unwrap();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            contexts: vec![home],
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 1_000);
    p.ea.apply(&mut p.dba, Command::DeleteContext(home))
        .unwrap();
    p.eb.apply_remote(
        &mut p.dbb,
        &env_for_kind(&p.dba, home.bytes(), "context.delete"),
    )
    .unwrap();
    assert!(
        task_at(&p.dbb, task).contexts.is_empty(),
        "purged on delete"
    );

    // A later task write does not bring the tombstoned context back.
    set_clock(&p.cb, T0 + 2_000);
    let (b_edits, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &patch(
            task,
            false,
            vec![("priority", set(Value::Integer(2.into())))],
        ),
    );
    p.ea.apply_remote(&mut p.dba, &b_edits).unwrap();
    assert!(assert_converged(&p, task).contexts.is_empty());

    set_clock(&p.ca, T0 + 3_000);
    let (restore, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &META_STREAM,
        &patch(home, false, vec![("deleted", set(Value::Bool(false)))]),
    );
    p.eb.apply_remote(&mut p.dbb, &restore).unwrap();
    let merged = assert_converged(&p, task);
    assert_eq!(merged.contexts, BTreeSet::from([home]), "shown again");
    assert_eq!(merged.priority, Some(2));
}

/// A `Patch` create that leaves out a required field with no default is
/// kept and not projected (ADR-0044 §4, "nothing is dropped while it
/// waits"), and the write that supplies the field projects it.
#[test]
fn an_entity_missing_a_required_field_waits_for_it() {
    let mut p = pair();
    let block = EntityRef::new(EntityKind::Block, [0x7a; 16]);
    let at = |ms: u64| {
        Value::serialized(&SunriseTime::Instant {
            at: ms_to_ts(i64::try_from(ms).unwrap()),
        })
        .unwrap()
    };
    let (create, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(
            block,
            true,
            vec![
                (
                    "stream_id",
                    set(Value::Text(inbox_stream_ref().to_string())),
                ),
                ("starts_at", set(at(T0))),
            ],
        ),
    );
    p.eb.apply_remote(&mut p.dbb, &create).unwrap();
    assert!(read_block(p.dbb.conn(), block.bytes()).unwrap().is_none());

    set_clock(&p.ca, T0 + 1_000);
    let (ends, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &patch(block, false, vec![("ends_at", set(at(T0 + 3_600_000)))]),
    );
    p.eb.apply_remote(&mut p.dbb, &ends).unwrap();
    let a = read_block(p.dba.conn(), block.bytes())
        .unwrap()
        .expect("projected");
    let b = read_block(p.dbb.conn(), block.bytes())
        .unwrap()
        .expect("projected");
    assert_eq!(a, b);
    assert_eq!(
        b.ends_at,
        SunriseTime::Instant {
            at: ms_to_ts(i64::try_from(T0 + 3_600_000).unwrap())
        }
    );
}

// ---- what a patch may not do ----

/// A field-op kind this build does not know cannot be merged correctly, so
/// the whole op is parked and none of it applied (ADR-0044 §8).
#[test]
fn a_patch_with_an_unknown_field_op_kind_is_parked_whole() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "t".into(),
            ..Default::default()
        },
    );
    let future = patch(
        task,
        false,
        vec![
            ("title", set(text("must not apply"))),
            ("x_body", op("splice", Value::Integer(0.into()))),
        ],
    );
    let env = super::emit_raw_inner(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &encode_inner_op(&future).unwrap(),
    );
    assert!(p.eb.apply_remote_all(&mut p.dbb, &env).unwrap().is_empty());
    assert_eq!(task_at(&p.dbb, task).title, "t", "nothing of it applied");
    let parked: Vec<(String, String)> = p
        .dbb
        .conn()
        .prepare("SELECT reason, kind FROM parked_ops")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(parked, vec![("unknown_kind".into(), "Patch.splice".into())]);
}

/// A patch that writes a field this build knows against its CRDT type, with a
/// value the field cannot hold, or to a field no patch may write, is refused
/// as malformed, before anything is written.
#[test]
fn an_ill_typed_patch_is_refused() {
    let task = EntityRef::new(EntityKind::Task, [0x78; 16]);
    let routine = EntityRef::new(EntityKind::Routine, [0x7b; 16]);
    let refused: Vec<(EntityRef, &str, Value)> = vec![
        (task, "title", set(Value::Integer(3.into()))),
        (task, "title", inc(1)),
        (task, "contexts", set(Value::Array(Vec::new()))),
        (task, "contexts", add(vec![Value::Integer(1.into())])),
        (task, "deferred_count", set(Value::Integer(1.into()))),
        (task, "id", set(eref(task))),
        (task, "created_at", set(Value::Integer(1.into()))),
        (task, "updated_at", set(Value::Integer(1.into()))),
        (task, "blocks", add(vec![text("blk_x")])),
        // The nested parent is not a register: each `template.*` field is.
        (routine, "template", set(Value::Map(Vec::new()))),
    ];
    for (target, field, value) in refused {
        let InnerOp::Patch(p) = patch(target, true, vec![(field, value.clone())]) else {
            unreachable!()
        };
        let verdict = check_patch(&p);
        assert!(
            matches!(verdict, Err(PatchProblem::Invalid(_))),
            "{field} <- {value:?} must be refused, got {verdict:?}"
        );
    }
    for (target, field, value) in [
        (task, "title", set(text("x"))),
        (routine, "template.title", set(text("x"))),
    ] {
        let InnerOp::Patch(p) = patch(target, true, vec![(field, value)]) else {
            unreachable!()
        };
        assert!(
            check_patch(&p).is_ok(),
            "a well-typed {field} set is accepted"
        );
    }
}

/// A `Patch` of an entity kind this build models but does not sync (Note,
/// Person) is a newer build's write, and waits for an upgrade. One of an
/// append-only or control kind is never a field-op write, and is refused.
/// Both verdicts come before any field is read.
#[test]
fn a_patch_of_a_kind_this_build_does_not_merge_is_parked_or_refused() {
    let body = vec![("deleted", set(Value::Bool(true)))];
    for kind in [EntityKind::Note, EntityKind::Person] {
        let InnerOp::Patch(p) = patch(EntityRef::new(kind, [0x7c; 16]), true, body.clone()) else {
            unreachable!()
        };
        assert_eq!(
            check_patch(&p),
            Err(PatchProblem::Park("Patch".into())),
            "{kind:?}"
        );
    }
    for kind in [
        EntityKind::FocusSession,
        EntityKind::ReviewSnapshot,
        EntityKind::Device,
        EntityKind::Identity,
    ] {
        let InnerOp::Patch(p) = patch(EntityRef::new(kind, [0x7d; 16]), true, body.clone()) else {
            unreachable!()
        };
        assert!(
            matches!(check_patch(&p), Err(PatchProblem::Invalid(_))),
            "{kind:?}"
        );
    }

    // And through the receive path: the Note patch is parked whole.
    let mut p = pair();
    let note = patch(EntityRef::new(EntityKind::Note, [0x7e; 16]), true, body);
    let env = super::emit_raw_inner(
        &p.ea,
        &mut p.dba,
        &META_STREAM,
        &encode_inner_op(&note).unwrap(),
    );
    assert!(p.eb.apply_remote_all(&mut p.dbb, &env).unwrap().is_empty());
    let parked: Vec<String> = p
        .dbb
        .conn()
        .prepare("SELECT kind FROM parked_ops")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(parked, vec!["Patch".to_owned()]);
}

/// The wire shape is ADR-0044 §1's: `{"Patch": {"ref": .., "fields": ..}}`,
/// with `create` absent when false.
#[test]
fn a_patch_encodes_as_the_adr_shape() {
    let task = EntityRef::new(EntityKind::Task, [0x79; 16]);
    let bytes = encode_inner_op(&patch(task, false, vec![("title", set(text("x")))])).unwrap();
    let value: Value = ciborium::de::from_reader(bytes.as_slice()).unwrap();
    let Value::Map(outer) = value else { panic!() };
    assert_eq!(outer[0].0, text("Patch"));
    let Value::Map(inner) = &outer[0].1 else {
        panic!()
    };
    let keys: Vec<&str> = inner.iter().filter_map(|(k, _)| k.as_text()).collect();
    assert_eq!(keys, ["ref", "fields"]);
    let back = decode_inner_op(&bytes).unwrap();
    assert_eq!(back.inner_kind(), "task.patch");
    assert_eq!(back.target_ref(), task);
}

// ---- convergence ----

/// One write a device makes to the shared task, before any device sees
/// another's.
#[derive(Debug, Clone)]
enum Write {
    Title(u8),
    Priority(u8),
    AddContext(usize),
    /// Remove a context the device itself added earlier, by the tags it saw.
    RemoveOwnContext(usize),
    Defer(i8),
    Label(u8, u8),
    /// A rename through the command path, which writes a full-state op: a
    /// write to every field, at its own stamp (ADR-0044 §7).
    FullState(u8),
}

fn arb_write() -> impl Strategy<Value = Write> {
    // Set edits are weighted up: a remove only means something after an add
    // of the same element by the same device.
    prop_oneof![
        2 => (0u8..4).prop_map(Write::Title),
        2 => (1u8..=5).prop_map(Write::Priority),
        3 => (0usize..2).prop_map(Write::AddContext),
        3 => (0usize..2).prop_map(Write::RemoveOwnContext),
        2 => prop_oneof![Just(1i8), Just(-1i8)].prop_map(Write::Defer),
        2 => (0u8..2, 0u8..3).prop_map(|(k, v)| Write::Label(k, v)),
        1 => (0u8..4).prop_map(Write::FullState),
    ]
}

/// A write's order, as the model compares it: `(hlc, device, seq)`, the hlc
/// packed into one integer. Every write here is in one stream.
type Order = (u64, [u8; 16], u64);

/// What a write should leave behind, computed without the engine.
#[derive(Debug, Default)]
struct Oracle {
    title: Option<(Order, String)>,
    priority: Option<(Order, u8)>,
    adds: Vec<(usize, OpRef)>,
    removes: Vec<(usize, OpRef)>,
    deferred: i64,
    labels: BTreeMap<u8, (Order, u8)>,
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        // The repository's convention (docs/10-cross-cutting/testing.md §2):
        // a shrunken counterexample is committed, at a path named here.
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/field_merge.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// ADR-0044's convergence property, for every field type at once. Three
    /// devices write to one task without seeing each other: registers, an
    /// OR-set with observed removes, a counter and a map, mixed with
    /// full-state renames from the command path. Receivers take every op
    /// forward, reversed, and in an arbitrary order with repeats, and must
    /// project byte for byte the same task. Where no full-state op was
    /// written, the projection must also be exactly what an engine-free model
    /// says: the greatest write of each register and map key, every add no
    /// remove observed, and the sum of the deltas.
    #[test]
    fn any_delivery_order_converges_and_loses_no_concurrent_write(
        writes in proptest::collection::vec((0usize..3, arb_write()), 1..14),
        order in proptest::collection::vec(any::<prop::sample::Index>(), 0..40),
    ) {
        let clocks: Vec<Arc<FakeClock>> =
            (0..3).map(|_| Arc::new(FakeClock(PLMutex::new(T0)))).collect();
        let devices: Vec<Engine> = (0..3u8)
            .map(|i| engine_seeded(ROOT, [i + 1; 32], clocks[usize::from(i)].clone()))
            .collect();
        let mut dbs: Vec<Db> = (0..3).map(|_| db_root(ROOT)).collect();
        for (i, e) in devices.iter().enumerate() {
            for (j, other) in devices.iter().enumerate() {
                if i != j {
                    trust(e, &mut dbs[i], other);
                }
            }
        }
        let contexts: Vec<EntityRef> = (0..3u8)
            .map(|i| EntityRef::new(EntityKind::Context, [0xc0 + i; 16]))
            .collect();

        // Device 0 creates the task and every device sees the create.
        let task = devices[0]
            .apply(&mut dbs[0], Command::CreateTask(TaskDraft {
                title: "t".into(),
                ..Default::default()
            }))
            .unwrap()
            .entity;
        let create = create_env_for(&dbs[0], task.bytes());
        for i in 1..3 {
            devices[i].apply_remote(&mut dbs[i], &create).unwrap();
        }

        let mut oracle = Oracle::default();
        let mut saw_full_state = false;
        let mut envelopes = vec![create];
        for (n, (who, write)) in writes.iter().enumerate() {
            set_clock(&clocks[*who], T0 + 1_000 + u64::try_from(n).unwrap() * 7);
            let device = devices[*who].keychain.device_id();
            let fields = match write {
                Write::Title(t) => vec![("title", set(text(&format!("title {t}"))))],
                Write::Priority(v) => vec![("priority", set(Value::Integer((*v).into())))],
                Write::AddContext(c) => vec![("contexts", add(vec![eref(contexts[*c])]))],
                Write::RemoveOwnContext(c) => {
                    let seen: Vec<OpRef> = oracle
                        .adds
                        .iter()
                        .filter(|(e, tag)| e == c && tag.device == device)
                        .map(|(_, tag)| *tag)
                        .collect();
                    if seen.is_empty() {
                        continue;
                    }
                    vec![("contexts", remove(eref(contexts[*c]), &seen))]
                }
                Write::Defer(d) => vec![("deferred_count", inc(i64::from(*d)))],
                Write::Label(k, v) => vec![(
                    "x_labels",
                    op("map", Value::Map(vec![(text(&format!("k{k}")), set(Value::Integer((*v).into())))])),
                )],
                Write::FullState(t) => {
                    let res = devices[*who]
                        .apply(&mut dbs[*who], Command::UpdateTask {
                            id: task,
                            patch: TaskPatch {
                                title: Some(format!("full {t}")),
                                ..Default::default()
                            },
                        })
                        .unwrap();
                    envelopes.push(env_bytes(&dbs[*who], &res.op_id));
                    saw_full_state = true;
                    continue;
                }
            };
            let (env, tag) = emit_patch(
                &devices[*who],
                &mut dbs[*who],
                &inbox(),
                &patch(task, false, fields),
            );
            let hlc = decode_envelope(&env).unwrap().hlc;
            let stamp = (hlc.physical_ms << 20 | u64::from(hlc.logical), device, tag.seq);
            match write {
                Write::Title(t) => {
                    if oracle.title.as_ref().is_none_or(|(s, _)| stamp > *s) {
                        oracle.title = Some((stamp, format!("title {t}")));
                    }
                }
                Write::Priority(v) => {
                    if oracle.priority.as_ref().is_none_or(|(s, _)| stamp > *s) {
                        oracle.priority = Some((stamp, *v));
                    }
                }
                Write::AddContext(c) => oracle.adds.push((*c, tag)),
                Write::RemoveOwnContext(c) => {
                    let seen: Vec<OpRef> = oracle
                        .adds
                        .iter()
                        .filter(|(e, t)| e == c && t.device == device)
                        .map(|(_, t)| *t)
                        .collect();
                    oracle.removes.extend(seen.into_iter().map(|t| (*c, t)));
                }
                Write::Defer(d) => oracle.deferred += i64::from(*d),
                Write::Label(k, v) => {
                    if oracle.labels.get(k).is_none_or(|(s, _)| stamp > *s) {
                        oracle.labels.insert(*k, (stamp, *v));
                    }
                }
                Write::FullState(_) => unreachable!("handled above"),
            }
            envelopes.push(env);
        }

        // Two receivers, two delivery orders, repeats included. The create
        // is not delivered first on purpose: whatever lands before it waits.
        let receiver = |deliveries: &[usize]| -> (Task, (i64, i64, i64, Vec<u8>)) {
            let e = engine_seeded(ROOT, [9u8; 32], Arc::new(FakeClock(PLMutex::new(T0 + 100_000))));
            let mut db = db_root(ROOT);
            for d in &devices {
                trust(&e, &mut db, d);
            }
            for i in deliveries {
                e.apply_remote(&mut db, &envelopes[*i]).unwrap();
            }
            for env in &envelopes {
                e.apply_remote(&mut db, env).unwrap();
            }
            (task_at(&db, task), row_stamp(&db, "tasks", "id", &task))
        };
        let forward: Vec<usize> = (0..envelopes.len()).collect();
        let shuffled: Vec<usize> = order.iter().map(|ix| ix.index(envelopes.len())).collect();
        let reversed: Vec<usize> = (0..envelopes.len()).rev().collect();
        let (a, a_stamp) = receiver(&forward);
        let (b, b_stamp) = receiver(&shuffled);
        let (c, c_stamp) = receiver(&reversed);
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(&a, &c);
        prop_assert_eq!(&a_stamp, &b_stamp);
        prop_assert_eq!(&a_stamp, &c_stamp);

        // The model has no full-state ops: one writes every field from its
        // writer's own view, which the delivery orders above already agree on.
        if saw_full_state {
            return Ok(());
        }
        prop_assert_eq!(
            a.title,
            oracle.title.map_or_else(|| "t".to_owned(), |(_, t)| t)
        );
        prop_assert_eq!(a.priority, oracle.priority.map(|(_, v)| v));
        let live: BTreeSet<EntityRef> = oracle
            .adds
            .iter()
            .filter(|add| !oracle.removes.contains(add))
            .map(|(c, _)| contexts[*c])
            .collect();
        prop_assert_eq!(&a.contexts, &live);
        prop_assert_eq!(a.deferred_count, oracle.deferred);
        let labels = a.unknown.get("x_labels").map(|v| v.get().clone());
        let expected = (!oracle.labels.is_empty()).then(|| {
            Value::Map(
                oracle
                    .labels
                    .iter()
                    .map(|(k, (_, v))| (text(&format!("k{k}")), Value::Integer((*v).into())))
                    .collect(),
            )
        });
        prop_assert_eq!(labels, expected);
    }
}
