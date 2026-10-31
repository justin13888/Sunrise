//! Per-device cursors and the op run they read over.

use super::*;

/// The relay filters its replay by this number, so it has to mean "I have
/// everything through n". A hole must hold it back — otherwise the frame
/// carrying the missing op is skipped on the next subscribe and the hole
/// becomes permanent and invisible.
#[test]
fn a_cursor_stops_at_a_hole_rather_than_tracking_the_maximum() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    // Three ops from A, seqs 1..3 on the inbox stream.
    let mut envs = Vec::new();
    for title in ["one", "two", "three"] {
        let res = ea
            .apply(
                &mut dba,
                Command::CreateTask(TaskDraft {
                    title: title.into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        envs.push(env_bytes(&dba, &res.op_id));
    }
    let head = sunrise_cbor::decode_envelope_header(&envs[0]).unwrap();
    let (stream, device) = (head.stream_id, head.device_id);

    eb.apply_remote(&mut dbb, &envs[0]).unwrap();
    assert_eq!(cursor_for(&dbb, &stream, &device), 1);

    // Seq 2 is "lost"; seq 3 arrives. The cursor must stay at 1.
    eb.apply_remote(&mut dbb, &envs[2]).unwrap();
    assert_eq!(
        cursor_for(&dbb, &stream, &device),
        1,
        "a max-based cursor would claim 3 and strand seq 2 forever"
    );

    // The hole is filled: the prefix jumps to cover everything at once.
    eb.apply_remote(&mut dbb, &envs[1]).unwrap();
    assert_eq!(cursor_for(&dbb, &stream, &device), 3);
}

/// A device has certainly applied its own ops, so its own cursor belongs in
/// `sync_cursors` — otherwise every Subscribe claims nothing about this
/// device and the relay replays its whole history back to it.
#[test]
fn a_local_commit_advances_this_devices_own_cursor() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "mine".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let head = sunrise_cbor::decode_envelope_header(&env_bytes(&dba, &res.op_id)).unwrap();
    assert_eq!(cursor_for(&dba, &head.stream_id, &head.device_id), head.seq);
}

proptest::proptest! {
    // Each case builds two engines and two databases and delivers up to 18
    // envelopes, so the case count is deliberately low.
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/cursors.txt",
            ),
        )),
        ..proptest::prelude::ProptestConfig::with_cases(24)
    })]

    /// `docs/05-sync/wire-protocol.md:447-453` calls the contiguous-prefix
    /// cursor "a correctness invariant": a `MAX(seq)` cursor "converts an
    /// accidental, self-healing data-loss window into a permanent and
    /// silent one", because the relay filters its replay by exactly this
    /// number and would skip the frame carrying the hole forever.
    ///
    /// One three-op example enforced it. This delivers an arbitrary noisy
    /// run — duplicates, repeats, holes at any position — followed by a
    /// shuffled permutation of every op, and asserts after **every single
    /// delivery** that the cursor equals the length of the contiguous
    /// prefix actually applied, never the maximum seq seen. Once every op
    /// is in, the run left no chain evidence and the two digests agree.
    #[test]
    fn a_cursor_equals_the_contiguous_prefix_after_every_delivery(
        schedule in delivery_schedule(),
    ) {
        begin_case();
        let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dba = db_root(ROOT);
        let mut dbb = db_root(ROOT);
        trust(&eb, &mut dbb, &ea);

        let mut envs = Vec::with_capacity(DELIVERY_OPS);
        for n in 0..DELIVERY_OPS {
            let res = ea
                .apply(
                    &mut dba,
                    Command::CreateTask(TaskDraft {
                        title: format!("op {n}"),
                        ..Default::default()
                    }),
                )
                .unwrap();
            envs.push(env_bytes(&dba, &res.op_id));
        }
        let heads: Vec<_> = envs
            .iter()
            .map(|e| sunrise_cbor::decode_envelope_header(e).unwrap())
            .collect();
        let (stream, device) = (heads[0].stream_id, heads[0].device_id);
        proptest::prop_assert_eq!(
            heads.iter().map(|h| h.seq).collect::<Vec<_>>(),
            (1..=DELIVERY_OPS as u64).collect::<Vec<_>>(),
            "the fixture assumes seqs 1..=N on one stream from one device"
        );

        let mut applied = BTreeSet::new();
        for idx in schedule {
            eb.apply_remote(&mut dbb, &envs[idx]).unwrap();
            applied.insert(heads[idx].seq);

            let mut prefix = 0u64;
            while applied.contains(&(prefix + 1)) {
                prefix += 1;
            }
            proptest::prop_assert_eq!(
                cursor_for(&dbb, &stream, &device),
                prefix,
                "after applying {:?} the cursor must claim {} (a max-based \
                     cursor would claim {})",
                applied,
                prefix,
                applied.iter().copied().max().unwrap_or(0)
            );
        }

        proptest::prop_assert_eq!(
            cursor_for(&dbb, &stream, &device),
            DELIVERY_OPS as u64,
            "every op arrived, so the prefix covers all of them"
        );
        // Chain links are evidence whatever order they arrive in
        // (`engine::chain`): an honest history delivered with duplicates,
        // repeats and holes leaves no fork, no divergence and nothing
        // wanted, and the receiver's stream digest is the writer's.
        proptest::prop_assert_eq!(eb.chain_integrity(&dbb).unwrap(), crate::ChainIntegrity::default());
        proptest::prop_assert_eq!(
            op_chain::digest_of(&eb, &mut dbb, &stream),
            op_chain::digest_of(&ea, &mut dba, &stream)
        );
    }
}

