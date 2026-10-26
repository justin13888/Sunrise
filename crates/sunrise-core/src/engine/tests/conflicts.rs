//! The register fold with conflict classes, the conflicts query, and the
//! commands that settle a conflict (ADR-0065 §3–§4, PR 5).
//!
//! This build writes full-state ops from its commands (clarification D9),
//! and two full-state ops never surface a conflict (D4). So the concurrent
//! writes here are `Patch` versions naming parents, sealed the way a build
//! that writes them will ([`emit_patch`]), and handed to the other replica.

use super::field_merge::{emit_patch, inbox, pair, set, shared_task, task_at, text, Pair};
use super::testutil::*;
use super::*;
use crate::conflicts::{ConflictClass, EntityConflict, WriteOrigin};
use crate::engine::merge::{heads, OpRef};
use crate::inner_op::{OpRefWire, PatchPayload};
use ciborium::value::Value;
use proptest::prelude::*;

// ---- helpers ----

/// A `Patch` of `task` setting `fields`, naming `parents`.
pub(super) fn versioned(
    task: EntityRef,
    fields: &[(&str, Value)],
    parents: Option<&[OpRef]>,
    generated: bool,
) -> InnerOp {
    InnerOp::Patch(Box::new(PatchPayload {
        target: task,
        create: false,
        origin: generated.then(|| PatchPayload::GENERATED.to_owned()),
        parents: parents.map(|ps| {
            ps.iter()
                .map(|p| OpRefWire(p.stream, p.device, p.seq))
                .collect()
        }),
        group: None,
        fields: fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), sunrise_domain::CborValue(set(v.clone()))))
            .collect(),
        unknown: Unknowns::new(),
    }))
}

pub(super) fn heads_in(db: &Db, task: EntityRef) -> Vec<OpRef> {
    let tx = db.conn().unchecked_transaction().unwrap();
    let out = heads(&tx, task.bytes()).unwrap();
    tx.rollback().unwrap();
    out
}

/// Write `fields` on `who`'s side of `p` at `at`, naming the heads that side
/// has applied, and return the envelope and the version.
fn write_on(
    p: &mut Pair,
    who: usize,
    at: u64,
    task: EntityRef,
    fields: &[(&str, Value)],
) -> (Vec<u8>, OpRef) {
    let (e, db, clock) = if who == 0 {
        (&p.ea, &mut p.dba, &p.ca)
    } else {
        (&p.eb, &mut p.dbb, &p.cb)
    };
    set_clock(clock, T0 + at);
    let parents = heads_in(db, task);
    emit_patch(
        e,
        db,
        &inbox(),
        &versioned(task, fields, Some(&parents), false),
    )
}

pub(super) fn conflicts_of(e: &Engine, db: &Db, quiet: bool) -> Vec<EntityConflict> {
    match e
        .query(
            db,
            Query::Conflicts {
                entity: None,
                include_quiet: quiet,
            },
        )
        .unwrap()
    {
        QueryResult::Conflicts(c) => c,
        other => panic!("{other:?}"),
    }
}

fn state(s: &str) -> Value {
    text(s)
}

/// The registers and losers `task` holds, row by row.
fn register_state(db: &Db, task: EntityRef) -> Vec<String> {
    let mut out = Vec::new();
    for table in ["merge_registers", "register_losers"] {
        let mut stmt = db
            .conn()
            .prepare(&format!("SELECT * FROM {table} WHERE entity_id = ?"))
            .unwrap();
        let n = stmt.column_count();
        let rows = stmt
            .query_map([&task.bytes()[..]], |r| {
                (0..n)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(|cols| format!("{table}: {}", cols.join(" | ")))
            })
            .unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }
    out.sort();
    out
}

fn deliver(to: &Engine, db: &mut Db, envs: &[&Vec<u8>]) {
    for env in envs {
        to.apply_remote(db, env).unwrap();
    }
}

/// A replica that trusts both sides of `p` and has applied nothing.
fn third(p: &Pair) -> (Engine, Db) {
    let e = engine_seeded(
        ROOT,
        [9u8; 32],
        Arc::new(FakeClock(PLMutex::new(T0 + 1_000_000))),
    );
    let mut db = db_root(ROOT);
    trust(&e, &mut db, &p.ea);
    trust(&e, &mut db, &p.eb);
    (e, db)
}

