//! Client op-log compaction and stream snapshots (ADR-0059, issue #330).
//!
//! Real engines throughout: the writer seals through the op-log path, the
//! receiver applies through `apply_remote_all`, and a device's
//! acknowledgement is the stream digest it publishes, delivered like any
//! other op. A relay that re-sends, or a device that equivocates, is played
//! by the test choosing which envelopes to hand over.

use super::testutil::*;
use super::*;
use crate::engine::chain::digest_payload;
use crate::engine::compaction::floor_of;
use crate::engine::{CompactionPolicy, SnapshotApplied};
use crate::ChainIntegrity;
use sunrise_crypto::decode_envelope;
use sunrise_domain::INBOX_STREAM_BYTES;

const INBOX: [u8; 16] = INBOX_STREAM_BYTES;

const DAY: u64 = 24 * 60 * 60 * 1000;

/// A one-day retention, so a test reaches past it by moving the clock two.
fn policy() -> CompactionPolicy {
    CompactionPolicy {
        retention_ms: DAY,
        ..CompactionPolicy::default()
    }
}

/// Replicas of one account that trust each other, on one clock.
fn replicas(n: u8, clock: &Arc<FakeClock>) -> Vec<(Engine, Db)> {
    let engines: Vec<Engine> = (1..=n)
        .map(|i| engine_seeded(ROOT, [i; 32], Arc::clone(clock)))
        .collect();
    engines
        .iter()
        .map(|e| {
            let mut db = db_root(ROOT);
            for other in &engines {
                if other.keychain.device_id() != e.keychain.device_id() {
                    trust(e, &mut db, other);
                }
            }
            (e.clone(), db)
        })
        .collect()
}

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock(PLMutex::new(T0)))
}

/// Every envelope `db` holds, in the order a relay would hand them over:
/// by stream, device and seq.
fn all_envs(db: &Db) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops ORDER BY stream_id, device_id, seq")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// `device`'s envelopes in `stream` with seq above `after`.
fn envs_after(db: &Db, stream: &[u8; 16], device: &[u8; 16], after: u64) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT envelope FROM ops WHERE stream_id = ? AND device_id = ? AND seq > ?
             ORDER BY seq",
        )
        .unwrap();
    stmt.query_map(
        params![&stream[..], &device[..], i64::try_from(after).unwrap()],
        |r| r.get(0),
    )
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

/// Deliver every envelope of `from` to `to`, in relay order, except the
/// ones `to` wrote itself, which a relay does not echo back.
fn sync(from: &Db, to: &Engine, to_db: &mut Db) {
    let me = to.keychain.device_id();
    for env in all_envs(from) {
        if decode_envelope(&env).unwrap().device_id != me {
            to.apply_remote_all(to_db, &env).unwrap();
        }
    }
}

/// Have `e` publish its digest of `stream`, and return the digest op.
fn digest_op(e: &Engine, db: &mut Db, stream: &[u8; 16]) -> Vec<u8> {
    assert!(e.publish_stream_digest(db, stream).unwrap());
    let me = e.keychain.device_id();
    db.conn()
        .query_row(
            "SELECT envelope FROM ops WHERE stream_id = ? AND device_id = ?
             ORDER BY seq DESC LIMIT 1",
            params![&stream[..], &me[..]],
            |r| r.get(0),
        )
        .unwrap()
}

fn ops_in(db: &Db, stream: &[u8; 16]) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM ops WHERE stream_id = ?",
            params![&stream[..]],
            |r| r.get(0),
        )
        .unwrap()
}

fn cursor(db: &Db, stream: &[u8; 16], device: &[u8; 16]) -> u64 {
    db.conn()
        .query_row(
            "SELECT last_applied_seq FROM sync_cursors WHERE stream_id = ? AND device_id = ?",
            params![&stream[..], &device[..]],
            |r| r.get::<_, i64>(0),
        )
        .map_or(0, |n| u64::try_from(n).unwrap())
}

fn digest(e: &Engine, db: &mut Db, stream: &[u8; 16]) -> [u8; 32] {
    let now = e.clock.now_ms();
    db.with_tx(|tx| digest_payload(tx, stream, now))
        .unwrap()
        .expect("the replica holds ops in the stream")
        .digest
}

