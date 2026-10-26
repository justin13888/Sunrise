//! ADR-0065 PR 8: `core.field_ops` and the write mode (§7).
//!
//! Once a vault requires the feature, every command writes the `Patch` of
//! the fields it changes, naming its writer's heads, and a commit of several
//! entities is a group of them. Until then, while a paired device lacks the
//! feature, commands write full-state ops (clarification D9).

use super::*;
use crate::feature::FIELD_OPS_FEATURE;
use crate::inner_op::{decode_inner_op, PatchPayload};
use ciborium::value::Value;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::{BlockPatch, ContextPatch, StreamPatch};

const ROOT: [u8; 32] = [0x65; 32];

/// Replicas of one vault that trust each other and have each advertised
/// what their build supports, as a fresh `Core::open` does.
struct Peers {
    engines: Vec<Engine>,
    clocks: Vec<Arc<FakeClock>>,
    dbs: Vec<Db>,
}

fn peers_with(features: &[&'static [crate::feature::Feature]]) -> Peers {
    let clocks: Vec<Arc<FakeClock>> = features
        .iter()
        .map(|_| Arc::new(FakeClock(PLMutex::new(T0))))
        .collect();
    let engines: Vec<Engine> = features
        .iter()
        .zip(&clocks)
        .enumerate()
        .map(|(i, (f, c))| {
            engine_seeded(ROOT, [u8::try_from(i).unwrap() + 1; 32], c.clone()).with_features(f)
        })
        .collect();
    let mut dbs: Vec<Db> = engines.iter().map(|_| db_root(ROOT)).collect();
    for (i, e) in engines.iter().enumerate() {
        for (j, other) in engines.iter().enumerate() {
            if i != j {
                trust(e, &mut dbs[i], other);
            }
        }
        e.advertise_features(&mut dbs[i]).unwrap();
    }
    let mut p = Peers {
        engines,
        clocks,
        dbs,
    };
    p.sync_all();
    p
}

/// Two current devices.
fn peers() -> Peers {
    peers_with(&[crate::feature::FEATURES, crate::feature::FEATURES])
}

impl Peers {
    /// Every op `from` logged under its own device id, delivered to `to` in
    /// seq order. Re-delivery is a no-op.
    fn deliver(&mut self, from: usize, to: usize) {
        for env in own_envelopes(&self.engines[from], &self.dbs[from]) {
            self.engines[to]
                .apply_remote(&mut self.dbs[to], &env)
                .unwrap();
        }
    }

    fn sync_all(&mut self) {
        for from in 0..self.engines.len() {
            for to in 0..self.engines.len() {
                if from != to {
                    self.deliver(from, to);
                }
            }
        }
    }

    fn apply(&mut self, who: usize, cmd: Command) -> CommandResult {
        self.engines[who].apply(&mut self.dbs[who], cmd).unwrap()
    }
}

/// Every envelope `e`'s device logged in `db`, in stream and seq order.
fn own_envelopes(e: &Engine, db: &Db) -> Vec<Vec<u8>> {
    let device = e.keychain.device_id();
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY stream_id, seq")
        .unwrap();
    stmt.query_map(params![&device[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The greatest `ops` rowid, so a test can read what a command wrote.
fn mark(db: &Db) -> i64 {
    db.conn()
        .query_row("SELECT COALESCE(MAX(rowid), 0) FROM ops", [], |r| r.get(0))
        .unwrap()
}

/// Every op `e`'s device logged after `since`, as `(inner_kind, op)`.
fn written_since(e: &Engine, db: &Db, since: i64) -> Vec<(String, InnerOp)> {
    let device = e.keychain.device_id();
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT inner_kind, envelope FROM ops WHERE device_id = ? AND rowid > ?
             ORDER BY rowid",
        )
        .unwrap();
    let rows: Vec<(String, Vec<u8>)> = stmt
        .query_map(params![&device[..], since], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    rows.into_iter()
        .map(|(kind, env)| {
            let cbor = e.keychain.open_op(&env).unwrap();
            (kind, decode_inner_op(&cbor).unwrap())
        })
        .collect()
}

/// The entity ops among `ops`: everything but control ops and the
/// append-only records.
fn entity_writes(ops: Vec<(String, InnerOp)>) -> Vec<(String, InnerOp)> {
    ops.into_iter()
        .filter(|(_, op)| {
            !op.is_control() && matches!(op.target_ref().kind().spec().merge, Merge::Lww)
        })
        .collect()
}

/// The `Patch`es among `ops`, by their target.
fn patches(ops: &[(String, InnerOp)]) -> Vec<PatchPayload> {
    ops.iter()
        .filter_map(|(_, op)| match op {
            InnerOp::Patch(p) => Some((**p).clone()),
            _ => None,
        })
        .collect()
}

fn required(db: &Db) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT feature FROM vault_required_features ORDER BY feature")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn new_task(e: &Engine, db: &mut Db, title: &str) -> EntityRef {
    e.apply(
        db,
        Command::CreateTask(TaskDraft {
            title: title.into(),
            ..Default::default()
        }),
    )
    .unwrap()
    .entity
}

fn retitle(id: EntityRef, title: &str) -> Command {
    Command::UpdateTask {
        id,
        patch: TaskPatch {
            title: Some(title.into()),
            ..Default::default()
        },
    }
}

fn conflicts(e: &Engine, db: &Db) -> Vec<crate::conflicts::EntityConflict> {
    match e
        .query(
            db,
            Query::Conflicts {
                entity: None,
                include_quiet: true,
            },
        )
        .unwrap()
    {
        QueryResult::Conflicts(c) => c,
        other => panic!("{other:?}"),
    }
}

/// Whether `kind` is a full-state entity op's `inner_kind`.
fn full_state_kind(kind: &str) -> bool {
    sunrise_id::registry::ENTITIES
        .iter()
        .filter(|e| matches!(e.merge, Merge::Lww))
        .flat_map(|e| e.ops.iter())
        .any(|o| o.inner_kind == kind)
}

use sunrise_id::registry::Merge;

// ---- when the vault requires the feature ----

/// The first write of an entity into a vault whose every device supports
/// `core.field_ops` makes the vault require it, in the same transaction and
/// just before the `Patch`; a peer that applies the requirement writes
/// `Patch`es too, and emits none of its own.
#[test]
fn the_first_entity_write_requires_field_ops_when_every_device_supports_it() {
    let mut p = peers();
    assert!(!required(&p.dbs[0]).contains(&FIELD_OPS_FEATURE.to_owned()));
    let since = mark(&p.dbs[0]);
    let task = new_task(&p.engines[0], &mut p.dbs[0], "t");
    let written = written_since(&p.engines[0], &p.dbs[0], since);
    let kinds: Vec<&str> = written.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, ["vault.requires", "task.patch"], "{written:?}");
    assert!(
        matches!(&written[0].1, InnerOp::VaultRequires(r) if r.features == [FIELD_OPS_FEATURE])
    );
    assert!(matches!(&written[1].1, InnerOp::Patch(p) if p.create && p.target == task));
    assert!(required(&p.dbs[0]).contains(&FIELD_OPS_FEATURE.to_owned()));

    p.sync_all();
    assert!(required(&p.dbs[1]).contains(&FIELD_OPS_FEATURE.to_owned()));
    let since = mark(&p.dbs[1]);
    p.apply(1, retitle(task, "t2"));
    let kinds: Vec<String> = written_since(&p.engines[1], &p.dbs[1], since)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(kinds, ["task.patch"]);

    // A vault with no other device requires it at its first write too.
    let solo = engine_seeded(ROOT, [7; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    new_task(&solo, &mut db, "alone");
    assert_eq!(required(&db), [FIELD_OPS_FEATURE]);
}

/// A command that writes no entity requires nothing: the requirement rides
/// with the first `Patch`, not with the vault's first command.
#[test]
fn a_command_that_writes_no_entity_requires_nothing() {
    let (e, mut db) = (engine(), db());
    e.apply(&mut db, Command::MaterializeRoutines { now_ms: T0 })
        .unwrap();
    assert!(required(&db).is_empty());
    assert_eq!(op_count(&db), 0);
}

/// While a paired device has not said it supports `core.field_ops`, commands
/// write full-state ops and the vault requires nothing (D9). Requiring it
/// unconfirmed is refused naming the device; confirmed, it is required, and
/// from then on writes are `Patch`es.
#[test]
fn a_device_lacking_field_ops_keeps_the_vault_in_legacy_mode() {
    static WITHOUT: std::sync::LazyLock<Vec<crate::feature::Feature>> =
        std::sync::LazyLock::new(|| {
            crate::feature::FEATURES
                .iter()
                .filter(|f| f.id != FIELD_OPS_FEATURE)
                .copied()
                .collect()
        });
    let mut p = peers_with(&[crate::feature::FEATURES, WITHOUT.as_slice()]);
    let since = mark(&p.dbs[0]);
    let task = new_task(&p.engines[0], &mut p.dbs[0], "t");
    p.apply(0, retitle(task, "t2"));
    let kinds: Vec<String> = written_since(&p.engines[0], &p.dbs[0], since)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(kinds, ["task.create", "task.update"]);
    assert!(required(&p.dbs[0]).is_empty());

    let lacking = p.engines[1].keychain.device_id();
    match p.engines[0].require_features(&mut p.dbs[0], &[FIELD_OPS_FEATURE], false) {
        Err(EngineError::FeatureUnsupportedByDevices { feature, devices }) => {
            assert_eq!(feature, FIELD_OPS_FEATURE);
            assert_eq!(devices, vec![lacking]);
        }
        other => panic!("expected the refusal, got {other:?}"),
    }
    let added = p.engines[0]
        .require_features(&mut p.dbs[0], &[FIELD_OPS_FEATURE], true)
        .unwrap();
    assert_eq!(added, [FIELD_OPS_FEATURE]);
    let since = mark(&p.dbs[0]);
    p.apply(0, retitle(task, "t3"));
    let kinds: Vec<String> = written_since(&p.engines[0], &p.dbs[0], since)
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(kinds, ["task.patch"]);
}

/// A build without `core.field_ops`, in a vault that requires it, refuses
/// every local entity write and goes on applying what it receives, `Patch`es
/// included (ADR-0045 §8).
#[test]
fn a_build_without_field_ops_refuses_local_writes_and_keeps_applying_patches() {
    static WITHOUT: std::sync::LazyLock<Vec<crate::feature::Feature>> =
        std::sync::LazyLock::new(|| {
            crate::feature::FEATURES
                .iter()
                .filter(|f| f.id != FIELD_OPS_FEATURE)
                .copied()
                .collect()
        });
    let mut p = peers_with(&[crate::feature::FEATURES, WITHOUT.as_slice()]);
    p.engines[0]
        .require_features(&mut p.dbs[0], &[FIELD_OPS_FEATURE], true)
        .unwrap();
    let task = new_task(&p.engines[0], &mut p.dbs[0], "t");
    p.apply(0, retitle(task, "from a current device"));
    p.sync_all();
    assert_eq!(
        task_of(&p.engines[1], &p.dbs[1], task).title,
        "from a current device"
    );
    let refused = p.engines[1].apply(&mut p.dbs[1], retitle(task, "from an older one"));
    assert!(
        matches!(&refused, Err(EngineError::FeatureMissing { feature }) if feature == FIELD_OPS_FEATURE),
        "{refused:?}"
    );
    let refused = p.engines[1].apply(
        &mut p.dbs[1],
        Command::CreateTask(TaskDraft {
            title: "new".into(),
            ..Default::default()
        }),
    );
    assert!(matches!(refused, Err(EngineError::FeatureMissing { .. })));
    p.engines[1].assert_rebuild_identity(&p.dbs[1]);
}

/// The seal guard refuses a `Patch` of a versioned entity in a vault that
/// does not require `core.field_ops`: forgetting the requirement is a
/// refusal, never a full-state writer's entity overwritten (ADR-0065 §7).
#[test]
fn a_patch_sealed_before_the_vault_requires_field_ops_is_refused() {
    let (e, mut db) = (engine(), db());
    legacy_peer(&db);
    let task = new_task(&e, &mut db, "t");
    let op = super::field_merge::patch(
        task,
        false,
        vec![("title", super::field_merge::set(Value::Text("x".into())))],
    );
    let refused = db.with_tx(|tx| {
        e.append_and_fold(tx, &op, &sunrise_domain::INBOX_STREAM_BYTES, T0)
            .map(drop)
    });
    let refused = refused.map_err(|e| EngineError::from(e).lift_seal_refusal());
    assert!(
        matches!(&refused, Err(EngineError::Invalid(why)) if why.contains(FIELD_OPS_FEATURE)),
        "{refused:?}"
    );
    assert_eq!(read_task_t(&e, &db, task).title, "t");
}

// ---- what is written ----

/// The defaults a `Patch` create's projection reads a field it left out as
/// are the registry's, hashed into the schema (ADR-0044 §2): the four fields
/// the domain decoders require, and no others.
#[test]
fn the_registry_declares_the_defaults_a_create_reads() {
    use crate::engine::merge::field_default;
    assert_eq!(
        field_default(EntityKind::Task, "title"),
        Some(Value::Text(String::new()))
    );
    assert_eq!(
        field_default(EntityKind::Task, "state"),
        Value::serialized(&TaskState::Todo).ok()
    );
    assert_eq!(
        field_default(EntityKind::Task, "stream_id"),
        Value::serialized(&inbox_stream_ref()).ok()
    );
    assert_eq!(
        field_default(EntityKind::Context, "name"),
        Some(Value::Text(String::new()))
    );
    let declared: Vec<(&str, &str)> = sunrise_id::registry::ENTITIES
        .iter()
        .flat_map(|e| e.records.iter().map(move |r| (e, r)))
        .flat_map(|(e, r)| {
            r.fields
                .iter()
                .filter(|f| f.default.is_some())
                .map(move |f| (e.tag, f.name))
        })
        .collect();
    assert_eq!(
        declared,
        [
            ("task", "title"),
            ("task", "stream_id"),
            ("task", "state"),
            ("context", "name")
        ]
    );
    // A create that writes only its title projects with the other three.
    let (e, mut db) = (engine(), db());
    let task = new_task(&e, &mut db, "t");
    let t = read_task_t(&e, &db, task);
    assert_eq!((t.state, t.stream_id), (TaskState::Todo, None));
}

/// A vault every device of which reads field ops never logs a full-state
/// entity op once it requires the feature, whatever command writes.
#[test]
fn no_full_state_op_is_emitted_once_field_ops_is_required() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_seeded(ROOT, [1; 32], clock.clone());
    let mut db = db_root(ROOT);
    let apply = |db: &mut Db, cmd: Command| e.apply(db, cmd).unwrap();
    let stream = apply(
        &mut db,
        Command::CreateStream(StreamDraft {
            name: "Work".into(),
            ..Default::default()
        }),
    )
    .entity;
    let ctx = new_context(&e, &mut db, "errands");
    let task = apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "t".into(),
            contexts: vec![ctx],
            ..Default::default()
        }),
    )
    .entity;
    let other = new_task(&e, &mut db, "other");
    apply(&mut db, retitle(task, "t2"));
    apply(
        &mut db,
        Command::DeferTaskTo {
            id: task,
            to: ms_to_ts(NOW + DAY_MS).into(),
        },
    );
    apply(&mut db, Command::PromoteToStream { id: other, stream });
    apply(&mut db, Command::CompleteTask(other));
    apply(&mut db, Command::DeleteTask(task));
    apply(&mut db, Command::Restore(task));
    apply(
        &mut db,
        Command::UpdateStream {
            id: stream,
            patch: StreamPatch {
                name: Some("Job".into()),
                ..Default::default()
            },
        },
    );
    apply(
        &mut db,
        Command::UpdateContext {
            id: ctx,
            patch: ContextPatch {
                name: Some("chores".into()),
                ..Default::default()
            },
        },
    );
    let (rid, occ) = seed_routine(&e, &mut db);
    set_clock(&clock, (occ[0].1 + 3_600_000) as u64);
    apply(&mut db, Command::CompleteTask(occ[0].0));
    let occurrence_key = routine_of(&e, &db, rid)
        .occurrence_key_of(&occ[1].0, ms_to_ts(occ[1].1))
        .unwrap();
    apply(
        &mut db,
        Command::SkipRoutineOccurrence {
            id: rid,
            occurrence_key,
        },
    );
    apply(
        &mut db,
        Command::UpdateRoutine {
            scope: sunrise_domain::EditScope::All,
            id: rid,
            patch: sunrise_domain::RoutinePatch {
                rrule: Some(sunrise_domain::RRule::parse("FREQ=WEEKLY").unwrap()),
                ..Default::default()
            },
        },
    );
    let block = apply(
        &mut db,
        Command::CreateBlock(block_draft(9, 1, Some("Desk"))),
    )
    .entity;
    apply(&mut db, Command::BindTask { block, task });
    apply(
        &mut db,
        Command::UpdateBlock {
            id: block,
            patch: BlockPatch {
                title: Some(Some("Deep work".into())),
                ..Default::default()
            },
        },
    );
    apply(&mut db, Command::UnbindTask { block, task });
    let second = apply(
        &mut db,
        Command::CreateBlock(block_draft(10, 1, Some("Desk"))),
    )
    .entity;
    apply(
        &mut db,
        Command::MergeBlocks {
            a: block,
            b: second,
        },
    );
    let att = apply(&mut db, Command::AttachFile(attachment_draft(task))).entity;
    apply(&mut db, Command::DetachFile(att));
    apply(
        &mut db,
        Command::MoveTasks {
            ids: vec![task],
            after: None,
            before: None,
        },
    );
    apply(
        &mut db,
        Command::BulkTasks(vec![crate::commands::BulkTaskItem {
            id: task,
            action: crate::commands::BulkTaskAction::Complete,
        }]),
    );
    apply(
        &mut db,
        Command::ResolveConflict {
            entity: task,
            field: "title".into(),
            value: sunrise_domain::CborValue(Value::Text("settled".into())),
        },
    );
    apply(&mut db, Command::DeleteRoutine(rid));
    apply(&mut db, Command::DeleteContext(ctx));
    apply(
        &mut db,
        Command::DeleteStream {
            id: stream,
            disposition: sunrise_domain::Disposition::Detach,
        },
    );

    let mut stmt = db
        .conn()
        .prepare("SELECT DISTINCT inner_kind FROM ops")
        .unwrap();
    let kinds: BTreeSet<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let full: Vec<&String> = kinds.iter().filter(|k| full_state_kind(k)).collect();
    assert!(full.is_empty(), "full-state ops were written: {full:?}");
    for k in [
        "task.patch",
        "stream.patch",
        "context.patch",
        "routine.patch",
        "block.patch",
    ] {
        assert!(kinds.contains(k), "{k} in {kinds:?}");
    }
    assert!(kinds.contains("attachment.patch"), "{kinds:?}");
    e.assert_rebuild_identity(&db);
}

/// The commands of the field-ops vault, each with the fields it writes on
/// each entity: only those it changed.
#[test]
fn every_command_writes_only_the_fields_it_changed() {
    /// What each case starts from.
    struct Fixture {
        e: Engine,
        db: Db,
        clock: Arc<FakeClock>,
        task: EntityRef,
        stream: EntityRef,
        ctx: EntityRef,
        ctx2: EntityRef,
        block: EntityRef,
        att: EntityRef,
        rid: EntityRef,
        occ: Vec<(EntityRef, i64)>,
    }
    fn fixture() -> Fixture {
        let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
        let e = engine_seeded(ROOT, [1; 32], clock.clone());
        let mut db = db_root(ROOT);
        let stream = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let ctx = new_context(&e, &mut db, "errands");
        let ctx2 = new_context(&e, &mut db, "calls");
        let task = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "t".into(),
                    contexts: vec![ctx],
                    priority: Some(2),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let block = e
            .apply(
                &mut db,
                Command::CreateBlock(block_draft(9, 1, Some("Desk"))),
            )
            .unwrap()
            .entity;
        let att = e
            .apply(&mut db, Command::AttachFile(attachment_draft(task)))
            .unwrap()
            .entity;
        let (rid, occ) = seed_routine(&e, &mut db);
        Fixture {
            e,
            db,
            clock,
            task,
            stream,
            ctx,
            ctx2,
            block,
            att,
            rid,
            occ,
        }
    }
    type Case = (
        &'static str,
        fn(&mut Fixture) -> Command,
        fn(&Fixture) -> Vec<(EntityRef, &'static [&'static str])>,
    );
    let cases: Vec<Case> = vec![
        (
            "retitle",
            |f| retitle(f.task, "renamed"),
            |f| vec![(f.task, &["title"])],
        ),
        (
            "priority",
            |f| Command::UpdateTask {
                id: f.task,
                patch: TaskPatch {
                    priority: Some(Some(4)),
                    ..Default::default()
                },
            },
            |f| vec![(f.task, &["priority"])],
        ),
        (
            "an edit naming every field, of which one changes",
            |f| {
                let t = read_task_t(&f.e, &f.db, f.task);
                Command::UpdateTask {
                    id: f.task,
                    patch: TaskPatch {
                        title: Some(t.title),
                        body: Some(t.body),
                        contexts: Some(t.contexts.into_iter().collect()),
                        priority: Some(Some(5)),
                        energy: Some(t.energy),
                        estimated_duration_s: Some(t.estimated_duration_s),
                        planned_at: Some(t.planned_at),
                        hard_due_at: Some(t.hard_due_at),
                        ..Default::default()
                    },
                }
            },
            |f| vec![(f.task, &["priority"])],
        ),
        (
            "add a context",
            |f| Command::UpdateTask {
                id: f.task,
                patch: TaskPatch {
                    contexts: Some(vec![f.ctx, f.ctx2]),
                    ..Default::default()
                },
            },
            |f| vec![(f.task, &["contexts"])],
        ),
        (
            "complete",
            |f| Command::CompleteTask(f.task),
            |f| vec![(f.task, &["completed_at", "state"])],
        ),
        (
            "defer",
            |f| Command::DeferTaskTo {
                id: f.task,
                to: ms_to_ts(NOW + 2 * DAY_MS).into(),
            },
            |f| vec![(f.task, &["deferred_count", "scheduled_at"])],
        ),
        (
            "delete",
            |f| Command::DeleteTask(f.task),
            |f| vec![(f.task, &["deleted"])],
        ),
        (
            "restore",
            |f| {
                f.e.apply(&mut f.db, Command::DeleteTask(f.task)).unwrap();
                Command::Restore(f.task)
            },
            |f| vec![(f.task, &["deleted"])],
        ),
        (
            "promote",
            |f| Command::PromoteToStream {
                id: f.task,
                stream: f.stream,
            },
            |f| vec![(f.task, &["stream_id"])],
        ),
        (
            "rename a stream",
            |f| Command::UpdateStream {
                id: f.stream,
                patch: StreamPatch {
                    name: Some("Job".into()),
                    ..Default::default()
                },
            },
            |f| vec![(f.stream, &["name"])],
        ),
        (
            "delete a stream",
            |f| Command::DeleteStream {
                id: f.stream,
                disposition: sunrise_domain::Disposition::Detach,
            },
            |f| vec![(f.stream, &["deleted"])],
        ),
        (
            "rename a context",
            |f| Command::UpdateContext {
                id: f.ctx,
                patch: ContextPatch {
                    name: Some("chores".into()),
                    ..Default::default()
                },
            },
            |f| vec![(f.ctx, &["name"])],
        ),
        // A context delete writes the context alone: its tasks stop showing
        // it at read time, and keep it in their sets.
        (
            "delete a context",
            |f| Command::DeleteContext(f.ctx),
            |f| vec![(f.ctx, &["deleted"])],
        ),
        (
            "tune a routine",
            |f| Command::UpdateRoutine {
                scope: sunrise_domain::EditScope::All,
                id: f.rid,
                patch: sunrise_domain::RoutinePatch {
                    grace_window_s: Some(Some(600)),
                    ..Default::default()
                },
            },
            |f| vec![(f.rid, &["grace_window_s"])],
        ),
        (
            "skip an occurrence",
            |f| Command::SkipRoutineOccurrence {
                id: f.rid,
                occurrence_key: routine_of(&f.e, &f.db, f.rid)
                    .occurrence_key_of(&f.occ[1].0, ms_to_ts(f.occ[1].1))
                    .unwrap(),
            },
            |f| vec![(f.rid, &["skipped_keys"]), (f.occ[1].0, &["deleted"])],
        ),
        (
            "complete an occurrence on time",
            |f| {
                set_clock(&f.clock, (f.occ[0].1 + 3_600_000) as u64);
                Command::CompleteTask(f.occ[0].0)
            },
            |f| {
                vec![
                    (f.occ[0].0, &["completed_at", "state"]),
                    (f.rid, &["streak_keys"]),
                ]
            },
        ),
        (
            "retitle a block",
            |f| Command::UpdateBlock {
                id: f.block,
                patch: BlockPatch {
                    title: Some(Some("Deep work".into())),
                    ..Default::default()
                },
            },
            |f| vec![(f.block, &["title"])],
        ),
        (
            "bind",
            |f| Command::BindTask {
                block: f.block,
                task: f.task,
            },
            |f| vec![(f.block, &["tasks"])],
        ),
        (
            "unbind",
            |f| {
                f.e.apply(
                    &mut f.db,
                    Command::BindTask {
                        block: f.block,
                        task: f.task,
                    },
                )
                .unwrap();
                Command::UnbindTask {
                    block: f.block,
                    task: f.task,
                }
            },
            |f| vec![(f.block, &["tasks"])],
        ),
        (
            "detach",
            |f| Command::DetachFile(f.att),
            |f| vec![(f.att, &["deleted"])],
        ),
        (
            "move",
            |f| Command::MoveTasks {
                ids: vec![f.task],
                after: None,
                before: None,
            },
            |f| vec![(f.task, &["sort_order"])],
        ),
        (
            "resolve to the value it holds",
            |f| Command::ResolveConflict {
                entity: f.task,
                field: "title".into(),
                value: sunrise_domain::CborValue(Value::Text("t".into())),
            },
            |f| vec![(f.task, &["title"])],
        ),
    ];
    for (name, command, expected) in cases {
        let mut f = fixture();
        let cmd = command(&mut f);
        let since = mark(&f.db);
        f.e.apply(&mut f.db, cmd).unwrap();
        let written = entity_writes(written_since(&f.e, &f.db, since));
        assert!(
            written
                .iter()
                .all(|(k, _)| k.split_once('.').is_some_and(|(_, op)| op == "patch")),
            "{name}: {written:?}"
        );
        let got: BTreeMap<EntityRef, BTreeSet<String>> = patches(&written)
            .into_iter()
            .map(|p| (p.target, p.fields.keys().cloned().collect()))
            .collect();
        let want: BTreeMap<EntityRef, BTreeSet<String>> = expected(&f)
            .into_iter()
            .map(|(id, fields)| (id, fields.iter().map(|s| (*s).to_owned()).collect()))
            .collect();
        assert_eq!(got, want, "{name}");
        // A write of more than one entity is one group.
        let groups: BTreeSet<Option<([u8; 16], u64)>> = patches(&written)
            .iter()
            .map(|p| p.group.as_ref().map(|g| (g.id, g.size)))
            .collect();
        if want.len() > 1 {
            assert_eq!(groups.len(), 1, "{name}: one group");
            assert_eq!(
                groups.into_iter().next().flatten().map(|g| g.1),
                Some(want.len() as u64),
                "{name}"
            );
        } else {
            assert_eq!(groups, BTreeSet::from([None]), "{name}: no group");
        }
        f.e.assert_rebuild_identity(&f.db);
    }
}