fn draft(title: &str) -> TaskDraft {
    TaskDraft {
        title: title.into(),
        ..Default::default()
    }
}

/// Both sides of `p` apply what the other wrote, then hold the same task,
/// the same register state and the same conflicts, and each rebuilds to
/// exactly what it holds.
fn exchange(p: &mut Pair, task: EntityRef, from_a: &[&Vec<u8>], from_b: &[&Vec<u8>]) {
    deliver(&p.eb, &mut p.dbb, from_a);
    deliver(&p.ea, &mut p.dba, from_b);
    assert_eq!(task_at(&p.dba, task), task_at(&p.dbb, task));
    assert_eq!(register_state(&p.dba, task), register_state(&p.dbb, task));
    assert_eq!(
        conflicts_of(&p.ea, &p.dba, true),
        conflicts_of(&p.eb, &p.dbb, true)
    );
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

// ---- state: terminal over open ----

/// A completion and a concurrent move to `in_progress`: the terminal state
/// wins whatever the stamps, and the other is kept and reported.
#[test]
fn terminal_beats_a_concurrent_reopen() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (done, _) = write_on(&mut p, 0, 1_000, task, &[("state", state("done"))]);
    let (open, open_v) = write_on(&mut p, 1, 2_000, task, &[("state", state("in_progress"))]);
    exchange(&mut p, task, &[&done], &[&open]);

    assert_eq!(task_at(&p.dba, task).state, TaskState::Done);
    let c = conflicts_of(&p.ea, &p.dba, false);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(
        (c[0].field.as_str(), c[0].class),
        ("state", ConflictClass::State)
    );
    assert_eq!(c[0].losers.len(), 1);
    assert_eq!(c[0].losers[0].version.seq, open_v.seq);
    assert_eq!(
        c[0].losers[0].value.as_ref().map(|v| v.get()),
        Some(&state("in_progress"))
    );
}

/// A reopen written after seeing the completion is an ordinary later write.
#[test]
fn a_reopen_that_saw_the_completion_wins() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (done, _) = write_on(&mut p, 0, 1_000, task, &[("state", state("done"))]);
    deliver(&p.eb, &mut p.dbb, &[&done]);
    let (reopen, _) = write_on(&mut p, 1, 2_000, task, &[("state", state("todo"))]);
    exchange(&mut p, task, &[], &[&reopen]);

    assert_eq!(task_at(&p.dba, task).state, TaskState::Todo);
    assert!(conflicts_of(&p.ea, &p.dba, true).is_empty());
}

/// Two concurrent terminal states are decided by stamp.
#[test]
fn two_terminals_are_decided_by_stamp() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (done, _) = write_on(&mut p, 0, 1_000, task, &[("state", state("done"))]);
    let (cancel, _) = write_on(&mut p, 1, 2_000, task, &[("state", state("cancelled"))]);
    exchange(&mut p, task, &[&done], &[&cancel]);

    assert_eq!(task_at(&p.dba, task).state, TaskState::Cancelled);
    let c = conflicts_of(&p.ea, &p.dba, false);
    assert_eq!(c.len(), 1);
    assert_eq!(
        c[0].losers[0].value.as_ref().map(|v| v.get()),
        Some(&state("done"))
    );
}

