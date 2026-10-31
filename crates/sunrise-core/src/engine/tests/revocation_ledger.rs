//! The revocation ledger: tie breaks, compaction and the per-sender cap.

use super::*;

/// Two devices revoke each other, and both replicas end up holding both
/// revocations whichever order they see them in.
///
/// This goes through `apply_remote` — real envelopes, the whole delivery
/// path — because the defect lived in a step the direct-`apply_control_op`
/// helpers skip entirely. The revocation gate refused *every* family from a
/// revoked sender, so a replica that applied B's revocation of A first then
/// refused A's revocation of B, and never learned of it: on that replica a
/// compromised device stays a member, silently, forever, while a replica
/// that saw them the other way round holds both.
#[test]
fn two_devices_revoking_each_other_converge_on_both_revocations() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], clock());
    let eb = engine_seeded(ROOT, [2u8; 32], clock());
    let x = engine_seeded(ROOT, [4u8; 32], clock());
    let y = engine_seeded(ROOT, [5u8; 32], clock());
    let (mut dba, mut dbb, mut dbx, mut dby) =
        (db_root(ROOT), db_root(ROOT), db_root(ROOT), db_root(ROOT));
    // Each of A and B knows the other, so each can revoke it; both replicas
    // know both.
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);
    for (e, db) in [(&x, &mut dbx), (&y, &mut dby)] {
        trust(e, db, &ea);
        trust(e, db, &eb);
    }
    let (a_id, b_id) = (ea.keychain.device_id(), eb.keychain.device_id());

    // The crossing: each revokes the other, neither having seen the other's op.
    let a_revokes_b = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, b_id),
                reason: RevokeReason::Compromised,
            },
        )
        .unwrap();
    let b_revokes_a = eb
        .apply(
            &mut dbb,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, a_id),
                reason: RevokeReason::Compromised,
            },
        )
        .unwrap();
    let env_ab = env_bytes(&dba, &a_revokes_b.op_id);
    let env_ba = env_bytes(&dbb, &b_revokes_a.op_id);

    // X sees B's op first; Y sees A's first.
    x.apply_remote(&mut dbx, &env_ba).unwrap();
    x.apply_remote(&mut dbx, &env_ab).unwrap();
    y.apply_remote(&mut dby, &env_ab).unwrap();
    y.apply_remote(&mut dby, &env_ba).unwrap();

    for (name, e, db) in [("X", &x, &dbx), ("Y", &y, &dby)] {
        assert!(
            e.is_revoked(db.conn(), &a_id).unwrap(),
            "{name} lost the revocation of A"
        );
        assert!(
            e.is_revoked(db.conn(), &b_id).unwrap(),
            "{name} lost the revocation of B"
        );
    }
    assert_eq!(
        revocation_row(&dbx, &a_id),
        revocation_row(&dby, &a_id),
        "and the two replicas agree about A"
    );
    assert_eq!(
        revocation_row(&dbx, &b_id),
        revocation_row(&dby, &b_id),
        "and about B"
    );
}

/// With an identical `(physical, logical)`, the higher revoker id wins —
/// the same direction `lww_wins` breaks a tie on `device`.
///
/// Nothing pinned this. The register's third key is the only thing standing
/// between two revocations minted in the same logical instant and a pair of
/// replicas that disagree about who revoked and why.
#[test]
fn an_exact_tie_is_broken_by_the_higher_revoker_id() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let one = engine_seeded(ROOT, [2u8; 32], clock());
    let two = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    // Device ids are derivations of a signing key, so which of the two is
    // greater is not something the test gets to choose.
    let (lower, higher) = if one.keychain.device_id() < two.keychain.device_id() {
        (&one, &two)
    } else {
        (&two, &one)
    };

    let at = Hlc {
        physical_ms: T0,
        logical: 3,
    };
    revoke_at(&r1, &mut db1, lower, t, at);
    revoke_at(&r1, &mut db1, higher, t, at);
    revoke_at(&r2, &mut db2, higher, t, at);
    revoke_at(&r2, &mut db2, lower, t, at);

    assert_eq!(revocation_row(&db1, &t), revocation_row(&db2, &t));
    assert_eq!(
        revocation_row(&db1, &t).unwrap().2.as_slice(),
        &higher.keychain.device_id()[..],
        "an exact tie resolves upwards on the revoker id, as `lww_wins` does"
    );
}

