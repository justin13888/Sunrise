//! Manual task order: `Command::MoveTasks` and the listings that honour
//! `Task.sort_order` (issue #340, `docs/02-domain/tasks.md`).

use super::field_merge::{pair, task_at, Pair};
use super::testutil::*;
use super::*;
use proptest::prelude::*;

// ---- helpers ----

fn inbox_order(e: &Engine, db: &Db) -> Vec<EntityRef> {
    match e.query(db, Query::Inbox).unwrap() {
        QueryResult::StreamTasks(t) => t.into_iter().map(|t| t.id).collect(),
        other => panic!("{other:?}"),
    }
}

fn key(db: &Db, id: EntityRef) -> Option<String> {
    task_at(db, id).sort_order
}

fn mv(
    e: &Engine,
    db: &mut Db,
    ids: &[EntityRef],
    after: Option<EntityRef>,
    before: Option<EntityRef>,
) -> Result<CommandResult, EngineError> {
    e.apply(
        db,
        Command::MoveTasks {
            ids: ids.to_vec(),
            after,
            before,
        },
    )
}

/// `n` inbox tasks on one replica, created in order, none placed.
fn tasks(e: &Engine, db: &mut Db, n: usize) -> Vec<EntityRef> {
    (0..n).map(|i| new_task(e, db, &format!("t{i}"))).collect()
}

/// Every op `from` logged under its own device, delivered to `to` in seq
/// order. Re-delivering one `to` already holds is a no-op.
fn deliver(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
    let device = from.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY stream_id, seq")
            .unwrap();
        stmt.query_map(rusqlite::params![&device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        to.apply_remote_all(to_db, &env).unwrap();
    }
}

fn sync(p: &mut Pair) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba);
}

/// Ops in `db`'s log, all of them.
fn op_count(db: &Db) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))
        .unwrap()
}

// ---- the command ----

#[test]
fn a_single_move_writes_one_key_and_touches_no_sibling() {
    let (e, mut db) = (engine(), db());
    let t = tasks(&e, &mut db, 3);
    mv(&e, &mut db, &t, None, None).unwrap();
    let before: Vec<Option<String>> = t.iter().map(|id| key(&db, *id)).collect();
    assert!(before.windows(2).all(|w| w[0] < w[1]), "{before:?}");
    assert_eq!(inbox_order(&e, &db), t);

    let ops = op_count(&db);
    mv(&e, &mut db, &[t[2]], Some(t[0]), Some(t[1])).unwrap();
    assert_eq!(op_count(&db), ops + 1, "one task, one op");
    assert_eq!(inbox_order(&e, &db), vec![t[0], t[2], t[1]]);
    assert_eq!(key(&db, t[0]), before[0], "the neighbours keep their keys");
    assert_eq!(key(&db, t[1]), before[1]);
    e.assert_rebuild_identity(&db);
}

#[test]
fn a_multi_row_move_keeps_its_relative_order() {
    let (e, mut db) = (engine(), db());
    let t = tasks(&e, &mut db, 5);
    mv(&e, &mut db, &t, None, None).unwrap();
    // The last two, in the order given, between the first and the second.
    mv(&e, &mut db, &[t[4], t[3]], Some(t[0]), Some(t[1])).unwrap();
    assert_eq!(inbox_order(&e, &db), vec![t[0], t[4], t[3], t[1], t[2]]);
    e.assert_rebuild_identity(&db);
}

#[test]
fn moves_to_the_top_and_the_bottom() {
    let (e, mut db) = (engine(), db());
    let t = tasks(&e, &mut db, 3);
    mv(&e, &mut db, &t, None, None).unwrap();
    mv(&e, &mut db, &[t[2]], None, Some(t[0])).unwrap();
    assert_eq!(inbox_order(&e, &db), vec![t[2], t[0], t[1]]);
    mv(&e, &mut db, &[t[2]], Some(t[1]), None).unwrap();
    assert_eq!(inbox_order(&e, &db), vec![t[0], t[1], t[2]]);
}