/// The trap a pairwise fold falls into: `a ≺ c` and `b` concurrent with
/// both. `a` is a completion, `c` a reopen that saw it, `b` a concurrent
/// `in_progress`. The antichain is `{b, c}`, `a` is never compared, and
/// every delivery order lands on the same state and the same loser.
#[test]
fn the_fold_is_order_independent_when_a_precedes_c_and_b_is_concurrent_with_both() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let create = create_env_for(&p.dba, task.bytes());
    let (done, _) = write_on(&mut p, 0, 1_000, task, &[("state", state("done"))]);
    let (open, open_v) = write_on(&mut p, 1, 2_000, task, &[("state", state("in_progress"))]);
    let (reopen, _) = write_on(&mut p, 0, 3_000, task, &[("state", state("todo"))]);

    let orders: [[&Vec<u8>; 3]; 6] = [
        [&done, &open, &reopen],
        [&done, &reopen, &open],
        [&open, &done, &reopen],
        [&open, &reopen, &done],
        [&reopen, &done, &open],
        [&reopen, &open, &done],
    ];
    let mut seen = Vec::new();
    for order in orders {
        let (receiver, mut db) = third(&p);
        deliver(&receiver, &mut db, &[&create]);
        deliver(&receiver, &mut db, &order);
        assert_eq!(
            task_at(&db, task).state,
            TaskState::Todo,
            "c is the later of b and c"
        );
        let found = conflicts_of(&receiver, &db, false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].losers.len(), 1, "a is not in the antichain");
        assert_eq!(found[0].losers[0].version.seq, open_v.seq);
        receiver.assert_rebuild_identity(&db);
        seen.push((register_state(&db, task), found));
    }
    assert!(seen.windows(2).all(|w| w[0] == w[1]));
}

// ---- generated writes ----

/// A generated write concurrent with a user completion yields, though it is
/// stamped later (ADR-0065 §3, "generated writes yield").
#[test]
fn a_generated_create_concurrent_with_a_completion_yields() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("occurrence"));
    let (done, _) = write_on(&mut p, 1, 1_000, task, &[("state", state("done"))]);
    set_clock(&p.ca, T0 + 2_000);
    let parents = heads_in(&p.dba, task);
    let (generated, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &versioned(task, &[("state", state("todo"))], Some(&parents), true),
    );
    exchange(&mut p, task, &[&generated], &[&done]);

    assert_eq!(task_at(&p.dba, task).state, TaskState::Done);
    let c = conflicts_of(&p.ea, &p.dba, false);
    assert_eq!(c[0].winner.origin, WriteOrigin::User);
    assert_eq!(c[0].losers[0].origin, WriteOrigin::Generated);
}

/// A generated write that descends from a user write still replaces it.
#[test]
fn a_generated_write_beats_a_user_write_it_descends_from() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (user, _) = write_on(&mut p, 0, 1_000, task, &[("title", text("by hand"))]);
    set_clock(&p.ca, T0 + 2_000);
    let parents = heads_in(&p.dba, task);
    let (generated, _) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &versioned(
            task,
            &[("title", text("from the template"))],
            Some(&parents),
            true,
        ),
    );
    exchange(&mut p, task, &[&user, &generated], &[]);

    assert_eq!(task_at(&p.dbb, task).title, "from the template");
    assert!(conflicts_of(&p.eb, &p.dbb, true).is_empty());
}

// ---- tombstones ----

/// A delete beats a concurrent `deleted = false`, stamped later or not.
#[test]
fn delete_wins_over_a_concurrent_restore() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (delete, _) = write_on(&mut p, 0, 1_000, task, &[("deleted", Value::Bool(true))]);
    let (restore, _) = write_on(&mut p, 1, 2_000, task, &[("deleted", Value::Bool(false))]);
    exchange(&mut p, task, &[&delete], &[&restore]);

    assert!(task_at(&p.dba, task).deleted);
    let c = conflicts_of(&p.ea, &p.dba, false);
    assert_eq!(c.len(), 1);
    assert_eq!(
        (c[0].field.as_str(), c[0].class),
        ("deleted", ConflictClass::Tombstone)
    );
    assert_eq!(
        c[0].losers[0].value.as_ref().map(|v| v.get()),
        Some(&Value::Bool(false))
    );
}

/// An edit concurrent with a delete never resurrects the entity. It is
/// applied inside the tombstone and reported as edited while deleted.
#[test]
fn an_edit_concurrent_with_a_delete_is_kept_and_reported() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (delete, _) = write_on(&mut p, 0, 1_000, task, &[("deleted", Value::Bool(true))]);
    let (edit, edit_v) = write_on(&mut p, 1, 2_000, task, &[("title", text("renamed"))]);
    exchange(&mut p, task, &[&delete], &[&edit]);

    let t = task_at(&p.dba, task);
    assert!(t.deleted, "the edit did not resurrect it");
    assert_eq!(t.title, "renamed", "and is kept inside the tombstone");
    let c = conflicts_of(&p.ea, &p.dba, false);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].field, "deleted");
    assert!(c[0].losers.is_empty());
    assert_eq!(c[0].edited_while_deleted.len(), 1);
    assert_eq!(c[0].edited_while_deleted[0].version.seq, edit_v.seq);
    assert_eq!(c[0].edited_while_deleted[0].fields, ["title"]);
}

