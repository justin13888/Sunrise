//! ADR-0065 PR 7: the values `project` derives, the read-time cycle rules,
//! and the history readers that understand `Patch`.
//!
//! The pairs here advertise what they support to each other, as a fresh
//! `Core::open` does, so their vault requires `core.field_ops` at its first
//! write and every command is written as a `Patch` (ADR-0065 §7).

use super::field_merge::{add, emit_patch, set, text};
use super::*;
use crate::conflicts::ConflictClass;
use crate::engine::task::actionable_scan;
use ciborium::value::Value;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::{ActivityKind, StreamPatch};

const ROOT: [u8; 32] = [0x47; 32];

/// Two trusting replicas that have told each other what they support.
struct Peers {
    ea: Engine,
    eb: Engine,
    ca: Arc<FakeClock>,
    cb: Arc<FakeClock>,
    dba: Db,
    dbb: Db,
}

fn peers(advertise: bool) -> Peers {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    if advertise {
        assert!(ea.advertise_features(&mut dba).unwrap());
        assert!(eb.advertise_features(&mut dbb).unwrap());
    }
    let mut p = Peers {
        ea,
        eb,
        ca,
        cb,
        dba,
        dbb,
    };
    sync(&mut p);
    p
}

/// Every op each side logged under its own device id, delivered to the
/// other in seq order. Re-delivery is a no-op.
fn sync(p: &mut Peers) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba);
}

