//! Identity-signed revocations (ADR-0058, issue #454): the authority row in
//! the fold, the command that signs one, and the carry-forward across a
//! rotation; and their identity-signed withdrawal (§6, issue #1239).

use super::*;
use crate::bytes::hex_bytes;
use crate::commands::WithdrawalOutcome;
use crate::control_op::{
    DeviceRevokeByIdentityPayload, DeviceRevokeWithdrawByIdentityPayload,
    DeviceRevokeWithdrawPayload,
};
use crate::feature::{
    REVOKE_AUTHORITY_FEATURE, REVOKE_WITHDRAW_AUTHORITY_FEATURE, REVOKE_WITHDRAW_FEATURE,
};
use sunrise_crypto::{
    identity_id_from_pub, IdentitySigningKeyPair, RevocationClaim, WithdrawalClaim,
};

/// The `ID_S_priv` every test keychain is built with
/// (`Keychain::for_test_seeded`), so the identity at the head of a vault that
/// has not rotated.
fn genesis_key() -> IdentitySigningKeyPair {
    IdentitySigningKeyPair::from_secret_bytes(&[0x5a; 32])
}

/// `sender` revokes `target` as the identity `key` holds, delivered to
/// `receiver`. `sig` replaces the signature when it is `Some`.
fn revoke_by_identity_with(
    receiver: &Engine,
    db: &mut Db,
    sender: &Engine,
    target: [u8; 16],
    hlc_ms: u64,
    key: &IdentitySigningKeyPair,
    sig: Option<[u8; 64]>,
) {
    let sender_id = sender.keychain.device_id();
    let hlc = Hlc::at(hlc_ms);
    let identity_id = identity_id_from_pub(&key.public_bytes());
    let claim = RevocationClaim {
        identity_id,
        sender_device_id: sender_id,
        revoked_device_id: target,
        hlc_physical_ms: hlc.physical_ms,
        hlc_logical: hlc.logical,
        reason_code: RevokeReason::Stolen.as_str().to_owned(),
    };
    let identity_sig = sig.unwrap_or_else(|| claim.sign(key).unwrap());
    let inner = InnerOp::DeviceRevokeByIdentity(DeviceRevokeByIdentityPayload {
        revoked_device_id: target,
        reason_code: RevokeReason::Stolen,
        identity_id,
        identity_sig,
    });
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, &inner, &sender_id, hlc, hlc.physical_ms, 1)
            .map(|_| ())
    })
    .unwrap();
}

/// [`revoke_by_identity_with`] signed by the genesis identity.
pub(super) fn revoke_by_identity(
    receiver: &Engine,
    db: &mut Db,
    sender: &Engine,
    target: [u8; 16],
    hlc_ms: u64,
) {
    revoke_by_identity_with(receiver, db, sender, target, hlc_ms, &genesis_key(), None);
}

/// Whether the register says an authority row names `device`.
fn by_authority(db: &Db, device: &[u8; 16]) -> bool {
    db.conn()
        .query_row(
            "SELECT by_authority FROM device_revocations WHERE device_id = ?",
            params![&device[..]],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
        .unwrap_or(false)
}

fn engines(n: u8) -> Vec<Engine> {
    (1..=n)
        .map(|i| engine_seeded(ROOT, [i; 32], Arc::new(FakeClock(PLMutex::new(T0)))))
        .collect()
}

/// **The shape #394 could not close, closed by the authority** (ADR-0058
/// §4). The five steps of
/// `a_second_expelled_device_discounts_the_third_device_that_revoked_the_first`,
/// with O's two revocations signed by the identity in force.
///
/// O's authority rows seat `IDENTITY` in the sets of X1 and X2, so X1's
/// revocation of O is gated and O stays current. X1 and X2 can discount every
/// ordinary revoker and never `IDENTITY`, so every op either writes stays
/// gated: X2 revokes nobody, D and T included.
#[test]
fn an_authority_revocation_keeps_the_second_expelled_device_gated() {
    let e = engines(6);
    let (ex1, ex2, eo, et, ed, er) = (&e[0], &e[1], &e[2], &e[3], &e[4], &e[5]);
    let mut db = db_root(ROOT);
    let (x1_id, x2_id) = (ex1.keychain.device_id(), ex2.keychain.device_id());
    let (o_id, t_id) = (eo.keychain.device_id(), et.keychain.device_id());
    let d_id = ed.keychain.device_id();

    // 1. O expels both, by authority.
    revoke_by_identity(er, &mut db, eo, x1_id, T0);
    revoke_by_identity(er, &mut db, eo, x2_id, T0 + 10_000);
    assert!(by_authority(&db, &x1_id) && by_authority(&db, &x2_id));

    // 2. X1 revokes O. Gated: X1's revoker is the identity, not O.
    revoke(er, &mut db, ex1, o_id, T0 + 20_000);
    assert_eq!(revocation_row(&db, &o_id), None, "O stays current");
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());

    // 3-5. T revokes X2, X1 names T, X2 names D and T.
    revoke(er, &mut db, et, x2_id, T0 + 30_000);
    revoke(er, &mut db, ex1, t_id, T0 + 40_000);
    revoke(er, &mut db, ex2, d_id, T0 + 50_000);
    revoke(er, &mut db, ex2, t_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &d_id),
        None,
        "X2 stays gated: D is current"
    );
    assert_eq!(revocation_row(&db, &t_id), None, "and so is T");
    assert!(er.is_revoked(db.conn(), &x1_id).unwrap());
    assert!(er.is_revoked(db.conn(), &x2_id).unwrap());
    assert!(
        by_authority(&db, &x2_id),
        "the authority still names X2 after T's ordinary row won the register"
    );
}