/// Restore is a later write of `deleted = false`: the task is back, with the
/// edit made while it was deleted, and nothing is in conflict, on both sides.
#[test]
fn restore_clears_the_tombstone_conflict() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (delete, _) = write_on(&mut p, 0, 1_000, task, &[("deleted", Value::Bool(true))]);
    let (edit, _) = write_on(&mut p, 1, 2_000, task, &[("title", text("renamed"))]);
    exchange(&mut p, task, &[&delete], &[&edit]);
    assert_eq!(conflicts_of(&p.ea, &p.dba, false).len(), 1);

    set_clock(&p.ca, T0 + 3_000);
    let res = p.ea.apply(&mut p.dba, Command::Restore(task)).unwrap();
    let restore = env_bytes(&p.dba, &res.op_id);
    exchange(&mut p, task, &[&restore], &[]);

    let t = task_at(&p.dbb, task);
    assert!(!t.deleted);
    assert_eq!(t.title, "renamed");
    assert!(conflicts_of(&p.ea, &p.dba, true).is_empty());
    assert!(conflicts_of(&p.eb, &p.dbb, true).is_empty());
}

/// Each entity reports its own branches: one whose delete raced a restore,
/// and another whose delete raced an edit, each under its own `deleted` row.
#[test]
fn each_deleted_entity_reports_its_own_branches() {
    let mut p = pair();
    let first = shared_task(&mut p, draft("first"));
    let second = shared_task(&mut p, draft("second"));
    let mut from_a = Vec::new();
    let mut from_b = Vec::new();
    for (task, b_fields) in [
        (first, vec![("deleted", Value::Bool(false))]),
        (second, vec![("title", text("renamed"))]),
    ] {
        from_a.push(write_on(&mut p, 0, 1_000, task, &[("deleted", Value::Bool(true))]).0);
        from_b.push(write_on(&mut p, 1, 2_000, task, &b_fields).0);
    }
    let (a, b): (Vec<&Vec<u8>>, Vec<&Vec<u8>>) = (from_a.iter().collect(), from_b.iter().collect());
    deliver(&p.eb, &mut p.dbb, &a);
    deliver(&p.ea, &mut p.dba, &b);

    let found = conflicts_of(&p.ea, &p.dba, false);
    let row = |task: EntityRef| {
        found
            .iter()
            .find(|c| c.entity == task && c.field == "deleted")
            .unwrap_or_else(|| panic!("{task}'s delete is reported: {found:?}"))
    };
    assert_eq!(row(first).losers.len(), 1, "the restore that lost");
    assert_eq!(row(first).edited_while_deleted[0].fields, ["deleted"]);
    assert!(row(second).losers.is_empty());
    assert_eq!(row(second).edited_while_deleted[0].fields, ["title"]);
    assert_eq!(found.len(), 2);
}