/// A move into a list that holds no placed task spans the whole key space,
/// and the placed tasks then lead the unplaced ones.
#[test]
fn a_move_into_an_empty_list() {
    let (e, mut db) = (engine(), db());
    let t = tasks(&e, &mut db, 3);
    let unplaced: Vec<EntityRef> = inbox_order(&e, &db)
        .into_iter()
        .filter(|id| *id != t[2])
        .collect();
    mv(&e, &mut db, &[t[2]], None, None).unwrap();
    assert_eq!(key(&db, t[2]).as_deref(), Some("N"));
    assert_eq!(key(&db, t[0]), None, "nothing else is placed");
    let order = inbox_order(&e, &db);
    assert_eq!(order[0], t[2], "the placed task leads");
    assert_eq!(order[1..], unplaced[..], "the rest keep the order they had");
}

#[test]
fn a_move_refuses_what_has_no_place() {
    let (e, mut db) = (engine(), db());
    let t = tasks(&e, &mut db, 3);
    let invalid = |r: Result<CommandResult, EngineError>| {
        assert!(matches!(r, Err(EngineError::Invalid(_))), "{r:?}");
    };
    invalid(mv(&e, &mut db, &[], None, None));
    invalid(mv(&e, &mut db, &[t[0], t[0]], None, None));
    invalid(mv(&e, &mut db, &[t[0]], Some(t[0]), None));
    // An anchor nobody placed.
    invalid(mv(&e, &mut db, &[t[0]], Some(t[1]), None));
    // Anchors out of order.
    mv(&e, &mut db, &[t[1], t[2]], None, None).unwrap();
    invalid(mv(&e, &mut db, &[t[0]], Some(t[2]), Some(t[1])));
    // A stream is not a task.
    assert!(mv(
        &e,
        &mut db,
        &[sunrise_domain::inbox_stream_ref()],
        None,
        None
    )
    .is_err());
    let gone = EntityRef::new(EntityKind::Task, [0x77; 16]);
    assert!(matches!(
        mv(&e, &mut db, &[gone], None, None),
        Err(EngineError::NotFound(_))
    ));
    e.apply(&mut db, Command::DeleteTask(t[0])).unwrap();
    assert!(matches!(
        mv(&e, &mut db, &[t[0]], None, None),
        Err(EngineError::NotFound(_))
    ));
}

/// Two equal keys (two devices that dropped into one gap) tie-break by id,
/// and a move that names the tie's later task as an anchor is refused rather
/// than given a key that cannot exist.
#[test]
fn equal_keys_tie_break_by_id_on_both_replicas() {
    let mut p = pair();
    let t = tasks(&p.ea, &mut p.dba, 3);
    mv(&p.ea, &mut p.dba, &[t[0]], None, None).unwrap();
    sync(&mut p);
    // Each drops a different task just after t0, while apart: one key.
    mv(&p.ea, &mut p.dba, &[t[1]], Some(t[0]), None).unwrap();
    mv(&p.eb, &mut p.dbb, &[t[2]], Some(t[0]), None).unwrap();
    sync(&mut p);
    assert_eq!(key(&p.dba, t[1]), key(&p.dba, t[2]), "one gap, one key");
    let (lo, hi) = if t[1] < t[2] {
        (t[1], t[2])
    } else {
        (t[2], t[1])
    };
    assert_eq!(inbox_order(&p.ea, &p.dba), vec![t[0], lo, hi]);
    assert_eq!(inbox_order(&p.eb, &p.dbb), vec![t[0], lo, hi]);
    assert!(matches!(
        mv(&p.ea, &mut p.dba, &[t[0]], Some(lo), Some(hi)),
        Err(EngineError::Invalid(_))
    ));
    // Moving the later one too settles it.
    mv(&p.ea, &mut p.dba, &[t[0], hi], Some(lo), None).unwrap();
    assert_eq!(inbox_order(&p.ea, &p.dba), vec![lo, t[0], hi]);
}

/// A move of several tasks is one group: the peer shows all of it or none.
#[test]
fn a_multi_row_move_reaches_a_peer_whole() {
    let mut p = pair();
    let t = tasks(&p.ea, &mut p.dba, 3);
    sync(&mut p);
    mv(&p.ea, &mut p.dba, &t, None, None).unwrap();
    let pending: i64 = p
        .dba
        .conn()
        .query_row("SELECT COUNT(*) FROM pending_groups", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pending, 3, "one group of three members");
    sync(&mut p);
    for id in &t {
        assert_eq!(task_at(&p.dbb, *id), task_at(&p.dba, *id));
    }
    assert_eq!(inbox_order(&p.eb, &p.dbb), t);
}

