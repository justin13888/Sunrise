//! The revocation register: the fold, its gate and the discount.

use super::*;

/// The relay's half of a revocation is queued by the same transaction that
/// writes the op, so the two cannot diverge.
///
/// A vault that believes it revoked a device and never queued the telling
/// is the failure the queue exists to prevent — and it is silent, because
/// the local half succeeds and the user sees the device marked revoked. It
/// is one transaction rather than two writes for the same reason the outbox
/// row shares its op's transaction.
///
/// The queue is a queue rather than a call because `revoke_device` has to
/// work with no network at all: a device that is gone is the whole
/// scenario, and it is exactly the moment a user is least likely to be
/// online. `sync_driver::drain_relay_revocations` makes the call.
#[test]
fn revoking_a_device_queues_the_relays_half_in_the_same_transaction() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    assert!(
        pending_relay_revocations(&dba).is_empty(),
        "nothing is owed before a revocation"
    );

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Stolen,
        },
    )
    .unwrap();

    assert_eq!(
        pending_relay_revocations(&dba),
        vec![eb.keychain.device_id()],
        "the op and the intent are written together or not at all"
    );
    assert!(
        revocation_row(&dba, &eb.keychain.device_id()).is_some(),
        "and the register itself is still written"
    );
}

/// Two replicas, the same two revocation ops, opposite arrival orders, one
/// final state — the register, and the effect it has on a third device's
/// ops.
///
/// The row is an LWW register on the declaring op's own `(hlc, device_id)`,
/// the comparison every per-field register here resolves on (ADR-0044). It
/// converges regardless of order, and — unlike the `MIN` join it replaces —
/// it is reversible: a later revocation supersedes an earlier one, so a cut
/// that landed wrong can be corrected by revoking again.
///
/// `revoke_reason` and `revoked_by` are asserted alongside the timestamp on
/// purpose. Convergence on the cut alone would still leave two replicas
/// disagreeing about who did it and why, which is what the previous
/// `(cut, revoker)` tie-break left order-dependent.
#[test]
fn concurrent_revocations_converge_on_one_register_whatever_the_order() {
    let clock = || Arc::new(FakeClock(PLMutex::new(T0)));
    let target = engine_seeded(ROOT, [1u8; 32], clock());
    let earlier = engine_seeded(ROOT, [2u8; 32], clock());
    let later = engine_seeded(ROOT, [3u8; 32], clock());
    let r1 = engine_seeded(ROOT, [4u8; 32], clock());
    let r2 = engine_seeded(ROOT, [5u8; 32], clock());
    let mut db1 = db_root(ROOT);
    let mut db2 = db_root(ROOT);
    trust(&r1, &mut db1, &target);
    trust(&r2, &mut db2, &target);
    let t = target.keychain.device_id();

    // The same two ops. r1 sees the later one first; r2 sees it last.
    revoke(&r1, &mut db1, &later, t, T0 + 9_000);
    revoke(&r1, &mut db1, &earlier, t, T0 + 1_000);
    revoke(&r2, &mut db2, &earlier, t, T0 + 1_000);
    revoke(&r2, &mut db2, &later, t, T0 + 9_000);

    let row1 = revocation_row(&db1, &t);
    assert_eq!(
        row1,
        revocation_row(&db2, &t),
        "arrival order decided nothing"
    );
    assert_eq!(
        row1.as_ref().unwrap().0,
        i64::try_from(T0).unwrap() + 9_000,
        "the last writer wins, so a later revocation can correct an earlier one"
    );
    assert_eq!(
        row1.as_ref().unwrap().2.as_slice(),
        &later.keychain.device_id()[..],
        "and every column follows the winning op, not just the timestamp"
    );

    // The effect converges with the row. A recorded revocation is
    // effective on both replicas -- there is no time test to disagree
    // about -- and the columns they agree on above are what decide *which*
    // of two racing revocations is the one on record.
    for (e, db) in [(&r1, &db1), (&r2, &db2)] {
        assert!(e.is_revoked(db.conn(), &t).unwrap());
    }
}

/// A device cannot move its own revocation cut.
///
/// Register hygiene, independent of any gate: a device that can rewrite its
/// own row can push its cut forward and undo a revocation somebody else
/// made of it, and the register is last-writer-wins, so its op would win.
/// `Command::RevokeDevice` refuses self-revocation locally for a different
/// reason — rotating every key away from the only device holding them is
/// not a recoverable state — and neither check implies the other.
#[test]
fn a_device_cannot_move_its_own_revocation_cut() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    trust(&er, &mut db, &ea);
    let a_id = ea.keychain.device_id();

    // B revokes A.
    revoke(&er, &mut db, &eb, a_id, T0);
    let after_b = revocation_row(&db, &a_id).expect("B's revocation stands");

    // A tries to move its own cut forward, which under last-writer-wins
    // would otherwise supersede B's and un-revoke it for everything after.
    revoke(&er, &mut db, &ea, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &a_id),
        Some(after_b),
        "a device must not be able to edit the register entry about itself"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and it is still revoked: the row it tried to edit is what decides that"
    );

    // A revocation of A by anyone else still lands, so this is a check on
    // the subject, not a freeze on the row.
    revoke(&er, &mut db, &eb, a_id, T0 + 90_000);
    assert_eq!(
        revocation_row(&db, &a_id).unwrap().0,
        i64::try_from(T0).unwrap() + 90_000
    );
}