/// Two revocations inside one millisecond still converge, because the
/// register compares the HLC's logical half too.
#[test]
fn two_revocations_in_one_millisecond_converge_on_the_logical_half() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let a = engine_seeded(ROOT, [2u8; 32], clock());
    let b = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    let lo = Hlc {
        physical_ms: T0,
        logical: 1,
    };
    let hi = Hlc {
        physical_ms: T0,
        logical: 7,
    };
    revoke_at(&r1, &mut db1, &a, t, hi);
    revoke_at(&r1, &mut db1, &b, t, lo);
    revoke_at(&r2, &mut db2, &b, t, lo);
    revoke_at(&r2, &mut db2, &a, t, hi);

    assert_eq!(revocation_row(&db1, &t), revocation_row(&db2, &t));
    assert_eq!(
        revocation_row(&db1, &t).unwrap().2.as_slice(),
        &a.keychain.device_id()[..],
        "the greater logical half wins, so a physical-only compare is not enough"
    );
}

/// **One sender, one HLC, two targets: two ledger rows and two revocations.**
///
/// The ledger's primary key decides this, and it is the reason
/// `revoked_device_id` is in it. An HLC is monotonic per device only where
/// `MonotonicHlc` stamps it — a *peer's* stamp is whatever that peer wrote,
/// and nothing in the envelope ties it to that sender's `seq`, to its meta
/// epoch, or to any earlier stamp it sent. So two `device_revoke` ops from one
/// member at one `(physical, logical)` naming two different devices are two
/// ops, they both reach `apply_device_revoke`, and they must be two rows.
///
/// Keyed on `(op_hlc_ms, op_hlc_logical, sender)` alone they were one:
/// `INSERT OR IGNORE` kept whichever arrived first, so a replica that received
/// them in the other order folded a different register and the two never
/// reconciled. That is the divergence ADR-0034 corollary 3 forbids peer-side
/// enforcement from reintroducing, asserted here rather than deduced from the
/// schema.
#[test]
fn two_revocations_at_one_hlc_from_one_sender_are_two_ledger_rows() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let sender = engine_seeded(ROOT, [1u8; 32], clock());
    let first = engine_seeded(ROOT, [2u8; 32], clock());
    let second = engine_seeded(ROOT, [3u8; 32], clock());
    let er = engine_seeded(ROOT, [4u8; 32], clock());
    let mut db = db_root(ROOT);
    let (one, two) = (first.keychain.device_id(), second.keychain.device_id());

    let at = Hlc {
        physical_ms: T0,
        logical: 5,
    };
    revoke_at(&er, &mut db, &sender, one, at);
    revoke_at(&er, &mut db, &sender, two, at);

    assert_eq!(
        ledger_rows(&db),
        2,
        "two ops naming two devices must not collapse onto one ledger row"
    );
    assert!(
        er.is_revoked(db.conn(), &one).unwrap(),
        "the first target is revoked"
    );
    assert!(
        er.is_revoked(db.conn(), &two).unwrap(),
        "and so is the second: neither op is the other's duplicate"
    );
}

/// Every `device_revocations` row, ordered, for comparing two replicas' whole
/// registers rather than one device's entry.
fn whole_register(db: &Db) -> Vec<(Vec<u8>, RevocationRow)> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT device_id, cut_ms, cut_logical, revoked_by, reason
             FROM device_revocations ORDER BY device_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows
}

/// Every `device_revoke_ops` row, ordered.
fn whole_ledger(db: &Db) -> Vec<(i64, i64, Vec<u8>, Vec<u8>)> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare(
            "SELECT op_hlc_ms, op_hlc_logical, sender, revoked_device_id
             FROM device_revoke_ops
             ORDER BY op_hlc_ms, op_hlc_logical, sender, revoked_device_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows
}