/// Put bare `ops` rows for `(stream, device)` at each of `seqs`.
///
/// `ops_run_end` reads only `stream_id`, `device_id` and `seq`, so the rest of
/// the row is filler that satisfies the schema's `NOT NULL` columns.
fn put_op_rows(db: &Db, stream: &[u8; 16], device: &[u8; 16], seqs: &[u64]) {
    for &seq in seqs {
        db.conn()
            .execute(
                "INSERT INTO ops (op_id, stream_id, device_id, seq, ts_ms, envelope,
                                  inner_kind, target_kind, received_at)
                 VALUES (?, ?, ?, ?, 0, x'', 'test', 'test', 0)",
                params![
                    &remote_op_id(stream, device, seq)[..],
                    &stream[..],
                    &device[..],
                    i64::try_from(seq).unwrap()
                ],
            )
            .unwrap();
    }
}

/// `ops_run_end` for `(stream, device)` from `start`, in a transaction of its
/// own. An `Err` is returned rather than unwrapped, because one mutation of
/// the SQL fails as `InvalidColumnType` and the test should say which.
fn run_end(db: &mut Db, stream: &[u8; 16], device: &[u8; 16], start: i64) -> rusqlite::Result<i64> {
    let tx = db.conn_mut().transaction().unwrap();
    let end = ops_run_end(&tx, stream, device, start);
    tx.rollback().unwrap();
    end
}

const RUN_S: [u8; 16] = [0x51; 16];

const RUN_S2: [u8; 16] = [0x52; 16];

const RUN_D1: [u8; 16] = [0xd1; 16];

const RUN_D2: [u8; 16] = [0xd2; 16];

/// A run ends per `(stream_id, device_id)`, not per stream: another device's
/// seqs on the same stream neither extend this device's run nor cut it short.
///
/// Issue #294. `ops_run_end` names `stream_id = ?1 AND device_id = ?2` three
/// times — the `EXISTS` probe, the `o` scan, and the `n` successor anti-join —
/// and every cursor assertion elsewhere in this file observes a stream fed by
/// one device, so none of them could tell the difference. Each case below
/// names the mutation it catches; each was established by making the mutation
/// and watching the assertion fail.
///
/// The `o` scan's pair is the exception, and deliberately so: dropping it alone
/// is an equivalent mutant. The probe has established that this device holds
/// `start`, so its run `start..=e` exists. A foreign row at any `s` in
/// `start..e` has this device's `s + 1` as a successor and is excluded; one at
/// `e` is the same candidate as this device's own; one above `e` loses to it
/// in `MIN`. The pair bounds the scan, not the answer. What an `o`-side test
/// can catch is the `(stream)`-scoped run end — both pairs dropped together —
/// and the first assertion here is that one.
#[test]
fn ops_run_end_is_scoped_per_device_on_a_shared_stream() {
    let mut db = db();
    put_op_rows(&db, &RUN_S, &RUN_D1, &[1, 2, 3]);
    put_op_rows(&db, &RUN_S, &RUN_D2, &[4]);

    // A stream-scoped run end reads 4 here; dropping only the anti-join's
    // `device_id` excludes 3 (D2's 4 is its successor) and every other
    // candidate, so `MIN` is NULL and the read fails as `InvalidColumnType`.
    assert_eq!(
        run_end(&mut db, &RUN_S, &RUN_D1, 1),
        Ok(3),
        "D1's run is 1..=3 whatever D2 holds on the same stream"
    );
    // Dropping the probe's `device_id` lets D1's seq 1 answer for D2, and the
    // run is then read from D2's rows alone: 4, a cursor past three holes.
    assert_eq!(
        run_end(&mut db, &RUN_S, &RUN_D2, 1),
        Ok(0),
        "D2 holds no seq 1, so its run from 1 is empty"
    );

    // And the cursor `upsert_sync_cursor` writes carries the same answer.
    let tx = db.conn_mut().transaction().unwrap();
    upsert_sync_cursor(&tx, &RUN_S, &RUN_D1, 0).unwrap();
    upsert_sync_cursor(&tx, &RUN_S, &RUN_D2, 0).unwrap();
    tx.commit().unwrap();
    assert_eq!(cursor_for(&db, &RUN_S, &RUN_D1), 3);
    assert_eq!(cursor_row(&db, &RUN_S, &RUN_D2), Some(0));
}