/// **A revoked device cannot revoke another device.**
///
/// The hole ADR-0041 closes. Before it, the only `device_revoke` the arm
/// refused was one naming its own sender, so a laptop the account had already
/// expelled could expel everything else in the account: its cert is still on
/// the chain, it still holds the vault-meta key for the epoch it was cut at,
/// and peers keep old epoch keys, so the op verifies, decrypts and applies
/// everywhere. Revocation has no inverse — nothing deletes a
/// `device_revocations` row and `is_revoked` is presence and nothing else — so
/// the result was permanent on every replica
/// ([#82](https://github.com/justin13888/Sunrise/issues/82)).
#[test]
fn a_revoked_device_cannot_revoke_another_device() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // B revokes A: the ordinary administrative action.
    revoke(&er, &mut db, &eb, a_id, T0);
    assert!(er.is_revoked(db.conn(), &a_id).unwrap());

    // A then tries to revoke C, which is the account turning on itself.
    revoke(&er, &mut db, &ea, c_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "an expelled device must not be able to expel anything else"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A is still out"
    );

    // The op is kept, not dropped: that is what makes the skip reversible.
    assert_eq!(
        ledger_rows(&db),
        2,
        "the refused op is stored and only its effect is refused"
    );

    // Anyone else revoking C still works, so this gates the sender and does
    // not freeze the row.
    revoke(&er, &mut db, &eb, c_id, T0 + 90_000);
    assert!(revocation_row(&db, &c_id).is_some());
}

/// **And it cannot get around that by dating the op earlier.**
///
/// The test above puts A's op *after* its own cut, which is the polite case.
/// The sort key is A's to choose, and choosing is free: [`Hlc`] bounds only
/// the future through `MAX_DRIFT_MS`, a reading in the past is ordinary and
/// `crates/sunrise-core/src/engine/sync.rs` says so, and nothing ties an op's
/// HLC to that sender's own `seq` or to any earlier stamp it sent.
///
/// So while the gate judged a sender against the **prefix** of the walk, the
/// whole of ADR-0041 was one subtraction away from being bypassed: date the op
/// a millisecond below your own cut, sort first, and land. Iterating the
/// remaining device ids then revokes the account, which is precisely the
/// outcome #82 is about and which §Alternatives (f) rejects a rival design for
/// admitting. Judging the sender against the whole ledger is what closes it,
/// and this is the assertion that a date cannot reopen it.
#[test]
fn a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // B revokes A: the ordinary administrative action.
    revoke(&er, &mut db, &eb, a_id, T0);
    assert!(er.is_revoked(db.conn(), &a_id).unwrap());

    // A answers by reaching for C one millisecond *below* its own cut, which
    // is the entire attack.
    revoke(&er, &mut db, &ea, c_id, T0 - 1);

    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "an expelled device must not expel a third party by back-dating the op"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A is still out: its op did not move its own cut either"
    );
    assert_eq!(
        ledger_rows(&db),
        2,
        "the refused op is stored and only its effect is refused"
    );
}

/// **The skip does not depend on which of the two ops arrived first.**
///
/// This is the property ADR-0034 corollary 3 demands of any peer-side
/// enforcement, and the reason the register is a fold over a kept ledger rather
/// than a refusal taken where an op lands. A replica that sees A's revocation
/// of C *before* it learns A was itself revoked must end up with the same
/// register as one that sees them the other way round — otherwise two replicas
/// holding identical op sets disagree forever about who is a member.
#[test]
fn the_register_is_the_same_whichever_order_the_two_revocations_arrive() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // Replica one: B-revokes-A first, then A-revokes-C.
    let mut one = db_root(ROOT);
    revoke(&er, &mut one, &eb, a_id, T0);
    revoke(&er, &mut one, &ea, c_id, T0 + 60_000);

    // Replica two: the same two ops, delivered the other way round. A's op
    // lands while A is still, as far as this replica knows, a member.
    let mut two = db_root(ROOT);
    revoke(&er, &mut two, &ea, c_id, T0 + 60_000);
    assert!(
        revocation_row(&two, &c_id).is_some(),
        "before it knows better, this replica believes A's op"
    );
    revoke(&er, &mut two, &eb, a_id, T0);

    assert_eq!(revocation_row(&one, &a_id), revocation_row(&two, &a_id));
    assert_eq!(
        revocation_row(&one, &c_id),
        revocation_row(&two, &c_id),
        "the same op set must give the same register in either delivery order"
    );
    assert_eq!(
        revocation_row(&two, &c_id),
        None,
        "and learning the sender was revoked undoes what it had already written"
    );

    // **The register converges and the read bound does not, and that is the
    // shipped behaviour rather than an oversight.**
    //
    // This test builds the two arrival orders that separate them, so asserting
    // only the register leaves the interesting half unobserved. `one` applied
    // `B -> A` first: its first fold produced the register `{A}`, its second
    // already gated `A -> C`, so C never entered a register on this replica and
    // the `INSERT OR IGNORE` had nothing to write. `two` applied `A -> C` first,
    // bounded C on that fold, and keeps the row forever because nothing deletes
    // from `device_read_bounds`.
    //
    // Pinned as the **known current behaviour**, not as the behaviour anyone
    // wants: on `two` the bound holds C out of every recipient set, and on `one`
    // C is an ordinary member that will be sealed every epoch this replica
    // mints. #282 closed the systematic case, a joiner that paired with
    // nothing (`a_joiner_keeps_its_sponsors_bound_on_c_whichever_order_it_meets_the_ops`
    // below). These two replicas already exist, and closing
    // [#411](https://github.com/justin13888/Sunrise/issues/411) means they
    // become equal and these assertions turn red — which is the point of
    // writing them down rather than leaving the gap undiscovered.
    assert_eq!(
        read_bound_row(&one, &c_id),
        None,
        "known gap (#411): the replica that learned of A's own revocation first \
         never bounds C, so it goes on sealing C every epoch it mints"
    );
    assert!(
        read_bound_row(&two, &c_id).is_some(),
        "while the replica that believed A's op first keeps the bound, because \
         the ratchet never gives a row back"
    );
    assert!(
        read_bound_row(&one, &a_id).is_some() && read_bound_row(&two, &a_id).is_some(),
        "both replicas bound A, which is why the divergence is C's alone and not \
         a difference in what either replica believes about the ledger"
    );
}

