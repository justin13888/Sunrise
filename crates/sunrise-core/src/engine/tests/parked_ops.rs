//! Parked ops: what parks, what releases it and the schema fingerprint.

use super::*;

/// An op that arrives before the key that opens it is parked, not lost, and
/// the `key_envelope` op carrying that key releases it.
///
/// The two halves are both assertions. Parking must be **silent**: no
/// event, no error, and no entity — a `RemoteOpInvalid` here would make the
/// driver treat a perfectly good op as a damaged frame and ask for a
/// resync it cannot be helped by. Draining must be **complete**: the
/// envelope op itself changes nothing on screen, so if the delivery did not
/// hand back the released op's event the screen would never learn about a
/// task the database now holds.
#[test]
fn an_op_that_arrives_before_its_key_is_parked_and_the_envelope_releases_it() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    // Each knows the other's identity-signed cert, so B knows A's key to
    // verify with and A knows B's `D_D_pub` to seal an envelope to.
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    // A captures a task. This mints the Inbox key and the vault-meta key,
    // and emits a `key_envelope` op per recipient for each.
    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "buy milk".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;

    // B was paired, so it holds the vault-meta key and can read control
    // ops. It was never told the Inbox key.
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    assert!(
        eb.keychain
            .stream_keys_at(&sunrise_domain::INBOX_STREAM_BYTES, 1)
            .is_empty(),
        "the premise: B holds no Inbox key"
    );

    // The task op arrives first.
    let create_env = create_env_for(&dba, task.bytes());
    let events = eb.apply_remote_all(&mut dbb, &create_env).unwrap();
    assert!(events.is_empty(), "parking is silent");
    assert_eq!(deferred_rows(&dbb), 1, "and it is parked, not dropped");
    assert!(
        read_task(dbb.conn(), task.bytes()).unwrap().is_none(),
        "nothing materialized from an op nobody could open"
    );

    // Re-delivery of the same op must not pile up a second row.
    assert!(eb
        .apply_remote_all(&mut dbb, &create_env)
        .unwrap()
        .is_empty());
    assert_eq!(deferred_rows(&dbb), 1, "parking is idempotent on op id");

    // Now A's `key_envelope` ops. One per (stream, recipient); the delivery
    // that lands B's copy of the Inbox key is the one that releases the
    // task, and the rest are silent.
    let mut released = Vec::new();
    let envs = key_envelope_envs(&dba);
    assert!(!envs.is_empty(), "A emitted envelopes to seal to B at all");
    for env in envs {
        released.extend(eb.apply_remote_all(&mut dbb, &env).unwrap());
    }
    assert!(
        matches!(released.as_slice(), [DomainEvent::Created(r)] if *r == task),
        "the envelope op is silent; the op it released is not, got {released:?}"
    );
    assert_eq!(deferred_rows(&dbb), 0, "the park is emptied by the drain");
    assert_eq!(
        read_task_t(&eb, &dbb, task).title,
        "buy milk",
        "and the task is really there"
    );
}

/// `deferred_ops` is bounded, and overflow evicts the oldest rows.
///
/// Everything about a deferred op is unchecked — the key that would open it
/// is exactly what is missing — and `epoch` is a free field of the signed
/// envelope, so without a cap a member can park bytes of its choosing on
/// every peer in the account at any `(stream, epoch)` it likes, forever.
#[test]
fn the_parked_op_buffer_is_bounded() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let eb = engine_random_keys(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let over = usize::try_from(DEFERRED_PER_EPOCH_CAP).unwrap() + 16;
    let mut envs = Vec::with_capacity(over);
    for i in 0..over {
        set_clock(&ca, T0 + i as u64);
        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: format!("parked {i}"),
                    ..Default::default()
                }),
            )
            .unwrap();
        envs.push(env_bytes(&dba, &res.op_id));
    }
    // B holds the vault-meta key but never the Inbox key, so every task op
    // A wrote parks on B. The hand-over is after the writes because the
    // first one is what mints the key at all.
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    for (i, env) in envs.iter().enumerate() {
        // `received_at_ms` is the *receiver's* reading, so it is B's clock
        // that has to move for the rows to be orderable at all.
        set_clock(&cb, T0 + i as u64);
        assert!(eb.apply_remote_all(&mut dbb, env).unwrap().is_empty());
    }
    assert_eq!(
        deferred_rows(&dbb),
        DEFERRED_PER_EPOCH_CAP,
        "the bucket is capped, not unbounded"
    );

    // Oldest-first eviction: what survives is the newest window, because a
    // row that has waited longest is the one whose key is least likely to
    // still be in flight.
    let oldest: i64 = dbb
        .conn()
        .query_row("SELECT MIN(received_at_ms) FROM deferred_ops", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        oldest,
        i64::try_from(T0).unwrap() + 16,
        "the first 16 were the ones dropped"
    );
}