/// Every projected row a user can see, with its stamp: what two replicas
/// that hold the same ops must agree on byte for byte.
fn projection(db: &Db) -> Vec<String> {
    let mut out = Vec::new();
    for (table, key) in [
        ("tasks", "id"),
        ("contexts", "id"),
        ("task_contexts", "task_id, context_id"),
        ("task_blockers", "task_id, blocker_id"),
        ("focus_sessions", "id"),
    ] {
        let mut stmt = db
            .conn()
            .prepare(&format!("SELECT * FROM {table} ORDER BY {key}"))
            .unwrap();
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..n)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap();
        for row in rows {
            out.push(format!("{table}: {}", row.unwrap().join(" | ")));
        }
    }
    out
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

fn tag(e: &Engine, db: &mut Db, task: EntityRef, contexts: Vec<EntityRef>) {
    e.apply(
        db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                contexts: Some(contexts),
                ..Default::default()
            },
        },
    )
    .unwrap();
}

/// A device whose acknowledgement is missing holds compaction back, and the
/// digest it publishes releases it. The fold changes nothing a reader sees:
/// the projection, the cursors and the stream digest are the same before and
/// after, and the tip of each prefix stays.
#[test]
fn compaction_waits_for_every_known_device_and_changes_nothing_visible() {
    let c = clock();
    let mut r = replicas(2, &c);
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    let a = ea.keychain.device_id();
    let b = eb.keychain.device_id();
    new_task(&eb, &mut dbb, "from b");
    for t in ["one", "two", "three", "four"] {
        new_task(&ea, &mut dba, t);
    }
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    ack_outbox(&mut dba);
    set_clock(&c, T0 + 2 * DAY);

    assert_eq!(
        ea.compact_op_log(&mut dba, &policy()).unwrap().ops_removed,
        0,
        "B has acknowledged nothing yet"
    );
    assert_eq!(ops_in(&dba, &INBOX), 5);

    let ack = digest_op(&eb, &mut dbb, &INBOX);
    ea.apply_remote_all(&mut dba, &ack).unwrap();
    let before = (projection(&dba), digest(&ea, &mut dba, &INBOX));
    let report = ea.compact_op_log(&mut dba, &policy()).unwrap();
    assert_eq!(
        report.ops_removed, 4,
        "every op below each device's tip: A's 1-3 and B's task"
    );
    assert_eq!(
        floor_of(dba.conn(), &INBOX, &a).unwrap().map(|f| f.seq),
        Some(3)
    );
    assert_eq!(
        floor_of(dba.conn(), &INBOX, &b).unwrap().map(|f| f.seq),
        Some(1)
    );
    // A's tip and B's digest are what is left.
    assert_eq!(ops_in(&dba, &INBOX), 2);
    assert_eq!(cursor(&dba, &INBOX, &a), 4);
    assert_eq!(cursor(&dba, &INBOX, &b), 2);
    assert_eq!(
        (projection(&dba), digest(&ea, &mut dba, &INBOX)),
        before,
        "nothing a reader or a peer sees moved"
    );

    // A second run finds nothing more to fold.
    let again = ea.compact_op_log(&mut dba, &policy()).unwrap();
    assert_eq!(again.ops_removed, 0);
    assert_eq!(again.floors_raised, 0);
}

/// An op younger than the retention window is kept however many devices
/// have acknowledged it.
#[test]
fn the_retention_window_keeps_recent_ops() {
    let c = clock();
    let mut r = replicas(2, &c);
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    for t in ["one", "two", "three"] {
        new_task(&ea, &mut dba, t);
    }
    sync(&dba, &eb, &mut dbb);
    ack_outbox(&mut dba);
    let ack = digest_op(&eb, &mut dbb, &INBOX);
    ea.apply_remote_all(&mut dba, &ack).unwrap();
    let report = ea.compact_op_log(&mut dba, &policy()).unwrap();
    assert_eq!(
        (report.floors_raised, report.ops_removed),
        (0, 0),
        "{report:?}"
    );
    set_clock(&c, T0 + 2 * DAY);
    assert_eq!(
        ea.compact_op_log(&mut dba, &CompactionPolicy::default())
            .unwrap()
            .ops_removed,
        0,
        "the default window is thirty days"
    );
    assert_eq!(
        ea.compact_op_log(&mut dba, &policy()).unwrap().ops_removed,
        2
    );
}

