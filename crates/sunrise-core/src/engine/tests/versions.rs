//! The version DAG (ADR-0065 §1–§2, PR 4): every applied entity op is a
//! version, ordered by the parents it names and by its own device's chain,
//! whatever order the versions arrive in.

use super::field_merge::{emit_patch, inbox, pair, set, shared_task, text};
use super::testutil::*;
use super::*;
use crate::engine::merge::{heads, precedes, OpRef};
use crate::inner_op::{OpRefWire, PatchPayload};
use ciborium::value::Value;
use proptest::prelude::*;
use rusqlite::OptionalExtension;
use std::collections::{BTreeMap, BTreeSet};

// ---- helpers ----

/// A version row as a test reads it back: `(kind, origin, is_create, parents,
/// fields)`.
type Row = (String, String, i64, Option<Vec<u8>>, Vec<u8>);

/// A version's op-ref, `(hlc_ms, hlc_logical)` and stored `parents`.
type StoredVersion = (OpRef, (i64, i64), Option<Vec<u8>>);

/// A title `Patch` of `task` naming `parents`.
fn titled(task: EntityRef, title: &str, parents: Option<&[OpRef]>) -> InnerOp {
    InnerOp::Patch(Box::new(PatchPayload {
        target: task,
        create: false,
        origin: None,
        parents: parents.map(|ps| {
            ps.iter()
                .map(|p| OpRefWire(p.stream, p.device, p.seq))
                .collect()
        }),
        group: None,
        fields: [(
            "title".to_owned(),
            sunrise_domain::CborValue(set(text(title))),
        )]
        .into_iter()
        .collect(),
        unknown: Unknowns::new(),
    }))
}

/// Run `f` in a transaction on `db` that is rolled back after.
fn read<T>(db: &Db, f: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>) -> T {
    let tx = db.conn().unchecked_transaction().unwrap();
    let out = f(&tx).unwrap();
    tx.rollback().unwrap();
    out
}

fn heads_of(db: &Db, task: EntityRef) -> Vec<OpRef> {
    read(db, |tx| heads(tx, task.bytes()))
}

fn precedes_in(db: &Db, task: EntityRef, a: OpRef, b: OpRef) -> bool {
    read(db, |tx| precedes(tx, task.bytes(), a, b))
}