/// A parked op older than [`DEFERRED_TTL_MS`] is swept by the next drain.
///
/// The caps are tested; the TTL was not, which left "a row this old is
/// ciphertext nobody will ever open" as a sentence rather than a property.
/// A drain is the only moment the table is known to be changing, so it is
/// where the sweep runs — and this is what fails if it stops running.
#[test]
fn a_parked_op_older_than_the_ttl_is_swept_by_the_next_drain() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    // A writes to the Inbox. B holds the vault-meta key but never the
    // Inbox key, so the op parks.
    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "parked and forgotten".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    let envelopes_before = key_envelope_envs(&dba).len();
    assert!(eb
        .apply_remote_all(&mut dbb, &env_bytes(&dba, &task.op_id))
        .unwrap()
        .is_empty());
    assert_eq!(deferred_rows(&dbb), 1, "the premise: it is parked");

    // A month passes on B, and then some *other* key arrives — a fresh
    // Stream A creates, whose key B does receive.
    set_clock(&cb, T0 + DEFERRED_TTL_MS + 1);
    set_clock(&ca, T0 + DEFERRED_TTL_MS + 1);
    let stream = ea
        .apply(
            &mut dba,
            Command::CreateStream(StreamDraft {
                name: "later".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "in the new stream".into(),
            stream_id: Some(stream),
            ..Default::default()
        }),
    )
    .unwrap();

    // Only the envelopes minted after the park, so B never learns the
    // Inbox key and the row can only leave by being swept.
    for env in key_envelope_envs(&dba).into_iter().skip(envelopes_before) {
        let _ = eb.apply_remote_all(&mut dbb, &env);
    }
    assert!(
        eb.keychain
            .stream_keys_at(&sunrise_domain::INBOX_STREAM_BYTES, 1)
            .is_empty(),
        "B still has no Inbox key, so the row was not drained normally"
    );
    assert_eq!(
        deferred_rows(&dbb),
        0,
        "an op parked past the TTL is swept by the next drain"
    );
    assert!(
        read_task(dbb.conn(), task.entity.bytes())
            .unwrap()
            .is_none(),
        "and it never materialized"
    );
}

// --- Parked ops (issue #320, ADR-0045 §4) ------------------------------------

/// A verified op of a kind this build does not know is kept, not refused: it
/// is in `ops` unapplied, marked in `parked_ops`, and the cursor counts it, so
/// the relay stops re-sending it and the ops above it are not held behind it.
/// A re-delivery adds nothing.
#[test]
fn an_op_of_a_kind_this_build_does_not_know_is_parked_and_counted() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);
    let a_id = ea.keychain.device_id();

    let first = new_task(&ea, &mut dba, "first");
    let first_env = create_env_for(&dba, first.bytes());
    let first_op = decode_envelope(&first_env).unwrap();
    let stream = first_op.stream_id;
    assert!(eb.apply_remote(&mut dbb, &first_env).unwrap().is_some());
    assert_eq!(cursor_for(&dbb, &stream, &a_id), 1);

    let future = emit_raw_inner(&ea, &mut dba, &stream, &future_kind_inner("FutureKind"));
    let future_op = decode_envelope(&future).unwrap();
    let future_id = remote_op_id(&stream, &a_id, future_op.seq);
    assert!(
        eb.apply_remote_all(&mut dbb, &future).unwrap().is_empty(),
        "parked, not refused, and nothing on screen changed"
    );
    assert_eq!(cursor_for(&dbb, &stream, &a_id), 2, "the parked op counts");
    assert_eq!(op_row(&dbb, &future_id), ("unknown".into(), None));
    assert_eq!(
        parked_rows(&dbb),
        vec![(
            "unknown_kind".into(),
            "FutureKind".into(),
            i64::from(sunrise_cbor::version::DOC_SCHEMA_V)
        )]
    );

    // The op above it applies and the cursor runs past both.
    let second = new_task(&ea, &mut dba, "second");
    assert!(eb
        .apply_remote(&mut dbb, &create_env_for(&dba, second.bytes()))
        .unwrap()
        .is_some());
    assert_eq!(cursor_for(&dbb, &stream, &a_id), 3);

    // Re-delivery and a replay by the same build both change nothing.
    let ops_before = op_count(&dbb);
    assert!(eb.apply_remote_all(&mut dbb, &future).unwrap().is_empty());
    assert!(eb.replay_parked_ops(&mut dbb).is_empty());
    assert_eq!(op_count(&dbb), ops_before);
    assert_eq!(parked_rows(&dbb).len(), 1);
    assert_eq!(op_row(&dbb, &future_id), ("unknown".into(), None));
}