/// The holder closes the shape after the fact (ADR-0058 §4): where a paired
/// O made ordinary revocations and X1's answer landed, the holder H revokes
/// X1 and X2 by authority, X1's revocation of O is gated, and O is current
/// again.
#[test]
fn the_holder_closes_the_shape_after_the_fact() {
    let e = engines(4);
    let (ex1, ex2, eo, eh) = (&e[0], &e[1], &e[2], &e[3]);
    let mut db = db_root(ROOT);
    let (x1_id, x2_id, o_id) = (
        ex1.keychain.device_id(),
        ex2.keychain.device_id(),
        eo.keychain.device_id(),
    );

    revoke(eh, &mut db, eo, x1_id, T0);
    revoke(eh, &mut db, eo, x2_id, T0 + 10_000);
    revoke(eh, &mut db, ex1, o_id, T0 + 20_000);
    assert!(eh.is_revoked(db.conn(), &o_id).unwrap(), "the mutual pair");
    assert!(!eh.is_revoked(db.conn(), &x2_id).unwrap(), "X2 unwound");

    revoke_by_identity(eh, &mut db, eh, x1_id, T0 + 30_000);
    revoke_by_identity(eh, &mut db, eh, x2_id, T0 + 40_000);
    assert!(
        !eh.is_revoked(db.conn(), &o_id).unwrap(),
        "O is current again"
    );
    assert!(eh.is_revoked(db.conn(), &x1_id).unwrap());
    assert!(eh.is_revoked(db.conn(), &x2_id).unwrap());
}

/// A signature that does not verify makes the row an ordinary one from its
/// sender: the mutual exception reaches it as before, and the register does
/// not call the device revoked by authority.
#[test]
fn an_unverifiable_signature_folds_as_an_ordinary_row() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());

    revoke_by_identity_with(er, &mut db, eo, x_id, T0, &genesis_key(), Some([7; 64]));
    assert!(
        er.is_revoked(db.conn(), &x_id).unwrap(),
        "an ordinary revocation"
    );
    assert!(!by_authority(&db, &x_id));

    revoke(er, &mut db, ex, o_id, T0 + 10_000);
    assert!(
        er.is_revoked(db.conn(), &o_id).unwrap(),
        "X's answer lands under the mutual exception, as it would for an unsigned row"
    );
}

/// A row signed by an identity that is not the head folds as ordinary: the
/// authority is the identity in force, never a retired one (ADR-0058
/// §Alternatives (b)). Here the replica's chain moves first, so the genesis
/// key is retired by the time its row arrives.
#[test]
fn a_row_signed_under_a_retired_identity_folds_as_ordinary() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    trust(er, &mut db, ex);
    trust(er, &mut db, eo);
    let (x_id, o_id) = (ex.keychain.device_id(), eo.keychain.device_id());

    er.rotate_identity(&mut db, None, true).unwrap();
    assert_ne!(
        er.current_identity(db.conn()).unwrap().identity_id,
        identity_id_from_pub(&genesis_key().public_bytes()),
        "the head has moved off the genesis identity"
    );

    revoke_by_identity(er, &mut db, eo, x_id, T0);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        !by_authority(&db, &x_id),
        "a retired identity has no authority"
    );
    revoke(er, &mut db, ex, o_id, T0 + 10_000);
    assert!(
        er.is_revoked(db.conn(), &o_id).unwrap(),
        "and X's answer lands, as against any ordinary row"
    );
}