/// Every version of `task` in `db`, oldest first.
fn versions_of(db: &Db, task: EntityRef) -> Vec<OpRef> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT stream, device, seq FROM entity_versions WHERE entity_id = ?
             ORDER BY hlc_ms, hlc_logical, device, seq, stream",
        )
        .unwrap();
    stmt.query_map([&task.bytes()[..]], |r| {
        Ok(OpRef {
            stream: blob16(&r.get::<_, Vec<u8>>(0)?),
            device: blob16(&r.get::<_, Vec<u8>>(1)?),
            seq: u64::try_from(r.get::<_, i64>(2)?).unwrap(),
        })
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// The version DAG's two tables for `task`, row by row, sorted.
fn dag(db: &Db, task: EntityRef) -> Vec<String> {
    let mut out = Vec::new();
    for table in ["entity_versions", "entity_frontier"] {
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

/// A replica that trusts every engine in `peers` and has applied nothing.
fn receiver(peers: &[&Engine]) -> (Engine, Db) {
    let e = engine_seeded(
        ROOT,
        [9u8; 32],
        Arc::new(FakeClock(PLMutex::new(T0 + 1_000_000))),
    );
    let mut db = db_root(ROOT);
    for p in peers {
        trust(&e, &mut db, p);
    }
    (e, db)
}

fn rename(e: &Engine, db: &mut Db, task: EntityRef, title: &str) {
    e.apply(
        db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some(title.into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
}

// ---- the order ----

/// D2: a device's own full-state ops name no parents, and are still a chain.
#[test]
fn a_devices_own_versions_are_ordered() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "one".into(),
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 1_000);
    rename(&p.ea, &mut p.dba, task, "two");
    set_clock(&p.ca, T0 + 2_000);
    rename(&p.ea, &mut p.dba, task, "three");

    let [v1, v2, v3] = versions_of(&p.dba, task)[..] else {
        panic!("three versions");
    };
    for (a, b) in [(v1, v2), (v2, v3), (v1, v3)] {
        assert!(precedes_in(&p.dba, task, a, b), "{a:?} precedes {b:?}");
        assert!(
            !precedes_in(&p.dba, task, b, a),
            "{b:?} does not precede {a:?}"
        );
    }
    assert!(
        !precedes_in(&p.dba, task, v2, v2),
        "a version is not its own ancestor"
    );
    assert_eq!(heads_of(&p.dba, task), vec![v3]);
}

/// Two devices' full-state ops name no parents, so they are concurrent, and
/// each is also concurrent with the other device's create: a legacy version
/// is ordered only after its own device's earlier versions (ADR-0065 §2).
#[test]
fn versions_without_parents_from_two_devices_are_concurrent() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 1_000);
    rename(&p.ea, &mut p.dba, task, "from a");
    set_clock(&p.cb, T0 + 2_000);
    rename(&p.eb, &mut p.dbb, task, "from b");
    let a = update_envs_for(&p.dba, task.bytes());
    let b = update_envs_for(&p.dbb, task.bytes());
    p.eb.apply_remote(&mut p.dbb, &a[0]).unwrap();
    p.ea.apply_remote(&mut p.dba, &b[0]).unwrap();

    let [create, from_a, from_b] = versions_of(&p.dba, task)[..] else {
        panic!("three versions");
    };
    for db in [&p.dba, &p.dbb] {
        assert!(!precedes_in(db, task, from_a, from_b));
        assert!(!precedes_in(db, task, from_b, from_a));
        assert!(precedes_in(db, task, create, from_a), "the same device's");
        assert!(!precedes_in(db, task, create, from_b), "another device's");
        let mut both = vec![from_a, from_b];
        both.sort();
        assert_eq!(heads_of(db, task), both);
    }
    assert_eq!(dag(&p.dba, task), dag(&p.dbb, task));
}

/// A version that arrives after one naming it as a parent refines that
/// version's frontier, and the result is the one in-order delivery gives.
#[test]
fn a_late_parent_refines_the_frontier() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    let create = versions_of(&p.dba, task)[0];
    set_clock(&p.ca, T0 + 1_000);
    let (parent_env, parent) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &titled(task, "a", Some(&[create])),
    );
    p.eb.apply_remote(&mut p.dbb, &parent_env).unwrap();
    set_clock(&p.cb, T0 + 2_000);
    let (child_env, child) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &titled(task, "b", Some(&[parent])),
    );

    let create_env = create_env_for(&p.dba, task.bytes());
    let (late, mut late_db) = receiver(&[&p.ea, &p.eb]);
    late.apply_remote(&mut late_db, &create_env).unwrap();
    late.apply_remote(&mut late_db, &child_env).unwrap();
    assert!(
        !precedes_in(&late_db, task, parent, child),
        "the parent is not applied yet"
    );
    assert!(
        !precedes_in(&late_db, task, create, child),
        "nor is anything the child reaches only through it"
    );
    late.apply_remote(&mut late_db, &parent_env).unwrap();
    assert!(precedes_in(&late_db, task, parent, child));
    assert!(
        precedes_in(&late_db, task, create, child),
        "through the parent"
    );
    assert_eq!(heads_of(&late_db, task), vec![child]);

    let (in_order, mut in_order_db) = receiver(&[&p.ea, &p.eb]);
    for env in [&create_env, &parent_env, &child_env] {
        in_order.apply_remote(&mut in_order_db, env).unwrap();
    }
    assert_eq!(dag(&late_db, task), dag(&in_order_db, task));
    assert_eq!(dag(&late_db, task), dag(&p.dbb, task));
    late.assert_rebuild_identity(&late_db);
}