/// Parking observes the sender's stamp. The row is in `ops`, which
/// `prime_hlc` restores the clock from, so a park that skipped the observe
/// would leave the next open above a reading this session never took.
#[test]
fn parking_an_op_observes_the_senders_stamp() {
    // A leads B by 200 s: inside `MAX_DRIFT_MS`, so the stamp is admitted and
    // is above anything B's own clock has read.
    const LEAD_MS: u64 = 200_000;
    let ea = engine_seeded(
        ROOT,
        [1u8; 32],
        Arc::new(FakeClock(PLMutex::new(T0 + LEAD_MS))),
    );
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "stamped ahead");
    let env = create_env_for(&dba, task.bytes());
    let op = decode_envelope(&env).unwrap();
    assert!(
        eb.hlc.peek() < op.hlc,
        "the premise: B's clock has not seen the stamp"
    );

    eb.park_op(
        &mut dbb,
        &env,
        &op,
        "TaskCreate",
        sunrise_cbor::version::DOC_SCHEMA_V,
    )
    .unwrap();
    assert!(
        eb.hlc.peek() >= op.hlc,
        "the park observed the sender's stamp"
    );
    assert_eq!(parked_rows(&dbb).len(), 1);
}

/// A stamp beyond the drift window is refused by the park as it is by the
/// apply path, before anything is written: no `ops` row, no marker, and the
/// cursor does not count it.
#[test]
fn parking_an_op_beyond_the_drift_window_is_refused_and_leaves_no_row() {
    let ea = engine_seeded(
        ROOT,
        [1u8; 32],
        Arc::new(FakeClock(PLMutex::new(
            T0 + sunrise_cbor::hlc::MAX_DRIFT_MS + 1,
        ))),
    );
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);
    let a_id = ea.keychain.device_id();

    let task = new_task(&ea, &mut dba, "from the future");
    let env = create_env_for(&dba, task.bytes());
    let op = decode_envelope(&env).unwrap();
    let ops_before = op_count(&dbb);
    let hlc_before = eb.hlc.peek();

    let err = eb
        .park_op(
            &mut dbb,
            &env,
            &op,
            "TaskCreate",
            sunrise_cbor::version::DOC_SCHEMA_V,
        )
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::RemoteOpInvalid(m) if m.contains("hlc")),
        "expected an HLC drift rejection, got {err:?}"
    );
    assert_eq!(op_count(&dbb), ops_before, "no ops row");
    let op_id = remote_op_id(&op.stream_id, &a_id, op.seq);
    let rows: i64 = dbb
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ops WHERE op_id = ?",
            params![&op_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "no ops row for the refused op");
    assert!(parked_rows(&dbb).is_empty(), "no parked_ops row");
    assert_eq!(eb.hlc.peek(), hlc_before, "the clock was not dragged along");
}

