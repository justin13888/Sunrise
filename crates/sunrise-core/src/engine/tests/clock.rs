//! The hybrid clock: restored at open, its drift window and the stamp observed.

use super::*;

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/clock.txt",
            ),
        )),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// Any epoch-ms in a sane range survives the `ms_to_ts` ->
    /// `as_millisecond` round-trip that bridges SQLite INTEGER storage
    /// and `jiff::Timestamp`.
    #[test]
    fn ms_timestamp_round_trip(ms in -10_000_000_000_000i64..=10_000_000_000_000i64) {
        let ts = ms_to_ts(ms);
        proptest::prop_assert_eq!(ts.as_millisecond(), ms);
    }
}

/// **A restart must not make a device emit beneath its own ops** (#112).
///
/// `MonotonicHlc` starts at `Hlc::default()`, so before
/// [`Engine::prime_hlc`] a reopened vault's clock was the bare wall clock.
/// That is not the same thing as where the device left off: `Hlc::receive`
/// admits a peer up to `MAX_DRIFT_MS` ahead, and `Hlc::send` keeps that
/// lead until local time catches up. Four minutes is inside the window and
/// is what B's clock leads by here.
///
/// The consequence is not a tie for `seq` to break. `lww_wins` compares
/// `hlc` first, so A's *second* edit sorts below its first and every
/// replica that merges both keeps the first — while A, which runs no LWW
/// gate on its own writes, keeps the second. The last assertion is that
/// divergence, made concrete on B.
#[test]
fn a_restart_does_not_make_this_device_emit_beneath_its_own_ops() {
    const LEAD_MS: u64 = 4 * 60 * 1000;

    let a_clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], a_clock.clone());
    let eb = engine_seeded(
        ROOT,
        [2u8; 32],
        Arc::new(FakeClock(PLMutex::new(T0 + LEAD_MS))),
    );
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    // B, whose clock leads, creates a task. A absorbs it and with it B's
    // stamp: A's HLC is now four minutes above A's own wall clock.
    let task = eb
        .apply(
            &mut dbb,
            Command::CreateTask(TaskDraft {
                title: "from b".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let create_env = create_env_for(&dbb, task.bytes());
    ea.apply_remote_all(&mut dba, &create_env).unwrap();
    assert!(
        ea.hlc.peek().physical_ms >= T0 + LEAD_MS,
        "A has adopted the leading stamp it just absorbed"
    );

    // A edits it, carrying that lead into its own op.
    ea.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("edited before the restart".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let before = task_stamp(&dba, task.bytes());
    assert!(before.physical_ms >= T0 + LEAD_MS);

    // The restart: a fresh `Engine` over the same database, which is what
    // `Core::open` constructs — and, like `Core::open`, primed from the log.
    let ea2 = engine_seeded(ROOT, [1u8; 32], a_clock);
    ea2.prime_hlc(&dba).unwrap();
    ea2.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("edited after the restart".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let after = task_stamp(&dba, task.bytes());

    assert!(
        after > before,
        "a device's own later op must sort after its earlier one across a \
             restart: before={before:?} after={after:?}"
    );

    // And the whole point of that: B, merging both of A's edits under LWW,
    // must end on the later one.
    for env in update_envs_for(&dba, task.bytes()) {
        eb.apply_remote_all(&mut dbb, &env).unwrap();
    }
    assert_eq!(
        task_title(&dbb, task.bytes()),
        "edited after the restart",
        "the peer must keep A's newer edit, not silently discard it"
    );
    assert_eq!(
        task_title(&dba, task.bytes()),
        task_title(&dbb, task.bytes()),
        "and the two replicas must agree"
    );
}

/// The logical half has to come back too, which is why `prime_hlc` decodes
/// envelopes instead of reading `MAX(ts_ms)` and stopping there.
///
/// Nothing here involves a peer or a skewed clock: one device, one stalled
/// millisecond, several ops. `Hlc::send` puts them at `(t, 1)`, `(t, 2)`,
/// `(t, 3)`; a restart primed only from `ts_ms` would resume at `(t, 0)`
/// and emit `(t, 1)` again — below the ops already stamped, and an
/// inversion rather than the tie `seq` breaks.
#[test]
fn priming_restores_the_logical_half_not_only_the_physical_one() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], clock.clone());
    let mut dba = db_root(ROOT);

    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "one".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    for title in ["two", "three"] {
        ea.apply(
            &mut dba,
            Command::UpdateTask {
                id: task,
                patch: TaskPatch {
                    title: Some((*title).to_string()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    }
    let before = task_stamp(&dba, task.bytes());
    assert_eq!(before.physical_ms, T0, "the wall clock never moved");
    assert!(
        before.logical > 0,
        "the ops are separated by the logical counter alone"
    );

    let ea2 = engine_seeded(ROOT, [1u8; 32], clock);
    ea2.prime_hlc(&dba).unwrap();
    ea2.apply(
        &mut dba,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("four".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let after = task_stamp(&dba, task.bytes());

    assert_eq!(after.physical_ms, before.physical_ms);
    assert!(
        after.logical > before.logical,
        "the counter must resume, not restart: before={before:?} after={after:?}"
    );
}

/// #21: before HLCs, a device whose wall clock ran fast won every conflict
/// it ever entered, permanently. Its peer could re-edit the value a hundred
/// times and every one of those edits would lose to the same stale op,
/// because the peer's honest `ts_ms` was still the smaller number. That is
/// not a tie-break, it is a veto, and nothing in the system announced it.
///
/// An HLC removes the veto because it is not a clock reading, it is a
/// causal position. B cannot edit a Task it has never seen, so by the time
/// B edits, B has ALREADY absorbed A's stamp and sits above it — whatever
/// A's wall clock says. The fast device gets no advantage it did not earn
/// by writing first.
#[test]
fn a_fast_clock_does_not_win_every_conflict() {
    // A is four minutes fast — inside the drift window, so its ops are
    // legitimate, just early.
    let skew = 4 * 60 * 1000;
    let ca = Arc::new(FakeClock(PLMutex::new(T0 + skew)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let created = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "orig".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
        .unwrap();

    // Both edit without seeing the other's edit. A's wall clock reads four
    // minutes later than B's throughout.
    let a_up = ea
        .apply(
            &mut dba,
            Command::UpdateTask {
                id: created.entity,
                patch: TaskPatch {
                    title: Some("from-the-fast-device".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let b_up = eb
        .apply(
            &mut dbb,
            Command::UpdateTask {
                id: created.entity,
                patch: TaskPatch {
                    title: Some("from-the-slow-device".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
        .unwrap();
    ea.apply_remote(&mut dba, &env_bytes(&dbb, &b_up.op_id))
        .unwrap();

    // B's edit is causally later — it was made from a replica that had
    // seen A's create — so it wins, on BOTH replicas. Under the old
    // `(ts_ms, device_id)` rule A's four-minute lead would have won here
    // and gone on winning every subsequent round.
    assert_eq!(
        read_task_t(&eb, &dbb, created.entity).title,
        "from-the-slow-device"
    );
    assert_eq!(
        read_task_t(&ea, &dba, created.entity).title,
        "from-the-slow-device",
        "the fast clock must not hold a permanent veto over its peer"
    );

    // And the reverse still works: A edits again, having seen B's op, and
    // A's edit now wins. Neither device is stuck losing either.
    let a_again = ea
        .apply(
            &mut dba,
            Command::UpdateTask {
                id: created.entity,
                patch: TaskPatch {
                    title: Some("and-back-to-a".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_again.op_id))
        .unwrap();
    assert_eq!(
        read_task_t(&eb, &dbb, created.entity).title,
        "and-back-to-a"
    );
}

/// ...and a clock that is not merely fast but wrong is refused outright,
/// so it cannot drag its peers' clocks along with it.
#[test]
fn an_op_beyond_the_drift_window_is_refused() {
    let ca = Arc::new(FakeClock(PLMutex::new(
        T0 + sunrise_cbor::hlc::MAX_DRIFT_MS + 1,
    )));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca);
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let created = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "from the future".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    let err = eb
        .apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
        .unwrap_err();
    assert!(
        matches!(&err, EngineError::RemoteOpInvalid(m) if m.contains("hlc")),
        "expected an HLC drift rejection, got {err:?}"
    );

    // Refused means refused: nothing was materialized and nothing was
    // recorded in the op log.
    let tasks: i64 = dbb
        .conn()
        .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tasks, 0);
    let ops: i64 = dbb
        .conn()
        .query_row("SELECT COUNT(*) FROM ops", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ops, 0);
    // And the cursor did not move, which is the half `upsert_sync_cursor`'s
    // doc asserts in prose — "a refused op is *not* decided and does not
    // appear here" — and nothing else in this suite held. This refusal is
    // the pre-insert kind: the drift gate runs before the op-log insert, so
    // `upsert_sync_cursor` is never reached at all. Counting rows for A
    // rather than reading one stream's cursor keeps the assertion true
    // whatever stream the command routed to.
    let cursors_for_a: i64 = dbb
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sync_cursors WHERE device_id = ?",
            params![&ea.keychain.device_id()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        cursors_for_a, 0,
        "a refused op left this replica claiming to have applied one"
    );
}

/// A peer that has been offline for a week is not a clock problem: its ops
/// are far in the PAST, which is legitimate and must be accepted.
#[test]
fn an_op_from_long_ago_is_accepted() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0 - 7 * 24 * 3_600_000)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca);
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let created = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "written a week ago".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
        .expect("an op from the past is not a drift violation");
    assert_eq!(
        read_task_t(&eb, &dbb, created.entity).title,
        "written a week ago"
    );
}

/// Every replica must record the SENDER's stamp, not its own post-merge
/// reading — otherwise the LWW winner would depend on delivery order.
#[test]
fn the_receiver_stores_the_senders_stamp() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0 + 60_000)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca);
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let created = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "t".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
        .unwrap();

    let read = |db: &Db| -> (i64, i64, i64) {
        db.conn()
            .query_row(
                "SELECT lww_hlc_ms, lww_hlc_logical, lww_seq FROM tasks WHERE id = ?",
                params![&created.entity.bytes()[..]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    };
    assert_eq!(
        read(&dba),
        read(&dbb),
        "both replicas must stamp the row identically"
    );
    assert_eq!(
        read(&dbb).0,
        T0 as i64,
        "B stored A's physical clock, not its own"
    );
}