/// The fields of the one `Patch` a command wrote, each with its field op.
fn only_patch(e: &Engine, db: &Db, since: i64) -> PatchPayload {
    let mut written = patches(&written_since(e, db, since));
    assert_eq!(written.len(), 1, "{written:?}");
    written.remove(0)
}

/// The value a `set` writes, in `p`'s field `name`.
fn set_of(p: &PatchPayload, name: &str) -> Value {
    let op = p
        .fields
        .get(name)
        .unwrap_or_else(|| panic!("{name} in {p:?}"));
    op.get()
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_text() == Some("set")))
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("{name} is not a set: {p:?}"))
}

/// Clearing a field writes the value its record reads the absent key as:
/// `null` for an optional field, the empty list for a list a serializer
/// leaves out empty, and `true` for a flag that defaults to it.
#[test]
fn clearing_a_field_writes_what_its_absence_reads_as() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_seeded(ROOT, [1; 32], clock);
    let mut db = db_root(ROOT);
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "t".into(),
                priority: Some(2),
                scheduling_constraints: vec![sample_constraint()],
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let since = mark(&db);
    e.apply(
        &mut db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                priority: Some(None),
                scheduling_constraints: Some(Vec::new()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let p = only_patch(&e, &db, since);
    assert_eq!(set_of(&p, "priority"), Value::Null);
    assert_eq!(
        set_of(&p, "scheduling_constraints"),
        Value::Array(Vec::new())
    );
    let t = read_task_t(&e, &db, task);
    assert_eq!((t.priority, t.scheduling_constraints.len()), (None, 0));

    let (rid, _) = seed_routine(&e, &mut db);
    let forgiveness = |on: bool| Command::UpdateRoutine {
        scope: sunrise_domain::EditScope::All,
        id: rid,
        patch: sunrise_domain::RoutinePatch {
            forgiveness_enabled: Some(on),
            ..Default::default()
        },
    };
    e.apply(&mut db, forgiveness(false)).unwrap();
    let since = mark(&db);
    e.apply(&mut db, forgiveness(true)).unwrap();
    let p = only_patch(&e, &db, since);
    assert_eq!(set_of(&p, "forgiveness_enabled"), Value::Bool(true));
    assert!(routine_of(&e, &db, rid).forgiveness_enabled);
    e.assert_rebuild_identity(&db);
}

/// A create writes the fields it sets to other than their defaults, and no
/// others, but for its stream, which it always writes (a build before
/// ADR-0046 has no default for a block's); a routine's generated occurrence
/// says it is generated, and a user's create does not.
#[test]
fn a_create_writes_only_the_fields_it_sets() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_seeded(ROOT, [1; 32], clock);
    let mut db = db_root(ROOT);
    let since = mark(&db);
    let task = new_task(&e, &mut db, "t");
    let p = only_patch(&e, &db, since);
    assert!(p.create && !p.is_generated());
    assert_eq!(p.fields.keys().collect::<Vec<_>>(), ["stream_id", "title"]);
    assert_eq!(read_task_t(&e, &db, task).title, "t");

    let since = mark(&db);
    e.apply(
        &mut db,
        Command::CreateStream(StreamDraft {
            name: "Work".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    new_context(&e, &mut db, "errands");
    e.apply(
        &mut db,
        Command::CreateBlock(block_draft(9, 1, Some("Desk"))),
    )
    .unwrap();
    e.apply(&mut db, Command::AttachFile(attachment_draft(task)))
        .unwrap();
    let (rid, occ) = seed_routine(&e, &mut db);
    let creates = patches(&written_since(&e, &db, since));
    let kinds: BTreeSet<EntityKind> = creates.iter().map(|p| p.target.kind()).collect();
    for k in [
        EntityKind::Stream,
        EntityKind::Context,
        EntityKind::Block,
        EntityKind::Attachment,
        EntityKind::Routine,
        EntityKind::Task,
    ] {
        assert!(kinds.contains(&k), "{k:?} created");
    }
    for p in &creates {
        assert!(p.create, "{p:?}");
        for default in ["deleted", "archived", "paused", "deferred_count"] {
            assert!(!p.fields.contains_key(default), "{default} in {p:?}");
        }
        let occurrence = p.fields.contains_key("routine_id");
        assert_eq!(p.is_generated(), occurrence, "{p:?}");
    }
    assert!(creates.iter().any(|p| p.target == rid));
    assert!(creates
        .iter()
        .any(|p| p.target == occ[0].0 && p.is_generated()));
}

/// An unknown key a newer build wrote is never written by a command here: the
/// command carries it back as it read it, so the newer build's value stands.
#[test]
fn an_edit_writes_no_key_a_newer_build_added() {
    let mut p = peers();
    let task = new_task(&p.engines[0], &mut p.dbs[0], "t");
    p.sync_all();
    let mut newer = read_task_t(&p.engines[1], &p.dbs[1], task);
    newer.unknown.insert(
        "x_newer".into(),
        sunrise_domain::CborValue(Value::Text("kept".into())),
    );
    let (env, _) = super::field_merge::emit_patch(
        &p.engines[1],
        &mut p.dbs[1],
        &sunrise_domain::INBOX_STREAM_BYTES,
        &InnerOp::TaskUpdate(newer),
    );
    p.engines[0].apply_remote(&mut p.dbs[0], &env).unwrap();
    let since = mark(&p.dbs[0]);
    p.apply(0, retitle(task, "renamed"));
    let written = only_patch(&p.engines[0], &p.dbs[0], since);
    assert_eq!(written.fields.keys().collect::<Vec<_>>(), ["title"]);
    let t = read_task_t(&p.engines[0], &p.dbs[0], task);
    assert_eq!(t.title, "renamed");
    assert_eq!(
        t.unknown.get("x_newer").map(|v| v.get().clone()),
        Some(Value::Text("kept".into()))
    );
}

/// A preferences `Patch` names its heads in a field-ops vault, and in a
/// vault still in legacy mode names none, as before.
#[test]
fn a_preferences_patch_names_its_heads_only_in_a_field_ops_vault() {
    let set_pref = |e: &Engine, db: &mut Db, n: u64| {
        e.apply(
            db,
            Command::SetPreference {
                key: "stale_after_days".into(),
                value: sunrise_domain::PrefValue::Uint(n),
                target: sunrise_domain::PrefTarget::Vault,
            },
        )
        .unwrap();
    };
    let (e, mut db) = (engine(), db());
    new_task(&e, &mut db, "makes the vault require core.field_ops");
    set_pref(&e, &mut db, 7);
    let since = mark(&db);
    set_pref(&e, &mut db, 8);
    let p = only_patch(&e, &db, since);
    assert_eq!(p.parents.as_ref().map(Vec::len), Some(1), "{p:?}");

    let (e, mut db) = (engine(), super::testutil::db());
    legacy_peer(&db);
    e.require_features(&mut db, &["preferences.entity"], true)
        .unwrap();
    set_pref(&e, &mut db, 7);
    let since = mark(&db);
    set_pref(&e, &mut db, 8);
    assert_eq!(only_patch(&e, &db, since).parents, None);
}

/// A defer writes its plan even onto the plan the task holds: a defer is a
/// statement of the plan, and two concurrent ones are a conflict of it.
#[test]
fn a_defer_onto_the_plan_it_holds_still_writes_the_plan() {
    let (e, mut db) = (engine(), db());
    let task = new_task(&e, &mut db, "t");
    let to: SunriseTime = ms_to_ts(NOW + DAY_MS).into();
    let defer = |db: &mut Db| {
        e.apply(
            db,
            Command::DeferTaskTo {
                id: task,
                to: to.clone(),
            },
        )
        .unwrap();
    };
    defer(&mut db);
    let since = mark(&db);
    defer(&mut db);
    let written = patches(&written_since(&e, &db, since));
    assert_eq!(written.len(), 1);
    let fields: Vec<&String> = written[0].fields.keys().collect();
    assert_eq!(fields, ["deferred_count", "scheduled_at"]);
    assert_eq!(read_task_t(&e, &db, task).deferred_count, 2);
}

/// A command that changes no field writes no op, and says so with an all-zero
/// op id.
#[test]
fn a_write_that_changes_nothing_writes_nothing() {
    let (e, mut db) = (engine(), db());
    let task = new_task(&e, &mut db, "t");
    let before = op_count(&db);
    let r = e.apply(&mut db, retitle(task, "t")).unwrap();
    assert_eq!((r.op_id, r.seq), ([0u8; 16], 0));
    assert_eq!(op_count(&db), before);
    // An unchanged re-import of a calendar event is nothing new either.
    let id = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let before = op_count(&db);
    e.apply(
        &mut db,
        import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
    )
    .unwrap();
    assert_eq!(op_count(&db), before);
    assert!(read_block(db.conn(), id.bytes()).unwrap().is_some());
}

/// A removed context names the add-tags this replica observed, and so
/// removes it; one added concurrently on another device survives (ADR-0044
/// §3).
#[test]
fn a_context_removal_takes_only_what_its_writer_observed() {
    let mut p = peers();
    let ctx = new_context(&p.engines[0], &mut p.dbs[0], "errands");
    let task = p
        .apply(
            0,
            Command::CreateTask(TaskDraft {
                title: "t".into(),
                contexts: vec![ctx],
                ..Default::default()
            }),
        )
        .entity;
    p.sync_all();
    let contexts = |cs: Vec<EntityRef>| Command::UpdateTask {
        id: task,
        patch: TaskPatch {
            contexts: Some(cs),
            ..Default::default()
        },
    };
    // A takes the context off; B, concurrently, re-adds it beside nothing.
    p.apply(0, contexts(vec![]));
    let since = mark(&p.dbs[0]);
    let removal = patches(&written_since(&p.engines[0], &p.dbs[0], since - 1));
    assert_eq!(removal.len(), 1);
    let op = removal[0].fields["contexts"].get();
    assert!(
        op.as_map()
            .is_some_and(|m| m.iter().any(|(k, _)| k.as_text() == Some("remove"))),
        "{op:?}"
    );
    let calls = new_context(&p.engines[1], &mut p.dbs[1], "calls");
    p.apply(1, contexts(vec![ctx, calls]));
    p.sync_all();
    for (e, db) in p.engines.iter().zip(&p.dbs) {
        assert_eq!(
            read_task_t(e, db, task).contexts,
            BTreeSet::from([calls]),
            "A removed the add it saw; B's add of another context survives"
        );
        e.assert_rebuild_identity(db);
    }
}

// ---- concurrency between current devices ----

/// Two current devices edit different fields of one task while apart: both
/// edits survive, and nothing is in conflict (#447).
#[test]
fn concurrent_edits_to_different_fields_survive_between_current_devices() {
    let mut p = peers();
    let ctx = new_context(&p.engines[1], &mut p.dbs[1], "errands");
    let task = new_task(&p.engines[0], &mut p.dbs[0], "t");
    p.sync_all();
    set_clock(&p.clocks[0], T0 + 1_000);
    p.apply(
        0,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                priority: Some(Some(1)),
                ..Default::default()
            },
        },
    );
    set_clock(&p.clocks[1], T0 + 2_000);
    p.apply(1, retitle(task, "renamed"));
    p.apply(
        1,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                contexts: Some(vec![ctx]),
                ..Default::default()
            },
        },
    );
    p.sync_all();
    let (a, b) = (
        read_task_t(&p.engines[0], &p.dbs[0], task),
        read_task_t(&p.engines[1], &p.dbs[1], task),
    );
    assert_eq!(a, b);
    assert_eq!(a.priority, Some(1), "A's edit survived");
    assert_eq!(a.title, "renamed", "B's edit survived");
    assert_eq!(a.contexts, BTreeSet::from([ctx]));
    assert!(conflicts(&p.engines[0], &p.dbs[0]).is_empty());
    for (e, db) in p.engines.iter().zip(&p.dbs) {
        e.assert_rebuild_identity(db);
    }
}