/// Applying a transition re-runs the fold (ADR-0058 §3): a row that was an
/// authority row stops being one the moment the chain moves past the
/// identity that signed it, with no revocation op in between.
#[test]
fn a_moved_head_refolds_the_register() {
    let e = engines(2);
    let (ex, er) = (&e[0], &e[1]);
    let mut db = db_root(ROOT);
    trust(er, &mut db, ex);
    let x_id = ex.keychain.device_id();
    revoke_by_identity(er, &mut db, er, x_id, T0);
    assert!(by_authority(&db, &x_id));
    // The rotation's roster is every device this replica has not bounded.
    // Releasing X's bound puts X on it, so the successor re-certifies X and
    // the carry-forward leaves it alone: what changes below is the fold's
    // reading of the one row it already held, and nothing else.
    db.with_tx(|tx| {
        tx.execute("DELETE FROM device_read_bounds", [])?;
        Ok(())
    })
    .unwrap();
    er.rotate_identity(&mut db, None, true).unwrap();
    assert_eq!(
        kinds_emitted(&db, "device.revoke_by_identity"),
        0,
        "nothing was carried forward"
    );
    assert!(
        er.is_revoked(db.conn(), &x_id).unwrap(),
        "the row still lands, as an ordinary row from a current device"
    );
    assert!(
        !by_authority(&db, &x_id),
        "and is no longer an authority row once the head moved"
    );
}

/// The ledger keeps an ordinary row and an authority row of one pair: the
/// compaction partitions on the identity too (migration 0048), so the
/// authority row is not lost to a greater-stamped ordinary one.
#[test]
fn the_compaction_keeps_an_authority_row_beside_an_ordinary_one() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(er, &mut db, eo, x_id, T0);
    revoke(er, &mut db, eo, x_id, T0 + 10_000);
    assert_eq!(
        ledger_rows(&db),
        2,
        "one row per (sender, target, identity)"
    );
    assert!(by_authority(&db, &x_id));
}

/// **The read bound converges once the revocation of C is signed**
/// ([#411](https://github.com/justin13888/Sunrise/issues/411), ADR-0058
/// §Consequences).
///
/// It uses the same two arrival orders as
/// `the_register_is_the_same_whichever_order_the_two_revocations_arrive`. That
/// test leaves the two replicas with different bounds on C, because `A -> C`
/// is an ordinary row there and is gated at every fold run on the replica that
/// learned of `B -> A` first. Here the holder signs `A -> C`, so it is an
/// authority row and is never gated. Both replicas put C in the register and
/// bound it, whichever op arrived first. `B -> A` stays ordinary, because the
/// gated row is the one that decides convergence.
#[test]
fn the_read_bound_is_the_same_whichever_order_an_authority_revocation_arrives() {
    let e = engines(4);
    let (ea, eb, er) = (&e[0], &e[1], &e[3]);
    let (a_id, c_id) = (ea.keychain.device_id(), e[2].keychain.device_id());

    // Replica one: B revokes A first, then A's signed revocation of C.
    let mut one = db_root(ROOT);
    revoke(er, &mut one, eb, a_id, T0);
    revoke_by_identity(er, &mut one, ea, c_id, T0 + 60_000);

    // Replica two: the same two ops in the other order.
    let mut two = db_root(ROOT);
    revoke_by_identity(er, &mut two, ea, c_id, T0 + 60_000);
    revoke(er, &mut two, eb, a_id, T0);

    for id in [&a_id, &c_id] {
        assert_eq!(revocation_row(&one, id), revocation_row(&two, id));
        assert_eq!(
            read_bound_row(&one, id).is_some(),
            read_bound_row(&two, id).is_some(),
            "the same op set gives the same read bound in either delivery order"
        );
    }
    assert!(
        read_bound_row(&one, &c_id).is_some(),
        "and that bound holds C out, on the replica that left it unbounded \
         when the row was ordinary"
    );
    assert!(by_authority(&one, &c_id) && by_authority(&two, &c_id));
}