/// **Repeating a revocation does not grow the ledger** (#250).
///
/// Before compaction every `device_revoke` op was a row for good, and the
/// whole ledger was re-folded on every applied op — so a device naming one
/// target over and over, with a fresh stamp each time, grew the table and the
/// per-op fold without bound. A revoked device can do it too: its ops are
/// stored unconditionally and gated only in the fold.
///
/// Both a believed sender and a gated one are exercised, and the register is
/// checked to be the one the greatest stamp would have produced, so this pins
/// the bound and not only a row count.
#[test]
fn repeating_a_revocation_keeps_one_ledger_row_per_pair() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // B revokes A fifty times, stamps alternating either side of T0: the
    // greatest is T0 + 48s (index 48), and index 49 arrives after it,
    // back-dated to T0 - 49s.
    for i in 0..50u64 {
        let at = if i % 2 == 0 {
            T0 + i * 1_000
        } else {
            T0 - i * 1_000
        };
        revoke(&er, &mut db, &eb, a_id, at);
    }
    assert_eq!(ledger_rows(&db), 1, "fifty ops on one pair are one row");
    assert_eq!(
        revocation_row(&db, &a_id).unwrap().0,
        i64::try_from(T0 + 48_000).unwrap(),
        "and it is the greatest stamp, which is the row the fold would pick"
    );

    // A, now revoked, names C fifty times. Every op is gated, and every op
    // is still one row.
    for i in 0..50u64 {
        revoke(&er, &mut db, &ea, c_id, T0 + 100_000 + i);
    }
    assert_eq!(
        ledger_rows(&db),
        2,
        "a gated sender repeating itself costs one row, not fifty"
    );
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "and compaction kept it gated: the pair survives, so the gate does"
    );
    assert!(er.is_revoked(db.conn(), &a_id).unwrap());
}

/// **Compaction changes no fold, and reaches rows written before it existed.**
///
/// Replica `full` has the whole op set written straight into the ledger — the
/// shape a vault that accumulated repeats before compaction holds — and then
/// takes one more op through the ordinary path, so its fold runs over every
/// row. Replica `live` takes every op through the ordinary path, in the
/// opposite order, and compacts at each one. If deleting a pair's lesser rows
/// could change a register, the two would disagree; they must hold the same
/// register and, afterwards, the same ledger.
///
/// The op set has a gated pair with repeats (A, revoked by B, naming C), a
/// believed pair whose repeats bracket the gate's other rows (B -> A), and a
/// believed pair that wins C with its greatest, not its first, stamp (D -> C).
#[test]
fn compaction_changes_no_fold_and_heals_an_uncompacted_ledger() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ee = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ef = engine_seeded(ROOT, [7u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let (a_id, c_id, f_id) = (
        ea.keychain.device_id(),
        ec.keychain.device_id(),
        ef.keychain.device_id(),
    );

    let ops: Vec<(&Engine, [u8; 16], u64)> = vec![
        (&eb, a_id, T0),
        (&ea, c_id, T0 + 60_000),
        (&eb, a_id, T0 - 30_000),
        (&ea, c_id, T0 + 10_000),
        (&ed, c_id, T0 + 1_000),
        (&eb, a_id, T0 + 120_000),
        (&ed, c_id, T0 + 5_000),
    ];

    let mut full = db_root(ROOT);
    for (sender, target, at) in &ops {
        full.conn()
            .execute(
                "INSERT INTO device_revoke_ops
                 (op_hlc_ms, op_hlc_logical, sender, revoked_device_id, reason,
                  recorded_at_ms)
                 VALUES (?1, 0, ?2, ?3, ?4, ?1)",
                params![
                    i64::try_from(*at).unwrap(),
                    &sender.keychain.device_id()[..],
                    &target[..],
                    RevokeReason::Lost.as_str(),
                ],
            )
            .unwrap();
    }
    assert_eq!(ledger_rows(&full), 7, "uncompacted, as written");
    revoke(&er, &mut full, &ee, f_id, T0 + 200_000);

    let mut live = db_root(ROOT);
    for (sender, target, at) in ops.iter().rev() {
        revoke(&er, &mut live, sender, *target, *at);
    }
    revoke(&er, &mut live, &ee, f_id, T0 + 200_000);

    assert_eq!(
        whole_register(&full),
        whole_register(&live),
        "a fold over every row and a fold over each pair's greatest agree"
    );
    assert_eq!(
        revocation_row(&live, &a_id).unwrap().0,
        i64::try_from(T0 + 120_000).unwrap()
    );
    assert_eq!(
        revocation_row(&live, &c_id).unwrap().0,
        i64::try_from(T0 + 5_000).unwrap(),
        "D's greatest stamp wins C, and A's gated rows do not"
    );
    assert!(er.is_revoked(live.conn(), &f_id).unwrap());

    assert_eq!(
        whole_ledger(&full),
        whole_ledger(&live),
        "the next op compacted the uncompacted ledger to the same rows"
    );
    assert_eq!(
        ledger_rows(&live),
        4,
        "one row per pair: B->A, A->C, D->C, E->F"
    );
}

/// A wall clock that steps **backwards** does not move the cut, because the
/// cut is the op's HLC and `Hlc::send` takes `max(now, local)`.
///
/// This is H2's second half, and it was reachable with no attacker at all:
/// the cut defaulted to the wall clock, so a ten-minute NTP step
/// back produced a revocation whose cut every peer refused as out of range
/// — the user was told the stolen laptop was revoked while no other replica
/// agreed and nothing said so.
#[test]
fn a_backward_clock_step_does_not_move_the_cut() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    // Something happens, so this device's HLC is at T0.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before the step".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    // The clock steps back ten minutes — twice `MAX_DRIFT_MS`.
    set_clock(&ca, T0 - 10 * 60 * 1000);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Stolen,
        },
    )
    .unwrap();

    let cut = revocation_row(&dba, &eb.keychain.device_id()).unwrap().0;
    assert!(
        cut >= i64::try_from(T0).unwrap(),
        "the HLC is monotone, so the cut cannot follow the clock backwards: {cut}"
    );
}