/// `ResolveConflict` and `Restore` each write one `Patch` of their field
/// naming every head of the entity, which clears the set on every replica
/// that applies it (§4; clarification D11 is the legacy mode's form).
#[test]
fn resolve_and_restore_write_one_field_naming_every_head() {
    let mut p = peers();
    let task = new_task(&p.engines[0], &mut p.dbs[0], "t");
    p.sync_all();
    set_clock(&p.clocks[0], T0 + 1_000);
    p.apply(0, retitle(task, "a"));
    set_clock(&p.clocks[1], T0 + 2_000);
    p.apply(1, retitle(task, "b"));
    p.sync_all();
    assert_eq!(conflicts(&p.engines[0], &p.dbs[0]).len(), 1);
    let tx = p.dbs[0].conn().unchecked_transaction().unwrap();
    let heads = crate::engine::merge::parents_of(&tx, task.bytes()).unwrap();
    drop(tx);
    assert_eq!(heads.len(), 2, "two branches");

    let since = mark(&p.dbs[0]);
    p.apply(
        0,
        Command::ResolveConflict {
            entity: task,
            field: "title".into(),
            value: sunrise_domain::CborValue(Value::Text("a".into())),
        },
    );
    let written = patches(&written_since(&p.engines[0], &p.dbs[0], since));
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].fields.keys().collect::<Vec<_>>(), ["title"]);
    assert_eq!(written[0].parents.as_deref(), Some(heads.as_slice()));
    p.sync_all();
    for (e, db) in p.engines.iter().zip(&p.dbs) {
        assert!(conflicts(e, db).is_empty());
        assert_eq!(read_task_t(e, db, task).title, "a");
    }

    // A delete against a concurrent edit, then a restore.
    set_clock(&p.clocks[0], T0 + 3_000);
    p.apply(0, Command::DeleteTask(task));
    set_clock(&p.clocks[1], T0 + 4_000);
    p.apply(1, retitle(task, "edited while deleted"));
    p.sync_all();
    assert!(
        read_task_t(&p.engines[1], &p.dbs[1], task).deleted,
        "delete wins"
    );
    let c = conflicts(&p.engines[1], &p.dbs[1]);
    assert!(c
        .iter()
        .any(|c| c.field == "deleted" && !c.edited_while_deleted.is_empty()));
    let since = mark(&p.dbs[1]);
    p.apply(1, Command::Restore(task));
    let written = patches(&written_since(&p.engines[1], &p.dbs[1], since));
    assert_eq!(written[0].fields.keys().collect::<Vec<_>>(), ["deleted"]);
    assert_eq!(written[0].parents.as_ref().map(Vec::len), Some(2));
    p.sync_all();
    for (e, db) in p.engines.iter().zip(&p.dbs) {
        assert!(conflicts(e, db).is_empty());
        let t = read_task_t(e, db, task);
        assert!(!t.deleted);
        assert_eq!(t.title, "edited while deleted", "the edit shows again");
        e.assert_rebuild_identity(db);
    }
}

