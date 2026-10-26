//! Groups: a commit of several entity ops that every replica folds whole or
//! not at all (ADR-0065 §6, clarifications D8 and D13, PR 6).
//!
//! The commits here are the two commands that write one: `MergeBlocks` and
//! `BulkTasks`, whose members are full-state ops marked with
//! [`FULL_STATE_GROUP_KEY`], and `Patch` versions naming a `group`, sealed
//! the way a build that writes them will.

use super::field_merge::{emit_patch, pair, set, task_at, text, Pair};
use super::testutil::*;
use super::*;
use crate::commands::{BulkTaskAction, BulkTaskItem};
use crate::engine::merge::OpRef;
use crate::inner_op::{GroupWire, PatchPayload, FULL_STATE_GROUP_KEY};
use ciborium::value::Value;
use proptest::prelude::*;
use sunrise_crypto::decode_envelope;
use sunrise_domain::INBOX_STREAM_BYTES;

const INBOX: [u8; 16] = INBOX_STREAM_BYTES;

// ---- helpers ----

/// The envelopes of every group member `db` records, in stamp order.
fn member_envs(db: &Db) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT o.envelope FROM pending_groups g
             JOIN ops o ON o.stream_id = g.stream AND o.device_id = g.device AND o.seq = g.seq
             ORDER BY g.hlc_ms, g.hlc_logical, g.device, g.seq",
        )
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Every envelope `db` holds that is not a group member, in log order.
fn other_envs(db: &Db) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT o.envelope FROM ops o
             WHERE NOT EXISTS (SELECT 1 FROM pending_groups g
                               WHERE g.stream = o.stream_id AND g.device = o.device_id
                                 AND g.seq = o.seq)
             ORDER BY o.rowid",
        )
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// `(applied, rows)` of `pending_groups`, by `applied`.
fn group_rows(db: &Db) -> Vec<(i64, i64)> {
    let mut stmt = db
        .conn()
        .prepare("SELECT applied, COUNT(*) FROM pending_groups GROUP BY applied ORDER BY 1")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn parked(db: &Db) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM parked_ops", [], |r| r.get(0))
        .unwrap()
}

/// Whether `db` projects block `id`, and if so whether it is deleted.
fn block_state(db: &Db, id: EntityRef) -> Option<bool> {
    read_block(db.conn(), id.bytes())
        .unwrap()
        .map(|b| b.deleted)
}

fn deliver_all(to: &Engine, db: &mut Db, envs: &[Vec<u8>]) {
    for env in envs {
        to.apply_remote_all(db, env).unwrap();
    }
}

/// Two blocks on A, delivered to B, then merged on A. Returns the two
/// originals and the union.
fn merged_on_a(p: &mut Pair) -> (EntityRef, EntityRef, EntityRef) {
    let a =
        p.ea.apply(
            &mut p.dba,
            Command::CreateBlock(block_draft(9, 1, Some("a"))),
        )
        .unwrap()
        .entity;
    let b =
        p.ea.apply(
            &mut p.dba,
            Command::CreateBlock(block_draft(10, 1, Some("b"))),
        )
        .unwrap()
        .entity;
    deliver_all(&p.eb, &mut p.dbb, &other_envs(&p.dba));
    let union =
        p.ea.apply(&mut p.dba, Command::MergeBlocks { a, b })
            .unwrap()
            .entity;
    (a, b, union)
}

/// A `Patch` of `task` setting `title`, as one member of `group`.
fn grouped_title(task: EntityRef, title: &str, group: [u8; 16], size: u64) -> InnerOp {
    InnerOp::Patch(Box::new(PatchPayload {
        target: task,
        create: false,
        origin: None,
        parents: None,
        group: Some(GroupWire {
            id: group,
            size,
            unknown: Unknowns::new(),
        }),
        fields: [(
            "title".to_owned(),
            sunrise_domain::CborValue(set(text(title))),
        )]
        .into_iter()
        .collect(),
        unknown: Unknowns::new(),
    }))
}

fn complete_all(ids: &[EntityRef]) -> Command {
    Command::BulkTasks(
        ids.iter()
            .map(|id| BulkTaskItem {
                id: *id,
                action: BulkTaskAction::Complete,
            })
            .collect(),
    )
}

// ---- local groups ----