/// Advertise `core.revoke_authority` from `who` to `receiver`.
fn advertise(receiver: &Engine, db: &mut Db, who: &Engine) {
    let inner = InnerOp::DeviceFeatures(crate::control_op::DeviceFeaturesPayload {
        features: vec![REVOKE_AUTHORITY_FEATURE.to_owned()],
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

fn revoke_cmd(e: &Engine, db: &mut Db, target: [u8; 16]) -> CommandResult {
    e.apply(
        db,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, target),
            reason: RevokeReason::Stolen,
        },
    )
    .unwrap()
}

fn kinds_emitted(db: &Db, kind: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT count(*) FROM ops WHERE inner_kind = ?",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
}

/// **A holder whose vault cannot take the op yet revokes ordinarily and
/// says why** (ADR-0058 §7): it is never refused, the result says it carries
/// no authority, and it names the device to update. The device being revoked
/// is not counted.
#[test]
fn the_holder_revokes_ordinarily_and_names_the_devices_to_update() {
    let e = engines(3);
    let (eh, ex, eold) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    trust(eh, &mut db, ex);
    trust(eh, &mut db, eold);
    let (x_id, old_id) = (ex.keychain.device_id(), eold.keychain.device_id());

    let r = revoke_cmd(eh, &mut db, x_id);
    assert!(!r.revoked_by_authority);
    assert_eq!(r.authority_awaits_update, vec![hex_bytes(&old_id)]);
    assert!(
        eh.is_revoked(db.conn(), &x_id).unwrap(),
        "revoked all the same"
    );
    assert_eq!(kinds_emitted(&db, "device.revoke_by_identity"), 0);
    assert!(!eh
        .vault_requires(db.conn(), REVOKE_AUTHORITY_FEATURE)
        .unwrap());
}

/// **Once every other device advertises the feature, the holder signs**,
/// requiring the feature first, and its rotation carries the authority
/// forward under the successor (ADR-0058 §5, §7): the revoked device stays
/// revoked by authority across the rotation, and every ordinary revocation
/// it writes stays gated. A second authority revocation carries both
/// forward again.
#[test]
fn the_holder_signs_and_the_rotation_carries_the_authority_forward() {
    let e = engines(4);
    let (eh, ex1, ex2, eo) = (&e[0], &e[1], &e[2], &e[3]);
    let mut db = db_root(ROOT);
    for other in [ex1, ex2, eo] {
        trust(eh, &mut db, other);
        advertise(eh, &mut db, other);
    }
    let (x1_id, x2_id, o_id) = (
        ex1.keychain.device_id(),
        ex2.keychain.device_id(),
        eo.keychain.device_id(),
    );
    let genesis = eh.current_identity(db.conn()).unwrap().identity_id;

    let r = revoke_cmd(eh, &mut db, x1_id);
    assert!(r.revoked_by_authority);
    assert!(r.authority_awaits_update.is_empty());
    assert!(eh
        .vault_requires(db.conn(), REVOKE_AUTHORITY_FEATURE)
        .unwrap());
    let head = eh.current_identity(db.conn()).unwrap().identity_id;
    assert_ne!(head, genesis, "the holder rotated the identity");
    assert!(
        by_authority(&db, &x1_id),
        "carried forward under the successor"
    );
    let carried: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM device_revoke_ops
             WHERE revoked_device_id = ? AND authority_identity_id = ?",
            params![&x1_id[..], &head[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(carried, 1, "one row signed under the successor");

    revoke(eh, &mut db, ex1, o_id, eh.hlc.peek().physical_ms + 1);
    assert_eq!(revocation_row(&db, &o_id), None, "X1 stays gated");

    revoke_cmd(eh, &mut db, x2_id);
    assert!(by_authority(&db, &x1_id) && by_authority(&db, &x2_id));
    assert_eq!(
        kinds_emitted(&db, "device.revoke_by_identity"),
        // X1 signed, carried; X2 signed, then both carried.
        5
    );
    // The ledger keeps one row naming an identity per target, not one per
    // identity: the rows under the retired identities are compacted away, so
    // a holder's carry-forward never approaches the per-sender cap.
    let holder_rows: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM device_revoke_ops
             WHERE sender = ? AND authority_identity_id IS NOT NULL",
            params![&eh.keychain.device_id()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(holder_rows, 2, "one per revoked device");
}

/// **A holder's authority revocations stay under the per-sender cap however
/// often it rotates.** Each rotation re-signs every authority revocation
/// under the successor; without the compaction folding a target's rows
/// across identities, the holder's pairs grew about `n(n+1)/2` and the cap
/// evicted its oldest revocations, ordinary ones included.
#[test]
fn the_carry_forward_keeps_one_pair_per_target() {
    let n = 24;
    let e = engines(n + 2);
    let (eh, rest) = e.split_first().unwrap();
    let mut db = db_root(ROOT);
    for other in rest {
        trust(eh, &mut db, other);
        advertise(eh, &mut db, other);
    }
    // One ordinary revocation from before the feature, then n by authority.
    let early = rest[0].keychain.device_id();
    revoke(eh, &mut db, eh, early, T0);
    for x in &rest[1..=usize::from(n)] {
        revoke_cmd(eh, &mut db, x.keychain.device_id());
    }
    let pairs: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM (
                 SELECT DISTINCT revoked_device_id, authority_identity_id IS NULL
                 FROM device_revoke_ops WHERE sender = ?
             )",
            params![&eh.keychain.device_id()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pairs, i64::from(n) + 1);
    assert!(
        eh.is_revoked(db.conn(), &early).unwrap(),
        "the early ordinary revocation is still in the register"
    );
    for x in &rest[1..=usize::from(n)] {
        assert!(by_authority(&db, &x.keychain.device_id()));
    }
}

/// A device admitted by pairing holds no `ID_S_priv`, so it never signs and
/// has nobody to name: the revocation is ordinary and says so.
#[test]
fn a_paired_device_revokes_ordinarily() {
    let ep = Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(T0))),
        seeded_rng(),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [9; 32],
        )),
    );
    let ex = engine_seeded(ROOT, [1; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    trust(&ep, &mut db, &ex);
    advertise(&ep, &mut db, &ex);
    let r = revoke_cmd(&ep, &mut db, ex.keychain.device_id());
    assert!(!r.revoked_by_authority);
    assert!(r.authority_awaits_update.is_empty());
    assert_eq!(kinds_emitted(&db, "device.revoke_by_identity"), 0);
}

/// The device list carries the register's answer.
#[test]
fn the_device_list_says_which_kind_revoked_a_device() {
    let e = engines(3);
    let (eh, ex, ey) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    trust(eh, &mut db, ex);
    trust(eh, &mut db, ey);
    let (x_id, y_id) = (ex.keychain.device_id(), ey.keychain.device_id());
    revoke_by_identity(eh, &mut db, eh, x_id, T0);
    revoke(eh, &mut db, eh, y_id, T0 + 1);
    let QueryResult::Devices(rows) = eh.query(&db, Query::DeviceList).unwrap() else {
        panic!("a device list");
    };
    let row = |id: [u8; 16]| rows.iter().find(|r| r.device_id == id).unwrap().clone();
    assert!(row(x_id).revoked && row(x_id).revoked_by_authority);
    assert!(row(y_id).revoked && !row(y_id).revoked_by_authority);
}

// ---------------------------------------------------------------------------
// The identity-signed withdrawal (ADR-0058 §6, issue #1239)
// ---------------------------------------------------------------------------

/// `sender` withdraws its authority revocation of `target` as the identity
/// `key` holds, delivered to `receiver`. `sig` replaces the signature when it
/// is `Some`.
fn withdraw_by_identity_with(
    receiver: &Engine,
    db: &mut Db,
    sender: &Engine,
    target: [u8; 16],
    hlc_ms: u64,
    key: &IdentitySigningKeyPair,
    sig: Option<[u8; 64]>,
) {
    let sender_id = sender.keychain.device_id();
    let hlc = Hlc::at(hlc_ms);
    let identity_id = identity_id_from_pub(&key.public_bytes());
    let claim = WithdrawalClaim {
        identity_id,
        sender_device_id: sender_id,
        revoked_device_id: target,
        hlc_physical_ms: hlc.physical_ms,
        hlc_logical: hlc.logical,
    };
    let identity_sig = sig.unwrap_or_else(|| claim.sign(key).unwrap());
    let inner = InnerOp::DeviceRevokeWithdrawByIdentity(DeviceRevokeWithdrawByIdentityPayload {
        revoked_device_id: target,
        identity_id,
        identity_sig,
    });
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, &inner, &sender_id, hlc, hlc.physical_ms, 1)
            .map(|_| ())
    })
    .unwrap();
}