/// D3: an edge to a version stamped after its child is ignored, whatever
/// order the two arrive in.
#[test]
fn a_parent_stamped_after_its_child_is_ignored() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    set_clock(&p.ca, T0 + 9_000);
    let (later_env, later) = emit_patch(&p.ea, &mut p.dba, &inbox(), &titled(task, "a", None));
    // B never saw it, so its clock is behind, and it names it anyway.
    set_clock(&p.cb, T0 + 1_000);
    let (child_env, child) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &titled(task, "b", Some(&[later])),
    );
    let create_env = create_env_for(&p.dba, task.bytes());

    let mut dags = Vec::new();
    for order in [[&later_env, &child_env], [&child_env, &later_env]] {
        let (e, mut db) = receiver(&[&p.ea, &p.eb]);
        e.apply_remote(&mut db, &create_env).unwrap();
        for env in order {
            e.apply_remote(&mut db, env).unwrap();
        }
        assert!(!precedes_in(&db, task, later, child));
        assert!(!precedes_in(&db, task, child, later));
        let mut both = vec![later, child];
        both.sort();
        assert_eq!(heads_of(&db, task), both);
        e.assert_rebuild_identity(&db);
        dags.push(dag(&db, task));
    }
    assert_eq!(dags[0], dags[1]);
}

/// A version that names itself as a parent is not its own ancestor, also
/// when it arrives late and the entity's frontiers are recomputed: D3 drops
/// an edge whose stamp is not below the child's, and its own is equal.
#[test]
fn a_version_naming_itself_is_not_its_own_ancestor() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    let me = p.eb.keychain.device_id();
    let next_seq: i64 = p
        .dbb
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM ops WHERE stream_id = ? AND device_id = ?",
            rusqlite::params![&inbox()[..], &me[..]],
            |r| r.get(0),
        )
        .unwrap();
    let itself = OpRef {
        stream: inbox(),
        device: me,
        seq: u64::try_from(next_seq).unwrap(),
    };
    set_clock(&p.cb, T0 + 1_000);
    let (first_env, first) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &titled(task, "b", Some(&[itself])),
    );
    assert_eq!(first, itself, "the op-ref was predicted");
    set_clock(&p.cb, T0 + 2_000);
    let (second_env, second) = emit_patch(&p.eb, &mut p.dbb, &inbox(), &titled(task, "c", None));

    let (e, mut db) = receiver(&[&p.ea, &p.eb]);
    e.apply_remote(&mut db, &create_env_for(&p.dba, task.bytes()))
        .unwrap();
    e.apply_remote(&mut db, &second_env).unwrap();
    e.apply_remote(&mut db, &first_env).unwrap();
    assert!(!precedes_in(&db, task, first, first));
    assert!(precedes_in(&db, task, first, second));
    // A's full-state create names nothing, so it stays a head beside B's.
    let create = versions_of(&db, task)[0];
    let mut expected = vec![create, second];
    expected.sort();
    assert_eq!(heads_of(&db, task), expected);
    assert_eq!(dag(&db, task), dag(&p.dbb, task));
    e.assert_rebuild_identity(&db);
}