/// A merge is one group of three full-state ops, each marked with its own
/// op-ref, and its writer folds all of them in the command's transaction.
#[test]
fn a_merge_is_one_group_folded_where_it_is_written() {
    let mut p = pair();
    let (a, b, union) = merged_on_a(&mut p);
    assert_eq!(block_state(&p.dba, union), Some(false));
    assert_eq!(block_state(&p.dba, a), Some(true));
    assert_eq!(block_state(&p.dba, b), Some(true));
    assert_eq!(group_rows(&p.dba), vec![(1, 3)]);

    let mut ids = BTreeSet::new();
    for env in member_envs(&p.dba) {
        let header = decode_envelope(&env).unwrap();
        let inner = decode_inner_op(&p.ea.keychain.open_op(&env).unwrap()).unwrap();
        let mark = inner
            .full_state_unknowns()
            .and_then(|u| u.get(FULL_STATE_GROUP_KEY))
            .expect("a member carries the mark")
            .get()
            .clone();
        let get = |k: &str| {
            mark.as_map()
                .unwrap()
                .iter()
                .find(|(key, _)| key.as_text() == Some(k))
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let own = OpRef {
            stream: header.stream_id,
            device: header.device_id,
            seq: header.seq,
        };
        assert_eq!(get("op"), own.to_value(), "the mark names its own op");
        assert_eq!(get("size"), Value::Integer(3.into()));
        ids.insert(get("id").as_bytes().unwrap().clone());
    }
    assert_eq!(ids.len(), 1, "one group id");
}

/// The mark is the op's, not the entity's: no register, row or `Unknowns`
/// holds it.
#[test]
fn the_mark_is_never_folded_as_a_field() {
    let mut p = pair();
    let (_, _, union) = merged_on_a(&mut p);
    deliver_all(&p.eb, &mut p.dbb, &member_envs(&p.dba));
    for db in [&p.dba, &p.dbb] {
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM merge_registers WHERE field = ?",
                [FULL_STATE_GROUP_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
        let block = read_block(db.conn(), union.bytes()).unwrap().unwrap();
        assert!(!block.unknown.contains_key(FULL_STATE_GROUP_KEY));
    }
}

/// The refusals: the same block twice, a block this replica does not hold,
/// and one already deleted. None writes anything.
#[test]
fn a_merge_refuses_one_block_twice_a_missing_one_and_a_deleted_one() {
    let mut p = pair();
    let a =
        p.ea.apply(
            &mut p.dba,
            Command::CreateBlock(block_draft(9, 1, Some("a"))),
        )
        .unwrap()
        .entity;
    let b =
        p.ea.apply(
            &mut p.dba,
            Command::CreateBlock(block_draft(10, 1, Some("b"))),
        )
        .unwrap()
        .entity;
    let before = op_count(&p.dba);
    let missing = EntityRef::new(EntityKind::Block, [7u8; 16]);
    assert!(matches!(
        p.ea.apply(&mut p.dba, Command::MergeBlocks { a, b: a }),
        Err(EngineError::Invalid(_))
    ));
    assert!(matches!(
        p.ea.apply(&mut p.dba, Command::MergeBlocks { a, b: missing }),
        Err(EngineError::NotFound(_))
    ));
    p.ea.apply(&mut p.dba, Command::DeleteBlock(b)).unwrap();
    let after_delete = op_count(&p.dba);
    assert!(matches!(
        p.ea.apply(&mut p.dba, Command::MergeBlocks { a, b }),
        Err(EngineError::Invalid(_))
    ));
    assert_eq!(op_count(&p.dba), after_delete);
    assert_eq!(after_delete, before + 1);
}

/// One refused item refuses the batch, and nothing is written: an unknown
/// id, a repeated id, a task that is not deleted given `Restore`, and an
/// empty batch.
#[test]
fn a_bulk_command_is_all_or_nothing() {
    let mut p = pair();
    let t1 = new_task(&p.ea, &mut p.dba, "one");
    let t2 = new_task(&p.ea, &mut p.dba, "two");
    let before = op_count(&p.dba);
    let unknown = EntityRef::new(EntityKind::Task, [7u8; 16]);
    let refusals = [
        complete_all(&[t1, unknown]),
        complete_all(&[t1, t1]),
        Command::BulkTasks(vec![
            BulkTaskItem {
                id: t1,
                action: BulkTaskAction::Complete,
            },
            BulkTaskItem {
                id: t2,
                action: BulkTaskAction::Restore,
            },
        ]),
        Command::BulkTasks(Vec::new()),
    ];
    for cmd in refusals {
        assert!(p.ea.apply(&mut p.dba, cmd).is_err());
        assert_eq!(op_count(&p.dba), before, "a refused batch writes nothing");
    }
    assert_eq!(task_at(&p.dba, t1).state, TaskState::Todo);

    let res = p.ea.apply(&mut p.dba, complete_all(&[t1, t2])).unwrap();
    assert_eq!(res.entity, t1);
    assert_eq!(task_at(&p.dba, t1).state, TaskState::Done);
    assert_eq!(task_at(&p.dba, t2).state, TaskState::Done);
    assert_eq!(group_rows(&p.dba), vec![(1, 2)]);
}

/// Every action of a batch lands as its single command would.
#[test]
fn every_bulk_action_writes_what_its_single_command_writes() {
    let mut p = pair();
    let ids: Vec<EntityRef> = (0..5)
        .map(|i| new_task(&p.ea, &mut p.dba, &format!("t{i}")))
        .collect();
    p.ea.apply(&mut p.dba, Command::DeleteTask(ids[4])).unwrap();
    let (to, _) = block_at(9, 1);
    let items = vec![
        BulkTaskItem {
            id: ids[0],
            action: BulkTaskAction::Complete,
        },
        BulkTaskItem {
            id: ids[1],
            action: BulkTaskAction::DeferTo(to.clone()),
        },
        BulkTaskItem {
            id: ids[2],
            action: BulkTaskAction::Update(Box::new(TaskPatch {
                state: Some(TaskState::Cancelled),
                ..Default::default()
            })),
        },
        BulkTaskItem {
            id: ids[3],
            action: BulkTaskAction::Delete,
        },
        BulkTaskItem {
            id: ids[4],
            action: BulkTaskAction::Restore,
        },
    ];
    p.ea.apply(&mut p.dba, Command::BulkTasks(items)).unwrap();
    assert_eq!(task_at(&p.dba, ids[0]).state, TaskState::Done);
    let deferred = task_at(&p.dba, ids[1]);
    assert_eq!(
        (deferred.planned_at, deferred.deferred_count),
        (Some(to), 1)
    );
    assert_eq!(task_at(&p.dba, ids[2]).state, TaskState::Cancelled);
    assert!(task_at(&p.dba, ids[3]).deleted);
    assert!(!task_at(&p.dba, ids[4]).deleted);
    assert_eq!(group_rows(&p.dba), vec![(1, 5)]);
}

/// Two occurrences of one routine completed in one batch both count: the
/// routine is advanced once per occurrence and written once, in the group.
#[test]
fn a_bulk_completion_counts_every_occurrence_of_one_routine() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_seeded(ROOT, [1u8; 32], clock.clone());
    let mut db = db_root(ROOT);
    legacy_peer(&db);
    let (rid, occ) = seed_routine(&e, &mut db);
    set_clock(&clock, (occ[1].1 + 3_600_000) as u64);
    let before = routine_op_count(&db);
    e.apply(&mut db, complete_all(&[occ[0].0, occ[1].0]))
        .unwrap();
    let routine = routine_of(&e, &db, rid);
    assert_eq!(routine.streak_keys.len(), 2, "{:?}", routine.streak_keys);
    assert_eq!(routine_op_count(&db), before + 1, "one routine op");
    assert_eq!(group_rows(&db), vec![(1, 3)]);
}

// ---- receiving a group ----

/// One member alone is held: stored and counted, not parked, not folded,
/// and nothing is announced.
#[test]
fn a_group_member_alone_is_pending_not_parked() {
    let mut p = pair();
    let (a, b, union) = merged_on_a(&mut p);
    let members = member_envs(&p.dba);
    let events = p.eb.apply_remote_all(&mut p.dbb, &members[0]).unwrap();
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(group_rows(&p.dbb), vec![(0, 1)]);
    assert_eq!(parked(&p.dbb), 0, "pending is not parked");
    assert_eq!(block_state(&p.dbb, union), None);
    assert_eq!(block_state(&p.dbb, a), Some(false));
    assert_eq!(block_state(&p.dbb, b), Some(false));
    // Delivered again, it is the duplicate any op is.
    assert!(p
        .eb
        .apply_remote_all(&mut p.dbb, &members[0])
        .unwrap()
        .is_empty());
    assert_eq!(group_rows(&p.dbb), vec![(0, 1)]);
}

/// A held member advances the sync cursor like any op that arrived: what it
/// waits for is the rest of its group, not a resend.
#[test]
fn a_pending_group_counts_toward_the_cursor() {
    let mut p = pair();
    merged_on_a(&mut p);
    let members = member_envs(&p.dba);
    let a = p.ea.keychain.device_id();
    deliver_all(&p.eb, &mut p.dbb, &members[..2]);
    let second = decode_envelope(&members[1]).unwrap();
    assert_eq!(second.stream_id, INBOX);
    assert_eq!(cursor_for(&p.dbb, &INBOX, &a), second.seq);
    assert_eq!(group_rows(&p.dbb), vec![(0, 2)]);
}

/// Delivered last first, nothing shows until the last member, and that
/// delivery folds the whole group and announces every entity it wrote.
#[test]
fn a_complete_group_folds_in_one_transaction() {
    let mut p = pair();
    let (a, b, union) = merged_on_a(&mut p);
    let mut members = member_envs(&p.dba);
    members.reverse();
    let last = members.pop().unwrap();
    for env in &members {
        assert!(p.eb.apply_remote_all(&mut p.dbb, env).unwrap().is_empty());
        assert_eq!(block_state(&p.dbb, union), None);
        assert_eq!(block_state(&p.dbb, a), Some(false));
        assert_eq!(block_state(&p.dbb, b), Some(false));
    }
    let events = p.eb.apply_remote_all(&mut p.dbb, &last).unwrap();
    let mut announced: Vec<EntityRef> = events
        .iter()
        .map(|e| match e {
            DomainEvent::Created(t) | DomainEvent::Updated(t) | DomainEvent::Deleted(t) => *t,
            other => panic!("{other:?}"),
        })
        .collect();
    announced.sort();
    let mut expected = vec![a, b, union];
    expected.sort();
    assert_eq!(announced, expected);
    assert_eq!(block_state(&p.dbb, union), Some(false));
    assert_eq!(block_state(&p.dbb, a), Some(true));
    assert_eq!(block_state(&p.dbb, b), Some(true));
    assert_eq!(group_rows(&p.dbb), vec![(1, 3)]);
    assert_eq!(
        day_rows(&p.ea, &p.dba, day_ms())
            .into_iter()
            .map(|r| r.block)
            .collect::<Vec<_>>(),
        day_rows(&p.eb, &p.dbb, day_ms())
            .into_iter()
            .map(|r| r.block)
            .collect::<Vec<_>>()
    );
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// A group is complete once `size` members are present (D8); a member
/// beyond that applies the moment it arrives.
#[test]
fn an_extra_member_after_completion_applies_at_once() {
    let mut p = pair();
    let tasks: Vec<EntityRef> = (0..3)
        .map(|i| new_task(&p.ea, &mut p.dba, &format!("t{i}")))
        .collect();
    deliver_all(&p.eb, &mut p.dbb, &other_envs(&p.dba));
    let group = [5u8; 16];
    let envs: Vec<Vec<u8>> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            emit_patch(
                &p.ea,
                &mut p.dba,
                &INBOX,
                &grouped_title(*t, &format!("g{i}"), group, 2),
            )
            .0
        })
        .collect();
    assert!(p
        .eb
        .apply_remote_all(&mut p.dbb, &envs[0])
        .unwrap()
        .is_empty());
    assert_eq!(task_at(&p.dbb, tasks[0]).title, "t0");
    assert_eq!(
        p.eb.apply_remote_all(&mut p.dbb, &envs[1]).unwrap().len(),
        2
    );
    assert_eq!(task_at(&p.dbb, tasks[0]).title, "g0");
    assert_eq!(task_at(&p.dbb, tasks[1]).title, "g1");
    assert_eq!(
        p.eb.apply_remote_all(&mut p.dbb, &envs[2]).unwrap().len(),
        1
    );
    assert_eq!(task_at(&p.dbb, tasks[2]).title, "g2");
    assert_eq!(group_rows(&p.dbb), vec![(1, 3)]);
}