/// The upgrade. A build that did not know `TaskCreate` parked one; this
/// build knows it, and the replay `Core::open` runs materializes it through
/// the full apply path, releases the marker, and is idempotent afterwards.
///
/// The parked row is written by `park_op` itself with the older build's
/// `DOC_SCHEMA_V`, which is exactly the row that build's apply path would
/// have left: one table, one writer.
#[test]
fn a_parked_op_is_materialized_once_a_build_that_knows_its_kind_replays_it() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);
    let a_id = ea.keychain.device_id();

    let task = new_task(&ea, &mut dba, "from the future");
    let env = create_env_for(&dba, task.bytes());
    let op = decode_envelope(&env).unwrap();
    let op_id = remote_op_id(&op.stream_id, &a_id, op.seq);
    let older = sunrise_cbor::version::DOC_SCHEMA_V - 1;
    eb.park_op(&mut dbb, &env, &op, "TaskCreate", older)
        .unwrap();
    assert!(
        eb.query(&dbb, Query::EntityById(task)).is_err(),
        "a parked op materializes nothing"
    );
    assert_eq!(cursor_for(&dbb, &op.stream_id, &a_id), 1);

    let events = eb.replay_parked_ops(&mut dbb);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, DomainEvent::Created(r) if *r == task)),
        "the replay reports what it materialized"
    );
    assert_eq!(read_task_t(&eb, &dbb, task).title, "from the future");
    assert!(parked_rows(&dbb).is_empty(), "the marker is released");
    let (kind, applied) = op_row(&dbb, &op_id);
    assert_eq!(kind, "task.create");
    assert!(applied.is_some());
    assert_eq!(op_count(&dbb), 1, "the same op row, now applied");

    // Idempotent: a second replay and a relay re-send both stop at the gate.
    assert!(eb.replay_parked_ops(&mut dbb).is_empty());
    assert!(eb.apply_remote(&mut dbb, &env).unwrap().is_none());
    assert_eq!(read_task_t(&eb, &dbb, task).title, "from the future");
}

/// A replay by a build that still cannot read the op leaves it parked and
/// re-stamps it, so the next open does not try it again.
#[test]
fn a_parked_op_this_build_still_cannot_read_stays_parked_and_is_restamped() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let anchor = new_task(&ea, &mut dba, "anchor");
    let stream = decode_envelope(&create_env_for(&dba, anchor.bytes()))
        .unwrap()
        .stream_id;
    let env = emit_raw_inner(&ea, &mut dba, &stream, &future_kind_inner("StillUnknown"));
    let op = decode_envelope(&env).unwrap();
    let current = sunrise_cbor::version::DOC_SCHEMA_V;
    eb.park_op(&mut dbb, &env, &op, "StillUnknown", current - 1)
        .unwrap();
    assert_eq!(OpLog::parked_for_replay(&dbb, current).unwrap().len(), 1);

    assert!(eb.replay_parked_ops(&mut dbb).is_empty());
    assert_eq!(
        parked_rows(&dbb),
        vec![(
            "unknown_kind".into(),
            "StillUnknown".into(),
            i64::from(current)
        )]
    );
    assert!(
        OpLog::parked_for_replay(&dbb, current).unwrap().is_empty(),
        "tried by this build, so not tried again until the build changes"
    );
}

/// A storage failure while replaying is neither a refusal nor fatal. The
/// replay logs it, leaves the op's old stamp alone so the next try applies it
/// (a re-stamp would keep an op this build can read parked until the next
/// `DOC_SCHEMA_V` bump), and goes on to the next parked op.
///
/// The failure is a trigger on the release's `DELETE FROM parked_ops`, which
/// runs inside the apply transaction and nowhere in the read or the re-stamp.
/// It fires on every try, the fault that would lock the vault if a replay
/// failure could fail `Core::open`.
#[test]
fn a_storage_failure_during_replay_leaves_the_op_unstamped_and_the_replay_goes_on() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "held up by the disk");
    let env = create_env_for(&dba, task.bytes());
    let op = decode_envelope(&env).unwrap();
    let older = sunrise_cbor::version::DOC_SCHEMA_V - 1;
    eb.park_op(&mut dbb, &env, &op, "TaskCreate", older)
        .unwrap();
    // A second parked op, behind the first in replay order, that this build
    // still cannot read: the failure above it must not stop its re-stamp.
    let later = emit_raw_inner(
        &ea,
        &mut dba,
        &op.stream_id,
        &future_kind_inner("StillUnknown"),
    );
    let later_op = decode_envelope(&later).unwrap();
    eb.park_op(&mut dbb, &later, &later_op, "StillUnknown", older)
        .unwrap();

    dbb.conn()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_release BEFORE DELETE ON parked_ops
             BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
    // The fault recurs on every try; each one returns, and neither stamps it.
    for _ in 0..2 {
        assert!(eb.replay_parked_ops(&mut dbb).is_empty());
        assert_eq!(
            parked_rows(&dbb),
            vec![
                ("unknown_kind".into(), "TaskCreate".into(), i64::from(older)),
                (
                    "unknown_kind".into(),
                    "StillUnknown".into(),
                    i64::from(sunrise_cbor::version::DOC_SCHEMA_V)
                ),
            ],
            "the failed op keeps its stamp, and the replay went on to the next"
        );
        assert!(eb.query(&dbb, Query::EntityById(task)).is_err());
    }

    // The next open, with the disk healthy, applies it.
    dbb.conn()
        .execute_batch("DROP TRIGGER fail_release;")
        .unwrap();
    let events = eb.replay_parked_ops(&mut dbb);
    assert!(events
        .iter()
        .any(|e| matches!(e, DomainEvent::Created(r) if *r == task)));
    assert_eq!(read_task_t(&eb, &dbb, task).title, "held up by the disk");
    assert_eq!(
        parked_rows(&dbb).len(),
        1,
        "only the still-unknown op is left"
    );
}

