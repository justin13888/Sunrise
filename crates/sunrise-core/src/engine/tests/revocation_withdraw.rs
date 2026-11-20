//! Withdrawing a revocation (ADR-0056, issue #383): the fold reads each
//! pair's head, the read bound is released only on §4's three conditions,
//! and `Command::WithdrawRevocation` is gated, reports and backfills.

use super::*;
use crate::commands::WithdrawalOutcome;
use crate::control_op::DeviceRevokeWithdrawPayload;
use crate::feature::REVOKE_WITHDRAW_FEATURE;

fn engines(n: u8) -> Vec<Engine> {
    (1..=n)
        .map(|i| engine_seeded(ROOT, [i; 32], Arc::new(FakeClock(PLMutex::new(T0)))))
        .collect()
}

/// `sender` withdraws its revocation of `target` at `hlc_ms`, delivered to
/// `receiver` through the path a synced op takes.
fn withdraw(receiver: &Engine, db: &mut Db, sender: &Engine, target: [u8; 16], hlc_ms: u64) {
    let sender_id = sender.keychain.device_id();
    let hlc = Hlc::at(hlc_ms);
    let inner = InnerOp::DeviceRevokeWithdraw(DeviceRevokeWithdrawPayload {
        revoked_device_id: target,
    });
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, &inner, &sender_id, hlc, hlc.physical_ms, 1)
            .map(|_| ())
    })
    .unwrap();
}

/// Advertise `core.revoke_withdraw` from `who` to `receiver`.
fn advertise(receiver: &Engine, db: &mut Db, who: &Engine) {
    let inner = InnerOp::DeviceFeatures(crate::control_op::DeviceFeaturesPayload {
        features: vec![REVOKE_WITHDRAW_FEATURE.to_owned()],
        unknown: Unknowns::new(),
    });
    let id = who.keychain.device_id();
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, &inner, &id, Hlc::at(T0), T0, 1)
            .map(|_| ())
    })
    .unwrap();
}

fn withdraw_cmd(e: &Engine, db: &mut Db, target: [u8; 16]) -> Result<CommandResult, EngineError> {
    e.apply(
        db,
        Command::WithdrawRevocation {
            device_id: EntityRef::new(EntityKind::Device, target),
        },
    )
}

fn ledger_len(db: &Db) -> i64 {
    db.conn()
        .query_row("SELECT count(*) FROM device_revoke_ops", [], |r| r.get(0))
        .unwrap()
}

fn emitted(db: &Db, kind: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT count(*) FROM ops WHERE inner_kind = ?",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
}

/// **A withdrawn pair is inert, and a later revocation from the same sender
/// makes it live again** (ADR-0056 §2). The register is the one a ledger
/// without the pair would fold, so the device is current; the next
/// revocation is the pair's head and the device is revoked again. The
/// compaction keeps one row, the head, throughout.
#[test]
fn a_withdrawn_pair_is_inert_until_its_sender_revokes_again() {
    let e = engines(3);
    let (eo, ex, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke(er, &mut db, eo, x_id, T0);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());

    withdraw(er, &mut db, eo, x_id, T0 + 1_000);
    assert!(!er.is_revoked(db.conn(), &x_id).unwrap(), "inert");
    assert_eq!(ledger_len(&db), 1, "the pair keeps its head");

    revoke(er, &mut db, eo, x_id, T0 + 2_000);
    assert!(
        er.is_revoked(db.conn(), &x_id).unwrap(),
        "live again under a later revocation"
    );
    assert_eq!(ledger_len(&db), 1);
}