/// **A device paired from the replica that bounded C keeps C bounded, whatever
/// order it then meets the ops in** (#282).
///
/// The test above leaves two existing replicas apart. What pairing can close is
/// the systematic case: a joiner used to start with no bound and learn both ops
/// from the relay later, so it met `A -> C` already gated and never bounded C —
/// the weakest replica in the account by construction. `sponsor` here is `two`
/// above, which bounded C; the joiner adopts its bound at vault creation, then
/// meets `B -> A` first, the order that left `one` unbounded.
///
/// Asserted after `B -> A` alone as well as after both ops: at that point the
/// joiner holds no cert for C and no ledger row naming it, which is exactly
/// the row `release_orphan_read_bounds` deletes when this replica's own fold
/// wrote it. An adopted row is exempt, or the bound would be gone before C's
/// cert arrived.
#[test]
fn a_joiner_keeps_its_sponsors_bound_on_c_whichever_order_it_meets_the_ops() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    let mut sponsor = db_root(ROOT);
    revoke(&er, &mut sponsor, &ea, c_id, T0 + 60_000);
    revoke(&er, &mut sponsor, &eb, a_id, T0);
    let carried = super::read_bounds_for_pairing(sponsor.conn()).unwrap();
    assert_eq!(carried, std::collections::BTreeSet::from([a_id, c_id]));

    // What `Keychain::create` does with a pairing grant's bound.
    let mut joiner = db_root(ROOT);
    joiner
        .with_tx(|tx| super::adopt_sponsor_read_bounds(tx, &carried, T0))
        .unwrap();

    revoke(&er, &mut joiner, &eb, a_id, T0);
    assert!(
        er.is_read_bounded(joiner.conn(), &c_id).unwrap(),
        "an adopted bound survives the orphan release, though no cert or ledger \
         row here names C yet"
    );
    revoke(&er, &mut joiner, &ea, c_id, T0 + 60_000);
    assert!(
        er.is_read_bounded(joiner.conn(), &c_id).unwrap(),
        "and survives meeting A -> C already gated, which is where a joiner that \
         paired with nothing lost it"
    );
    assert_eq!(
        revocation_row(&joiner, &c_id),
        revocation_row(&sponsor, &c_id),
        "the register is untouched by the adoption: it is still the fold"
    );

    // The same arrival order without the adoption, which is every joiner
    // before this change.
    let mut bare = db_root(ROOT);
    revoke(&er, &mut bare, &eb, a_id, T0);
    revoke(&er, &mut bare, &ea, c_id, T0 + 60_000);
    assert!(!er.is_read_bounded(bare.conn(), &c_id).unwrap());
}

/// **The orphan release still takes back what this replica's own fold wrote**
/// (#315), with the adopted rows beside it.
///
/// The exemption is for rows a sponsor handed over, and it must not widen: a
/// certless id only a capped sender named, whose bound this replica's fold
/// wrote, is released exactly as before.
#[test]
fn the_orphan_release_spares_only_adopted_bounds() {
    use super::revocation::REVOKE_TARGETS_PER_SENDER as CAP;
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let cap = u64::try_from(CAP).unwrap();
    let adopted = made_up_device(9_999);
    let mut db = db_root(ROOT);
    db.with_tx(|tx| {
        super::adopt_sponsor_read_bounds(tx, &std::collections::BTreeSet::from([adopted]), T0)
    })
    .unwrap();

    for n in 0..=cap {
        revoke(&er, &mut db, &eb, made_up_device(n), T0 + 1_000 + n);
    }

    assert!(
        !er.is_read_bounded(db.conn(), &made_up_device(0)).unwrap(),
        "the id B's own flood evicted gives its bound back, as before"
    );
    assert!(
        er.is_read_bounded(db.conn(), &adopted).unwrap(),
        "while a bound adopted from the sponsor, certless and unnamed here, stays"
    );
}

/// **An adopted bound is not a revocation this replica applied, so it marks
/// nobody as joined after one.**
///
/// A joiner adopts its sponsor's bound before it holds any cert but its own.
/// If the readmission check counted those rows, the joiner's first sync would
/// mark its sponsor and every device that paired before any revocation as
/// `admitted_after_revocation`. No older replica marks them, and the #105 signal
/// would become a badge every sibling wears. The joiner counts a revocation
/// once its own fold applies it, which is the order an older replica met it
/// in. A device that turns up after that is marked. That second half also
/// checks that the fold clears `from_sponsor` on the adopted row it bounds
/// again, because otherwise the joiner would count nothing.
#[test]
fn an_adopted_bound_marks_no_device_as_joined_after_a_revocation() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let (a_id, b_id, c_id, d_id) = (
        ea.keychain.device_id(),
        eb.keychain.device_id(),
        ec.keychain.device_id(),
        ed.keychain.device_id(),
    );
    let from_sponsor = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT from_sponsor FROM device_read_bounds WHERE device_id = ?",
                params![&c_id[..]],
                |r| r.get(0),
            )
            .unwrap()
    };

    let mut joiner = db_root(ROOT);
    joiner
        .with_tx(|tx| {
            super::adopt_sponsor_read_bounds(tx, &std::collections::BTreeSet::from([c_id]), T0)
        })
        .unwrap();

    // The sponsor and a sibling that were here before C was revoked.
    trust(&er, &mut joiner, &ea);
    trust(&er, &mut joiner, &eb);
    assert!(
        !device_list_row(&er, &joiner, &a_id).admitted_after_revocation,
        "the sponsor did not join after anything this replica applied"
    );
    assert!(
        !device_list_row(&er, &joiner, &b_id).admitted_after_revocation,
        "nor did a sibling that paired before any revocation"
    );

    // The joiner now applies A -> C itself, from the relay.
    revoke(&er, &mut joiner, &ea, c_id, T0);
    assert_eq!(
        from_sponsor(&joiner),
        0,
        "the fold bounding an adopted device makes the row this replica's own"
    );
    assert!(er.is_read_bounded(joiner.conn(), &c_id).unwrap());

    trust_at(&er, &mut joiner, &ed, T0 + 1);
    assert!(
        device_list_row(&er, &joiner, &d_id).admitted_after_revocation,
        "a device that turns up after a revocation this replica applied is marked"
    );
}