/// A mark that names another op, as an older build writes back the copy it
/// kept on the entity, marks nothing: the op applies at once, and the key is
/// not folded.
#[test]
fn a_copied_mark_naming_another_op_marks_nothing() {
    let mut p = pair();
    let task = new_task(&p.ea, &mut p.dba, "t");
    deliver_all(&p.eb, &mut p.dbb, &other_envs(&p.dba));
    let mut copy = task_at(&p.dba, task);
    copy.title = "edited on an older build".into();
    let elsewhere = OpRef {
        stream: INBOX,
        device: [3u8; 16],
        seq: 1,
    };
    copy.unknown.insert(
        FULL_STATE_GROUP_KEY.to_owned(),
        sunrise_domain::CborValue(Value::Map(vec![
            (Value::Text("id".into()), Value::Bytes(vec![1; 16])),
            (Value::Text("op".into()), elsewhere.to_value()),
            (Value::Text("size".into()), Value::Integer(2.into())),
        ])),
    );
    let (env, _) = emit_patch(&p.ea, &mut p.dba, &INBOX, &InnerOp::TaskUpdate(copy));
    assert_eq!(p.eb.apply_remote_all(&mut p.dbb, &env).unwrap().len(), 1);
    let applied = task_at(&p.dbb, task);
    assert_eq!(applied.title, "edited on an older build");
    assert!(!applied.unknown.contains_key(FULL_STATE_GROUP_KEY));
    assert!(group_rows(&p.dbb).is_empty());
}