/// A device silent for longer than the window stops holding compaction back,
/// and a revoked one never did.
#[test]
fn a_silent_or_revoked_device_is_not_waited_for() {
    let c = clock();
    let mut r = replicas(3, &c);
    let (ec, _dbc) = r.pop().unwrap();
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    // C wrote once, at T0, and was never heard from again.
    let mut dbc = db_root(ROOT);
    trust(&ec, &mut dbc, &ea);
    new_task(&ec, &mut dbc, "from c");
    sync(&dbc, &ea, &mut dba);
    for t in ["one", "two", "three"] {
        new_task(&ea, &mut dba, t);
    }
    sync(&dba, &eb, &mut dbb);
    ack_outbox(&mut dba);
    set_clock(&c, T0 + 31 * DAY);
    let ack = digest_op(&eb, &mut dbb, &INBOX);
    ea.apply_remote_all(&mut dba, &ack).unwrap();
    let report = ea.compact_op_log(&mut dba, &policy()).unwrap();
    assert_eq!(
        report.ops_removed, 2,
        "C has been silent for 31 days and is not waited for"
    );

    // With B revoked, A folds its new ops without waiting for B to say it
    // holds them.
    let a = ea.keychain.device_id();
    revoke(&ea, &mut dba, &ea, eb.keychain.device_id(), T0 + 31 * DAY);
    new_task(&ea, &mut dba, "four");
    new_task(&ea, &mut dba, "five");
    ack_outbox(&mut dba);
    set_clock(&c, T0 + 33 * DAY);
    let report = ea.compact_op_log(&mut dba, &policy()).unwrap();
    assert_eq!(report.ops_removed, 2, "seqs 3 and 4, below the new tip");
    assert_eq!(
        floor_of(dba.conn(), &INBOX, &a).unwrap().map(|f| f.seq),
        Some(4)
    );
}

/// After a fold the replica keeps writing: its next op continues the seq and
/// names the tip it kept, and a peer that applies it records nothing amiss.
#[test]
fn a_compacted_replica_keeps_writing_and_its_peer_finds_nothing_amiss() {
    let c = clock();
    let mut r = replicas(2, &c);
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    let a = ea.keychain.device_id();
    for t in ["one", "two", "three"] {
        new_task(&ea, &mut dba, t);
    }
    sync(&dba, &eb, &mut dbb);
    ack_outbox(&mut dba);
    set_clock(&c, T0 + 2 * DAY);
    let ack = digest_op(&eb, &mut dbb, &INBOX);
    ea.apply_remote_all(&mut dba, &ack).unwrap();
    assert_eq!(
        ea.compact_op_log(&mut dba, &policy()).unwrap().ops_removed,
        2
    );

    let task = new_task(&ea, &mut dba, "four");
    let fresh = envs_after(&dba, &INBOX, &a, 3);
    assert_eq!(fresh.len(), 1);
    assert_eq!(decode_envelope(&fresh[0]).unwrap().seq, 4);
    eb.apply_remote(&mut dbb, &fresh[0]).unwrap();
    assert_eq!(read_task_t(&eb, &dbb, task).title, "four");
    assert_eq!(eb.chain_integrity(&dbb).unwrap(), ChainIntegrity::default());

    // A's own digest after the fold still agrees with B's.
    let a_digest = digest_op(&ea, &mut dba, &INBOX);
    eb.apply_remote_all(&mut dbb, &a_digest).unwrap();
    assert_eq!(eb.chain_integrity(&dbb).unwrap(), ChainIntegrity::default());
    assert_eq!(digest(&ea, &mut dba, &INBOX), digest(&eb, &mut dbb, &INBOX));
}