/// An op whose kind this build now knows, but which the apply path refuses on
/// replay, stays parked under this build's stamp, and its reason stops saying
/// the kind is unknown (ADR-0045 §4, "stays parked with its reason updated").
///
/// The refusal is the sender's device being unknown to this replica: the
/// parked row is written directly, and this replica never trusted the sender,
/// so the replay's signer lookup refuses it.
#[test]
fn a_parked_op_refused_on_replay_stays_parked_with_its_reason_updated() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);

    let task = new_task(&ea, &mut dba, "refused on replay");
    let env = create_env_for(&dba, task.bytes());
    let op = decode_envelope(&env).unwrap();
    assert!(matches!(
        eb.apply_remote_all(&mut db_root(ROOT), &env),
        Err(EngineError::UnknownDevice)
    ));
    let older = sunrise_cbor::version::DOC_SCHEMA_V - 1;
    eb.park_op(&mut dbb, &env, &op, "TaskCreate", older)
        .unwrap();

    assert!(eb.replay_parked_ops(&mut dbb).is_empty());
    assert_eq!(
        parked_rows(&dbb),
        vec![(
            "replay_refused".into(),
            "TaskCreate".into(),
            i64::from(sunrise_cbor::version::DOC_SCHEMA_V)
        )]
    );
    assert!(eb.query(&dbb, Query::EntityById(task)).is_err());
}

/// Only an unknown *name* parks. A family this build reads, with a payload
/// that does not decode, is still damage: refused, with no op row and no
/// marker, exactly as before.
#[test]
fn a_known_kind_with_a_damaged_payload_is_still_refused() {
    use ciborium::value::Value;
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let anchor = new_task(&ea, &mut dba, "anchor");
    let stream = decode_envelope(&create_env_for(&dba, anchor.bytes()))
        .unwrap()
        .stream_id;
    let mut damaged = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![(
            Value::Text("TaskCreate".into()),
            Value::Integer(7.into()),
        )]),
        &mut damaged,
    )
    .unwrap();
    let env = emit_raw_inner(&ea, &mut dba, &stream, &damaged);
    let ops_before = op_count(&dbb);
    assert!(matches!(
        eb.apply_remote_all(&mut dbb, &env),
        Err(EngineError::RemoteOpInvalid(_))
    ));
    assert_eq!(op_count(&dbb), ops_before);
    assert!(parked_rows(&dbb).is_empty());
}

// --- Schema fingerprint (issue #438, ADR-0045 §3) ----------------------------

/// `env`, an op `sender` emitted, re-sealed and re-signed by `sender` (whose
/// keychain seed is `sender_seed`) with field 12 set to `doc_schema_v` and
/// field 13 to `schema_fp`. This is how a writer that disagrees with this
/// build's registry looks to it: every envelope check passes, and only the
/// schema binding differs.
fn reseal_schema(
    sender: &Engine,
    sender_seed: [u8; 32],
    env: &[u8],
    doc_schema_v: u32,
    schema_fp: Option<[u8; 8]>,
) -> Vec<u8> {
    let mut e = decode_envelope(env).unwrap();
    let key = sender
        .keychain
        .stream_keys_at(&e.stream_id, e.epoch)
        .into_iter()
        .next()
        .expect("the sender holds the key it sealed under");
    e.payload = sunrise_crypto::open_envelope_unverified(&e, &key).unwrap();
    e.doc_schema_v = doc_schema_v;
    e.schema_fp = schema_fp;
    sunrise_crypto::seal_envelope(
        e,
        Some(&key),
        &sunrise_crypto::DeviceSigningKeyPair::from_secret_bytes(&sender_seed),
    )
    .unwrap()
}