fn deliver(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
    let device = from.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY stream_id, seq")
            .unwrap();
        stmt.query_map(params![&device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        to.apply_remote(to_db, &env).unwrap();
    }
}

fn kinds_logged(db: &Db, target: EntityRef) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT inner_kind FROM ops WHERE target_id = ? ORDER BY rowid")
        .unwrap();
    stmt.query_map(params![&target.bytes()[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn stored_routine(db: &Db, rid: EntityRef) -> Routine {
    crate::engine::routine::read_routine(db.conn(), rid.bytes())
        .unwrap()
        .expect("projected")
}

fn task_of(db: &Db, id: EntityRef) -> Task {
    read_task(db.conn(), id.bytes())
        .unwrap()
        .expect("projected")
}

fn conflicts(e: &Engine, db: &Db) -> Vec<crate::conflicts::EntityConflict> {
    match e
        .query(
            db,
            Query::Conflicts {
                entity: None,
                include_quiet: false,
            },
        )
        .unwrap()
    {
        QueryResult::Conflicts(c) => c,
        other => panic!("{other:?}"),
    }
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

/// The titles of the tasks a search for `q` finds, sorted.
fn search_titles(e: &Engine, db: &Db, q: &str) -> Vec<String> {
    let QueryResult::Search(r) = e
        .query(
            db,
            Query::SearchGrouped {
                text: q.into(),
                now_ms: T0,
                prefix: false,
                per_group: None,
            },
        )
        .unwrap()
    else {
        panic!("search shape")
    };
    let mut titles: Vec<String> = r
        .groups
        .iter()
        .filter(|g| g.kind == sunrise_domain::search::SearchKind::Task)
        .flat_map(|g| g.hits.iter().map(|h| h.title.text.clone()))
        .collect();
    titles.sort();
    titles
}

fn defer(e: &Engine, db: &mut Db, id: EntityRef, to_ms: i64) {
    e.apply(
        db,
        Command::DeferTaskTo {
            id,
            to: ms_to_ts(to_ms).into(),
        },
    )
    .unwrap();
}

// ---- #334: defers ----

/// Two devices defer one task without seeing each other's defer: the count
/// keeps both, and the two plans are a tracked conflict.
#[test]
fn concurrent_defers_count_twice() {
    let mut p = peers(true);
    let task = new_task(&p.ea, &mut p.dba, "renew passport");
    sync(&mut p);
    set_clock(&p.ca, T0 + 1_000);
    defer(&p.ea, &mut p.dba, task, NOW + 2 * DAY_MS);
    set_clock(&p.cb, T0 + 2_000);
    defer(&p.eb, &mut p.dbb, task, NOW + 3 * DAY_MS);
    assert!(kinds_logged(&p.dba, task).ends_with(&["task.patch".to_owned()]));
    sync(&mut p);

    let (a, b) = (task_of(&p.dba, task), task_of(&p.dbb, task));
    assert_eq!(a, b);
    assert_eq!(a.deferred_count, 2, "both defers count (#334)");
    let c = conflicts(&p.ea, &p.dba);
    assert!(
        c.iter()
            .any(|c| c.entity == task && c.field == "scheduled_at" && c.losers.len() == 1),
        "{c:?}"
    );
    assert_eq!(c, conflicts(&p.eb, &p.dbb));
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);

    // A third defer that saw both counts on top and settles the plan.
    defer(&p.ea, &mut p.dba, task, NOW + 4 * DAY_MS);
    sync(&mut p);
    assert_eq!(task_of(&p.dbb, task).deferred_count, 3);
    assert!(conflicts(&p.eb, &p.dbb)
        .iter()
        .all(|c| c.field != "scheduled_at"));
}

/// Until every paired device has said it folds field ops, a defer is the
/// full-state op every build reads (clarification D9). A vault with no peer
/// requires `core.field_ops` at its first write, and defers by `Patch`.
#[test]
fn a_defer_is_a_full_state_op_while_a_peer_lacks_field_ops() {
    let mut p = peers(false);
    let task = new_task(&p.ea, &mut p.dba, "t");
    defer(&p.ea, &mut p.dba, task, NOW + DAY_MS);
    assert_eq!(
        kinds_logged(&p.dba, task).last().map(String::as_str),
        Some("task.update")
    );

    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let (solo, mut db) = (engine_clocked(clock), db());
    let task = new_task(&solo, &mut db, "t");
    defer(&solo, &mut db, task, NOW + DAY_MS);
    assert_eq!(kinds_logged(&db, task), ["task.patch", "task.patch"]);
    assert_eq!(task_of(&db, task).deferred_count, 1);
}

// ---- #331: streaks ----

/// Two devices complete two different occurrences of one routine without
/// seeing each other's completion: both keys survive, and both count.
#[test]
fn concurrent_completions_of_different_occurrences_both_count() {
    let mut p = peers(true);
    let (rid, occ) = seed_routine(&p.ea, &mut p.dba);
    sync(&mut p);
    set_clock(&p.ca, (occ[0].1 + 3_600_000) as u64);
    p.ea.apply(&mut p.dba, Command::CompleteTask(occ[0].0))
        .unwrap();
    set_clock(&p.cb, (occ[1].1 + 3_600_000) as u64);
    p.eb.apply(&mut p.dbb, Command::CompleteTask(occ[1].0))
        .unwrap();
    assert!(kinds_logged(&p.dbb, rid).ends_with(&["routine.patch".to_owned()]));
    set_clock(&p.ca, (occ[1].1 + 3_600_000) as u64);
    sync(&mut p);

    let (a, b) = (stored_routine(&p.dba, rid), stored_routine(&p.dbb, rid));
    assert_eq!(a, b);
    assert_eq!(a.streak_keys.len(), 2);
    assert_eq!(a.streak_counter, 2, "both completions count (#331)");
    assert_eq!(a.forgivenesses_in_window, 0);
    assert_eq!(
        a.last_completed_at,
        Some(SunriseTime::instant(ms_to_ts(occ[1].1)))
    );
    p.ea.assert_rebuild_identity(&p.dba);
    p.eb.assert_rebuild_identity(&p.dbb);
}

/// Between peers that fold field ops, a completion past its grace window
/// adds no key: it writes no routine op, and every read counts it as the miss
/// it is, as the completion rule did (D17).
#[test]
fn a_late_completion_between_peers_adds_no_key_and_reads_as_a_miss() {
    let mut p = peers(true);
    let (rid, occ) = seed_routine(&p.ea, &mut p.dba);
    set_clock(&p.ca, (occ[0].1 + 3_600_000) as u64);
    p.ea.apply(&mut p.dba, Command::CompleteTask(occ[0].0))
        .unwrap();
    let ops_before = kinds_logged(&p.dba, rid).len();
    // 25 hours late.
    set_clock(&p.ca, (occ[1].1 + 25 * 3_600_000) as u64);
    p.ea.apply(&mut p.dba, Command::CompleteTask(occ[1].0))
        .unwrap();
    assert_eq!(kinds_logged(&p.dba, rid).len(), ops_before, "no key, no op");
    let stored = stored_routine(&p.dba, rid);
    assert_eq!((stored.streak_keys.len(), stored.streak_counter), (1, 1));
    // Read now, the late occurrence is a miss the allowance absorbs.
    let read = routine_of(&p.ea, &p.dba, rid);
    assert_eq!((read.streak_counter, read.forgivenesses_in_window), (1, 1));
    // The next on-time completion counts on top of it.
    set_clock(&p.ca, (occ[2].1 + 3_600_000) as u64);
    p.ea.apply(&mut p.dba, Command::CompleteTask(occ[2].0))
        .unwrap();
    let stored = stored_routine(&p.dba, rid);
    assert_eq!(
        (stored.streak_counter, stored.forgivenesses_in_window),
        (2, 1)
    );
    p.ea.assert_rebuild_identity(&p.dba);
}

/// The streak a full-state op carried is what a routine shows while only
/// full-state ops wrote its keys, even where the keys alone would add up to
/// another streak. Once a `Patch` writes a key, the keys are what count.
#[test]
fn a_legacy_streak_is_kept_until_a_patch_writes_the_keys() {
    let clock = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let e = engine_clocked(clock.clone());
    let mut db = db();
    legacy_peer(&db);
    let (rid, occ) = seed_routine(&e, &mut db);
    set_clock(&clock, (occ[0].1 + 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[0].0)).unwrap();
    // 25 hours late: the completion rule forgives it rather than counting it.
    set_clock(&clock, (occ[1].1 + 25 * 3_600_000) as u64);
    e.apply(&mut db, Command::CompleteTask(occ[1].0)).unwrap();
    let legacy = stored_routine(&db, rid);
    assert_eq!(
        (legacy.streak_counter, legacy.forgivenesses_in_window),
        (1, 1),
        "the registers the full-state op carried"
    );
    assert_ne!(
        legacy.derived_streak().streak_counter,
        legacy.streak_counter,
        "the keys alone would say otherwise"
    );
    e.assert_rebuild_identity(&db);

    // A field-level write of a third key.
    set_clock(&clock, (occ[2].1 + 3_600_000) as u64);
    let key = legacy
        .occurrence_key_of(&occ[2].0, ms_to_ts(occ[2].1))
        .expect("the occurrence's key");
    emit_patch(
        &e,
        &mut db,
        &META_STREAM,
        &super::field_merge::patch(rid, false, vec![("streak_keys", add(vec![text(&key)]))]),
    );
    let derived = stored_routine(&db, rid);
    assert_eq!(derived.streak_keys.len(), 3);
    assert_eq!(
        (derived.streak_counter, derived.forgivenesses_in_window),
        (3, 0),
        "derived from the three keys"
    );
    assert_eq!(
        derived.streak_started_at,
        Some(ms_to_ts(occ[0].1)),
        "the run starts at its first occurrence"
    );
    e.assert_rebuild_identity(&db);
}

// ---- #333: cycles ----

/// `t1` waits on `t2` on one device while `t2` waits on `t1` on the other.
/// Both pass the local check and merge into a cycle; every replica reads
/// the newer edge as absent, so `t2` is actionable, and reports it.
#[test]
fn a_concurrent_dependency_cycle_suppresses_the_newer_edge() {
    let mut p = peers(false);
    let t1 = new_task(&p.ea, &mut p.dba, "t1");
    let t2 = new_task(&p.ea, &mut p.dba, "t2");
    sync(&mut p);
    let blocked_by = |b: EntityRef| TaskPatch {
        blocked_by: Some(vec![b]),
        ..Default::default()
    };
    set_clock(&p.ca, T0 + 1_000);
    p.ea.apply(
        &mut p.dba,
        Command::UpdateTask {
            id: t1,
            patch: blocked_by(t2),
        },
    )
    .unwrap();
    set_clock(&p.cb, T0 + 2_000);
    p.eb.apply(
        &mut p.dbb,
        Command::UpdateTask {
            id: t2,
            patch: blocked_by(t1),
        },
    )
    .unwrap();
    sync(&mut p);

    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        // The rows keep both edges: the rule is read-time only.
        assert_eq!(task_of(db, t2).blocked_by, BTreeSet::from([t1]));
        let scan: BTreeMap<EntityRef, u32> = actionable_scan(db.conn(), None, 100, false)
            .unwrap()
            .into_iter()
            .map(|(id, open, _)| (EntityRef::new(EntityKind::Task, id), open))
            .collect();
        assert_eq!(scan.get(&t1), Some(&1), "t1 still waits on t2");
        assert_eq!(scan.get(&t2), Some(&0), "the newer edge t2→t1 is set aside");
        let c: Vec<_> = conflicts(e, db)
            .into_iter()
            .filter(|c| c.class == ConflictClass::DependencyCycle)
            .collect();
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!((c[0].entity, c[0].field.as_str()), (t2, "blocked_by"));
        assert_eq!(c[0].cycle, vec![t2, t1, t2]);
        assert_eq!(
            c[0].winner.value.as_ref().map(|v| v.get()),
            Some(&Value::serialized(&t1).unwrap())
        );
        // Search reads the same graph: the set-aside edge neither blocks t2
        // nor is a dependency of it.
        assert_eq!(search_titles(e, db, "is:blocked"), ["t1"]);
        assert_eq!(search_titles(e, db, "has:dependency"), ["t1"]);
        // The local check reads the same graph: t2 may now wait on t1's
        // blocker without being told it closes a loop it does not.
        e.assert_rebuild_identity(db);
    }
    assert_eq!(conflicts(&p.ea, &p.dba), conflicts(&p.eb, &p.dbb));
    // Asking for t1's conflicts finds the loop it is a member of.
    let QueryResult::Conflicts(of_t1) =
        p.ea.query(
            &p.dba,
            Query::Conflicts {
                entity: Some(t1),
                include_quiet: false,
            },
        )
        .unwrap()
    else {
        panic!("conflicts shape")
    };
    assert_eq!(of_t1.len(), 1);
    assert_eq!(of_t1[0].entity, t2);

    // Dropping the kept edge ends the cycle, and the other edge applies.
    p.ea.apply(
        &mut p.dba,
        Command::UpdateTask {
            id: t1,
            patch: TaskPatch {
                blocked_by: Some(Vec::new()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    sync(&mut p);
    let scan = actionable_scan(p.dbb.conn(), None, 100, false).unwrap();
    assert!(scan
        .iter()
        .any(|(id, open, _)| id == t2.bytes() && *open == 1));
    assert!(conflicts(&p.eb, &p.dbb)
        .iter()
        .all(|c| c.class != ConflictClass::DependencyCycle));
}

/// Two devices nest each of two streams under the other. The stream whose
/// move is newer reads as a root on every replica, and is reported.
#[test]
fn a_concurrent_stream_nesting_loop_roots_the_newer_move() {
    let mut p = peers(false);
    let stream = |e: &Engine, db: &mut Db, name: &str| {
        e.apply(
            db,
            Command::CreateStream(sunrise_domain::StreamDraft {
                name: name.into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity
    };
    let s1 = stream(&p.ea, &mut p.dba, "Work");
    let s2 = stream(&p.ea, &mut p.dba, "Acme");
    sync(&mut p);
    let nest = |parent: EntityRef| StreamPatch {
        parent_id: Some(Some(parent)),
        ..Default::default()
    };
    set_clock(&p.ca, T0 + 1_000);
    p.ea.apply(
        &mut p.dba,
        Command::UpdateStream {
            id: s1,
            patch: nest(s2),
        },
    )
    .unwrap();
    set_clock(&p.cb, T0 + 2_000);
    p.eb.apply(
        &mut p.dbb,
        Command::UpdateStream {
            id: s2,
            patch: nest(s1),
        },
    )
    .unwrap();
    sync(&mut p);

    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        let read = |s: EntityRef| {
            crate::engine::stream::read_stream(db.conn(), s.bytes())
                .unwrap()
                .unwrap()
                .parent_id
        };
        assert_eq!(read(s1), Some(s2));
        assert_eq!(read(s2), None, "the newer move reads as a root");
        let c: Vec<_> = conflicts(e, db)
            .into_iter()
            .filter(|c| c.class == ConflictClass::DependencyCycle)
            .collect();
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!((c[0].entity, c[0].field.as_str()), (s2, "parent_id"));
        assert_eq!(c[0].cycle, vec![s2, s1, s2]);
    }
}

// ---- history readers ----

fn completion_patch(task: EntityRef, at_ms: i64) -> InnerOp {
    super::field_merge::patch(
        task,
        false,
        vec![
            ("state", set(text("done"))),
            (
                "completed_at",
                set(Value::serialized(&Some(SunriseTime::instant(ms_to_ts(at_ms)))).unwrap()),
            ),
        ],
    )
}

/// A completion written as a `Patch`, and a defer, show in the task's feed as
/// the full-state ops they replace would.
#[test]
fn the_activity_feed_reads_patches() {
    let mut p = peers(true);
    let task = new_task(&p.ea, &mut p.dba, "write it");
    set_clock(&p.ca, T0 + 60_000);
    defer(&p.ea, &mut p.dba, task, NOW + DAY_MS);
    set_clock(&p.ca, T0 + 120_000);
    emit_patch(
        &p.ea,
        &mut p.dba,
        &sunrise_domain::INBOX_STREAM_BYTES,
        &completion_patch(task, NOW + 120_000),
    );
    assert_eq!(
        kinds_logged(&p.dba, task),
        ["task.patch", "task.patch", "task.patch"]
    );
    sync(&mut p);
    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        let QueryResult::Activity(events) = e
            .query(
                db,
                Query::ActivityTimeline {
                    entity: task,
                    limit: 10,
                },
            )
            .unwrap()
        else {
            panic!("activity shape")
        };
        let kinds: Vec<ActivityKind> = events.iter().map(|e| e.kind.clone()).collect();
        assert!(kinds.contains(&ActivityKind::TaskCreated), "{kinds:?}");
        assert!(
            kinds.contains(&ActivityKind::TaskDeferred { count: 1 }),
            "{kinds:?}"
        );
        assert!(kinds.contains(&ActivityKind::TaskCompleted), "{kinds:?}");
    }
}

/// A week's trend counts a completion and a defer written as `Patch`es.
#[test]
fn trends_count_completions_written_as_patches() {
    let mut p = peers(true);
    let a = new_task(&p.ea, &mut p.dba, "a");
    let b = new_task(&p.ea, &mut p.dba, "b");
    set_clock(&p.ca, T0 + 60_000);
    defer(&p.ea, &mut p.dba, b, NOW + DAY_MS);
    emit_patch(
        &p.ea,
        &mut p.dba,
        &sunrise_domain::INBOX_STREAM_BYTES,
        &completion_patch(a, NOW + 60_000),
    );
    sync(&mut p);
    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        let QueryResult::Trends(t) = e
            .query(
                db,
                Query::StreamTrends {
                    weeks: 1,
                    now_ms: (NOW + 120_000) as u64,
                },
            )
            .unwrap()
        else {
            panic!("trends shape")
        };
        let week = t.overall.last().expect("one week");
        assert_eq!((week.completed, week.deferred, week.created), (1, 1, 2));
    }
}

/// A stream's own `Patch` in its feed is never read as a task.
#[test]
fn a_stream_patch_in_a_stream_feed_is_not_a_task() {
    let mut p = peers(false);
    let stream =
        p.ea.apply(
            &mut p.dba,
            Command::CreateStream(sunrise_domain::StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    emit_patch(
        &p.ea,
        &mut p.dba,
        &META_STREAM,
        &super::field_merge::patch(stream, true, vec![("name", set(text("Acme")))]),
    );
    let QueryResult::Activity(events) =
        p.ea.query(
            &p.dba,
            Query::ActivityTimeline {
                entity: stream,
                limit: 10,
            },
        )
        .unwrap()
    else {
        panic!("activity shape")
    };
    assert!(
        events.iter().all(|e| e.kind != ActivityKind::TaskCreated),
        "{events:?}"
    );
}

// ---- the replay, field by field ----

fn replayed(base: Option<Task>, p: &InnerOp) -> (sunrise_domain::OpPayload, Option<Task>) {
    let InnerOp::Patch(p) = p else {
        panic!("not a patch")
    };
    crate::engine::review::replay_task_patch(base, p, (NOW + 5_000) as u64)
}

fn base_task(contexts: &[EntityRef], deferred: i64) -> Task {
    let mut t: Task = Value::Map(vec![
        (
            text("id"),
            Value::serialized(&EntityRef::new(EntityKind::Task, [7; 16])).unwrap(),
        ),
        (
            text("created_at"),
            Value::serialized(&ms_to_ts(NOW)).unwrap(),
        ),
        (
            text("updated_at"),
            Value::serialized(&ms_to_ts(NOW)).unwrap(),
        ),
        (text("title"), text("base")),
        (text("state"), text("todo")),
        (
            text("stream_id"),
            Value::serialized(&sunrise_domain::inbox_stream_ref()).unwrap(),
        ),
    ])
    .deserialized()
    .unwrap();
    t.contexts = contexts.iter().copied().collect();
    t.deferred_count = deferred;
    t
}

/// Every field op a task `Patch` carries is replayed onto the task before
/// it, and the effect it reads as is the one its fields say.
#[test]
fn a_patch_replays_each_field_op_onto_the_task_before_it() {
    use super::field_merge::{patch, remove};
    use sunrise_domain::OpPayload;
    let id = EntityRef::new(EntityKind::Task, [7; 16]);
    let ctx = |b: u8| EntityRef::new(EntityKind::Context, [b; 16]);
    let eref = |r: EntityRef| Value::serialized(&r).unwrap();

    // A create with nothing before it starts from the defaults.
    let (created, after) = replayed(None, &patch(id, true, vec![("title", set(text("new")))]));
    assert!(matches!(&created, OpPayload::TaskCreated(t) if t.title == "new"));
    assert_eq!(after.map(|t| t.title), Some("new".to_owned()));
    // No create and nothing before it: nothing to replay onto.
    let (none, after) = replayed(None, &patch(id, false, vec![("title", set(text("x")))]));
    assert!(matches!(none, OpPayload::Ignored) && after.is_none());

    // Sets, an add that is new, an add already held, an inc.
    let before = base_task(&[ctx(1), ctx(2)], 3);
    let (updated, after) = replayed(
        Some(before.clone()),
        &patch(
            id,
            false,
            vec![
                ("title", set(text("renamed"))),
                ("contexts", add(vec![eref(ctx(3)), eref(ctx(1))])),
                (
                    "deferred_count",
                    super::field_merge::op("inc", Value::Integer(2.into())),
                ),
            ],
        ),
    );
    let t = match updated {
        OpPayload::TaskUpdated(t) => *t,
        other => panic!("{other:?}"),
    };
    assert_eq!(t.title, "renamed");
    assert_eq!(t.contexts, BTreeSet::from([ctx(1), ctx(2), ctx(3)]));
    assert_eq!(t.deferred_count, 5);
    assert_eq!(t.updated_at, ms_to_ts(NOW + 5_000));
    assert_eq!(after, Some(t));
    // A remove takes out only the element it names.
    let (removed, _) = replayed(
        Some(before.clone()),
        &patch(
            id,
            false,
            vec![(
                "contexts",
                remove(
                    eref(ctx(2)),
                    &[crate::engine::merge::OpRef {
                        stream: [1; 16],
                        device: [2; 16],
                        seq: 1,
                    }],
                ),
            )],
        ),
    );
    assert!(
        matches!(&removed, OpPayload::TaskUpdated(t) if t.contexts == BTreeSet::from([ctx(1)]))
    );

    // `deleted` set to true reads as a delete; set to false as an update.
    let (deleted, after) = replayed(
        Some(before.clone()),
        &patch(id, false, vec![("deleted", set(Value::Bool(true)))]),
    );
    assert!(matches!(deleted, OpPayload::TaskDeleted));
    assert_eq!(after.map(|t| t.deleted), Some(true));
    let (restored, _) = replayed(
        Some(before),
        &patch(id, false, vec![("deleted", set(Value::Bool(false)))]),
    );
    assert!(matches!(restored, OpPayload::TaskUpdated(_)));
}

/// A build without `core.field_ops` writes defers as full-state ops, even
/// when every paired device has the feature: it cannot require it.
#[test]
fn a_build_lacking_the_feature_never_writes_field_ops() {
    // Another feature, so the test is not about an empty list.
    let others = &crate::feature::FEATURES[..1];
    assert_ne!(others[0].id, crate::feature::FIELD_OPS_FEATURE);
    let mut p = peers(true);
    let lacking = engine_seeded(ROOT, [1u8; 32], p.ca.clone()).with_features(others);
    let task = new_task(&lacking, &mut p.dba, "t");
    defer(&lacking, &mut p.dba, task, NOW + DAY_MS);
    assert_eq!(
        kinds_logged(&p.dba, task).last().map(String::as_str),
        Some("task.update")
    );
}