/// [`withdraw_by_identity_with`] signed by the genesis identity.
fn withdraw_by_identity(
    receiver: &Engine,
    db: &mut Db,
    sender: &Engine,
    target: [u8; 16],
    hlc_ms: u64,
) {
    withdraw_by_identity_with(receiver, db, sender, target, hlc_ms, &genesis_key(), None);
}

/// `sender` withdraws its **ordinary** pair on `target`, unsigned.
fn withdraw_ordinarily(
    receiver: &Engine,
    db: &mut Db,
    sender: &Engine,
    target: [u8; 16],
    hlc_ms: u64,
) {
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

/// **The identity in force withdraws its own authority revocation**, and a
/// later authority revocation from the same sender makes the pair live
/// again (ADR-0058 §6, ADR-0056 §2).
#[test]
fn the_identity_withdraws_its_own_authority_revocation() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(er, &mut db, eo, x_id, T0);
    assert!(by_authority(&db, &x_id));
    withdraw_by_identity(er, &mut db, eo, x_id, T0 + 10_000);
    assert!(!er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(!by_authority(&db, &x_id));

    revoke_by_identity(er, &mut db, eo, x_id, T0 + 20_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(by_authority(&db, &x_id), "the later revocation is the head");
}

/// **A revocation wins a tie**, in either delivery order, as it does in an
/// ordinary pair.
#[test]
fn an_authority_revocation_wins_a_tie_with_its_withdrawal() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let x_id = ex.keychain.device_id();

    let mut one = db_root(ROOT);
    revoke_by_identity(er, &mut one, eo, x_id, T0);
    withdraw_by_identity(er, &mut one, eo, x_id, T0);
    let mut two = db_root(ROOT);
    withdraw_by_identity(er, &mut two, eo, x_id, T0);
    revoke_by_identity(er, &mut two, eo, x_id, T0);
    for db in [&one, &two] {
        assert!(er.is_revoked(db.conn(), &x_id).unwrap());
        assert!(by_authority(db, &x_id));
    }
}

/// **The register is the same whichever order the revocation and its
/// withdrawal arrive.**
#[test]
fn an_identity_withdrawal_converges_in_either_delivery_order() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let x_id = ex.keychain.device_id();

    let mut one = db_root(ROOT);
    revoke_by_identity(er, &mut one, eo, x_id, T0);
    withdraw_by_identity(er, &mut one, eo, x_id, T0 + 10_000);
    let mut two = db_root(ROOT);
    withdraw_by_identity(er, &mut two, eo, x_id, T0 + 10_000);
    revoke_by_identity(er, &mut two, eo, x_id, T0);
    assert_eq!(revocation_row(&one, &x_id), revocation_row(&two, &x_id));
    assert_eq!(revocation_row(&one, &x_id), None);
}