/// **A cut correction does not bring a skipped revocation back, and the
/// remedy is to make it again.**
///
/// The question [#82](https://github.com/justin13888/Sunrise/issues/82) defers,
/// answered with its second option and pinned here so the answer is asserted
/// rather than assumed. **The gate reads no cut of any kind**: it is a
/// set-membership test over the ledger's `(sender, revoked)` pairs, and an HLC
/// decides only which row wins the register. Correcting a cut appends a second
/// revocation of the same sender by the same party, which changes that winner
/// and changes nothing about who has revoked whom — so the gate answers the
/// same question the same way, whether the correction is dated **after** the op
/// it would rescue or **before** it. Both directions are asserted below, and
/// asserting both is the whole of what this test pins: the "either direction"
/// half of that sentence, which is the half a reader has no other reason to
/// believe.
///
/// What it does **not** do is tell the shipped whole-ledger rule apart from one
/// reading the walk's prefix. Replay the prefix rule over this test's own four
/// rows, ascending — `(T0-30s, B, A)`, `(T0, B, A)`, `(T0+60s, A, C)`,
/// `(T0+120s, B, A)` — and it gates A's op too: the first two rows are ungated
/// and seat `revokers[A] = {B}`, so by the third row `B != C` already holds and
/// C stays off, which is the answer the shipped rule gives. That is structural
/// and not an artefact of these constants: a revocation of the sender can only
/// ever **add** to a prefix's gating set for that sender, never take a revoker
/// out of it, so back-dating a correction can only make a prefix rule gate more
/// readily and can never be the direction that separates the two rules. The
/// direction that separates them is a cut sorting *above* the op with none
/// below it, and that is pinned by
/// `a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op`.
///
/// What *does* un-skip a row is the one thing that empties its sender's revoker
/// set: somebody revoking that sender's revoker. A cut correction is not that,
/// in either direction.
///
/// That is acceptable here and would not be acceptable for a task edit, which
/// is exactly why ADR-0041 scopes the gate to this op family. What is lost is
/// an administrative act by a device the account has expelled — a thing a human
/// can simply do again, from a device the account still trusts, and a thing
/// they would want to look at again anyway.
#[test]
fn a_cut_correction_does_not_re_fold_a_skipped_revocation() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // A device with a slow clock cuts A at T0, before A's honest revocation of
    // C at T0 + 60s — so that one is skipped.
    revoke(&er, &mut db, &eb, a_id, T0);
    revoke(&er, &mut db, &ea, c_id, T0 + 60_000);
    assert_eq!(revocation_row(&db, &c_id), None);

    // Correcting the cut forward appends a second revocation of A by B. It
    // changes which row wins A's entry in the register and nothing about who
    // has revoked whom, so C stays off.
    revoke(&er, &mut db, &eb, a_id, T0 + 120_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "the gate reads no cut, so moving it forward does not un-skip the op"
    );
    // And backward, past the op it would rescue. This half is here because the
    // sentence above claims *either* direction, not because it discriminates: a
    // prefix rule gates A's op here too, since the T0 cut already sits below it
    // and a fourth row revoking A can only add to A's gating set. What does
    // discriminate is a cut sorting above the op with none below it, in
    // `a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op`.
    revoke(&er, &mut db, &eb, a_id, T0 - 30_000);
    assert_eq!(
        ledger_rows(&db),
        2,
        "B's three ops on A compact to their greatest stamp (#250), which folds \
         exactly as all three do: see Engine::compact_device_revoke_ops"
    );
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "and moving it backward does not either, for the same reason — which \
         pins the sentence's either-direction half and not a difference from a \
         prefix rule, since that gates this op as well"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A is still revoked: correcting the cut is not un-revoking"
    );

    // The remedy, which is the whole reason the loss is tolerable: anyone the
    // account still trusts revokes C again.
    revoke(&er, &mut db, &eb, c_id, T0 + 180_000);
    assert!(er.is_revoked(db.conn(), &c_id).unwrap());
}

/// **Two devices revoking each other still converge on both revocations.**
///
/// The gate's exception, asserted from the direction that makes it necessary.
/// An HLC sorts first by being dated earlier and nothing charges for that, so a
/// rule of "whoever revoked first silences the other" would hand a stolen
/// laptop the account: back-date a revocation of the owner's Mac and the Mac's
/// answer never lands. Both revocations standing is the safe resolution, and it
/// is what the engine did before this gate existed.
#[test]
fn a_gate_on_the_sender_does_not_let_a_back_dated_revocation_silence_its_target() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, b_id) = (ea.keychain.device_id(), eb.keychain.device_id());

    // A, holding stolen credentials, gets its revocation of B in a year early.
    revoke(&er, &mut db, &ea, b_id, T0 - 365 * 24 * 60 * 60 * 1000);
    // B answers at the real time.
    revoke(&er, &mut db, &eb, a_id, T0);

    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "the answer to a back-dated revocation must still land"
    );
    assert!(er.is_revoked(db.conn(), &b_id).unwrap());
}