/// A `TaskCreate` from A to B, re-sealed under `(doc_schema_v, schema_fp)`,
/// with B trusting A. Returns the engines, B's database, the task and the
/// re-sealed envelope.
fn schema_bound_create(
    doc_schema_v: u32,
    schema_fp: Option<[u8; 8]>,
) -> (Engine, Engine, Db, EntityRef, Vec<u8>) {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);
    let task = new_task(&ea, &mut dba, "schema bound");
    let env = create_env_for(&dba, task.bytes());
    let env = reseal_schema(&ea, [1u8; 32], &env, doc_schema_v, schema_fp);
    (ea, eb, dbb, task, env)
}

/// ADR-0045 §3, third row: an envelope at a version this build registers,
/// whose field 13 differs from the entry's prefix or is missing, is parked
/// with reason `schema_fp_mismatch` rather than applied. It counts toward the
/// cursor like any parked op, is not refused as damage, and a re-delivery
/// adds nothing.
#[test]
fn a_registered_version_whose_fingerprint_differs_or_is_missing_is_parked() {
    use sunrise_cbor::version::{DOC_SCHEMA_FP_FIRST, DOC_SCHEMA_V};
    let cases = [
        (u32::from(DOC_SCHEMA_V), Some([0xaa; 8]), "differs"),
        (u32::from(DOC_SCHEMA_V), None, "missing"),
        (
            u32::from(DOC_SCHEMA_FP_FIRST),
            None,
            "missing at the first entry",
        ),
    ];
    for (doc_schema_v, schema_fp, case) in cases {
        let (ea, eb, mut dbb, task, env) = schema_bound_create(doc_schema_v, schema_fp);
        let a_id = ea.keychain.device_id();
        let op = decode_envelope(&env).unwrap();
        let op_id = remote_op_id(&op.stream_id, &a_id, op.seq);

        assert!(
            eb.apply_remote_all(&mut dbb, &env).unwrap().is_empty(),
            "{case}: parked, not refused, and nothing on screen changed"
        );
        assert!(
            eb.query(&dbb, Query::EntityById(task)).is_err(),
            "{case}: a mismatched op materializes nothing"
        );
        assert_eq!(
            cursor_for(&dbb, &op.stream_id, &a_id),
            1,
            "{case}: the parked op counts"
        );
        assert_eq!(op_row(&dbb, &op_id), ("unknown".into(), None), "{case}");
        let parked = vec![(
            "schema_fp_mismatch".into(),
            "TaskCreate".into(),
            i64::from(DOC_SCHEMA_V),
        )];
        assert_eq!(parked_rows(&dbb), parked, "{case}");

        let ops_before = op_count(&dbb);
        assert!(eb.apply_remote_all(&mut dbb, &env).unwrap().is_empty());
        assert_eq!(
            op_count(&dbb),
            ops_before,
            "{case}: a re-delivery adds nothing"
        );
        assert_eq!(parked_rows(&dbb), parked, "{case}");
    }
}

/// ADR-0045 §3, the other three rows: a legacy version with no field 13, a
/// registered version whose field 13 matches (here an older entry than this
/// build's own), and a version newer than this build, which has no entry to
/// compare with whatever field 13 says, all apply as they did before.
#[test]
fn an_envelope_with_no_entry_here_or_a_matching_fingerprint_applies() {
    use sunrise_cbor::version::{doc_schema_fp_prefix, DOC_SCHEMA_FP_FIRST, DOC_SCHEMA_V};
    let first = u32::from(DOC_SCHEMA_FP_FIRST);
    let cases = [
        (first - 1, None, "legacy"),
        (first, doc_schema_fp_prefix(first), "matches an older entry"),
        (
            u32::from(DOC_SCHEMA_V),
            doc_schema_fp_prefix(u32::from(DOC_SCHEMA_V)),
            "matches this build's entry",
        ),
        (u32::from(DOC_SCHEMA_V) + 1, Some([0xaa; 8]), "newer"),
    ];
    for (doc_schema_v, schema_fp, case) in cases {
        let (_ea, eb, mut dbb, task, env) = schema_bound_create(doc_schema_v, schema_fp);
        assert!(
            eb.apply_remote(&mut dbb, &env).unwrap().is_some(),
            "{case}: applied"
        );
        assert_eq!(read_task_t(&eb, &dbb, task).title, "schema bound", "{case}");
        assert!(parked_rows(&dbb).is_empty(), "{case}: nothing parked");
    }
}