/// A revocation whose cut landed in the past is **superseded** by revoking
/// again from a healthy device, in either arrival order.
///
/// A device that has not yet heard from a peer carries whatever HLC its own
/// clock gives it, so a badly-set one can still emit a backdated cut — the
/// register does not prevent that. What LWW decides is which op is the one
/// on record: under the `MIN` join this replaces, the earliest cut won
/// forever and no later op could correct the record; under LWW the later op
/// wins, which is the whole reason for choosing it.
///
/// What is *not* corrected is the fact of revocation. A recorded row is
/// effective whatever its `cut_ms` says (see [`Engine::is_revoked`]), so
/// revoking again replaces the cut, the author and the reason — it does not
/// un-revoke the device. Nothing in this engine ever did: the cut gated
/// which epochs a revoked device was sealed, never whether its past ops
/// were accepted, and a revoked device's writes are unbounded either way.
#[test]
fn a_backdated_revocation_is_superseded_by_a_healthy_one() {
    const YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1000;
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let mis_set = engine_seeded(ROOT, [2u8; 32], clock());
    let healthy = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    // The damage: a device a year slow revokes T, and its cut refuses
    // everything T ever wrote.
    revoke(&r1, &mut db1, &mis_set, t, T0 - YEAR_MS);
    assert_eq!(
        revocation_row(&db1, &t).unwrap().0,
        i64::try_from(T0 - YEAR_MS).unwrap(),
        "the premise: the mis-set device's backdated cut is the one on record"
    );

    // The repair, and the same two ops in the other order on a second
    // replica.
    revoke(&r1, &mut db1, &healthy, t, T0);
    revoke(&r2, &mut db2, &healthy, t, T0);
    revoke(&r2, &mut db2, &mis_set, t, T0 - YEAR_MS);

    assert_eq!(
        revocation_row(&db1, &t),
        revocation_row(&db2, &t),
        "order decided nothing"
    );
    assert_eq!(
        revocation_row(&db1, &t).unwrap().0,
        i64::try_from(T0).unwrap(),
        "the healthy cut supersedes the backdated one"
    );
    for (e, db) in [(&r1, &db1), (&r2, &db2)] {
        assert_eq!(
            revocation_row(db, &t).unwrap().2.as_slice(),
            &healthy.keychain.device_id()[..],
            "and both replicas record the healthy device as the revoker"
        );
        assert!(
            e.is_revoked(db.conn(), &t).unwrap(),
            "the record is corrected, not withdrawn: T stays revoked"
        );
    }
}