/// A revoked device's revocations from **before** its own cut are unwound.
///
/// This test asserted the opposite until ADR-0041 §Decision 1 was rewritten,
/// and the inversion is the point of it rather than an accident of one. The
/// old property — revocation is not retroactive, so a revoked device's earlier
/// revocations still stand — reads well and was *the vulnerability*, because
/// "earlier" is not a fact the ledger holds. The cut is the op's own HLC, and
/// an HLC is whatever its sender wrote: `MAX_DRIFT_MS` bounds only the future,
/// a reading in the past is ordinary, and nothing ties an op's stamp to that
/// sender's `seq` or to any earlier stamp it sent. So nothing distinguishes an
/// honestly-earlier revocation from one a revoked device back-dated a minute
/// ago, and a gate that believed the first believed the second — which is the
/// whole account, one small integer at a time. See
/// `a_revoked_device_cannot_revoke_a_third_party_however_it_dates_the_op`.
///
/// So non-retroactivity is retired here deliberately, for this op family only.
/// Revocation stays non-retroactive everywhere else in this module — a cert
/// issued under a superseded identity still identifies its device — and this
/// family is the exception because its effect can be re-derived, which is the
/// same scoping argument ADR-0041 makes for gating control ops and not entity
/// ops. What is lost is an administrative act by a device the account has
/// expelled: a thing a human can simply do again from a device the account
/// still trusts, and a thing they would want to look at again anyway.
#[test]
fn a_revocation_written_before_the_senders_own_cut_is_unwound_when_the_sender_is_revoked() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // A retires C, apparently long before anything happens to A.
    revoke(&er, &mut db, &ea, c_id, T0);
    assert!(
        revocation_row(&db, &c_id).is_some(),
        "before it knows better, this replica believes A's op"
    );
    // A is itself revoked a minute later.
    revoke(&er, &mut db, &eb, a_id, T0 + 60_000);

    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "a revoked device's revocations do not stand, whatever date they carry"
    );
    assert!(
        er.is_revoked(db.conn(), &a_id).unwrap(),
        "and A itself is out"
    );

    // The remedy, which is what makes the loss tolerable: anyone the account
    // still trusts revokes C again.
    revoke(&er, &mut db, &eb, c_id, T0 + 120_000);
    assert!(er.is_revoked(db.conn(), &c_id).unwrap());
}

/// **A mutual pair locks both devices out of revoking anybody else.**
///
/// Green before this test existed and green after, which is the point of it:
/// the behaviour is deliberate and pinned, not accidental and undiscovered.
///
/// The mutual exception is what keeps two devices revoking each other
/// converging on *both* revocations — see
/// `a_gate_on_the_sender_does_not_let_a_back_dated_revocation_silence_its_target`
/// for the takeover it prevents. Its cost is here. Once X and O have revoked
/// each other, each one's revoker set holds exactly one entry and it is the
/// other, so each is forgiven for revoking the other and gated for revoking
/// anyone else — including the honest device of the pair, until a third
/// current device says which of the two it meant
/// (`a_third_current_device_settles_which_half_of_a_mutual_pair_the_account_meant`).
///
/// What it costs is *third-party* revocation, and it costs it to the two
/// devices in the relationship and to nobody else. That bound was asserted
/// here through `et` — a device neither of them ever named — which is exactly
/// the device the bound is least interesting about, while `ed`, the device
/// **both** of them named, went unchecked. It was also, for one revision of
/// the fold, false of `ed`: each gated op seated its sender in `ed`'s revoker
/// set, so being named by a device the account had expelled cost `ed` its own
/// ability to revoke. The discount pass closed that and the assertion on `ed`
/// is here so the bound is pinned where it can fail.
///
/// Revocation is not gated on `ID_S_priv` anywhere either, so identity
/// rotation and pairing sponsorship are untouched, and the last lines here are
/// the remedy: any current device outside the pair still revokes whoever it
/// likes. The lockout is total only in a two-device account, where there is no
/// third device to ask.
///
/// It is recorded rather than repaired because no ledger-only rule can do
/// better: after a mutual revocation the two devices are symmetric in the
/// ledger and nothing tells the honest one from the compromised one.
#[test]
fn a_mutual_pair_locks_both_devices_out_of_third_party_revocation() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ez = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());
    let (d_id, z_id) = (ed.keychain.device_id(), ez.keychain.device_id());

    // X is compromised and gets its revocation of O in first; O answers.
    revoke(&er, &mut db, &ex, o_id, T0 + 10_000);
    revoke(&er, &mut db, &eo, x_id, T0 + 20_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        er.is_revoked(db.conn(), &o_id).unwrap(),
        "the mutual exception is what puts both of them out"
    );

    // O now reaches for a third party, and is gated: its only revoker is X,
    // and X is not the device this op names.
    revoke(&er, &mut db, &eo, d_id, T0 + 30_000);
    assert_eq!(
        revocation_row(&db, &d_id),
        None,
        "the surviving half of a mutual pair cannot revoke a third party"
    );
    // And symmetrically, so this is a property of the pair and not of O.
    revoke(&er, &mut db, &ex, d_id, T0 + 40_000);
    assert_eq!(revocation_row(&db, &d_id), None);
    assert_eq!(
        ledger_rows(&db),
        4,
        "both refused ops are stored; only their effect is refused"
    );

    // **D is the device the pair actually reached**, and it is the assertion
    // this test was missing: two gated ops named it, and until the discount
    // pass existed each of those ops seated its sender in D's revoker set, so
    // D — which nobody has revoked — could revoke nothing. Both are
    // discounted, because each of X and O is revoked by the other, who is not
    // D.
    revoke(&er, &mut db, &ed, z_id, T0 + 45_000);
    assert!(
        er.is_revoked(db.conn(), &z_id).unwrap(),
        "being named by a gated op must not cost D its own ability to revoke"
    );

    // The remedy, and the bound on the damage: any other current device in the
    // account still revokes whoever it likes.
    revoke(&er, &mut db, &et, d_id, T0 + 50_000);
    assert!(
        er.is_revoked(db.conn(), &d_id).unwrap(),
        "a third current device is the way out, and a two-device account has none"
    );
    assert_eq!(
        revocation_row(&db, &z_id),
        None,
        "and D's own revocation goes with it, which is decision 1's unwinding \
         and not this gate"
    );
}