// ---- groups ----

/// The tasks a group's members name, by group, as the receiver holds them.
fn groups_of(written: &[PatchPayload]) -> BTreeMap<[u8; 16], Vec<EntityRef>> {
    let mut out: BTreeMap<[u8; 16], Vec<EntityRef>> = BTreeMap::new();
    for p in written {
        if let Some(g) = &p.group {
            out.entry(g.id).or_default().push(p.target);
        }
    }
    out
}

/// A routine re-key tombstones its untouched future occurrences in the
/// routine op's group, each as a generated write, so no replica shows the new
/// rule beside the old occurrences, and none of them beats a user's edit.
#[test]
fn a_re_key_and_its_tombstones_are_one_group_of_generated_writes() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_seeded(ROOT, [1; 32], clock);
    let mut db = db_root(ROOT);
    let (rid, _) = seed_routine(&e, &mut db);
    let since = mark(&db);
    e.apply(
        &mut db,
        Command::UpdateRoutine {
            scope: sunrise_domain::EditScope::All,
            id: rid,
            patch: sunrise_domain::RoutinePatch {
                rrule: Some(sunrise_domain::RRule::parse("FREQ=WEEKLY").unwrap()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let written = patches(&written_since(&e, &db, since));
    let groups = groups_of(&written);
    assert_eq!(groups.len(), 1, "{written:?}");
    let members = groups.values().next().unwrap();
    assert!(members.contains(&rid));
    let tombstones: Vec<&PatchPayload> = written
        .iter()
        .filter(|p| p.group.is_some() && p.target != rid)
        .collect();
    assert!(tombstones.len() >= 2, "{written:?}");
    for t in &tombstones {
        assert!(t.is_generated());
        assert_eq!(t.fields.keys().collect::<Vec<_>>(), ["deleted"]);
    }
    e.assert_rebuild_identity(&db);
}

/// A focus session that completes a routine occurrence writes the completion
/// and its streak key as one group.
#[test]
fn a_focus_completion_and_its_streak_key_are_one_group() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_seeded(ROOT, [1; 32], clock.clone());
    let mut db = db_root(ROOT);
    let (rid, occ) = seed_routine(&e, &mut db);
    set_clock(&clock, (occ[0].1 + 600_000) as u64);
    let session = open_session(&e, &mut db, occ[0].0);
    let since = mark(&db);
    end_session(&e, &mut db, session, true);
    let written = patches(&written_since(&e, &db, since));
    let groups = groups_of(&written);
    assert_eq!(groups.len(), 1, "{written:?}");
    let mut members = groups.into_values().next().unwrap();
    members.sort();
    let mut want = vec![occ[0].0, rid];
    want.sort();
    assert_eq!(members, want);
    assert_eq!(routine_of(&e, &db, rid).streak_counter, 1);
    e.assert_rebuild_identity(&db);
}

/// A group of `Patch`es carries no `op_group` mark, which is the legacy
/// mode's (clarification D13).
#[test]
fn a_field_ops_group_is_marked_by_its_group_key_alone() {
    let (e, mut db) = (engine(), db());
    let a = e
        .apply(&mut db, Command::CreateBlock(block_draft(9, 1, Some("x"))))
        .unwrap()
        .entity;
    let b = e
        .apply(&mut db, Command::CreateBlock(block_draft(10, 1, Some("x"))))
        .unwrap()
        .entity;
    let since = mark(&db);
    e.apply(&mut db, Command::MergeBlocks { a, b }).unwrap();
    let written = patches(&written_since(&e, &db, since));
    assert_eq!(written.len(), 3);
    for p in &written {
        assert_eq!(p.group.as_ref().map(|g| g.size), Some(3));
        assert!(!p.fields.contains_key(crate::inner_op::FULL_STATE_GROUP_KEY));
        assert!(!p
            .unknown
            .contains_key(crate::inner_op::FULL_STATE_GROUP_KEY));
    }
}

// ---- convergence ----

/// One step of the property.
#[derive(Debug, Clone)]
enum Step {
    Do { who: usize, action: Action },
    Sync { from: usize, to: usize },
}

/// One command on the shared task.
#[derive(Debug, Clone)]
enum Action {
    Retitle(u8),
    Priority(u8),
    Contexts(u8),
    Defer(u8),
    Complete,
    Reopen,
    Delete,
    Restore,
    Resolve(u8),
    /// Move the task to stream `n` of the two, or (2) to no stream
    /// (ADR-0046).
    Move(u8),
    /// Delete stream `n`, detaching what it held or (`true`) moving it into the
    /// other.
    DeleteStream(u8, bool),
    /// Bring stream `n` back.
    RestoreStream(u8),
    /// Triage the task out of the Inbox, or put it back.
    Triage(bool),
}

fn arb_action() -> impl Strategy<Value = Action> {
    prop_oneof![
        3 => (0u8..3).prop_map(Action::Retitle),
        2 => (0u8..3).prop_map(Action::Priority),
        2 => (0u8..4).prop_map(Action::Contexts),
        2 => (0u8..3).prop_map(Action::Defer),
        1 => Just(Action::Complete),
        1 => Just(Action::Reopen),
        1 => Just(Action::Delete),
        1 => Just(Action::Restore),
        1 => (0u8..3).prop_map(Action::Resolve),
        2 => (0u8..3).prop_map(Action::Move),
        1 => (0u8..2, any::<bool>()).prop_map(|(n, into)| Action::DeleteStream(n, into)),
        1 => (0u8..2).prop_map(Action::RestoreStream),
        1 => any::<bool>().prop_map(Action::Triage),
    ]
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0usize..3, arb_action()).prop_map(|(who, action)| Step::Do { who, action }),
        2 => (0usize..3, 0usize..3).prop_map(|(from, to)| Step::Sync { from, to }),
    ]
}