/// The local command writes the same register the remote path does,
/// because it now *is* the remote path.
///
/// There were two writers to these columns and only one took the join:
/// `apply_control_op` resolved concurrent revocations, while
/// `revoke_device` did a bare unconditional `UPDATE` that set neither the
/// HLC's logical half nor any guard. A device revoking one already revoked
/// therefore moved its own cut while every peer kept the other, and it was
/// then the only replica accepting a window of ops.
#[test]
fn a_local_revocation_writes_the_same_register_as_a_remote_one() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let target = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, target),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    // The register holds a full HLC and its sender, not half of one.
    let row = revocation_row(&dba, &target).expect("the local command records it");
    assert_eq!(row.0, i64::try_from(T0).unwrap());
    assert_eq!(
        row.2.as_slice(),
        &ea.keychain.device_id()[..],
        "the bare UPDATE this replaces wrote no HLC logical half and no \
             guard, so the register compared against a default here and a real \
             value on every other replica"
    );

    // And it participates in the register: a staler revocation arriving
    // afterwards does not overwrite it.
    let before = revocation_row(&dba, &target);
    revoke(&ea, &mut dba, &ec, target, T0 - 5_000);
    assert_eq!(
        revocation_row(&dba, &target),
        before,
        "a local write outside the register would have been overwritten here"
    );
}

/// A `device_revoke` naming a device whose cert has not arrived yet is
/// durable, and the cert does not resurrect it.
///
/// Ordinary, not exotic: a cert parked in `deferred_ops` at meta epoch *k*
/// drains after a revocation absorbed at *k+1*. A bare
/// `UPDATE ... WHERE device_id = ?` matched no row and dropped the
/// revocation silently; `DeviceCertPublish` then created the row
/// **unrevoked**. Fail-open, and decided by delivery order.
#[test]
fn a_revocation_that_arrives_before_the_cert_survives_it() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();

    // B has never seen A's cert when the revocation lands.
    revoke(&eb, &mut dbb, &eb, a_id, T0);
    assert!(
        eb.is_revoked(dbb.conn(), &a_id).unwrap(),
        "the revocation must be durable without a devices row to update"
    );
    // And it does not invent one. Upserting into `devices` made every
    // revocation of an unknown id a phantom entry in the user's device
    // list, and a phantom row also satisfies `revoke_device`'s
    // known-device guard.
    assert_eq!(
        device_rows(&dbb),
        0,
        "a revocation must not mint a device nobody has ever seen"
    );

    // A's cert arrives afterwards. It fills the identity columns in and
    // leaves the revocation alone.
    trust(&eb, &mut dbb, &ea);
    assert!(
        eb.is_revoked(dbb.conn(), &a_id).unwrap(),
        "the cert must not resurrect a revoked device"
    );
    assert_eq!(device_rows(&dbb), 1, "and it is one device, not two");
}

/// Revoking a device this vault has never seen is refused, and stays
/// refused however many revocations of unknown ids have gone past.
///
/// The guard reads `devices`, so while a `device_revoke` upserted into that
/// table a revocation of any id at all made its target "known" — and
/// `revoke_device` mints a fresh epoch for every stream in the account
/// before it gets anywhere near checking anything else.
#[test]
fn revoking_a_device_the_vault_has_never_seen_is_refused() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let ghost = [0x9e; 16];
    revoke(&ea, &mut dba, &eb, ghost, T0);
    assert_eq!(device_rows(&dba), 1, "the ghost is not in the device list");

    let err = ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, ghost),
            reason: RevokeReason::Lost,
        },
    );
    assert!(
        matches!(err, Err(EngineError::NotFound(_))),
        "revoking a ghost must not reach the rotation, got {err:?}"
    );
}