/// **A device-signed withdrawal cannot withdraw an authority revocation**:
/// it is a row of the sender's ordinary pair, which holds nothing here, so
/// the authority pair stays live.
#[test]
fn a_device_signed_withdrawal_leaves_an_authority_revocation_live() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(er, &mut db, eo, x_id, T0);
    withdraw_ordinarily(er, &mut db, eo, x_id, T0 + 10_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(by_authority(&db, &x_id));
}

/// **A withdrawal whose signature does not verify withdraws nothing an
/// authority row holds**, and the compaction keeps the revocation beside it
/// rather than letting the greater-stamped withdrawal displace it.
#[test]
fn an_unverifiable_identity_withdrawal_withdraws_nothing() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(er, &mut db, eo, x_id, T0);
    withdraw_by_identity_with(
        er,
        &mut db,
        eo,
        x_id,
        T0 + 10_000,
        &genesis_key(),
        Some([7; 64]),
    );
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(by_authority(&db, &x_id));
    assert_eq!(ledger_rows(&db), 2, "the revocation and the withdrawal");

    // Signed by another identity's key over a claim naming the head: the
    // signature does not verify under the head's `ID_S_pub`.
    let mut forged = db_root(ROOT);
    revoke_by_identity(er, &mut forged, eo, x_id, T0);
    let other = IdentitySigningKeyPair::from_secret_bytes(&[0x11; 32]);
    let claim = WithdrawalClaim {
        identity_id: identity_id_from_pub(&genesis_key().public_bytes()),
        sender_device_id: eo.keychain.device_id(),
        revoked_device_id: x_id,
        hlc_physical_ms: T0 + 10_000,
        hlc_logical: 0,
    };
    let sig = claim.sign(&other).unwrap();
    withdraw_by_identity_with(
        er,
        &mut forged,
        eo,
        x_id,
        T0 + 10_000,
        &genesis_key(),
        Some(sig),
    );
    assert!(by_authority(&forged, &x_id));
}