fn command_of(
    action: &Action,
    task: EntityRef,
    contexts: &[EntityRef],
    streams: &[EntityRef; 2],
) -> Command {
    let edit = |patch: TaskPatch| Command::UpdateTask { id: task, patch };
    match *action {
        Action::Retitle(v) => retitle(task, &format!("title {v}")),
        Action::Priority(v) => edit(TaskPatch {
            priority: Some(Some(v + 1)),
            ..Default::default()
        }),
        Action::Contexts(mask) => edit(TaskPatch {
            contexts: Some(
                contexts
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, c)| *c)
                    .collect(),
            ),
            ..Default::default()
        }),
        Action::Defer(d) => Command::DeferTaskTo {
            id: task,
            to: ms_to_ts(NOW + i64::from(d + 1) * DAY_MS).into(),
        },
        Action::Complete => Command::CompleteTask(task),
        Action::Reopen => edit(TaskPatch {
            state: Some(TaskState::Todo),
            ..Default::default()
        }),
        Action::Delete => Command::DeleteTask(task),
        Action::Restore => Command::Restore(task),
        Action::Resolve(v) => Command::ResolveConflict {
            entity: task,
            field: "title".into(),
            value: sunrise_domain::CborValue(Value::Text(format!("title {v}"))),
        },
        Action::Move(n) => edit(TaskPatch {
            stream_id: Some(streams.get(usize::from(n)).copied()),
            ..Default::default()
        }),
        Action::DeleteStream(n, into) => {
            let n = usize::from(n);
            Command::DeleteStream {
                id: streams[n],
                disposition: if into {
                    sunrise_domain::Disposition::MoveTo(streams[1 - n])
                } else {
                    sunrise_domain::Disposition::Detach
                },
            }
        }
        Action::RestoreStream(n) => Command::Restore(streams[usize::from(n)]),
        Action::Triage(true) => Command::MarkTriaged { ids: vec![task] },
        Action::Triage(false) => Command::ReturnToInbox { ids: vec![task] },
    }
}