/// **A third current device already says which half of a mutual pair the
/// account meant. That is why an un-revoke op is not what settles the
/// lockout.**
///
/// ADR-0041 said an un-revoke op would let the account say which reading of
/// a mutual pair it meant. ADR-0056
/// (`docs/11-adr/0056-a-revocation-is-withdrawn-only-by-its-author.md`) §3
/// rests on the claim that a revocation says it already. A current device T
/// revokes X, the half it believes compromised:
///
/// - X's revoker set becomes `{O, T}`. Nobody revokes T, so T is not
///   discounted, and X's revocation of O is gated.
/// - T's row revokes X from a sender other than O, so X is discounted out of
///   O's set. O's set is then empty, so O is current and ungated again.
///
/// The last assertion is the residual ADR-0056 §4 states. O's standing comes
/// back and its read bound does not. This replica bounded O when the mutual
/// pair landed, and the bound is a ratchet ([#411](https://github.com/justin13888/Sunrise/issues/411)).
#[test]
fn a_third_current_device_settles_which_half_of_a_mutual_pair_the_account_meant() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());
    let d_id = ed.keychain.device_id();

    // The mutual pair: X gets its revocation of O in first, and O answers.
    revoke(&er, &mut db, &ex, o_id, T0 + 10_000);
    revoke(&er, &mut db, &eo, x_id, T0 + 20_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(er.is_revoked(db.conn(), &o_id).unwrap());

    // T says X is the compromised half.
    revoke(&er, &mut db, &et, x_id, T0 + 30_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        !er.is_revoked(db.conn(), &o_id).unwrap(),
        "a revoker of X other than O gates X's revocation of O"
    );

    // O is ungated as well as current: it revokes a third party again.
    revoke(&er, &mut db, &eo, d_id, T0 + 40_000);
    assert!(
        er.is_revoked(db.conn(), &d_id).unwrap(),
        "T's row discounts X out of O's revoker set, so O is no longer locked out"
    );

    // The residual: the standing comes back, the keys do not.
    assert!(
        er.is_read_bounded(db.conn(), &o_id).unwrap(),
        "the read bound is a per-replica ratchet, and a revocation does not release it"
    );
}

/// **A gated op must not seat its sender in the revoker set of the device it
/// named.**
///
/// The hole the discount pass closes, asserted from the direction that makes
/// it an account takeover rather than a curiosity. The revoker map is built
/// from *every* row and only the walk judges one, so before the discount a row
/// the walk threw away still left its sender sitting in its target's set — and
/// one entry in that set gates the target against every third party, for good.
///
/// The cost to the attacker was N ordinary ops and nothing else. No crafted
/// stamp, no back-dating, no id discovery: X already holds every `devices`
/// row, a revoked device's ops are stored unconditionally, and its envelopes
/// still verify. X names each remaining device once; every op is correctly
/// gated and revokes nobody; and the account can never revoke a stolen device
/// again on any replica.
///
/// So the second half here is the assertion that matters. P not being revoked
/// was always true. P still being *able to revoke* is what was lost.
#[test]
fn a_gated_revocation_does_not_seat_its_sender_in_its_targets_revoker_set() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eq = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, p_id) = (ex.keychain.device_id(), ep.keychain.device_id());
    let (q_id, d_id) = (eq.keychain.device_id(), ed.keychain.device_id());

    // O expels X: the ordinary administrative action.
    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());

    // X answers by naming the devices that are left, one ordinary op each.
    revoke(&er, &mut db, &ex, p_id, T0 + 10_000);
    revoke(&er, &mut db, &ex, q_id, T0 + 20_000);
    assert_eq!(revocation_row(&db, &p_id), None);
    assert_eq!(revocation_row(&db, &q_id), None);
    assert_eq!(
        ledger_rows(&db),
        3,
        "the two refused ops are stored; only their effect is refused"
    );

    // And this is the whole of it: a device an expelled device merely *named*
    // must still be able to expel a genuinely stolen one.
    revoke(&er, &mut db, &ep, d_id, T0 + 30_000);
    assert!(
        er.is_revoked(db.conn(), &d_id).unwrap(),
        "a gated op must not cost the device it named its own ability to revoke"
    );
    assert!(
        er.is_revoked(db.conn(), &x_id).unwrap(),
        "and X is still out: naming third parties did not move its own cut"
    );
}