/// The heads are the versions nothing else descends from: two concurrent
/// branches are both heads until a version names them both.
#[test]
fn heads_are_the_maximal_versions() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    let create = versions_of(&p.dba, task)[0];
    set_clock(&p.ca, T0 + 1_000);
    let (a_env, a) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &titled(task, "a", Some(&[create])),
    );
    set_clock(&p.cb, T0 + 2_000);
    let (b_env, b) = emit_patch(
        &p.eb,
        &mut p.dbb,
        &inbox(),
        &titled(task, "b", Some(&[create])),
    );
    p.ea.apply_remote(&mut p.dba, &b_env).unwrap();
    let mut branches = vec![a, b];
    branches.sort();
    assert_eq!(heads_of(&p.dba, task), branches, "two concurrent branches");
    assert_eq!(
        heads_of(&p.dba, task),
        read(&p.dba, |tx| heads(tx, task.bytes()))
    );

    set_clock(&p.ca, T0 + 3_000);
    let (merge_env, merge) = emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &titled(task, "merged", Some(&branches)),
    );
    assert_eq!(heads_of(&p.dba, task), vec![merge]);
    for v in [create, a, b] {
        assert!(precedes_in(&p.dba, task, v, merge));
    }

    p.eb.apply_remote(&mut p.dbb, &merge_env).unwrap();
    p.eb.apply_remote(&mut p.dbb, &a_env).unwrap();
    assert_eq!(heads_of(&p.dbb, task), vec![merge]);
    assert_eq!(dag(&p.dba, task), dag(&p.dbb, task));
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// The version row records what the op said about itself.
#[test]
fn a_version_records_its_kind_origin_create_parents_and_fields() {
    let mut p = pair();
    let task = shared_task(
        &mut p,
        TaskDraft {
            title: "draft".into(),
            ..Default::default()
        },
    );
    let create = versions_of(&p.dba, task)[0];
    set_clock(&p.ca, T0 + 1_000);
    emit_patch(
        &p.ea,
        &mut p.dba,
        &inbox(),
        &titled(task, "a", Some(&[create])),
    );
    set_clock(&p.ca, T0 + 2_000);
    emit_patch(&p.ea, &mut p.dba, &inbox(), &titled(task, "b", Some(&[])));
    set_clock(&p.ca, T0 + 3_000);
    emit_patch(&p.ea, &mut p.dba, &inbox(), &titled(task, "c", None));

    let rows: Vec<Row> = {
        let mut stmt = p
            .dba
            .conn()
            .prepare(
                "SELECT kind, origin, is_create, parents, fields FROM entity_versions
                 WHERE entity_id = ? ORDER BY hlc_ms, hlc_logical",
            )
            .unwrap();
        stmt.query_map([&task.bytes()[..]], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    let cbor = |v: Value| sunrise_cbor::encode_canonical(&v).unwrap();
    let [legacy, named, empty, absent] = &rows[..] else {
        panic!("four versions");
    };
    assert_eq!(
        (legacy.0.as_str(), legacy.1.as_str(), legacy.2),
        ("legacy", "user", 1)
    );
    assert_eq!(legacy.3, None, "a full-state op carries no parents");
    let fields: Vec<String> = sunrise_cbor::decode_canonical::<Vec<String>>(&legacy.4).unwrap();
    assert!(fields.contains(&"title".to_owned()) && fields.contains(&"deleted".to_owned()));
    assert!(fields.windows(2).all(|w| w[0] < w[1]), "sorted");

    assert_eq!((named.0.as_str(), named.2), ("patch", 0));
    assert_eq!(named.3, Some(cbor(Value::Array(vec![create.to_value()]))));
    assert_eq!(named.4, cbor(Value::Array(vec![text("title")])));
    assert_eq!(
        empty.3,
        Some(cbor(Value::Array(Vec::new()))),
        "an empty list is not an absent key"
    );
    assert_eq!(absent.3, None);
}

// ---- rebuild at open ----

/// `rebuild_if_stale` rebuilds a vault with no record of a rebuild, records
/// [`FOLD_V`], and then leaves it alone until the record names another one.
#[test]
fn open_rebuilds_once_per_fold_version() {
    let e = engine();
    let mut db = db();
    new_task(&e, &mut db, "kept");
    let fold_v = |db: &Db| -> Option<i64> {
        db.conn()
            .query_row("SELECT fold_v FROM projection_rebuild", [], |r| r.get(0))
            .optional()
            .unwrap()
    };
    assert_eq!(fold_v(&db), None, "a vault no rebuild has run on");

    let report = e.rebuild_if_stale(&mut db).unwrap().expect("it rebuilds");
    assert!(report.ops_replayed >= 1);
    assert_eq!(fold_v(&db), Some(i64::from(crate::engine::merge::FOLD_V)));
    assert_eq!(e.rebuild_if_stale(&mut db).unwrap(), None, "and only once");

    db.conn()
        .execute("UPDATE projection_rebuild SET fold_v = fold_v + 1", [])
        .unwrap();
    assert!(
        e.rebuild_if_stale(&mut db).unwrap().is_some(),
        "a vault another fold rebuilt is rebuilt again"
    );
    assert_eq!(fold_v(&db), Some(i64::from(crate::engine::merge::FOLD_V)));
    assert_eq!(e.rebuild_if_stale(&mut db).unwrap(), None);
}

/// A rebuild that fails leaves the record unwritten, so the next open tries
/// again.
#[test]
fn a_failed_rebuild_at_open_is_tried_again() {
    let e = engine();
    let mut db = db();
    let task = new_task(&e, &mut db, "kept");
    let env = create_env_for(&db, task.bytes());
    db.conn()
        .execute(
            "UPDATE ops SET envelope = ?1 WHERE target_id = ?2",
            rusqlite::params![&env[..env.len() - 1], &task.bytes()[..]],
        )
        .unwrap();
    assert!(e.rebuild_if_stale(&mut db).is_err());
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM projection_rebuild", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        read_task_t(&e, &db, task).title,
        "kept",
        "the projection it had"
    );
}

// ---- the property ----

/// One step of the DAG property: a write by a device, as a `Patch` naming
/// its heads or as a full-state rename naming none, or a device catching up
/// on everything another one holds.
#[derive(Debug, Clone)]
enum Step {
    Patch(usize),
    FullState(usize),
    Sync { from: usize, to: usize },
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0usize..3).prop_map(Step::Patch),
        1 => (0usize..3).prop_map(Step::FullState),
        2 => (0usize..3, 0usize..3)
            .prop_filter("another device", |(a, b)| a != b)
            .prop_map(|(from, to)| Step::Sync { from, to }),
    ]
}

