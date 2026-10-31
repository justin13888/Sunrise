//! Last-writer-wins: the stamp order and what each op stamps on its row.

use super::*;

#[test]
fn lww_earlier_remote_update_loses_to_later_local() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    // A creates; B receives the create.
    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "orig".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &res.op_id))
        .unwrap();

    // B updates locally at a LATER ts.
    set_clock(&cb, T0 + 10_000);
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: res.entity,
            patch: TaskPatch {
                title: Some("B-late".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // A updates locally at an EARLIER ts, then that op arrives at B.
    set_clock(&ca, T0 + 5_000);
    let a_up = ea
        .apply(
            &mut dba,
            Command::UpdateTask {
                id: res.entity,
                patch: TaskPatch {
                    title: Some("A-early".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let ev = eb
        .apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
        .unwrap();
    // Op is applied (recorded + event emitted) but LWW keeps B's later value.
    assert!(matches!(ev, Some(DomainEvent::Updated(_))));
    assert_eq!(read_task_t(&eb, &dbb, res.entity).title, "B-late");
    // The losing op is still recorded in the op log.
    assert_eq!(op_count(&dbb), 3, "create + local update + remote update");
}

/// **A remote write that loses LWW keeps its op row, and the cursor counts it.**
///
/// `materialize_remote` refuses a losing write by returning `Ok(())` before it
/// touches the entity row. That is the entity path's refusal below
/// `apply_remote_all`'s idempotence gate, and `upsert_sync_cursor`'s doc says
/// every refusal there declines a write and never the delivery: the op row
/// went in first, so it stays, and step g counts it.
///
/// `lww_earlier_remote_update_loses_to_later_local` confirms the losing op
/// only by a total row count, and reaches it through `apply_remote`. This
/// holds the premise itself for the entity path, through `apply_remote_all`:
/// the op row is at the losing op's own `(stream, device, seq)`, and the
/// cursor for that sender covers it. The control path's two refusals are held
/// the same way by `a_self_refused_revoke_still_advances_the_cursor` and
/// `a_read_bounded_senders_recipient_claim_is_refused_and_the_cursor_counts_the_op`.
///
/// Turning the early return into an error -- the shape
/// `.github/scripts/apply-refusal-gate.py` rejects statically (#298) -- rolls
/// the transaction back, and this delivery then fails rather than applies.
#[test]
fn a_losing_lww_write_keeps_its_op_row_and_is_counted() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let created = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "orig".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    eb.apply_remote_all(&mut dbb, &env_bytes(&dba, &created.op_id))
        .expect("the create applies");

    // B's own later write is the one the remote op has to lose to.
    set_clock(&cb, T0 + 10_000);
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: created.entity,
            patch: TaskPatch {
                title: Some("B-late".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    set_clock(&ca, T0 + 5_000);
    let loser = ea
        .apply(
            &mut dba,
            Command::UpdateTask {
                id: created.entity,
                patch: TaskPatch {
                    title: Some("A-early".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let env = env_bytes(&dba, &loser.op_id);
    let head = sunrise_cbor::decode_envelope_header(&env).unwrap();
    assert_eq!(
        cursor_for(&dbb, &head.stream_id, &head.device_id),
        head.seq - 1,
        "going in, B's cursor for A covers exactly the ops below the loser"
    );

    eb.apply_remote_all(&mut dbb, &env)
        .expect("a write that loses LWW is still a well-formed delivery");

    // The refusal: the entity row is B's.
    assert_eq!(read_task_t(&eb, &dbb, created.entity).title, "B-late");
    // What it did not take: the op row, and the cursor that counts it.
    assert_eq!(
        ops_at(dbb.conn(), &head.stream_id, &head.device_id, head.seq),
        1,
        "the refusal is the entity row's, not the delivery's: the op row is in"
    );
    assert_eq!(
        cursor_for(&dbb, &head.stream_id, &head.device_id),
        head.seq,
        "`upsert_sync_cursor` runs at step g with that row in the log, so the \
         contiguous prefix covers it"
    );
}

#[test]
fn lww_later_remote_update_wins_over_earlier_local() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "orig".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &res.op_id))
        .unwrap();

    // B updates locally at an EARLIER ts.
    set_clock(&cb, T0 + 5_000);
    eb.apply(
        &mut dbb,
        Command::UpdateTask {
            id: res.entity,
            patch: TaskPatch {
                title: Some("B-early".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // A updates at a LATER ts; that op wins on B.
    set_clock(&ca, T0 + 10_000);
    let a_up = ea
        .apply(
            &mut dba,
            Command::UpdateTask {
                id: res.entity,
                patch: TaskPatch {
                    title: Some("A-late".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
        .unwrap();
    assert_eq!(read_task_t(&eb, &dbb, res.entity).title, "A-late");
}

/// Regression: a device's own successive ops must not lose to each other.
///
/// The device-id memcmp breaks *cross-device* ties. Applied to one device's
/// own ops it made the later op lose (`dev > dev` is false), so remote
/// replicas silently dropped it while the originating replica kept it —
/// permanent divergence with no error. Creating a task and patching it in
/// the same millisecond is enough to trigger it, which is exactly what the
/// blocker-convergence e2e was hitting about one run in six.
///
/// The HLC makes the equal-stamp case unreachable while a device's clock
/// state lives, but it is still reachable across a process restart (the
/// logical counter resets to 0), so the guarantee is asserted directly.
#[test]
fn same_device_ops_at_one_hlc_do_not_lose_to_each_other() {
    let dev = [7u8; 16];
    assert!(
        lww_wins(&stamp(1000, 0, dev, 2), &row_of(&stamp(1000, 0, dev, 1))),
        "a device's later op must win over its own earlier op at an equal HLC"
    );
    // Cross-device ties are unaffected: still decided by memcmp.
    let lower = [1u8; 16];
    let higher = [9u8; 16];
    assert!(lww_wins(
        &stamp(1000, 0, higher, 1),
        &row_of(&stamp(1000, 0, lower, 1))
    ));
    assert!(!lww_wins(
        &stamp(1000, 0, lower, 1),
        &row_of(&stamp(1000, 0, higher, 1))
    ));
    // And a real HLC difference still dominates the device comparison.
    assert!(!lww_wins(
        &stamp(999, 0, higher, 9),
        &row_of(&stamp(1000, 0, lower, 1))
    ));
    assert!(lww_wins(
        &stamp(1001, 0, lower, 1),
        &row_of(&stamp(1000, 0, higher, 9))
    ));
    // The logical component orders inside one millisecond, which a bare
    // wall clock could not do at all.
    assert!(lww_wins(
        &stamp(1000, 1, lower, 1),
        &row_of(&stamp(1000, 0, higher, 1))
    ));
}

/// `seq` is the LAST term, not the first: a lower-seq op from a device
/// whose HLC is ahead still wins.
#[test]
fn seq_never_overrides_the_hlc() {
    let a = [1u8; 16];
    let b = [2u8; 16];
    assert!(lww_wins(
        &stamp(2000, 0, a, 1),
        &row_of(&stamp(1000, 0, b, 9999))
    ));
}

/// A placeholder row that no op has stamped loses to everything.
#[test]
fn an_unstamped_row_loses_to_any_real_op() {
    let row = RowLww {
        hlc: Hlc::default(),
        device: None,
        seq: 0,
    };
    assert!(lww_wins(&stamp(0, 0, [0u8; 16], 0), &row));
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/lww.txt",
            ),
        )),
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// `docs/05-sync/conflict-resolution.md` specifies this comparator as a
    /// deterministic total order over `(hlc, device, seq)` — "memcmp,
    /// higher wins … so every replica picks the same winner". Five
    /// hand-picked examples enforced particular outcomes; nothing asserted
    /// it was an order at all, and each of the three laws fails in its own
    /// way:
    ///
    /// * not antisymmetric — both replicas decide their own write won, and
    ///   the two rows diverge permanently with no error anywhere;
    /// * not transitive — the surviving state depends on the order three
    ///   ops happened to arrive in, which is exactly what LWW exists to
    ///   remove;
    /// * not total — neither op wins, so whichever row each replica
    ///   already had stays.
    ///
    /// Antisymmetry is stated over *distinct* stamps only. Equal stamps
    /// deliberately win in both directions (`incoming.seq >= row.seq`):
    /// an equal stamp from the same device is the same op re-delivered, and
    /// letting the replay rewrite the row unchanged is what keeps
    /// re-delivery a harmless no-op. Totality and transitivity are stated
    /// over everything, equal stamps included.
    #[test]
    fn lww_is_a_total_order_and_antisymmetric_on_distinct_stamps(
        a in arb_stamp(),
        b in arb_stamp(),
        c in arb_stamp(),
    ) {
        let wins = |x: &LwwStamp, y: &LwwStamp| lww_wins(x, &row_of(y));

        proptest::prop_assert!(
            wins(&a, &b) || wins(&b, &a),
            "neither {a:?} nor {b:?} wins, so each replica keeps the row it \
                 already had and the two never converge"
        );

        if a != b {
            proptest::prop_assert_ne!(
                wins(&a, &b),
                wins(&b, &a),
                "both orderings agree for distinct stamps {:?} / {:?}, so the \
                     winner depends on which replica is asking",
                a,
                b
            );
        }

        if wins(&a, &b) && wins(&b, &c) {
            proptest::prop_assert!(
                wins(&a, &c),
                "{a:?} beats {b:?} beats {c:?} but loses to {c:?}, so the \
                     survivor depends on the order the three ops arrived in"
            );
        }
    }

    /// `remote_op_id` had no direct test of any kind, and both of its
    /// invariants are load-bearing:
    ///
    /// * **determinism** is what makes a re-delivered op compute the op-log
    ///   primary key it already has, so the `UNIQUE(stream, device, seq)`
    ///   gate sees it as the same row and the replay is a no-op;
    /// * **injectivity** is the other half — two different ops that derived
    ///   the same id would alias onto one op-log row, and whichever arrived
    ///   second would be silently swallowed as a duplicate.
    ///
    /// Both hold because the three fields are fixed-width (16 + 16 + 8), so
    /// the derivation's message determines the triple; a length-prefixed or
    /// variable-width encoding is where this stops being true.
    #[test]
    fn remote_op_id_is_deterministic_and_injective(
        (sa, da, qa) in arb_op_id_triple(),
        (sb, db_id, qb) in arb_op_id_triple(),
    ) {
        proptest::prop_assert_eq!(
            remote_op_id(&sa, &da, qa),
            remote_op_id(&sa, &da, qa),
            "the same triple derived two different ids, so a re-delivered op \
                 would be recorded twice"
        );

        let same_triple = (sa, da, qa) == (sb, db_id, qb);
        proptest::prop_assert_eq!(
            remote_op_id(&sa, &da, qa) == remote_op_id(&sb, &db_id, qb),
            same_triple,
            "({:?}, {:?}, {}) and ({:?}, {:?}, {}) must share an op id exactly \
                 when they are the same op",
            sa,
            da,
            qa,
            sb,
            db_id,
            qb
        );
    }
}

#[test]
fn lww_tie_break_higher_device_wins_both_directions() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let da = ea.keychain.device_id();
    let db_id = eb.keychain.device_id();
    assert_ne!(da, db_id);
    // Winner's title: the op from the lexicographically-higher device id.
    let (winner_title, a_wins) = if da.as_slice() > db_id.as_slice() {
        ("A-tie", true)
    } else {
        ("B-tie", false)
    };

    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    // Mutual trust so each side can apply the other's op.
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    // A creates; both sides get the create.
    let res = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "orig".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let create_env = env_bytes(&dba, &res.op_id);
    eb.apply_remote(&mut dbb, &create_env).unwrap();

    // Both update the SAME task at the SAME ts (a tie).
    set_clock(&ca, T0 + 1_000);
    set_clock(&cb, T0 + 1_000);
    let a_up = ea
        .apply(
            &mut dba,
            Command::UpdateTask {
                id: res.entity,
                patch: TaskPatch {
                    title: Some("A-tie".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    let b_up = eb
        .apply(
            &mut dbb,
            Command::UpdateTask {
                id: res.entity,
                patch: TaskPatch {
                    title: Some("B-tie".into()),
                    ..Default::default()
                },
            },
        )
        .unwrap();

    // Exchange the competing updates both ways.
    eb.apply_remote(&mut dbb, &env_bytes(&dba, &a_up.op_id))
        .unwrap();
    ea.apply_remote(&mut dba, &env_bytes(&dbb, &b_up.op_id))
        .unwrap();

    // Both replicas converge on the higher device's value, in both directions.
    assert_eq!(read_task_t(&ea, &dba, res.entity).title, winner_title);
    assert_eq!(read_task_t(&eb, &dbb, res.entity).title, winner_title);
    let _ = a_wins;
}

/// A command that emits TWO ops must stamp each row with the stamp of the
/// op that carries it. Stamping the routine row with the *task* op's stamp
/// left the origin holding different `lww_*` columns from every replica —
/// which merges identically today and resolves the NEXT concurrent edit
/// differently on each side.
#[test]
fn a_two_op_command_stamps_each_row_with_its_own_ops_stamp() {
    let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let (rid, occ) = seed_routine(&ea, &mut dba);
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
        .unwrap();
    eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
        .unwrap();

    set_clock(&ca, (occ[0].1 + 3_600_000) as u64);
    set_clock(&cb, (occ[0].1 + 3_600_000) as u64);
    // One command, two ops: a task.update and the routine.update carrying
    // the streak advance.
    ea.apply(&mut dba, Command::CompleteTask(occ[0].0)).unwrap();
    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, occ[0].0.bytes(), "task.update"),
    )
    .unwrap();
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
        .unwrap();

    assert_eq!(
        row_stamp(&dba, "routines", "id", &rid),
        row_stamp(&dbb, "routines", "id", &rid),
        "the routine row's stamp must match on both replicas"
    );
    assert_eq!(
        row_stamp(&dba, "tasks", "id", &occ[0].0),
        row_stamp(&dbb, "tasks", "id", &occ[0].0),
        "the task row's stamp must match on both replicas"
    );
}

/// The same rule on the other two-op command: `SkipRoutineOccurrence`
/// emits a routine.update and a task.delete.
#[test]
fn skipping_an_occurrence_stamps_the_task_row_with_the_task_ops_stamp() {
    let ca = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let cb = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca);
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let (rid, occ) = seed_routine(&ea, &mut dba);
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.create"))
        .unwrap();
    eb.apply_remote(&mut dbb, &create_env_for(&dba, occ[0].0.bytes()))
        .unwrap();

    let key = occurrence_key_at("UTC", ms_to_ts(occ[0].1)).unwrap();
    ea.apply(
        &mut dba,
        Command::SkipRoutineOccurrence {
            id: rid,
            occurrence_key: key,
        },
    )
    .unwrap();
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, rid.bytes(), "routine.update"))
        .unwrap();
    eb.apply_remote(
        &mut dbb,
        &env_for_kind(&dba, occ[0].0.bytes(), "task.delete"),
    )
    .unwrap();

    assert_eq!(
        row_stamp(&dba, "tasks", "id", &occ[0].0),
        row_stamp(&dbb, "tasks", "id", &occ[0].0),
        "the tombstoned task's stamp must match on both replicas"
    );
    assert_eq!(
        row_stamp(&dba, "routines", "id", &rid),
        row_stamp(&dbb, "routines", "id", &rid),
    );
}