/// **What the discount gives up (1): a third party revoking the sole revoker
/// returns its target to full standing.**
///
/// A known consequence, pinned so it stays deliberate. The discount asks of
/// the *ledger* — has anybody other than V expelled S? — and not of the
/// register, because asking the register is ADR-0041 §Alternatives (h)'s wider
/// form, which reopens decision 1's hole with the arrow reversed.
///
/// The price of asking the ledger is here. O expels X; P then expels O. Two
/// things follow, and only the second is new. Decision 1 already unwound O's
/// revocation of X, because a revoked device's revocations are not believed
/// whatever date they carry — so X was already off the revoked list before the
/// discount existed. What the discount adds is that X is no longer *gated*
/// either: P's row discounts O out of X's revoker set, and X revokes third
/// parties again.
///
/// It costs two revocations in one chain rather than the one ordinary op the
/// defect above cost, and the device doing the second of them is by
/// construction not the attacker.
/// A current device that revokes X again says which of the two readings the
/// account meant, and X is gated again. No un-revoke op is needed for that
/// (ADR-0056 §3).
#[test]
fn the_discount_rehabilitates_a_device_whose_sole_revoker_a_third_party_revokes() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());
    let t_id = et.keychain.device_id();

    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());

    // P expels O, and X falls off the revoked list with it. That half is
    // decision 1 and predates the discount entirely.
    revoke(&er, &mut db, &ep, o_id, T0 + 10_000);
    assert!(er.is_revoked(db.conn(), &o_id).unwrap());
    assert_eq!(
        revocation_row(&db, &x_id),
        None,
        "a revoked device's revocations stop being believed"
    );

    // This half is the discount's: X is ungated as well as unrevoked.
    revoke(&er, &mut db, &ex, t_id, T0 + 20_000);
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "discounting O out of X's set hands X back third-party revocation"
    );
}

/// **What the discount gives up (1a): one attacker holding two revoked
/// devices reaches shape (1) with a single op, and the "third party" is its
/// own second device.**
///
/// The sharpest reading of the condition above, and the one a threat model
/// has to carry, because "a third party revokes O" sounds like a bystander
/// and nothing requires it to be one. O revokes both X1 and X2 — one
/// administrative act against a pair of devices one person holds. X1 then
/// revokes O. The mutual exception lands both of those, so X1 and O are out;
/// and O being out unwinds O's *other* revocation, so X2 comes off the list.
/// That half is decision 1's retroactivity and predates the discount, which
/// is why the register up to here is what it was before the discount existed.
///
/// What the discount adds is the assertion after it: X1's row discounts O out
/// of X2's revoker set, because X1 is a sender other than X2 — so X2 is
/// ungated as well as unrevoked, and revokes the rest of the account.
///
/// The remedy is the mutual pair's remedy and no better: a device X2 reaches
/// may revoke X2 back, which re-gates X2 and leaves that device revoked as
/// well, because the exception lands X2's op too. A current device the
/// attacker never reached re-gates X2 only while X1 stays silent:
/// `a_second_expelled_device_discounts_the_third_device_that_revoked_the_first`
/// is the op that undoes it.
///
/// Pinned separately from the general statement of shape (1) because the cost
/// is one op from a device the account has already expelled, and the
/// precondition — two devices revoked by a single revoker — is an ordinary
/// thing for an account to do.
#[test]
fn the_discount_lets_one_of_two_devices_revoked_together_ungate_the_other() {
    let ex1 = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ex2 = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x1_id, x2_id) = (ex1.keychain.device_id(), ex2.keychain.device_id());
    let (o_id, t_id) = (eo.keychain.device_id(), et.keychain.device_id());

    // O expels both of the attacker's devices.
    revoke(&er, &mut db, &eo, x1_id, T0);
    revoke(&er, &mut db, &eo, x2_id, T0 + 10_000);
    assert!(er.is_revoked(db.conn(), &x1_id).unwrap());
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());

    // One op from X1. The register half of this predates the discount: the
    // mutual exception puts O out, and O being out unwinds its revocation of
    // X2.
    revoke(&er, &mut db, &ex1, o_id, T0 + 20_000);
    assert!(er.is_revoked(db.conn(), &o_id).unwrap());
    assert!(er.is_revoked(db.conn(), &x1_id).unwrap());
    assert_eq!(
        revocation_row(&db, &x2_id),
        None,
        "O's other revocation is unwound with O, which is decision 1"
    );

    // And this is the discount's half: X2 is ungated too, so the attacker's
    // surviving device now revokes the account.
    revoke(&er, &mut db, &ex2, t_id, T0 + 30_000);
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "X1's row discounts O out of X2's set, so X2 revokes third parties"
    );

    // What is left is the mutual pair again, with the same cost: T revokes X2
    // back and re-gates it, and stays revoked itself.
    revoke(&er, &mut db, &et, x2_id, T0 + 40_000);
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "and T stays out: the pair converges on both revocations"
    );
    revoke(&er, &mut db, &ex2, o_id, T0 + 50_000);
    assert_eq!(
        revocation_row(&db, &o_id).map(|r| r.2),
        Some(x1_id.to_vec()),
        "X2 is gated again, so its second attempt on O does not move the row"
    );
}