/// **An identity-signed withdrawal lifted into another device's envelope
/// withdraws nothing**: the claim names the sender, so it does not verify
/// for any other, and the pair it lands in is that other sender's, which
/// holds no revocation.
#[test]
fn an_identity_withdrawal_reaches_only_its_senders_pair() {
    let e = engines(4);
    let (ex, eo, ey, er) = (&e[0], &e[1], &e[2], &e[3]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(er, &mut db, eo, x_id, T0);
    // O's signature, carried by Y.
    let claim = WithdrawalClaim {
        identity_id: identity_id_from_pub(&genesis_key().public_bytes()),
        sender_device_id: eo.keychain.device_id(),
        revoked_device_id: x_id,
        hlc_physical_ms: T0 + 10_000,
        hlc_logical: 0,
    };
    let sig = claim.sign(&genesis_key()).unwrap();
    withdraw_by_identity_with(
        er,
        &mut db,
        ey,
        x_id,
        T0 + 10_000,
        &genesis_key(),
        Some(sig),
    );
    // And a validly signed withdrawal from Y, which has no pair on X.
    withdraw_by_identity(er, &mut db, ey, x_id, T0 + 20_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(by_authority(&db, &x_id));
}

/// **A replayed identity-signed withdrawal withdraws nothing it did not
/// withdraw the first time.** The holder revokes, withdraws, and revokes
/// again. Re-delivering the withdrawal byte for byte lands under the later
/// revocation and leaves the register as it was; the same signature carried
/// at a later envelope HLC, one that would outrank the re-revocation, does
/// not verify, because the claim it signs names the HLC it was made at.
#[test]
fn a_replayed_identity_withdrawal_does_not_undo_a_later_revocation() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(er, &mut db, eo, x_id, T0);
    withdraw_by_identity(er, &mut db, eo, x_id, T0 + 10_000);
    assert!(!er.is_revoked(db.conn(), &x_id).unwrap());
    revoke_by_identity(er, &mut db, eo, x_id, T0 + 20_000);
    let register = revocation_row(&db, &x_id);
    let rows = ledger_rows(&db);
    assert!(by_authority(&db, &x_id));

    // The identical op, delivered again.
    withdraw_by_identity(er, &mut db, eo, x_id, T0 + 10_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(by_authority(&db, &x_id));
    assert_eq!(revocation_row(&db, &x_id), register);
    assert_eq!(ledger_rows(&db), rows, "a re-delivery stores nothing new");

    // The first withdrawal's signature, lifted onto a later envelope.
    let claim = WithdrawalClaim {
        identity_id: identity_id_from_pub(&genesis_key().public_bytes()),
        sender_device_id: eo.keychain.device_id(),
        revoked_device_id: x_id,
        hlc_physical_ms: T0 + 10_000,
        hlc_logical: 0,
    };
    let sig = claim.sign(&genesis_key()).unwrap();
    withdraw_by_identity_with(
        er,
        &mut db,
        eo,
        x_id,
        T0 + 30_000,
        &genesis_key(),
        Some(sig),
    );
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        by_authority(&db, &x_id),
        "a signature over another HLC is no authority to withdraw"
    );
    assert_eq!(revocation_row(&db, &x_id), register);
}

/// **The identity cannot withdraw a revocation a device made** (ADR-0058
/// §6, ADR-0056 §Alternatives (b)): its withdrawal lands in the sender's
/// authority pair, and the ordinary pair keeps revoking.
#[test]
fn the_identity_cannot_withdraw_an_ordinary_revocation() {
    let e = engines(3);
    let (ex, eo, er) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    let x_id = ex.keychain.device_id();

    revoke(er, &mut db, eo, x_id, T0);
    withdraw_by_identity(er, &mut db, eo, x_id, T0 + 10_000);
    assert!(er.is_revoked(db.conn(), &x_id).unwrap());
    assert!(!by_authority(&db, &x_id));
}

/// **A withdrawal signed by a rotated-away identity cannot withdraw the
/// authority revocation carried forward under the successor.** The holder
/// signs a revocation, the rotation re-signs it under the successor, and a
/// later withdrawal from the same device signed by the retired key withdraws
/// nothing: it is not an authority row at the head, and the revocation it
/// outranks is one.
#[test]
fn a_withdrawal_signed_by_a_retired_identity_is_refused() {
    let e = engines(2);
    let (eh, ex) = (&e[0], &e[1]);
    let mut db = db_root(ROOT);
    trust(eh, &mut db, ex);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(eh, &mut db, eh, x_id, T0);
    eh.rotate_identity(&mut db, None, true).unwrap();
    assert_eq!(
        kinds_emitted(&db, "device.revoke_by_identity"),
        1,
        "the rotation carried the revocation forward"
    );
    assert!(by_authority(&db, &x_id));

    let later = eh.hlc.peek().physical_ms + 10_000;
    withdraw_by_identity(eh, &mut db, eh, x_id, later);
    assert!(eh.is_revoked(db.conn(), &x_id).unwrap());
    assert!(
        by_authority(&db, &x_id),
        "a retired identity has no authority to withdraw"
    );
}

/// **A rotation never undoes a withdrawal.** The holder withdraws its
/// authority revocation under the identity in force; the carry-forward has
/// nothing to re-sign, and after the rotation the revocation and its
/// withdrawal name one retired identity, so the pair stays inert rather than
/// folding back as an ordinary revocation from the holder.
#[test]
fn a_withdrawn_authority_pair_stays_inert_across_a_rotation() {
    let e = engines(2);
    let (eh, ex) = (&e[0], &e[1]);
    let mut db = db_root(ROOT);
    trust(eh, &mut db, ex);
    let x_id = ex.keychain.device_id();

    revoke_by_identity(eh, &mut db, eh, x_id, T0);
    withdraw_by_identity(eh, &mut db, eh, x_id, T0 + 10_000);
    assert!(!eh.is_revoked(db.conn(), &x_id).unwrap());
    let head = eh.current_identity(db.conn()).unwrap();
    assert!(
        Engine::authority_revocations_at(db.conn(), &head)
            .unwrap()
            .is_empty(),
        "nothing to carry forward"
    );

    eh.rotate_identity(&mut db, None, true).unwrap();
    assert_ne!(
        eh.current_identity(db.conn()).unwrap().identity_id,
        head.identity_id
    );
    assert_eq!(kinds_emitted(&db, "device.revoke_by_identity"), 0);
    assert!(
        !eh.is_revoked(db.conn(), &x_id).unwrap(),
        "the retired pair stays withdrawn"
    );
}

/// Advertise each of `features` from `who` to `receiver`.
fn advertise_features(receiver: &Engine, db: &mut Db, who: &Engine, features: &[&str]) {
    let inner = InnerOp::DeviceFeatures(crate::control_op::DeviceFeaturesPayload {
        features: features.iter().map(|f| (*f).to_owned()).collect(),
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

/// **The holder signs the withdrawal of its own authority revocation**
/// (ADR-0058 §6): the device list offers it, the command requires the
/// feature first and emits the identity-signed op and no ordinary one, and
/// the device is no longer revoked. The revocation rotated the identity with
/// the device left out, so it must be paired again.
#[test]
fn the_holder_withdraws_its_authority_revocation() {
    let all = [
        REVOKE_AUTHORITY_FEATURE,
        REVOKE_WITHDRAW_FEATURE,
        REVOKE_WITHDRAW_AUTHORITY_FEATURE,
    ];
    let e = engines(3);
    let (eh, ex, eo) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    for other in [ex, eo] {
        trust(eh, &mut db, other);
        advertise_features(eh, &mut db, other, &all);
    }
    let x_id = ex.keychain.device_id();
    assert!(revoke_cmd(eh, &mut db, x_id).revoked_by_authority);
    assert!(device_list_row(eh, &db, &x_id).withdrawable);

    let w = withdraw_cmd(eh, &mut db, x_id)
        .unwrap()
        .withdrawal
        .expect("a withdrawal reports");
    assert_eq!(w.outcome, WithdrawalOutcome::RepairRequired);
    assert!(eh
        .vault_requires(db.conn(), REVOKE_WITHDRAW_AUTHORITY_FEATURE)
        .unwrap());
    assert_eq!(kinds_emitted(&db, "device.revoke_withdraw_by_identity"), 1);
    assert_eq!(kinds_emitted(&db, "device.revoke_withdraw"), 0);
    assert!(!eh.is_revoked(db.conn(), &x_id).unwrap());
    assert!(!by_authority(&db, &x_id));
    assert!(!device_list_row(eh, &db, &x_id).withdrawable);
}

/// **An authority claim waits for its feature.** Where it is the holder's
/// only claim the command is refused, naming the devices to update; where
/// the holder also holds an ordinary claim it withdraws that one alone and
/// says the device is still claimed.
#[test]
fn the_identity_withdrawal_waits_for_its_feature() {
    let e = engines(3);
    let (eh, ex, eo) = (&e[0], &e[1], &e[2]);
    let mut db = db_root(ROOT);
    for other in [ex, eo] {
        trust(eh, &mut db, other);
        advertise_features(
            eh,
            &mut db,
            other,
            &[REVOKE_AUTHORITY_FEATURE, REVOKE_WITHDRAW_FEATURE],
        );
    }
    let x_id = ex.keychain.device_id();
    revoke_by_identity(eh, &mut db, eh, x_id, T0);

    let Err(EngineError::FeatureUnsupportedByDevices { feature, devices }) =
        withdraw_cmd(eh, &mut db, x_id)
    else {
        panic!("refused for want of the feature");
    };
    assert_eq!(feature, REVOKE_WITHDRAW_AUTHORITY_FEATURE);
    assert!(devices.contains(&x_id) && devices.contains(&eo.keychain.device_id()));
    assert_eq!(kinds_emitted(&db, "device.revoke_withdraw_by_identity"), 0);

    revoke(eh, &mut db, eh, x_id, T0 + 1_000);
    let w = withdraw_cmd(eh, &mut db, x_id).unwrap().withdrawal.unwrap();
    assert_eq!(w.outcome, WithdrawalOutcome::StillClaimed);
    assert_eq!(kinds_emitted(&db, "device.revoke_withdraw"), 1);
    assert_eq!(kinds_emitted(&db, "device.revoke_withdraw_by_identity"), 0);
    assert!(by_authority(&db, &x_id));
}

/// **A device that does not hold the identity in force cannot withdraw an
/// authority revocation it carried**: a paired device holds no `ID_S_priv`,
/// so the command reports it is not the author, emits nothing, and the row
/// does not offer it.
#[test]
fn a_device_without_the_identity_cannot_withdraw_an_authority_revocation() {
    let ep = Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(T0))),
        seeded_rng(),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [9; 32],
        )),
    );
    let ex = engine_seeded(ROOT, [1; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    trust(&ep, &mut db, &ex);
    let x_id = ex.keychain.device_id();
    // A row naming the head, carried by this device (as a restored or
    // stolen key would sign it).
    revoke_by_identity(&ep, &mut db, &ep, x_id, T0);
    assert!(!device_list_row(&ep, &db, &x_id).withdrawable);
    let w = withdraw_cmd(&ep, &mut db, x_id)
        .unwrap()
        .withdrawal
        .unwrap();
    assert_eq!(w.outcome, WithdrawalOutcome::NotAuthor);
    assert_eq!(kinds_emitted(&db, "device.revoke_withdraw_by_identity"), 0);
    assert!(by_authority(&db, &x_id));
}