/// Three devices retitle one task concurrently and two of them also set its
/// priority: one row per field, the title's with both losers, oldest first.
#[test]
fn a_register_reports_every_loser_in_one_row() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let third_e = engine_seeded(
        ROOT,
        [3u8; 32],
        Arc::new(FakeClock(PLMutex::new(T0 + 3_000))),
    );
    let mut third_db = db_root(ROOT);
    trust(&third_e, &mut third_db, &p.ea);
    trust(&p.ea, &mut p.dba, &third_e);
    trust(&p.eb, &mut p.dbb, &third_e);
    deliver(
        &third_e,
        &mut third_db,
        &[&create_env_for(&p.dba, task.bytes())],
    );
    let priority = |n: i64| Value::Integer(n.into());
    let (_, a_v) = write_on(
        &mut p,
        0,
        1_000,
        task,
        &[("title", text("a")), ("priority", priority(1))],
    );
    let (from_b, b_v) = write_on(
        &mut p,
        1,
        2_000,
        task,
        &[("title", text("b")), ("priority", priority(2))],
    );
    let parents = heads_in(&third_db, task);
    let (from_c, _) = emit_patch(
        &third_e,
        &mut third_db,
        &inbox(),
        &versioned(task, &[("title", text("c"))], Some(&parents), false),
    );
    deliver(&p.ea, &mut p.dba, &[&from_b, &from_c]);

    let found = conflicts_of(&p.ea, &p.dba, true);
    let fields: Vec<&str> = found.iter().map(|c| c.field.as_str()).collect();
    assert_eq!(fields, ["priority", "title"], "{found:?}");
    let seqs = |c: &EntityConflict| c.losers.iter().map(|l| l.version.seq).collect::<Vec<_>>();
    assert_eq!(seqs(&found[0]), [a_v.seq]);
    assert_eq!(
        seqs(&found[1]),
        [a_v.seq, b_v.seq],
        "both losers, oldest first"
    );
}

/// Restore refuses an entity that is not deleted, and one it does not hold.
#[test]
fn restore_refuses_a_live_or_missing_entity() {
    let e = engine();
    let mut db = db();
    let task = new_task(&e, &mut db, "live");
    assert!(matches!(
        e.apply(&mut db, Command::Restore(task)),
        Err(EngineError::Invalid(_))
    ));
    let missing = EntityRef::new(EntityKind::Task, [0x77; 16]);
    assert!(matches!(
        e.apply(&mut db, Command::Restore(missing)),
        Err(EngineError::NotFound(_))
    ));
}

/// A deleted task restored through the command path is back, with the
/// fields it had.
#[test]
fn restore_brings_back_a_deleted_task() {
    let e = engine();
    let mut db = db();
    let task = new_task(&e, &mut db, "keep me");
    e.apply(&mut db, Command::DeleteTask(task)).unwrap();
    assert!(read_task_t(&e, &db, task).deleted);
    let res = e.apply(&mut db, Command::Restore(task)).unwrap();
    assert_eq!(res.entity, task);
    assert_eq!(
        res.state,
        Some(TaskState::Todo),
        "a task's result names its state"
    );
    let t = read_task_t(&e, &db, task);
    assert!(!t.deleted);
    assert_eq!(t.title, "keep me");
}

// ---- resolving ----

/// A resolve, here to the losing value, clears the field's set on the writer
/// and on every replica that applies it, in any order.
#[test]
fn resolve_conflict_clears_the_unresolved_set_on_every_replica() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let create = create_env_for(&p.dba, task.bytes());
    let (from_a, _) = write_on(&mut p, 0, 1_000, task, &[("title", text("from a"))]);
    let (from_b, _) = write_on(&mut p, 1, 2_000, task, &[("title", text("from b"))]);
    exchange(&mut p, task, &[&from_a], &[&from_b]);
    let found = conflicts_of(&p.ea, &p.dba, false);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].field.as_str(), found[0].class),
        ("title", ConflictClass::Tracked)
    );
    assert_eq!(task_at(&p.dba, task).title, "from b");

    set_clock(&p.ca, T0 + 3_000);
    let res =
        p.ea.apply(
            &mut p.dba,
            Command::ResolveConflict {
                entity: task,
                field: "title".into(),
                value: sunrise_cbor::CborValue(text("from a")),
            },
        )
        .unwrap();
    let resolve = env_bytes(&p.dba, &res.op_id);
    exchange(&mut p, task, &[&resolve], &[]);
    assert_eq!(task_at(&p.dbb, task).title, "from a");
    assert!(conflicts_of(&p.eb, &p.dbb, true).is_empty());

    for order in [[&resolve, &from_a, &from_b], [&from_b, &resolve, &from_a]] {
        let (receiver, mut db) = third(&p);
        deliver(&receiver, &mut db, &[&create]);
        deliver(&receiver, &mut db, &order);
        assert_eq!(task_at(&db, task).title, "from a");
        assert!(conflicts_of(&receiver, &db, true).is_empty());
        assert_eq!(register_state(&db, task), register_state(&p.dba, task));
        receiver.assert_rebuild_identity(&db);
    }
}