/// A revocation is the first control op a fresh vault emits, so resolving
/// the vault-meta epoch mints that stream's own key mid-transaction and
/// emits `key_envelope` ops into the very stream whose `seq` the revocation
/// needs. A `seq` read before the transaction is already spent by then, and
/// `ops` has a `UNIQUE(stream_id, device_id, seq)`.
///
/// This is the only shape that fires it: a vault that has already written
/// anything has a meta key, so nothing is minted and the stale read happens
/// to be right.
#[test]
fn a_revocation_on_a_vault_with_no_meta_key_yet_gets_its_own_seq() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    // `trust` writes the peer's row directly, so the vault-meta stream
    // still holds no key and no op: exactly the premise.
    trust(&ea, &mut dba, &eb);
    assert_eq!(op_count(&dba), 0, "the premise: nothing written yet");

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Stolen,
        },
    )
    .expect("revoking as the first act of a vault must work");

    let revokes: i64 = dba
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ops WHERE inner_kind = 'device.revoke'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revokes, 1, "the revocation reached the op log");
    // Every op in the meta stream has a distinct seq. A stale read would
    // have collided with the `key_envelope` ops minting the epoch emitted.
    let (rows, distinct): (i64, i64) = dba
        .conn()
        .query_row(
            "SELECT COUNT(seq), COUNT(DISTINCT seq) FROM ops WHERE stream_id = ?",
            params![&META_STREAM[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(rows > 1, "the epoch mint emitted envelopes alongside it");
    assert_eq!(rows, distinct, "no two meta-stream ops share a seq");
}

/// `device_read_bounds` row count.
fn read_bound_rows(db: &Db) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM device_read_bounds", [], |r| r.get(0))
        .unwrap()
}

/// **A sender naming made-up ids stops growing the three tables at the cap**
/// (#315).
///
/// Pair compaction bounds rows per `(sender, target)` pair; nothing bounded
/// the pairs. B, a device the account trusts, first revokes C, which really
/// paired, and then names `REVOKE_TARGETS_PER_SENDER + 40` ids nobody has, each
/// with a later stamp. The ledger and the register keep B's newest
/// `REVOKE_TARGETS_PER_SENDER` targets, so C's revocation is the one B's own
/// flood withdrew; C keeps its read bound because this replica holds C's cert,
/// and the made-up ids that fell out give theirs back. D's revocation of E,
/// another sender's claim, is untouched throughout.
#[test]
fn a_sender_keeps_at_most_the_cap_of_distinct_revocation_targets() {
    use super::revocation::REVOKE_TARGETS_PER_SENDER as CAP;
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ee = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    trust(&er, &mut db, &ec);
    let (c_id, e_id) = (ec.keychain.device_id(), ee.keychain.device_id());
    let cap = u64::try_from(CAP).unwrap();

    revoke(&er, &mut db, &ed, e_id, T0);
    revoke(&er, &mut db, &eb, c_id, T0 + 1);
    assert!(er.is_read_bounded(db.conn(), &c_id).unwrap());

    for n in 0..cap + 40 {
        revoke(&er, &mut db, &eb, made_up_device(n), T0 + 1_000 + n);
    }

    assert_eq!(ledger_rows(&db), CAP + 1, "B's cap, plus D's one row");
    assert_eq!(
        whole_register(&db).len(),
        usize::try_from(CAP + 1).unwrap(),
        "the register folds from the capped ledger"
    );
    assert!(
        !er.is_revoked(db.conn(), &c_id).unwrap(),
        "B's oldest claim, on C, is the one its own flood withdrew"
    );
    assert!(
        er.is_read_bounded(db.conn(), &c_id).unwrap(),
        "and C, whose cert this replica holds, keeps its read bound"
    );
    assert!(!er.is_read_bounded(db.conn(), &made_up_device(0)).unwrap());
    assert!(er
        .is_read_bounded(db.conn(), &made_up_device(cap + 39))
        .unwrap());
    assert_eq!(
        read_bound_rows(&db),
        CAP + 2,
        "B's kept targets, E, and C: the evicted made-up ids gave theirs back"
    );
    assert!(
        er.is_revoked(db.conn(), &e_id).unwrap(),
        "another sender's claim is not B's to withdraw"
    );
}

/// **The cap is a function of the op set, not of arrival order** (#315).
///
/// The same ops, one sender over the cap with stamps interleaved around a
/// second sender's, applied forwards on one replica and backwards on the
/// other, must leave the same ledger and the same register. Each order also
/// meets a pair the cap evicts before that pair's greatest row arrives, which
/// is the case the merge argument in `cap_device_revoke_targets` turns on:
/// forwards it is X, whose flood row sorts below 274 others and whose
/// greatest row follows the flood; backwards it is Y, whose lesser row
/// arrives first and whose greatest row arrives last. The replay asserts
/// each eviction before the greatest row lands.
#[test]
fn the_revocation_target_cap_is_the_same_whichever_order_the_ops_arrive() {
    use super::revocation::REVOKE_TARGETS_PER_SENDER as CAP;
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let cap = u64::try_from(CAP).unwrap();
    let (x, y) = (made_up_device(3), made_up_device(cap + 20));

    let mut ops: Vec<(&Engine, [u8; 16], u64)> = vec![(&eb, y, T0 + 100_001)];
    for n in 0..cap + 20 {
        // Stamps not in id order, so rank is decided by stamp and not by id.
        // X's flood row sorts above only n = 0, so the flood evicts it.
        let at = if n == 3 {
            T0 + 5
        } else {
            T0 + ((n * 7919) % (cap + 20)) * 10
        };
        ops.push((&eb, made_up_device(n), at));
    }
    ops.push((&eb, x, T0 + 100_000));
    ops.push((&eb, x, T0 + 1));
    for n in 0..5 {
        ops.push((&ed, made_up_device(1_000 + n), T0 + n * 500));
    }
    ops.push((&eb, y, T0 + 2));

    let replay = |order: Vec<&(&Engine, [u8; 16], u64)>, head: ([u8; 16], u64)| {
        let mut db = db_root(ROOT);
        for &&(sender, target, at) in &order {
            if (target, at) == head {
                assert!(
                    !whole_ledger(&db).iter().any(|row| row.3 == target),
                    "the cap evicted this pair before its greatest row arrived"
                );
            }
            revoke(&er, &mut db, sender, target, at);
        }
        db
    };
    let forward = replay(ops.iter().collect(), (x, T0 + 100_000));
    let backward = replay(ops.iter().rev().collect(), (y, T0 + 100_001));

    assert_eq!(whole_ledger(&forward), whole_ledger(&backward));
    assert_eq!(whole_register(&forward), whole_register(&backward));
    assert_eq!(ledger_rows(&forward), CAP + 5);
    for target in [x, y] {
        assert!(
            er.is_revoked(forward.conn(), &target).unwrap(),
            "a pair whose greatest stamp is among B's newest is kept, in both orders"
        );
    }
}

/// **A gated sender cannot use the cap to escape its gate** (#315).
///
/// The cap drops only the flooding sender's own claims, and the gate on a
/// sender is built from rows revoking it, which it does not author. So X,
/// revoked by B, floods past the cap and then names C: the gate still holds,
/// and B's revocation of X, B's claim and not X's, stays in the ledger.
#[test]
fn a_gated_sender_flooding_past_the_cap_stays_gated() {
    use super::revocation::REVOKE_TARGETS_PER_SENDER as CAP;
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, c_id) = (ex.keychain.device_id(), ec.keychain.device_id());
    let cap = u64::try_from(CAP).unwrap();

    revoke(&er, &mut db, &eb, x_id, T0);
    for n in 0..cap + 5 {
        revoke(&er, &mut db, &ex, made_up_device(n), T0 + 1_000 + n);
    }
    revoke(&er, &mut db, &ex, c_id, T0 + 500_000);

    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        !er.is_revoked(db.conn(), &c_id).unwrap(),
        "X is still gated after its flood"
    );
    assert_eq!(ledger_rows(&db), CAP + 1, "X's cap, plus B's claim on X");
    assert_eq!(
        whole_register(&db).len(),
        1,
        "none of X's gated claims reached the register"
    );
}