/// Deliver every op `from_db` holds that `to` did not write.
fn sync_all(from_db: &Db, to: &Engine, to_db: &mut Db) {
    let me = to.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            .prepare("SELECT envelope FROM ops ORDER BY rowid")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        if sunrise_crypto::decode_envelope(&env).unwrap().device_id != me {
            to.apply_remote(to_db, &env).unwrap();
        }
    }
}

/// The order an engine-free model derives from the versions' own parents:
/// `a` precedes `b` when `b` reaches `a` through a parent stamped below its
/// child, or through its own device's previous version.
fn model_precedes(db: &Db, task: EntityRef) -> BTreeSet<(OpRef, OpRef)> {
    let rows: Vec<StoredVersion> = {
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT stream, device, seq, hlc_ms, hlc_logical, parents FROM entity_versions
                 WHERE entity_id = ?",
            )
            .unwrap();
        stmt.query_map([&task.bytes()[..]], |r| {
            Ok((
                OpRef {
                    stream: blob16(&r.get::<_, Vec<u8>>(0)?),
                    device: blob16(&r.get::<_, Vec<u8>>(1)?),
                    seq: u64::try_from(r.get::<_, i64>(2)?).unwrap(),
                },
                (r.get(3)?, r.get(4)?),
                r.get(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    let stamp = |op: &OpRef, hlc: (i64, i64)| (hlc, op.device, op.seq, op.stream);
    let stamps: BTreeMap<OpRef, _> = rows.iter().map(|(op, h, _)| (*op, stamp(op, *h))).collect();
    let mut edges: BTreeMap<OpRef, Vec<OpRef>> = BTreeMap::new();
    for (op, _, parents) in &rows {
        let mut out = Vec::new();
        for p in parents
            .as_deref()
            .map(|b| sunrise_cbor::decode_canonical::<Vec<OpRefWire>>(b).unwrap())
            .unwrap_or_default()
        {
            let p = OpRef {
                stream: p.0,
                device: p.1,
                seq: p.2,
            };
            if stamps.get(&p).is_some_and(|s| *s < stamps[op]) {
                out.push(p);
            }
        }
        if let Some(previous) = rows
            .iter()
            .filter(|(o, _, _)| o.device == op.device && stamps[o] < stamps[op])
            .max_by_key(|(o, _, _)| stamps[o])
        {
            out.push(previous.0);
        }
        edges.insert(*op, out);
    }
    let mut pairs = BTreeSet::new();
    for b in stamps.keys() {
        let mut stack = edges[b].clone();
        let mut seen = BTreeSet::new();
        while let Some(a) = stack.pop() {
            if seen.insert(a) {
                pairs.insert((a, *b));
                stack.extend(edges[&a].iter().copied());
            }
        }
    }
    pairs
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        // docs/10-cross-cutting/testing.md §2: a shrunken counterexample is
        // committed, at a path named here.
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/versions.txt",
            ),
        )),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        ..ProptestConfig::default()
    })]

    /// Three devices write one task, each `Patch` naming the heads its
    /// writer had applied, mixed with full-state renames that name none, and
    /// catch up on each other at random. Two receivers that take every op in
    /// different orders hold the same version rows and frontiers as each
    /// writer that has caught up, a rebuild of each lands on them, the order
    /// they store is the one the parents and device chains define, and the
    /// heads are exactly the versions nothing else descends from.
    #[test]
    fn the_version_dag_does_not_depend_on_delivery_order(
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
        let task = new_task(&devices[0], &mut dbs[0], "t");
        let create = create_env_for(&dbs[0], task.bytes());
        for i in 1..3 {
            devices[i].apply_remote(&mut dbs[i], &create).unwrap();
        }

        for (n, step) in steps.iter().enumerate() {
            let at = T0 + 1_000 + u64::try_from(n).unwrap() * 7;
            match step {
                Step::Patch(who) => {
                    set_clock(&clocks[*who], at);
                    let parents = heads_of(&dbs[*who], task);
                    emit_patch(
                        &devices[*who],
                        &mut dbs[*who],
                        &inbox(),
                        &titled(task, &format!("p{n}"), Some(&parents)),
                    );
                }
                Step::FullState(who) => {
                    set_clock(&clocks[*who], at);
                    rename(&devices[*who], &mut dbs[*who], task, &format!("f{n}"));
                }
                Step::Sync { from, to } => {
                    let (a, b) = if from < to {
                        let (l, r) = dbs.split_at_mut(*to);
                        (&l[*from], &mut r[0])
                    } else {
                        let (l, r) = dbs.split_at_mut(*from);
                        (&r[0], &mut l[*to])
                    };
                    sync_all(a, &devices[*to], b);
                }
            }
        }

        let mut envelopes: Vec<Vec<u8>> = Vec::new();
        for db in &dbs {
            let mut stmt = db.conn().prepare("SELECT envelope FROM ops ORDER BY rowid").unwrap();
            let envs: Vec<Vec<u8>> = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            envelopes.extend(envs);
        }
        let peers: Vec<&Engine> = devices.iter().collect();
        let deliver = |sequence: &[usize]| -> (Engine, Db) {
            let (e, mut db) = receiver(&peers);
            for i in sequence {
                e.apply_remote(&mut db, &envelopes[*i]).unwrap();
            }
            for env in &envelopes {
                e.apply_remote(&mut db, env).unwrap();
            }
            e.assert_rebuild_identity(&db);
            (e, db)
        };
        let shuffled: Vec<usize> = order.iter().map(|ix| ix.index(envelopes.len())).collect();
        let reversed: Vec<usize> = (0..envelopes.len()).rev().collect();
        let (_, a) = deliver(&shuffled);
        let (_, b) = deliver(&reversed);
        prop_assert_eq!(dag(&a, task), dag(&b, task));

        // Each writer, once it has caught up, holds the same DAG.
        for to in 0..3 {
            for from in 0..3 {
                if from != to {
                    let (l, r) = if from < to {
                        let (l, r) = dbs.split_at_mut(to);
                        (&l[from], &mut r[0])
                    } else {
                        let (l, r) = dbs.split_at_mut(from);
                        (&r[0], &mut l[to])
                    };
                    sync_all(l, &devices[to], r);
                }
            }
        }
        for (e, db) in devices.iter().zip(&dbs) {
            prop_assert_eq!(dag(db, task), dag(&a, task));
            e.assert_rebuild_identity(db);
        }

        // The stored order is the model's, and the heads its maximal elements.
        let model = model_precedes(&a, task);
        let all = versions_of(&a, task);
        for x in &all {
            for y in &all {
                prop_assert_eq!(
                    precedes_in(&a, task, *x, *y),
                    model.contains(&(*x, *y)),
                    "{:?} precedes {:?}", x, y
                );
            }
        }
        let maximal: Vec<OpRef> = all
            .iter()
            .copied()
            .filter(|x| !all.iter().any(|y| model.contains(&(*x, *y))))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        prop_assert_eq!(heads_of(&a, task), maximal);
    }
}