/// **A revocation and a withdrawal at one stamp: the revocation wins**, in
/// either delivery order, and both rows are kept because the kind is part of
/// the row's identity (ADR-0056 §2, migration 0053) until the compaction
/// keeps the head.
#[test]
fn a_revocation_wins_a_tie_with_a_withdrawal_in_either_order() {
    let e = engines(3);
    let (eo, ex, er) = (&e[0], &e[1], &e[2]);
    let x_id = ex.keychain.device_id();

    let mut one = db_root(ROOT);
    revoke(er, &mut one, eo, x_id, T0);
    withdraw(er, &mut one, eo, x_id, T0);

    let mut two = db_root(ROOT);
    withdraw(er, &mut two, eo, x_id, T0);
    revoke(er, &mut two, eo, x_id, T0);

    for db in [&one, &two] {
        assert!(er.is_revoked(db.conn(), &x_id).unwrap(), "fails closed");
        assert!(read_bound_row(db, &x_id).is_some());
        let kind: String = db
            .conn()
            .query_row("SELECT kind FROM device_revoke_ops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kind, "revoke", "the compaction keeps the revocation");
    }
}

/// **The register and the release are functions of the op set**: a
/// revocation and a later withdrawal fold to the same register and the same
/// bound whichever arrives first.
#[test]
fn a_withdrawal_converges_in_either_delivery_order() {
    let e = engines(3);
    let (eo, ex, er) = (&e[0], &e[1], &e[2]);
    let x_id = ex.keychain.device_id();

    let mut one = db_root(ROOT);
    revoke(er, &mut one, eo, x_id, T0);
    withdraw(er, &mut one, eo, x_id, T0 + 1_000);

    let mut two = db_root(ROOT);
    withdraw(er, &mut two, eo, x_id, T0 + 1_000);
    revoke(er, &mut two, eo, x_id, T0);

    for db in [&one, &two] {
        assert_eq!(revocation_row(db, &x_id), None);
        assert_eq!(
            read_bound_row(db, &x_id),
            None,
            "an ungated author's withdrawal releases the bound"
        );
    }
}

/// **The bound is released only once every claimant has withdrawn**
/// (ADR-0056 §4 (b)). O and P revoked X. O's withdrawal leaves P's live
/// claim, so X stays revoked and bounded; P's withdrawal releases it.
#[test]
fn the_bound_waits_for_every_claimant_to_withdraw() {
    let e = engines(4);
    let (eo, ep, ex, er) = (&e[0], &e[1], &e[2], &e[3]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke(er, &mut db, eo, x_id, T0);
    revoke(er, &mut db, ep, x_id, T0 + 1_000);
    withdraw(er, &mut db, eo, x_id, T0 + 2_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap(), "P's claim stands");
    assert!(read_bound_row(&db, &x_id).is_some());

    withdraw(er, &mut db, ep, x_id, T0 + 3_000);
    assert!(!er.is_revoked(db.conn(), &x_id).unwrap());
    assert_eq!(read_bound_row(&db, &x_id), None, "released");
}

/// **A revoked author cannot release an accomplice** (ADR-0056 §4 (c)).
/// O revoked a stolen X while O was honest; P then revoked O, which unwinds
/// O's revocation of X while X stays bounded (ADR-0041 item 5). O's
/// withdrawal of X changes nothing in the register, and since P gates O, it
/// does not release X's bound either.
#[test]
fn a_gated_authors_withdrawal_releases_nothing() {
    let e = engines(4);
    let (eo, ep, ex, er) = (&e[0], &e[1], &e[2], &e[3]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke(er, &mut db, eo, x_id, T0);
    revoke(er, &mut db, ep, eo.keychain.device_id(), T0 + 1_000);
    assert!(!er.is_revoked(db.conn(), &x_id).unwrap(), "unwound");
    assert!(read_bound_row(&db, &x_id).is_some(), "and still bounded");

    withdraw(er, &mut db, eo, x_id, T0 + 2_000);
    assert!(
        read_bound_row(&db, &x_id).is_some(),
        "a withdrawal from a gated sender releases no bound"
    );
}

/// **An unwound device stays bounded while its revoker's claim is live**,
/// even when a third device withdraws a claim of its own (ADR-0056 §4 (b)):
/// the release asks whether any live pair names the device, gated or not.
#[test]
fn a_live_gated_claim_keeps_the_bound() {
    let e = engines(5);
    let (eo, ep, eq, ex, er) = (&e[0], &e[1], &e[2], &e[3], &e[4]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke(er, &mut db, eo, x_id, T0);
    revoke(er, &mut db, eq, x_id, T0 + 500);
    revoke(er, &mut db, ep, eo.keychain.device_id(), T0 + 1_000);
    withdraw(er, &mut db, eq, x_id, T0 + 2_000);
    assert!(!er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        read_bound_row(&db, &x_id).is_some(),
        "O's gated claim on X is still live"
    );
}

/// **A withdrawal naming its own sender is refused**, as a revocation naming
/// its own sender is: nothing is stored.
#[test]
fn a_withdrawal_of_oneself_is_refused() {
    let e = engines(2);
    let (eo, er) = (&e[0], &e[1]);
    let mut db = db_root(ROOT);
    withdraw(er, &mut db, eo, eo.keychain.device_id(), T0);
    assert_eq!(ledger_len(&db), 0);
}

/// **The command refuses while a device could drop the op, naming it, the
/// device being reinstated included** (ADR-0056 §7). Nothing is emitted.
#[test]
fn the_command_names_the_devices_that_lack_the_feature() {
    let e = engines(3);
    let (eh, ex, eold) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    trust(eh, &mut db, ex);
    trust(eh, &mut db, eold);
    let x_id = ex.keychain.device_id();
    revoke(eh, &mut db, eh, x_id, T0);

    let err = withdraw_cmd(eh, &mut db, x_id).unwrap_err();
    let EngineError::FeatureUnsupportedByDevices { feature, devices } = err else {
        panic!("expected the feature refusal, got {err:?}");
    };
    assert_eq!(feature, REVOKE_WITHDRAW_FEATURE);
    let mut expected = vec![eold.keychain.device_id(), x_id];
    expected.sort_unstable();
    assert_eq!(devices, expected);
    assert_eq!(emitted(&db, "device.revoke_withdraw"), 0);
    assert!(eh.is_revoked(db.conn(), &x_id).unwrap());

    // The device being reinstated alone is enough to refuse.
    advertise(eh, &mut db, eold);
    let err = withdraw_cmd(eh, &mut db, x_id).unwrap_err();
    assert!(matches!(
        err,
        EngineError::FeatureUnsupportedByDevices { ref devices, .. } if devices == &vec![x_id]
    ));
}

/// **The command reinstates, re-seals and cancels the unsent relay
/// intent** (ADR-0056 §4, §5): the vault is told it requires the feature
/// before the op, the bound is released, the device is sent keys, and the
/// row stops offering a withdrawal.
#[test]
fn the_command_reinstates_and_resends_keys() {
    // Real keys, so the epochs minted below are rows this device holds and
    // a backfill has something to seal.
    let e: Vec<Engine> = (1..=3u8)
        .map(|i| engine_random_keys(ROOT, [i; 32], Arc::new(FakeClock(PLMutex::new(T0)))))
        .collect();
    let (eh, ex, eother) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    for d in [ex, eother] {
        trust(eh, &mut db, d);
        advertise(eh, &mut db, d);
    }
    let x_id = ex.keychain.device_id();
    revoke(eh, &mut db, eh, x_id, T0);
    // Epochs minted while X is revoked: X is sealed none of them.
    eh.apply(
        &mut db,
        Command::CreateTask(TaskDraft {
            title: "written while X was out".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(envelopes_to(eh, &db, &x_id).is_empty());
    db.conn()
        .execute(
            "INSERT INTO relay_revocation_intents (device_id, created_at_ms) VALUES (?, ?)",
            params![&x_id[..], 1_i64],
        )
        .unwrap();
    assert!(device_list_row(eh, &db, &x_id).withdrawable);

    let r = withdraw_cmd(eh, &mut db, x_id).unwrap();
    let w = r.withdrawal.expect("a withdrawal reports");
    assert_eq!(w.outcome, WithdrawalOutcome::Reinstated);
    assert!(w.read_bound_released);
    let held = db.with_tx(Keychain::held_epochs_tx).unwrap();
    assert!(held.len() >= 2, "the meta and Inbox epochs at least");
    let mut sealed = envelopes_to(eh, &db, &x_id);
    sealed.sort_unstable();
    let mut want = held;
    want.sort_unstable();
    assert_eq!(sealed, want, "the device is sent every epoch it missed");
    assert_eq!(usize::try_from(w.keys_resent).unwrap(), want.len());
    assert!(w.relay_intent_cancelled);

    assert!(eh
        .vault_requires(db.conn(), REVOKE_WITHDRAW_FEATURE)
        .unwrap());
    assert_eq!(emitted(&db, "device.revoke_withdraw"), 1);
    assert!(!eh.is_revoked(db.conn(), &x_id).unwrap());
    assert_eq!(read_bound_row(&db, &x_id), None);
    let intents: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM relay_revocation_intents", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(intents, 0);
    let row = device_list_row(eh, &db, &x_id);
    assert!(!row.withdrawable && !row.revoked && !row.read_bounded);

    // A second withdrawal has nothing of this device's left to withdraw.
    let again = withdraw_cmd(eh, &mut db, x_id).unwrap();
    assert_eq!(
        again.withdrawal.map(|w| w.outcome),
        Some(WithdrawalOutcome::NotAuthor)
    );
    assert_eq!(emitted(&db, "device.revoke_withdraw"), 1);
}

/// **Another device's revocation is not this device's to withdraw**
/// (ADR-0056 §1): the result says so, nothing is emitted, and the row does
/// not offer it.
#[test]
fn the_command_does_not_withdraw_another_devices_revocation() {
    let e = engines(3);
    let (eh, ep, ex) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    trust(eh, &mut db, ex);
    let x_id = ex.keychain.device_id();
    revoke(eh, &mut db, ep, x_id, T0);
    assert!(!device_list_row(eh, &db, &x_id).withdrawable);

    let r = withdraw_cmd(eh, &mut db, x_id).unwrap();
    assert_eq!(
        r.withdrawal.map(|w| w.outcome),
        Some(WithdrawalOutcome::NotAuthor)
    );
    assert_eq!(emitted(&db, "device.revoke_withdraw"), 0);
    assert!(eh.is_revoked(db.conn(), &x_id).unwrap());
}

/// **Another claimant keeps the device out, and the result says so**
/// (ADR-0056 §6): this device's withdrawal lands, the device stays revoked,
/// and no key is sent.
#[test]
fn the_command_reports_a_claim_that_remains() {
    let e = engines(3);
    let (eh, ep, ex) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    trust(eh, &mut db, ex);
    trust(eh, &mut db, ep);
    advertise(eh, &mut db, ex);
    advertise(eh, &mut db, ep);
    let x_id = ex.keychain.device_id();
    revoke(eh, &mut db, eh, x_id, T0);
    revoke(eh, &mut db, ep, x_id, T0 + 1_000);

    let w = withdraw_cmd(eh, &mut db, x_id).unwrap().withdrawal.unwrap();
    assert_eq!(w.outcome, WithdrawalOutcome::StillClaimed);
    assert!(!w.read_bound_released);
    assert_eq!(w.keys_resent, 0);
    assert!(eh.is_revoked(db.conn(), &x_id).unwrap());
}

/// **A claim of this device's own that remains keeps the relay's telling
/// queued** (ADR-0056 §5): the intents table holds one row per device, shared
/// by this device's ordinary revocation and its identity-signed one
/// (ADR-0058). Withdrawing the ordinary one leaves the authority claim live
/// and the device revoked, so the row stays, the drain still sends it, and
/// the result does not say it was cancelled.
#[test]
fn a_claim_that_remains_keeps_the_relay_intent() {
    let e = engines(3);
    let (eh, ex, eother) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    for d in [ex, eother] {
        trust(eh, &mut db, d);
        advertise(eh, &mut db, d);
    }
    let x_id = ex.keychain.device_id();
    revoke(eh, &mut db, eh, x_id, T0);
    super::revocation_authority::revoke_by_identity(eh, &mut db, eh, x_id, T0 + 1_000);
    db.conn()
        .execute(
            "INSERT INTO relay_revocation_intents (device_id, created_at_ms) VALUES (?, ?)",
            params![&x_id[..], 1_i64],
        )
        .unwrap();
    assert!(device_list_row(eh, &db, &x_id).withdrawable);

    let w = withdraw_cmd(eh, &mut db, x_id).unwrap().withdrawal.unwrap();
    assert_eq!(w.outcome, WithdrawalOutcome::StillClaimed);
    assert!(!w.relay_intent_cancelled);
    assert_eq!(emitted(&db, "device.revoke_withdraw"), 1);
    assert!(eh.is_revoked(db.conn(), &x_id).unwrap());
    assert_eq!(pending_relay_revocations(&db), vec![x_id]);
    let owed = db
        .conn()
        .prepare(crate::relay_intents::OWED_RELAY_REVOCATIONS_SQL)
        .unwrap()
        .query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(owed, vec![x_id.to_vec()], "the drain still sends it");
}

/// **A revocation that rotated the identity away cannot be undone by a
/// withdrawal** (ADR-0056 §4): the device's cert names a retired identity,
/// so it is not a member, the result says it must be paired again, and
/// nothing is sealed to it.
#[test]
fn the_command_reports_a_device_the_rotation_left_behind() {
    let e = engines(3);
    let (eh, ex, eother) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    for d in [ex, eother] {
        trust(eh, &mut db, d);
        advertise(eh, &mut db, d);
    }
    let x_id = ex.keychain.device_id();
    // The command revokes as the holder, which rotates the identity. The
    // revocation is ordinary, because `eother` does not advertise
    // `core.revoke_authority`; an authority one is not this command's to
    // withdraw.
    eh.apply(
        &mut db,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, x_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();
    assert!(!device_list_row(eh, &db, &x_id).current);

    let w = withdraw_cmd(eh, &mut db, x_id).unwrap().withdrawal.unwrap();
    assert_eq!(w.outcome, WithdrawalOutcome::RepairRequired);
    assert_eq!(
        w.keys_resent, 0,
        "a device that is not a member gets nothing"
    );
    assert!(!eh.is_revoked(db.conn(), &x_id).unwrap());
}

/// **A device cannot withdraw a revocation of itself.**
#[test]
fn the_command_refuses_this_device() {
    let e = engines(1);
    let mut db = db_root(ROOT);
    let me = e[0].keychain.device_id();
    assert!(matches!(
        withdraw_cmd(&e[0], &mut db, me),
        Err(EngineError::Invalid(_))
    ));
}