/// A bulk command crosses to a peer whole, whatever order its members come
/// in.
#[test]
fn a_bulk_command_reaches_a_peer_whole() {
    let mut p = pair();
    let ids: Vec<EntityRef> = (0..3)
        .map(|i| new_task(&p.ea, &mut p.dba, &format!("t{i}")))
        .collect();
    deliver_all(&p.eb, &mut p.dbb, &other_envs(&p.dba));
    p.ea.apply(&mut p.dba, complete_all(&ids)).unwrap();
    let members = member_envs(&p.dba);
    assert_eq!(members.len(), 3);
    for env in [&members[2], &members[0]] {
        p.eb.apply_remote_all(&mut p.dbb, env).unwrap();
        for id in &ids {
            assert_eq!(task_at(&p.dbb, *id).state, TaskState::Todo);
        }
    }
    p.eb.apply_remote_all(&mut p.dbb, &members[1]).unwrap();
    for id in &ids {
        assert_eq!(task_at(&p.dbb, *id), task_at(&p.dba, *id));
    }
}

// ---- property 3: a group is never half-visible ----

/// How many of `ids` `db` projects as done.
fn done_count(db: &Db, ids: &[EntityRef]) -> usize {
    ids.iter()
        .filter(|id| task_at(db, **id).state == TaskState::Done)
        .count()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 24,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/groups.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// A writer completes `n` tasks in one batch, beside ungrouped edits of
    /// another task. A receiver handed every envelope in any order, with
    /// repeats, never shows some of the batch done and some not, and ends
    /// on the writer's projection, which it rebuilds to (ADR-0065 property
    /// 3). The receive path's own check runs the rebuild after every
    /// delivery.
    #[test]
    fn a_group_is_never_half_visible(
        n in 2usize..=4,
        edits in 0usize..3,
        order in proptest::collection::vec(any::<prop::sample::Index>(), 0..24),
    ) {
        begin_case();
        let mut p = pair();
        let ids: Vec<EntityRef> = (0..n)
            .map(|i| new_task(&p.ea, &mut p.dba, &format!("t{i}")))
            .collect();
        let other = new_task(&p.ea, &mut p.dba, "other");
        deliver_all(&p.eb, &mut p.dbb, &other_envs(&p.dba));
        let mark = other_envs(&p.dba).len();
        p.ea.apply(&mut p.dba, complete_all(&ids)).unwrap();
        for i in 0..edits {
            let patch = TaskPatch {
                title: Some(format!("edit {i}")),
                ..Default::default()
            };
            p.ea.apply(&mut p.dba, Command::UpdateTask { id: other, patch }).unwrap();
        }
        let mut envs = member_envs(&p.dba);
        envs.extend(other_envs(&p.dba).into_iter().skip(mark));
        // The schedule: the picks `order` names, then everything, so every
        // envelope arrives at least once and some arrive twice.
        let mut schedule: Vec<Vec<u8>> =
            order.iter().map(|i| envs[i.index(envs.len())].clone()).collect();
        schedule.extend(envs.iter().cloned());
        for env in &schedule {
            p.eb.apply_remote_all(&mut p.dbb, env).unwrap();
            let done = done_count(&p.dbb, &ids);
            prop_assert!(done == 0 || done == n, "{done} of {n} done");
        }
        prop_assert_eq!(done_count(&p.dbb, &ids), n);
        for id in ids.iter().chain([&other]) {
            prop_assert_eq!(task_at(&p.dbb, *id), task_at(&p.dba, *id));
        }
        prop_assert_eq!(group_rows(&p.dbb), vec![(1, i64::try_from(n).unwrap())]);
    }
}