/// The replay is what retries a `schema_fp_mismatch` op once the registry
/// changes, and the registry changes only with a `DOC_SCHEMA_V` bump. A retry
/// against the same registry, from a row another build's stamp makes due,
/// leaves the op parked with its reason unchanged and re-stamps it.
#[test]
fn a_fingerprint_mismatch_retried_against_the_same_registry_stays_parked() {
    use sunrise_cbor::version::DOC_SCHEMA_V;
    let (_ea, eb, mut dbb, task, env) =
        schema_bound_create(u32::from(DOC_SCHEMA_V), Some([0xaa; 8]));
    assert!(eb.apply_remote_all(&mut dbb, &env).unwrap().is_empty());
    let parked = vec![(
        "schema_fp_mismatch".into(),
        "TaskCreate".into(),
        i64::from(DOC_SCHEMA_V),
    )];
    assert_eq!(parked_rows(&dbb), parked);
    assert!(
        OpLog::parked_for_replay(&dbb, DOC_SCHEMA_V)
            .unwrap()
            .is_empty(),
        "parked by this build, so this build does not retry it"
    );

    dbb.conn()
        .execute(
            "UPDATE parked_ops SET parked_under_doc_schema_v = ?",
            params![DOC_SCHEMA_V - 1],
        )
        .unwrap();
    assert_eq!(
        OpLog::parked_for_replay(&dbb, DOC_SCHEMA_V).unwrap().len(),
        1
    );
    assert!(eb.replay_parked_ops(&mut dbb).is_empty());
    assert_eq!(
        parked_rows(&dbb),
        parked,
        "still parked, reason unchanged, stamped by this build"
    );
    assert!(eb.query(&dbb, Query::EntityById(task)).is_err());
}

/// A retry moves the reason to the cause it finds now (ADR-0045 §4). An op an
/// older build parked as an unknown kind, whose field 13 this build's registry
/// disagrees with, stays parked as `schema_fp_mismatch`; and an op parked as
/// `schema_fp_mismatch` by a build whose registry disagreed is applied by one
/// whose registry agrees, and its marker released.
#[test]
fn a_replay_reparks_for_the_fingerprint_or_releases_once_the_registry_agrees() {
    use sunrise_cbor::version::{doc_schema_fp_prefix, DOC_SCHEMA_V};
    let current = u32::from(DOC_SCHEMA_V);
    let older = DOC_SCHEMA_V - 1;

    let (_ea, eb, mut dbb, task, env) = schema_bound_create(current, Some([0xaa; 8]));
    let op = decode_envelope(&env).unwrap();
    eb.park_op(&mut dbb, &env, &op, "TaskCreate", older)
        .unwrap();
    assert_eq!(parked_rows(&dbb)[0].0, "unknown_kind", "the premise");
    assert!(eb.replay_parked_ops(&mut dbb).is_empty());
    assert_eq!(
        parked_rows(&dbb),
        vec![(
            "schema_fp_mismatch".into(),
            "TaskCreate".into(),
            i64::from(DOC_SCHEMA_V)
        )]
    );
    assert!(eb.query(&dbb, Query::EntityById(task)).is_err());

    let (_ea, eb, mut dbb, task, env) = schema_bound_create(current, doc_schema_fp_prefix(current));
    let op = decode_envelope(&env).unwrap();
    eb.park_op_as(
        &mut dbb,
        &env,
        &op,
        sunrise_storage::ParkReason::SchemaFpMismatch,
        "TaskCreate",
        older,
    )
    .unwrap();
    let events = eb.replay_parked_ops(&mut dbb);
    assert!(events
        .iter()
        .any(|e| matches!(e, DomainEvent::Created(r) if *r == task)));
    assert_eq!(read_task_t(&eb, &dbb, task).title, "schema bound");
    assert!(parked_rows(&dbb).is_empty(), "the marker is released");
}