/// What a replica holds: every projection table, merge state and versions
/// included, and the conflicts, quiet ones included.
type Held = (
    Vec<(&'static str, Vec<String>)>,
    Vec<crate::conflicts::EntityConflict>,
);

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(24),
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/field_ops.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Three current devices run commands on one task and catch up on each
    /// other at random, so their `Patch`es name the heads each had. Every
    /// receiver of the same ops, in any order and with any repeats, holds
    /// byte for byte the same rows, registers, losers, versions and
    /// conflicts, and each rebuilds to exactly that; so does every writer
    /// once it has caught up.
    #[test]
    fn commands_converge_in_any_delivery_order(
        steps in proptest::collection::vec(arb_step(), 1..18),
        order in proptest::collection::vec(any::<prop::sample::Index>(), 0..48),
    ) {
        let mut p = peers_with(&[
            crate::feature::FEATURES,
            crate::feature::FEATURES,
            crate::feature::FEATURES,
        ]);
        let contexts: Vec<EntityRef> = ["a", "b"]
            .iter()
            .map(|n| new_context(&p.engines[0], &mut p.dbs[0], n))
            .collect();
        let streams = ["s0", "s1"].map(|n| {
            p.engines[0]
                .apply(
                    &mut p.dbs[0],
                    Command::CreateStream(StreamDraft {
                        name: n.into(),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .entity
        });
        let task = new_task(&p.engines[0], &mut p.dbs[0], "t");
        p.sync_all();
        for (n, step) in steps.iter().enumerate() {
            match step {
                Step::Do { who, action } => {
                    set_clock(&p.clocks[*who], T0 + 1_000 + u64::try_from(n).unwrap() * 7);
                    // A refusal (a restore of a live task, a transition the
                    // state does not allow) writes nothing, as it would. A
                    // storage or internal error is no refusal, and fails.
                    match p.engines[*who].apply(
                        &mut p.dbs[*who],
                        command_of(action, task, &contexts, &streams),
                    ) {
                        Ok(_)
                        | Err(
                            EngineError::Invalid(_)
                            | EngineError::NotFound(_)
                            | EngineError::Validation(_),
                        ) => {}
                        Err(other) => panic!("{action:?}: {other:?}"),
                    }
                }
                Step::Sync { from, to } if from != to => p.deliver(*from, *to),
                Step::Sync { .. } => {}
            }
        }
        let envelopes: Vec<Vec<u8>> = p
            .engines
            .iter()
            .zip(&p.dbs)
            .flat_map(|(e, db)| own_envelopes(e, db))
            .collect();
        let receive = |deliveries: &[usize]| -> Held {
            let e = engine_seeded(ROOT, [9u8; 32], Arc::new(FakeClock(PLMutex::new(T0 + 100_000))));
            let mut db = db_root(ROOT);
            for d in &p.engines {
                trust(&e, &mut db, d);
            }
            for i in deliveries {
                e.apply_remote(&mut db, &envelopes[*i]).unwrap();
            }
            for env in &envelopes {
                e.apply_remote(&mut db, env).unwrap();
            }
            e.assert_rebuild_identity(&db);
            (crate::engine::rebuild::check::dump(db.conn()), conflicts(&e, &db))
        };
        let forward: Vec<usize> = (0..envelopes.len()).collect();
        let shuffled: Vec<usize> = order.iter().map(|ix| ix.index(envelopes.len())).collect();
        let reversed: Vec<usize> = (0..envelopes.len()).rev().collect();
        let a = receive(&forward);
        prop_assert_eq!(&a, &receive(&shuffled));
        prop_assert_eq!(&a, &receive(&reversed));
        p.sync_all();
        let views: Vec<(Task, Vec<crate::conflicts::EntityConflict>)> = p
            .engines
            .iter()
            .zip(&p.dbs)
            .map(|(e, db)| {
                e.assert_rebuild_identity(db);
                (read_task_t(e, db, task), conflicts(e, db))
            })
            .collect();
        prop_assert_eq!(&views[0], &views[1]);
        prop_assert_eq!(&views[0], &views[2]);
    }
}