/// A move changes nothing but the key and the update time.
#[test]
fn a_move_changes_nothing_but_the_key() {
    let (e, mut db) = (engine(), db());
    let t = tasks(&e, &mut db, 2);
    let before = task_at(&db, t[0]);
    mv(&e, &mut db, &[t[0]], None, None).unwrap();
    let after = task_at(&db, t[0]);
    assert_eq!(
        Task {
            sort_order: None,
            updated_at: before.updated_at,
            ..after
        },
        before
    );
}

/// The Context listing honours the same keys.
#[test]
fn a_context_listing_is_in_manual_order() {
    let (e, mut db) = (engine(), db());
    let ctx = e
        .apply(
            &mut db,
            Command::CreateContext(sunrise_domain::ContextDraft {
                name: "errands".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let t: Vec<EntityRef> = (0..3)
        .map(|i| {
            e.apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: format!("c{i}"),
                    contexts: vec![ctx],
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity
        })
        .collect();
    mv(&e, &mut db, &[t[2], t[0]], None, None).unwrap();
    let QueryResult::StreamTasks(listed) = e.query(&db, Query::ContextTasks(ctx)).unwrap() else {
        panic!("context tasks");
    };
    let listed: Vec<EntityRef> = listed.into_iter().map(|t| t.id).collect();
    assert_eq!(listed, vec![t[2], t[0], t[1]]);
}

// ---- convergence ----

/// One step of the property: a move on one replica, by indices into that
/// replica's current list, or a sync.
#[derive(Debug, Clone)]
enum Step {
    /// On A (`true`) or B: move the task at `from` to just before the row at
    /// `to` of the list without it (the end when past it).
    Move {
        on_a: bool,
        from: usize,
        to: usize,
    },
    Sync,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (any::<bool>(), 0usize..6, 0usize..6).prop_map(|(on_a, from, to)| Step::Move {
            on_a,
            from,
            to
        }),
        1 => Just(Step::Sync),
    ]
}

/// The move `step` makes on one replica's list, or nothing when its anchors
/// tie (the client would move the tied row too; the property only needs
/// moves the core accepts).
fn apply_step(e: &Engine, db: &mut Db, from: usize, to: usize) {
    let list = inbox_order(e, db);
    let moving = list[from % list.len()];
    let rest: Vec<EntityRef> = list.into_iter().filter(|id| *id != moving).collect();
    let at = to % (rest.len() + 1);
    let after = at.checked_sub(1).map(|i| rest[i]);
    let before = rest.get(at).copied();
    match mv(e, db, &[moving], after, before) {
        Ok(_) | Err(EngineError::Invalid(_)) => {}
        Err(other) => panic!("{other:?}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/task_order.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Concurrent moves on two replicas converge to one total order: after
    /// any interleaving of moves and syncs, a final sync leaves both lists,
    /// and every task's key, equal, and each rebuilds to what it holds.
    #[test]
    fn concurrent_moves_converge_to_one_total_order(
        steps in proptest::collection::vec(step(), 1..24),
    ) {
        let mut p = pair();
        let t = tasks(&p.ea, &mut p.dba, 6);
        mv(&p.ea, &mut p.dba, &t, None, None).unwrap();
        sync(&mut p);
        for (i, s) in steps.into_iter().enumerate() {
            let tick = 1_000 * (i as u64 + 1);
            *p.ca.0.lock() = T0 + tick;
            *p.cb.0.lock() = T0 + tick;
            match s {
                Step::Move { on_a: true, from, to } => apply_step(&p.ea, &mut p.dba, from, to),
                Step::Move { on_a: false, from, to } => apply_step(&p.eb, &mut p.dbb, from, to),
                Step::Sync => sync(&mut p),
            }
        }
        sync(&mut p);
        let a = inbox_order(&p.ea, &p.dba);
        prop_assert_eq!(&a, &inbox_order(&p.eb, &p.dbb));
        prop_assert_eq!(a.len(), t.len());
        for id in &t {
            prop_assert_eq!(key(&p.dba, *id), key(&p.dbb, *id));
            prop_assert!(key(&p.dba, *id).is_some());
        }
        p.ea.assert_rebuild_identity(&p.dba);
        p.eb.assert_rebuild_identity(&p.dbb);
    }
}