/// A re-delivered op below the floor is the duplicate it is, and changes
/// nothing. A different op signed at the floor itself is fork evidence, as it
/// would be at any held position.
#[test]
fn a_delivery_below_the_floor_is_a_duplicate_and_a_different_one_at_it_is_a_fork() {
    let c = clock();
    let mut r = replicas(2, &c);
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    let b = eb.keychain.device_id();
    for t in ["one", "two", "three"] {
        new_task(&eb, &mut dbb, t);
    }
    sync(&dbb, &ea, &mut dba);
    let b_envs = envs_after(&dbb, &INBOX, &b, 0);
    set_clock(&c, T0 + 2 * DAY);
    let ack = digest_op(&eb, &mut dbb, &INBOX);
    ea.apply_remote_all(&mut dba, &ack).unwrap();
    ea.compact_op_log(&mut dba, &policy()).unwrap();
    // B's digest is its seq 4 and names its prefix through 3.
    assert_eq!(
        floor_of(dba.conn(), &INBOX, &b).unwrap().map(|f| f.seq),
        Some(3)
    );

    let before = (projection(&dba), ops_in(&dba, &INBOX));
    for env in &b_envs {
        assert!(ea.apply_remote_all(&mut dba, env).unwrap().is_empty());
    }
    assert_eq!((projection(&dba), ops_in(&dba, &INBOX)), before);
    assert_eq!(ea.chain_integrity(&dba).unwrap(), ChainIntegrity::default());

    // B's key, a second log: the same device signing other ops at seqs 1-3.
    let eb2 = engine_seeded(ROOT, [2; 32], Arc::clone(&c));
    let mut dbb2 = db_root(ROOT);
    for t in ["other one", "other two", "other three"] {
        new_task(&eb2, &mut dbb2, t);
    }
    let forged = envs_after(&dbb2, &INBOX, &b, 0);
    for env in &forged[..2] {
        ea.apply_remote_all(&mut dba, env).unwrap();
    }
    assert_eq!(
        ea.chain_integrity(&dba).unwrap().forks,
        0,
        "below the floor no single op's hash is known"
    );
    ea.apply_remote_all(&mut dba, &forged[2]).unwrap();
    assert_eq!(
        ea.chain_integrity(&dba).unwrap().forks,
        1,
        "at the floor the folded op's hash is known"
    );
    assert_eq!((projection(&dba), ops_in(&dba, &INBOX)), before);
}

/// What compaction never deletes: an op parked for a kind this build does
/// not know, a focus record, and an op still waiting in the outbox.
#[test]
fn compaction_keeps_parked_append_only_and_unsent_ops() {
    let c = clock();
    let mut r = replicas(2, &c);
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    let task = new_task(&ea, &mut dba, "one");
    let session = open_session(&ea, &mut dba, task);
    let _ = session;
    super::emit_raw_inner(
        &ea,
        &mut dba,
        &INBOX,
        &super::future_kind_inner("FutureKind"),
    );
    new_task(&ea, &mut dba, "two");
    new_task(&ea, &mut dba, "three");
    sync(&dba, &eb, &mut dbb);
    set_clock(&c, T0 + 2 * DAY);
    let b_ack = digest_op(&eb, &mut dbb, &INBOX);
    ea.apply_remote_all(&mut dba, &b_ack).unwrap();
    let a_ack = digest_op(&ea, &mut dba, &INBOX);
    eb.apply_remote_all(&mut dbb, &a_ack).unwrap();

    // B received the future op and parked it; A's own copy is unsent.
    let b_before: Vec<String> = kinds(&dbb, &INBOX);
    eb.compact_op_log(&mut dbb, &policy()).unwrap();
    let b_after = kinds(&dbb, &INBOX);
    assert!(
        b_after.contains(&"unknown".to_owned()),
        "the parked op stays"
    );
    assert!(
        b_before.len() > b_after.len(),
        "the task ops below the tip went"
    );
    let a_removed = ea.compact_op_log(&mut dba, &policy()).unwrap().ops_removed;
    assert_eq!(a_removed, 0, "every op of A is still waiting in its outbox");
}