/// A resolve names a register and a value it can hold.
#[test]
fn resolve_conflict_refuses_a_field_that_is_not_a_register() {
    let e = engine();
    let mut db = db();
    let task = new_task(&e, &mut db, "t");
    for (field, value) in [
        ("contexts", Value::Array(Vec::new())),
        ("deferred_count", Value::Integer(3.into())),
        ("id", text("tsk_x")),
        ("title", Value::Integer(3.into())),
    ] {
        let res = e.apply(
            &mut db,
            Command::ResolveConflict {
                entity: task,
                field: field.into(),
                value: sunrise_cbor::CborValue(value),
            },
        );
        assert!(
            matches!(res, Err(EngineError::Invalid(_))),
            "{field}: {res:?}"
        );
    }
}

// ---- what is not a conflict ----

/// Two devices completing one task agree (D5).
#[test]
fn an_equal_concurrent_value_is_not_a_conflict() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (a, _) = write_on(&mut p, 0, 1_000, task, &[("state", state("done"))]);
    let (b, _) = write_on(&mut p, 1, 2_000, task, &[("state", state("done"))]);
    exchange(&mut p, task, &[&a], &[&b]);
    assert!(conflicts_of(&p.ea, &p.dba, true).is_empty());
    let kept: i64 = p
        .dba
        .conn()
        .query_row("SELECT COUNT(*) FROM register_losers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1, "the loser is kept, only not reported");
}

/// A full-state op that loses to a concurrent `Patch` is kept and never
/// reported (D4).
#[test]
fn a_legacy_loser_is_never_surfaced() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    set_clock(&p.ca, T0 + 1_000);
    let legacy =
        p.ea.apply(
            &mut p.dba,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("full state".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let legacy = env_bytes(&p.dba, &legacy.op_id);
    set_clock(&p.cb, T0 + 2_000);
    let (patch, _) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &versioned(task, &[("title", text("patched"))], None, false),
    );
    exchange(&mut p, task, &[&legacy], &[&patch]);

    assert_eq!(task_at(&p.dba, task).title, "patched");
    let legacy_losers: i64 = p
        .dba
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM register_losers WHERE field = 'title' AND legacy = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy_losers, 1);
    assert!(conflicts_of(&p.ea, &p.dba, true).is_empty());
}

/// A full-state op from a device that never saw a field raises the floor past
/// that field's only write, and the register goes, winner and all, on both
/// sides: a write below the floor is not a candidate (D4).
#[test]
fn the_floor_drops_a_register_its_op_did_not_write() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (a, _) = write_on(&mut p, 0, 1_000, task, &[("x_note", text("a"))]);
    set_clock(&p.cb, T0 + 2_000);
    let res =
        p.eb.apply(
            &mut p.dbb,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some("from b".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let legacy = env_bytes(&p.dbb, &res.op_id);
    exchange(&mut p, task, &[&a], &[&legacy]);
    let left: i64 = p
        .dba
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM merge_registers WHERE field = 'x_note'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(left, 0);
    assert!(!task_at(&p.dba, task).unknown.contains_key("x_note"));
}

/// Concurrent full-state edits fold as entity-level last-writer-wins did,
/// and keep no loser at all (D4).
#[test]
fn legacy_only_history_reports_no_conflicts() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let mut envs = Vec::new();
    for (who, at, title) in [(0, 1_000, "a"), (1, 2_000, "b")] {
        let (e, db, clock) = if who == 0 {
            (&p.ea, &mut p.dba, &p.ca)
        } else {
            (&p.eb, &mut p.dbb, &p.cb)
        };
        set_clock(clock, T0 + at);
        let res = e
            .apply(
                db,
                Command::UpdateTask {
                    id: task,
                    patch: TaskPatch {
                        title: Some(title.into()),
                        state: Some(if who == 0 {
                            TaskState::Done
                        } else {
                            TaskState::Todo
                        }),
                        ..Default::default()
                    },
                },
            )
            .unwrap();
        envs.push(env_bytes(db, &res.op_id));
    }
    exchange(&mut p, task, &[&envs[0]], &[&envs[1]]);
    let t = task_at(&p.dba, task);
    assert_eq!(
        (t.title.as_str(), t.state),
        ("b", TaskState::Todo),
        "the later op"
    );
    assert!(conflicts_of(&p.ea, &p.dba, true).is_empty());
    let kept: i64 = p
        .dba
        .conn()
        .query_row("SELECT COUNT(*) FROM register_losers", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 0);
}