/// **What the discount gives up (1b): the attacker's first device undoes the
/// third current device's remedy with one gated op.**
///
/// Shape (1a) above ends with T, a current device the attacker never reached,
/// revoking X2, and X2 gated again. That holds only while X1 stays silent. The
/// discount of `s` from `v`'s revoker set asks whether the ledger holds a row
/// revoking `s` from a sender other than `v`, and it counts every stored row,
/// gated or not. So X1 names T. The op is gated and revokes nobody. It is
/// still a row revoking T from a sender other than X2, so it discounts T out
/// of X2's set. O is already discounted by X1's first row, so X2's set is
/// empty again. X2 then revokes a device nobody had touched, and then T
/// itself.
///
/// So two devices one attacker holds, once the same revoker has expelled both,
/// neutralise every revocation the account sends against either of them. Each
/// honest revoker costs the attacker one op. No current device settles this
/// through the ledger, because the answer is always another row the other
/// attacker device can discount. Pinned so the residual stays deliberate for
/// ordinary rows: ADR-0058's identity-signed revocations settle it, and
/// [#454](https://github.com/justin13888/Sunrise/issues/454) builds them.
#[test]
fn a_second_expelled_device_discounts_the_third_device_that_revoked_the_first() {
    let ex1 = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ex2 = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x1_id, x2_id) = (ex1.keychain.device_id(), ex2.keychain.device_id());
    let (o_id, t_id) = (eo.keychain.device_id(), et.keychain.device_id());
    let d_id = ed.keychain.device_id();

    // Shape (1a): O expels both, and one op from X1 ungates X2.
    revoke(&er, &mut db, &eo, x1_id, T0);
    revoke(&er, &mut db, &eo, x2_id, T0 + 10_000);
    revoke(&er, &mut db, &ex1, o_id, T0 + 20_000);

    // The documented remedy: T, which the attacker never reached, revokes X2.
    revoke(&er, &mut db, &et, x2_id, T0 + 30_000);
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());

    // X1 names T. The op is gated, so T stays current...
    revoke(&er, &mut db, &ex1, t_id, T0 + 40_000);
    assert_eq!(
        revocation_row(&db, &t_id),
        None,
        "X1 is gated, so its revocation of T does not land"
    );

    // ...and its stored row still discounts T out of X2's revoker set.
    revoke(&er, &mut db, &ex2, d_id, T0 + 50_000);
    assert!(
        er.is_revoked(db.conn(), &d_id).unwrap(),
        "the residual: X2 is on the revoked list and revokes an untouched device"
    );
    revoke(&er, &mut db, &ex2, t_id, T0 + 60_000);
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "and it revokes the device that tried to settle it"
    );
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());
}

/// **What the discount gives up (2): a device that is still on the revoked
/// list revokes third parties, when a chain revokes its revoker's revoker.**
///
/// The sharper shape of the consequence above, and the one that is genuinely a
/// hole rather than a re-reading: here X stays revoked *and* is ungated, which
/// is the pair of facts the gate exists to keep apart.
///
/// It needs three revocations arranged in a chain — O expels X, P expels O, Q
/// expels P — and it is the chain that does it. Q's row discounts P out of O's
/// set, so O's revocation of X lands and X is revoked; P's row discounts O out
/// of X's set, so X is ungated. Neither discount can be written by X, because
/// a device authors only rows whose sender is itself and every discount of S
/// from V's set needs a row from a sender that is not V. So X cannot reach
/// this state alone — though "not alone" is a weaker bound than it sounds,
/// and `the_discount_lets_one_of_two_devices_revoked_together_ungate_the_other`
/// is where that is asserted.
///
/// Recorded in ADR-0041 §"What a user sees" item 4 with the bound that
/// replaces the one the poisoning defect refuted.
#[test]
fn the_discount_leaves_a_revoked_device_revoking_when_a_chain_revokes_its_revoker() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eq = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [6u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());
    let (p_id, t_id) = (ep.keychain.device_id(), et.keychain.device_id());

    revoke(&er, &mut db, &eo, x_id, T0);
    revoke(&er, &mut db, &ep, o_id, T0 + 10_000);
    revoke(&er, &mut db, &eq, p_id, T0 + 20_000);

    assert!(
        er.is_revoked(db.conn(), &x_id).unwrap(),
        "Q's row gates P's, so O's revocation of X stands and X is revoked"
    );
    assert!(er.is_revoked(db.conn(), &p_id).unwrap());
    assert_eq!(
        revocation_row(&db, &o_id),
        None,
        "and O is not revoked, because the device that expelled it was expelled"
    );

    revoke(&er, &mut db, &ex, t_id, T0 + 30_000);
    assert!(
        er.is_revoked(db.conn(), &t_id).unwrap(),
        "the residual: X is on the revoked list and revokes a third party anyway"
    );
}

/// **A revocation that stops being believed says so.**
///
/// The one outcome of this design a user could be surprised by: a device that
/// was on the revoked list is on it no longer, because the fold learned that
/// whoever revoked it had itself been revoked. Judging the sender over the
/// whole ledger rather than over a prefix makes that routine rather than
/// exotic — it now happens whenever a revocation arrives *after* an op its
/// author back-dated — so the branch is worth an assertion rather than a
/// reading of the source.
///
/// It is a log line and not a returned value because the register it is about
/// is not the one the caller asked to change: `revoke_device` reports on its
/// own op through `CommandResult`, and the device list is what shows the rest.
/// `core.device.revocation_unwound` is catalogued in
/// `docs/10-cross-cutting/log-events.md`.
#[test]
fn a_revocation_the_fold_stops_believing_is_announced() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // A revokes C, and this replica believes it.
    revoke(&er, &mut db, &ea, c_id, T0);
    assert!(er.is_revoked(db.conn(), &c_id).unwrap());

    // Then it learns A was itself revoked, and C comes back off the list.
    let events = events_emitted_by(|| revoke(&er, &mut db, &eb, a_id, T0 + 60_000));

    assert_eq!(revocation_row(&db, &c_id), None);
    assert!(
        events
            .iter()
            .any(|ev| ev == "core.device.revocation_unwound"),
        "a device quietly leaving the revoked list is the one surprise this \
         design owes a user; got {events:?}"
    );
}