/// The anti-join's mutation is not only a failed read: where this device holds
/// a later seq past a hole, it is a cursor past the hole, which the relay's
/// replay filter honours by never re-sending the missing op.
#[test]
fn ops_run_end_does_not_cross_a_hole_another_device_fills() {
    let mut db = db();
    put_op_rows(&db, &RUN_S, &RUN_D1, &[1, 2, 3, 5]);
    put_op_rows(&db, &RUN_S, &RUN_D2, &[4]);

    // With the anti-join unscoped by device, D2's 4 excludes D1's 3 and the
    // next candidate is D1's 5: the run would claim D1's missing 4.
    assert_eq!(
        run_end(&mut db, &RUN_S, &RUN_D1, 1),
        Ok(3),
        "D1 is missing its own seq 4, and D2's seq 4 does not stand in for it"
    );
}

/// The same scoping on the other axis: one device's seqs on another stream
/// neither extend nor cut short its run on this one.
#[test]
fn ops_run_end_is_scoped_per_stream_for_one_device() {
    let mut db = db();
    put_op_rows(&db, &RUN_S, &RUN_D1, &[1, 2]);
    put_op_rows(&db, &RUN_S2, &RUN_D1, &[3]);

    // Dropping the anti-join's `stream_id` makes S2's 3 the successor of S's
    // 2, and `MIN` over the empty remainder fails the read.
    assert_eq!(
        run_end(&mut db, &RUN_S, &RUN_D1, 1),
        Ok(2),
        "D1's run on S is 1..=2 whatever it holds on S2"
    );
    // Dropping the probe's `stream_id` lets S's seq 1 answer for S2.
    assert_eq!(
        run_end(&mut db, &RUN_S2, &RUN_D1, 1),
        Ok(0),
        "D1 holds no seq 1 on S2"
    );
}

/// `start` other than 1, which `upsert_sync_cursor` never passes: the run is
/// read from `start` upward, and an absent `start` is `start - 1` whatever
/// lies above it.
#[test]
fn ops_run_end_reads_from_any_start() {
    let mut db = db();
    put_op_rows(&db, &RUN_S, &RUN_D1, &[1, 2, 3, 5, 6]);
    put_op_rows(&db, &RUN_S, &RUN_D2, &[4, 7]);

    assert_eq!(run_end(&mut db, &RUN_S, &RUN_D1, 1), Ok(3));
    assert_eq!(run_end(&mut db, &RUN_S, &RUN_D1, 3), Ok(3), "a run of one");
    assert_eq!(
        run_end(&mut db, &RUN_S, &RUN_D1, 4),
        Ok(3),
        "D1 holds no seq 4, and D2's does not count"
    );
    assert_eq!(
        run_end(&mut db, &RUN_S, &RUN_D1, 5),
        Ok(6),
        "the run above the hole ends at 6, not at D2's 7"
    );
    assert_eq!(run_end(&mut db, &RUN_S, &RUN_D2, 4), Ok(4));
}