/// A field this build does not know folds as quiet: decided by stamp, kept,
/// and reported only when asked for.
#[test]
fn an_unknown_field_folds_as_quiet() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (a, _) = write_on(&mut p, 0, 1_000, task, &[("x_note", text("a"))]);
    let (b, _) = write_on(&mut p, 1, 2_000, task, &[("x_note", text("b"))]);
    exchange(&mut p, task, &[&a], &[&b]);
    assert!(conflicts_of(&p.ea, &p.dba, false).is_empty());
    let all = conflicts_of(&p.ea, &p.dba, true);
    assert_eq!(all.len(), 1);
    assert_eq!(
        (all[0].field.as_str(), all[0].class),
        ("x_note", ConflictClass::Quiet)
    );
    assert_eq!(
        task_at(&p.dba, task)
            .unknown
            .get("x_note")
            .map(|v| v.get().clone()),
        Some(text("b"))
    );
}

/// A quiet field's losers are kept and reported only when asked for.
#[test]
fn quiet_conflicts_only_when_asked() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let (a, _) = write_on(
        &mut p,
        0,
        1_000,
        task,
        &[("priority", Value::Integer(1.into()))],
    );
    let (b, _) = write_on(
        &mut p,
        1,
        2_000,
        task,
        &[("priority", Value::Integer(3.into()))],
    );
    exchange(&mut p, task, &[&a], &[&b]);
    assert!(conflicts_of(&p.ea, &p.dba, false).is_empty());
    let all = conflicts_of(&p.ea, &p.dba, true);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].class, ConflictClass::Quiet);
    // One entity's conflicts, and another's none.
    let only = |entity| match p
        .ea
        .query(
            &p.dba,
            Query::Conflicts {
                entity: Some(entity),
                include_quiet: true,
            },
        )
        .unwrap()
    {
        QueryResult::Conflicts(c) => c,
        other => panic!("{other:?}"),
    };
    assert_eq!(only(task), all);
    assert!(only(EntityRef::new(EntityKind::Task, [0x55; 16])).is_empty());
}

/// A version that arrives after a child naming it orders writes the
/// registers had kept as concurrent, and the receiver drops them: its state
/// is the one in-order delivery gives.
#[test]
fn a_late_parent_retires_the_losers_it_orders() {
    let mut p = pair();
    let task = shared_task(&mut p, draft("t"));
    let create = create_env_for(&p.dba, task.bytes());
    let (parent, _) = write_on(&mut p, 0, 1_000, task, &[("title", text("a"))]);
    deliver(&p.eb, &mut p.dbb, &[&parent]);
    let (child, _) = write_on(&mut p, 1, 2_000, task, &[("title", text("b"))]);

    let (late, mut late_db) = third(&p);
    deliver(&late, &mut late_db, &[&create, &child]);
    let (in_order, mut in_order_db) = third(&p);
    deliver(&in_order, &mut in_order_db, &[&create, &parent, &child]);
    deliver(&late, &mut late_db, &[&parent]);

    assert_eq!(
        register_state(&late_db, task),
        register_state(&in_order_db, task)
    );
    assert!(conflicts_of(&late, &late_db, true).is_empty());
    late.assert_rebuild_identity(&late_db);
}

// ---- convergence (ADR-0065 §Consequences, properties 1 and 2) ----

/// One step of the property: a device writes one register, naming the heads
/// it has applied or none, or one device catches up on another's writes.
#[derive(Debug, Clone)]
enum Step {
    Write {
        who: usize,
        field: u8,
        value: u8,
        generated: bool,
        parents: bool,
    },
    Sync {
        from: usize,
        to: usize,
    },
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0usize..3, 0u8..4, 0u8..3, prop::bool::weighted(0.2), prop::bool::weighted(0.8))
            .prop_map(|(who, field, value, generated, parents)| Step::Write {
                who,
                field,
                value,
                generated,
                parents,
            }),
        2 => (0usize..3, 0usize..3).prop_map(|(from, to)| Step::Sync { from, to }),
    ]
}