fn kinds(db: &Db, stream: &[u8; 16]) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT inner_kind FROM ops WHERE stream_id = ? ORDER BY device_id, seq")
        .unwrap();
    stmt.query_map(params![&stream[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Mark every op this replica wrote as acknowledged by the relay, as a sync
/// session's acks would.
fn ack_outbox(db: &mut Db) {
    db.conn()
        .execute("UPDATE outbox SET acked_at_ms = 1", [])
        .unwrap();
}

/// The known device with the least id is the stream's compactor, and only
/// it writes a snapshot.
#[test]
fn only_the_least_known_device_writes_the_snapshot() {
    let c = clock();
    let mut r = replicas(2, &c);
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    new_task(&ea, &mut dba, "one");
    new_task(&eb, &mut dbb, "two");
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    let ra = ea.compact_op_log(&mut dba, &policy()).unwrap();
    let rb = eb.compact_op_log(&mut dbb, &policy()).unwrap();
    let a_first = ea.keychain.device_id() < eb.keychain.device_id();
    assert_eq!(
        (ra.snapshots_written > 0, rb.snapshots_written > 0),
        (a_first, !a_first)
    );
    // Not again within the day.
    let (e, db) = if a_first {
        (&ea, &mut dba)
    } else {
        (&eb, &mut dbb)
    };
    assert_eq!(
        e.compact_op_log(db, &policy()).unwrap().snapshots_written,
        0
    );
    assert!(e.stream_snapshot(db, &INBOX).unwrap().is_some());
}

/// The acceptance test: a device that joins from a snapshot and the tail
/// above it holds exactly what a device that replayed every op holds, whose
/// projection, cursors and stream digest all agree.
#[test]
fn a_device_bootstrapped_from_a_snapshot_equals_one_that_replayed_every_op() {
    let c = clock();
    let mut r = replicas(4, &c);
    let (ed, mut dbd) = r.pop().unwrap();
    let (ec, mut dbc) = r.pop().unwrap();
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();

    // A history with concurrent edits, sets, a counter, a delete and a
    // focus record, written on two devices.
    let errands = new_context(&ea, &mut dba, "errands");
    let home = new_context(&ea, &mut dba, "home");
    let t1 = new_task(&ea, &mut dba, "one");
    let t2 = new_task(&ea, &mut dba, "two");
    let t3 = new_task(&ea, &mut dba, "three");
    sync(&dba, &eb, &mut dbb);
    rename(&ea, &mut dba, t1, "one from a");
    rename(&eb, &mut dbb, t1, "one from b");
    tag(&ea, &mut dba, t2, vec![errands]);
    tag(&eb, &mut dbb, t2, vec![home]);
    ea.apply(
        &mut dba,
        Command::DeferTask {
            id: t3,
            to_ms: T0 + DAY,
        },
    )
    .unwrap();
    eb.apply(&mut dbb, Command::DeleteTask(t3)).unwrap();
    let session = open_session(&eb, &mut dbb, t1);
    end_session(&eb, &mut dbb, session, false);
    sync(&dbb, &ea, &mut dba);
    sync(&dba, &eb, &mut dbb);

    // A writes its snapshots, then the history goes on.
    let meta = ea
        .write_stream_snapshot(&mut dba, &META_STREAM)
        .unwrap()
        .unwrap();
    let inbox = ea.write_stream_snapshot(&mut dba, &INBOX).unwrap().unwrap();
    let a = ea.keychain.device_id();
    let b = eb.keychain.device_id();
    let at_snapshot: Vec<(u64, u64)> = [a, b]
        .iter()
        .map(|d| (cursor(&dba, &INBOX, d), cursor(&dba, &META_STREAM, d)))
        .collect();
    rename(&eb, &mut dbb, t2, "two, later");
    rename(&ea, &mut dba, t1, "one, later");
    sync(&dbb, &ea, &mut dba);
    sync(&dba, &eb, &mut dbb);

    // C: the snapshots, then only the tail above them.
    for record in [&meta, &inbox] {
        let (outcome, events) = ec.apply_snapshot(&mut dbc, record).unwrap();
        assert!(
            matches!(outcome, SnapshotApplied::Applied { .. }),
            "{outcome:?}"
        );
        assert!(!events.is_empty());
    }
    for (device, (inbox_at, meta_at)) in [a, b].iter().zip(&at_snapshot) {
        for env in envs_after(&dba, &INBOX, device, *inbox_at)
            .into_iter()
            .chain(envs_after(&dba, &META_STREAM, device, *meta_at))
        {
            ec.apply_remote_all(&mut dbc, &env).unwrap();
        }
    }
    // D: every op.
    sync(&dba, &ed, &mut dbd);

    assert_eq!(projection(&dbc), projection(&dbd));
    assert!(!projection(&dbc).is_empty());
    for stream in [INBOX, META_STREAM] {
        assert_eq!(
            digest(&ec, &mut dbc, &stream),
            digest(&ed, &mut dbd, &stream)
        );
        for d in [a, b] {
            assert_eq!(cursor(&dbc, &stream, &d), cursor(&dbd, &stream, &d));
        }
    }
    assert_eq!(ec.chain_integrity(&dbc).unwrap(), ChainIntegrity::default());
    assert!(
        ops_in(&dbc, &INBOX) < ops_in(&dbd, &INBOX),
        "C never held the ops the snapshot folded"
    );

    // And C keeps writing above the floor, which A reads without complaint.
    let t4 = new_task(&ec, &mut dbc, "from c");
    sync(&dbc, &ea, &mut dba);
    assert_eq!(read_task_t(&ea, &dba, t4).title, "from c");
    assert_eq!(ea.chain_integrity(&dba).unwrap(), ChainIntegrity::default());
}

/// The compactor's own flow: it folds its log first and writes the snapshot
/// from what is left, the merge state and the tips. A device that joins from
/// that snapshot sees what the compactor sees, including the tasks whose only
/// ops were local full-state writes the merge had never folded until the
/// compaction did.
#[test]
fn a_snapshot_written_after_compaction_bootstraps_a_new_device() {
    let c = clock();
    let mut r = replicas(3, &c);
    let (ec, mut dbc) = r.pop().unwrap();
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    let t1 = new_task(&ea, &mut dba, "one");
    new_task(&ea, &mut dba, "two");
    new_task(&eb, &mut dbb, "three");
    rename(&ea, &mut dba, t1, "one, renamed");
    new_task(&ea, &mut dba, "four");
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    ack_outbox(&mut dba);
    set_clock(&c, T0 + 2 * DAY);
    let ack = digest_op(&eb, &mut dbb, &INBOX);
    ea.apply_remote_all(&mut dba, &ack).unwrap();
    let report = ea.compact_op_log(&mut dba, &policy()).unwrap();
    assert!(report.ops_removed >= 4, "{report:?}");

    let record = ea.write_stream_snapshot(&mut dba, &INBOX).unwrap().unwrap();
    let (outcome, _) = ec.apply_snapshot(&mut dbc, &record).unwrap();
    assert!(
        matches!(outcome, SnapshotApplied::Applied { entities: 4, .. }),
        "{outcome:?}"
    );
    assert_eq!(projection(&dbc), projection(&dba));
    assert_eq!(digest(&ec, &mut dbc, &INBOX), digest(&ea, &mut dba, &INBOX));
    assert_eq!(read_task_t(&ec, &dbc, t1).title, "one, renamed");
}

/// A snapshot carries the ops whose effect is not in the merge state: here a
/// parked op of a kind this build does not know, which the bootstrapped
/// device parks in turn rather than losing.
#[test]
fn a_snapshot_carries_parked_ops_to_the_device_it_bootstraps() {
    let c = clock();
    let mut r = replicas(3, &c);
    let (ec, mut dbc) = r.pop().unwrap();
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    new_task(&ea, &mut dba, "one");
    super::emit_raw_inner(
        &ea,
        &mut dba,
        &INBOX,
        &super::future_kind_inner("FutureKind"),
    );
    new_task(&ea, &mut dba, "two");
    sync(&dba, &eb, &mut dbb);
    let record = eb.write_stream_snapshot(&mut dbb, &INBOX).unwrap().unwrap();
    let (outcome, _) = ec.apply_snapshot(&mut dbc, &record).unwrap();
    assert_eq!(
        outcome,
        SnapshotApplied::Applied {
            entities: 2,
            retained: 1
        }
    );
    assert_eq!(super::parked_rows(&dbc).len(), 1);
    assert_eq!(
        cursor(&dbc, &INBOX, &ea.keychain.device_id()),
        3,
        "the prefix runs through the parked op"
    );
}

/// A snapshot holding nothing this replica lacks is reported and ignored,
/// and applying one twice changes nothing the second time.
#[test]
fn a_snapshot_already_covered_is_ignored() {
    let c = clock();
    let mut r = replicas(3, &c);
    let (ec, mut dbc) = r.pop().unwrap();
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    new_task(&ea, &mut dba, "one");
    sync(&dba, &eb, &mut dbb);
    let record = ea.write_stream_snapshot(&mut dba, &INBOX).unwrap().unwrap();
    assert_eq!(
        eb.apply_snapshot(&mut dbb, &record).unwrap().0,
        SnapshotApplied::Covered
    );
    assert!(matches!(
        ec.apply_snapshot(&mut dbc, &record).unwrap().0,
        SnapshotApplied::Applied { .. }
    ));
    let once = projection(&dbc);
    assert_eq!(
        ec.apply_snapshot(&mut dbc, &record).unwrap().0,
        SnapshotApplied::Covered
    );
    assert_eq!(projection(&dbc), once);
}

/// A record whose bytes were changed anywhere fails its signature, one
/// signed by a device this account never had is refused, and one of a
/// history that disagrees with this replica's chain is refused.
#[test]
fn a_forged_foreign_or_divergent_snapshot_is_refused() {
    let c = clock();
    let mut r = replicas(3, &c);
    let (ec, mut dbc) = r.pop().unwrap();
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    new_task(&ea, &mut dba, "one");
    new_task(&ea, &mut dba, "two");
    let record = ea.write_stream_snapshot(&mut dba, &INBOX).unwrap().unwrap();

    for i in [8, record.len() / 2, record.len() - 1] {
        let mut bad = record.clone();
        bad[i] ^= 0x01;
        assert!(
            matches!(
                ec.apply_snapshot(&mut dbc, &bad),
                Err(EngineError::Invalid(_))
            ),
            "a flipped byte at {i} is caught"
        );
    }

    // Another account's device: its cert verifies under no identity here.
    let stranger = engine_seeded([0x77; 32], [9; 32], Arc::clone(&c));
    let mut dbs = db_root([0x77; 32]);
    new_task(&stranger, &mut dbs, "theirs");
    let theirs = stranger
        .write_stream_snapshot(&mut dbs, &INBOX)
        .unwrap()
        .unwrap();
    assert!(matches!(
        ec.apply_snapshot(&mut dbc, &theirs),
        Err(EngineError::Invalid(_))
    ));

    // B holds a different op at A's seq 2: the same key signing another log.
    let ea2 = engine_seeded(ROOT, [1; 32], Arc::clone(&c));
    let mut dba2 = db_root(ROOT);
    new_task(&ea2, &mut dba2, "one");
    new_task(&ea2, &mut dba2, "not two");
    sync(&dba2, &eb, &mut dbb);
    assert!(matches!(
        eb.apply_snapshot(&mut dbb, &record),
        Err(EngineError::Invalid(_))
    ));
    assert!(projection(&dbc).is_empty(), "nothing a refusal wrote");
}

/// A bootstrapped replica restores its clock from the floors after a
/// restart: every op the snapshot folded is gone, and what it writes next
/// still sorts above them.
#[test]
fn a_bootstrapped_replica_primes_its_clock_from_the_floors() {
    let c = clock();
    let mut r = replicas(2, &c);
    let (eb, mut dbb) = r.pop().unwrap();
    let (ea, mut dba) = r.pop().unwrap();
    set_clock(&c, T0 + DAY);
    new_task(&ea, &mut dba, "one");
    let record = ea.write_stream_snapshot(&mut dba, &INBOX).unwrap().unwrap();
    eb.apply_snapshot(&mut dbb, &record).unwrap();
    let a = ea.keychain.device_id();
    let floor = floor_of(dbb.conn(), &INBOX, &a).unwrap().unwrap();

    // B restarts on a clock a day behind.
    let behind = Arc::new(FakeClock(PLMutex::new(T0)));
    let restarted = engine_seeded(ROOT, [2; 32], behind);
    restarted.prime_hlc(&dbb).unwrap();
    assert!(restarted.hlc.peek() >= floor.hlc);
}