/// The register a step writes, one per conflict class, and its value.
fn field_write(field: u8, value: u8) -> (&'static str, Value) {
    match field {
        0 => ("title", text(&format!("title {value}"))),
        1 => ("priority", Value::Integer((value + 1).into())),
        2 => (
            "state",
            text(["todo", "done", "in_progress"][usize::from(value)]),
        ),
        _ => ("deleted", Value::Bool(value == 1)),
    }
}

/// What a replica holds of the task: the row, the register state, and the
/// conflicts, quiet ones included.
type Held = (Task, Vec<String>, Vec<EntityConflict>);

fn held(e: &Engine, db: &Db, task: EntityRef) -> Held {
    (
        task_at(db, task),
        register_state(db, task),
        conflicts_of(e, db, true),
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/conflicts.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Three devices write registers of every class to one task, naming the
    /// heads each has applied, and catch up on each other at random. Every
    /// receiver of the same versions, in any order and with any repeats,
    /// holds byte for byte the same row, register winners and losers, and
    /// conflicts (property 1), and rebuilds to exactly that. And no register
    /// keeps a generated winner over a concurrent user write (property 2).
    #[test]
    fn conflict_sets_are_equal_across_replicas(
        steps in proptest::collection::vec(arb_step(), 1..16),
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
        let task = devices[0]
            .apply(&mut dbs[0], Command::CreateTask(draft("t")))
            .unwrap()
            .entity;
        let create = create_env_for(&dbs[0], task.bytes());
        for i in 1..3 {
            devices[i].apply_remote(&mut dbs[i], &create).unwrap();
        }

        let mut written: Vec<Vec<Vec<u8>>> = vec![Vec::new(); 3];
        let mut envelopes = vec![create];
        for (n, step) in steps.iter().enumerate() {
            match *step {
                Step::Write { who, field, value, generated, parents } => {
                    set_clock(&clocks[who], T0 + 1_000 + u64::try_from(n).unwrap() * 7);
                    let named = heads_in(&dbs[who], task);
                    let (name, v) = field_write(field, value);
                    let op = versioned(
                        task,
                        &[(name, v)],
                        parents.then_some(named.as_slice()),
                        generated,
                    );
                    let (env, _) = emit_patch(&devices[who], &mut dbs[who], &inbox(), &op);
                    written[who].push(env.clone());
                    envelopes.push(env);
                }
                Step::Sync { from, to } if from != to => {
                    for env in &written[from] {
                        devices[to].apply_remote(&mut dbs[to], env).unwrap();
                    }
                }
                Step::Sync { .. } => {}
            }
        }

        let receive = |deliveries: &[usize]| -> Held {
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
            e.assert_rebuild_identity(&db);
            held(&e, &db, task)
        };
        for (writer, db) in devices.iter().zip(&dbs) {
            writer.assert_rebuild_identity(db);
        }
        let forward: Vec<usize> = (0..envelopes.len()).collect();
        let shuffled: Vec<usize> = order.iter().map(|ix| ix.index(envelopes.len())).collect();
        let reversed: Vec<usize> = (0..envelopes.len()).rev().collect();
        let a = receive(&forward);
        let b = receive(&shuffled);
        let c = receive(&reversed);
        prop_assert_eq!(&a, &b);
        prop_assert_eq!(&a, &c);

        // Property 2, on the registers the receiver holds: a generated
        // winner has no user write beside it in its antichain.
        let e = engine_seeded(ROOT, [9u8; 32], Arc::new(FakeClock(PLMutex::new(T0 + 100_000))));
        let mut db = db_root(ROOT);
        for d in &devices {
            trust(&e, &mut db, d);
        }
        for env in &envelopes {
            e.apply_remote(&mut db, env).unwrap();
        }
        let beaten: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM merge_registers w JOIN register_losers l
                   ON l.entity_id = w.entity_id AND l.field = w.field
                 WHERE w.origin = 'generated' AND l.origin = 'user'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        prop_assert_eq!(beaten, 0);
    }
}
