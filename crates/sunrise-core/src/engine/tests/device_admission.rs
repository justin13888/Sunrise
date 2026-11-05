//! Device certificates: who may publish one, backfill and the readmission mark.

use super::identity_rotation::apply_control_at;
use super::*;

/// A member cannot republish a *sibling's* cert under its own signature.
///
/// `ON CONFLICT(device_id) DO UPDATE` overwrites `cert_blob` — and with it
/// the `d_s_pub` every one of that sibling's envelopes is verified against —
/// and `d_d_pub`, which is where its `key_envelope`s are sealed. Publishing
/// a cert for someone else is therefore a converging denial of service plus
/// a redirect of their key material, from one well-formed op.
#[test]
fn a_device_cannot_publish_a_cert_naming_another_device() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&eb, &mut dbb, &ec);

    let stored = |db: &Db, id: &[u8; 16]| -> Vec<u8> {
        db.conn()
            .query_row(
                "SELECT cert_blob FROM devices WHERE device_id = ?",
                params![&id[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    let c_id = ec.keychain.device_id();
    let before = stored(&dbb, &c_id);

    // A issues a cert naming C, binding C's device id to **A's** DH key,
    // and publishes it. Pairing hands every device `ID_S_priv`, so this
    // cert verifies perfectly: the check that has to catch it is the one
    // about *who sent it*, not the one about whether it is well formed.
    let a_id = ea.keychain.device_id();
    let cert = ea
        .keychain
        .issue_cert_for(
            c_id,
            ec.keychain.device_signing_pub(),
            ea.keychain.device_dh_pub(),
            "impostor",
            "macos",
            T0,
        )
        .expect("issue the impostor cert");
    dbb.with_tx(|tx| {
        eb.apply_control_op(
            tx,
            &InnerOp::DeviceCertPublish(cert),
            &a_id,
            Hlc::at(T0),
            T0,
            1,
        )
        .map(|_| ())
    })
    .unwrap();
    assert_eq!(
        stored(&dbb, &c_id),
        before,
        "A must not be able to rewrite C's row"
    );
    let d_d: Vec<u8> = dbb
        .conn()
        .query_row(
            "SELECT d_d_pub FROM devices WHERE device_id = ?",
            params![&c_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        d_d,
        ec.keychain.device_dh_pub().to_vec(),
        "and C's future key envelopes still go to C"
    );

    // C publishing its own cert is of course fine, and is how the row is
    // written in the first place.
    let own = ec.keychain.cert_blob();
    dbb.with_tx(|tx| {
        eb.apply_control_op(
            tx,
            &InnerOp::DeviceCertPublish(own.clone()),
            &c_id,
            Hlc::at(T0),
            T0,
            1,
        )
        .map(|_| ())
    })
    .unwrap();
    assert_eq!(stored(&dbb, &c_id), own);
}

/// A device certified *after* an epoch was minted still receives that
/// epoch's key.
///
/// This is the gap dropping `ID_D_priv` from `PairingPayload` opens, and it
/// is not a corner: minting happens when a Stream is created, so a Stream
/// made on one device while another device's cert was still in flight would
/// have been unreadable on that device permanently. The identity copy used
/// to absorb this silently.
#[test]
fn a_device_certified_after_an_epoch_was_minted_is_backfilled() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    // A mints the vault-meta and Inbox epochs while it has never heard of
    // B, so B is on no recipient list.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before B was known".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "B cannot have been sealed to before A had its cert"
    );
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(
        held.len() >= 2,
        "the capture minted at least the meta and Inbox epochs"
    );

    // B's cert arrives.
    trust_at(&ea, &mut dba, &eb, T0);

    let mut sealed = envelopes_to(&ea, &dba, &b_id);
    sealed.sort_unstable();
    let mut want = held;
    want.sort_unstable();
    assert_eq!(
        sealed, want,
        "every epoch A holds must be sealed to B once its cert lands"
    );

    // And it is not done twice: a second publication of the same cert
    // emits nothing, because `key_envelope_recipients` already has the row.
    let before = envelopes_to(&ea, &dba, &b_id).len();
    trust_at(&ea, &mut dba, &eb, T0 + 1);
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        before,
        "a re-published cert must not re-send every key"
    );
}

/// A device certified after a **rotation** receives the epoch it missed,
/// not only the live one.
///
/// The gap this closes is [#107](https://github.com/justin13888/Sunrise/issues/107)
/// and it is silent: ops written under the superseded epoch park in
/// `deferred_ops` on the new device and expire at `DEFERRED_TTL_MS`, so it
/// presents to the user as "some items from around when I set up this
/// device never arrived" and to every log as nothing at all.
///
/// The rotation here is `Command::RotateStreamKey`, but the ordinary cause
/// is creating a Stream or revoking a device — both mint — so the race is
/// between a new device's certificate and any of the account's routine
/// key-minting activity, which is not a corner.
#[test]
fn a_device_certified_after_a_rotation_receives_the_epoch_it_missed() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    // Epoch 1 of the Inbox exists and carries a task, all while B is
    // unknown to A.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written under epoch 1".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    // The rotation that lands between B's pairing and B's certificate.
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    let live = dba
        .with_tx(|tx| ea.keychain.current_epoch_tx(tx, &INBOX_STREAM_BYTES))
        .unwrap()
        .expect("the Inbox has a key");
    assert!(
        live >= 2,
        "the rotation must have superseded epoch 1, or this test proves nothing"
    );

    trust_at(&ea, &mut dba, &eb, T0);

    let sealed = envelopes_to(&ea, &dba, &b_id);
    assert!(
        sealed.contains(&(INBOX_STREAM_BYTES, live)),
        "the live epoch is sealed to a newly certified device"
    );
    assert!(
        sealed.contains(&(INBOX_STREAM_BYTES, 1)),
        "and so is the superseded epoch, or every op written under it is \
             undecryptable on B and expires out of `deferred_ops` unremarked"
    );

    // Every epoch, not merely the two this assertion names, and not twice.
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    let mut want = held;
    want.sort_unstable();
    let mut got = sealed;
    got.sort_unstable();
    assert_eq!(
        got, want,
        "the backfill emits exactly the epochs this replica holds"
    );

    let before = envelopes_to(&ea, &dba, &b_id).len();
    trust_at(&ea, &mut dba, &eb, T0 + 1);
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        before,
        "`key_envelope_recipients` still makes a re-published cert emit nothing"
    );
}

/// A revoked device that republishes its cert is **not** backfilled.
///
/// Without this the read half would be trivially undone: revocation
/// rotates every key away, and the device then asks for all of them back
/// by re-sending the cert it already had.
#[test]
fn a_revoked_device_is_not_backfilled_by_republishing_its_cert() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    // B is revoked at a cut at or before the moment its cert is applied.
    revoke(&ea, &mut dba, &ea, b_id, T0);
    assert!(ea.is_revoked(dba.conn(), &b_id).unwrap());

    trust_at(&ea, &mut dba, &eb, T0);
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a revoked device must not recover its keys by re-sending its cert"
    );
}

/// **A revoked device cannot rejoin under a fresh device id.** The test
/// that had to flip, flipped.
///
/// [#105](https://github.com/justin13888/Sunrise/issues/105). The setup is
/// unchanged from the version that asserted the bypass, deliberately: same
/// engines, same vault root, same fresh id minted from the `ID_S_priv` the
/// revoked device kept, same publish. Every step the issue describes still
/// happens and still succeeds. The cert still verifies — it is not a
/// forgery and nothing here pretends it is.
///
/// What changed is underneath it. `RevokeDevice` now rotates the account
/// identity as well as the Stream keys, and the revoked device is not in
/// the new roster, so it holds no share of the successor's `ID_S_priv`. The
/// fresh cert it signs is therefore issued under an identity the account
/// has retired: `DeviceCertPublish` records *which* chain identity verified
/// it, that is not the head, and every membership test reads the row as not
/// current. The row lands — applying is unconditional, so every replica
/// agrees it landed — and it is a recipient of nothing.
///
/// ADR-0032 records why the fix is identity rotation and not any of the
/// narrower checks that were considered here.
#[test]
fn a_revoked_device_cannot_rejoin_under_a_fresh_device_id() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // The same account identity, a device id nobody has revoked: what the
    // revoked device produces for itself out of the `ID_S_priv` it kept.
    let ec2 = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let c_id = ec.keychain.device_id();
    let c2_id = ec2.keychain.device_id();

    trust(&ea, &mut dba, &ec);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();
    let after_the_cut = envelopes_to(&ea, &dba, &c_id).len();

    // The fresh cert verifies under this account's identity: nothing in
    // `DeviceCertPublish` has anything to check a revocation against,
    // because a `DeviceCert` names no issuer.
    trust_at(&ea, &mut dba, &ec2, T0 + 1);

    assert!(
        ea.is_revoked(dba.conn(), &c_id).unwrap(),
        "the register still names the old id"
    );
    assert!(
        !ea.is_revoked(dba.conn(), &c2_id).unwrap(),
        "and has nothing to say about the new one"
    );
    assert_eq!(
        envelopes_to(&ea, &dba, &c_id).len(),
        after_the_cut,
        "the read bound holds for the id it names, which is the whole of what it does"
    );

    // The fresh id is admitted. That is not a concession — it is the
    // property that keeps the fix convergent: facts about devices apply
    // unconditionally on every replica, and only standing is derived.
    let stored: Option<Vec<u8>> = dba
        .conn()
        .query_row(
            "SELECT identity_id FROM devices WHERE device_id = ?",
            params![&c2_id[..]],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    assert!(stored.is_some(), "the cert verifies, so the row lands");
    assert_ne!(
        stored.as_deref().and_then(to16),
        Some(head),
        "but under the identity the rotation retired, not the one in force"
    );

    // And the round trip stops. Step 4 of the issue -- "backfill_key_envelopes
    // then seals C' the current epoch of every stream" -- emits nothing.
    let want = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(!want.is_empty(), "the revocation rotated something");
    assert!(
        envelopes_to(&ea, &dba, &c2_id).is_empty(),
        "the keys the revocation rotated away are not handed back to the same \
             device under a new name"
    );

    // The device list says so, in the field a UI renders.
    let QueryResult::Devices(rows) = ea.query(&dba, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    let c2 = rows
        .iter()
        .find(|r| r.device_id == c2_id)
        .expect("the fresh id is listed");
    assert!(!c2.revoked, "the register has never heard of this id");
    assert!(
        !c2.current,
        "and `current` is the field that shows it anyway"
    );
}

/// A device id that turns up **after** this vault has recorded a revocation is
/// marked as such in the device list, not only in a log line.
///
/// `core.device.admitted_after_revocation` has existed since ADR-0032 and
/// reaches an operator reading NDJSON and nobody else, which is the whole of
/// issue #144's second half. The condition cannot be recomputed afterwards —
/// `0026_device_admitted_after_revocation.sql` walks the three columns that
/// look like they could and cannot — so it is written down where it is known.
#[test]
fn a_device_admitted_after_a_revocation_says_so_in_the_device_list() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    let c_id = ec.keychain.device_id();

    // B is here before anything has been revoked.
    trust(&ea, &mut dba, &eb);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .expect("revoke");
    // C turns up afterwards.
    trust_at(&ea, &mut dba, &ec, T0 + 1);

    assert!(
        device_list_row(&ea, &dba, &c_id).admitted_after_revocation,
        "the device list must carry the signal the log line has always had"
    );
    assert!(
        !device_list_row(&ea, &dba, &b_id).admitted_after_revocation,
        "a device that was already here did not join after anything"
    );
}

/// And an account that has revoked nothing marks nobody, so the field is a
/// signal rather than a badge every device wears.
#[test]
fn a_device_admitted_with_no_revocation_on_record_is_not_marked() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    assert!(
        !device_list_row(&ea, &dba, &eb.keychain.device_id()).admitted_after_revocation,
        "nothing has been revoked, so nobody joined after a revocation"
    );
}

/// The mark is not something the marked device can rub out.
///
/// `DeviceCertPublish` upserts, and a device that re-publishes its certificate
/// takes the `ON CONFLICT` arm — where `readmission` is necessarily false,
/// because the row it is conflicting with is what makes `known` true. Carrying
/// `excluded.admitted_after_revocation` across would therefore hand the one
/// party the signal is about a one-op way to clear it.
#[test]
fn a_republished_certificate_does_not_clear_the_readmission_mark() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let c_id = ec.keychain.device_id();

    trust(&ea, &mut dba, &eb);
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
            reason: RevokeReason::Stolen,
        },
    )
    .expect("revoke");
    trust_at(&ea, &mut dba, &ec, T0 + 1);
    assert!(device_list_row(&ea, &dba, &c_id).admitted_after_revocation);

    trust_at(&ea, &mut dba, &ec, T0 + 2);
    assert!(
        device_list_row(&ea, &dba, &c_id).admitted_after_revocation,
        "publishing the certificate again must not clear the mark"
    );
}

/// **The combination no other column in the device list can show**, and the
/// reason this one is worth a migration.
///
/// `revoked` names an id in the register and `current` names the identity that
/// verified a cert, so between them they already cover the readmission ADR-0032
/// describes *when the revocation rotated the identity*: the fresh cert is
/// under a retired link and `current` is false. They cover nothing when it did
/// not. A replica that has applied a `device_revoke` and not (yet, or ever) the
/// transition beside it — the ordinary shape of a remote revocation, and the
/// permanent shape of one run from a device that holds no `ID_S_priv` and logs
/// `core.identity.rotation_unavailable` — sees the fresh device id certified
/// under the identity in force. It reads `revoked: false, current: true`: the
/// same three columns as any honest member.
///
/// So this asserts the negative half first. Without the fourth column the row
/// is indistinguishable, and the user is told nothing.
#[test]
fn a_readmission_under_the_live_identity_is_visible_in_no_other_column() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let c_id = ec.keychain.device_id();

    trust(&ea, &mut dba, &eb);
    // A revocation that arrived as an op, with no transition beside it.
    revoke(&ea, &mut dba, &ea, eb.keychain.device_id(), T0);
    trust_at(&ea, &mut dba, &ec, T0 + 1);

    let row = device_list_row(&ea, &dba, &c_id);
    assert!(
        !row.revoked,
        "the register names ids and has never heard of this one"
    );
    assert!(
        row.current,
        "and the identity that certified it is the one in force, so every \
         other column reads exactly like an honest member's"
    );
    assert!(
        row.admitted_after_revocation,
        "which leaves this as the only field that can tell the user anything"
    );
}

/// **#105 closed rather than bounded.** The device that gets revoked cannot
/// certify itself back in because it never held the key to do it with —
/// not because a rotation retired the identity it would have used.
///
/// The test above is the *remedy*: the revoked device mints a cert, the
/// cert verifies, and rotation makes it worthless. It had to be, while
/// every paired device held `ID_S_priv`. It also only bought one
/// revocation at a time — the device paired *after* a rotation held the new
/// key, so revoking that one replayed the whole trick one identity along.
///
/// This is the fix. A device admitted by pairing is handed `ID_S_pub`, so
/// the first step of the attack — sign a cert for a fresh device id — does
/// not complete, before or after any revocation. Nothing about the
/// revocation is what stops it, which is exactly why it keeps working on
/// the second device and the third.
#[test]
fn a_paired_device_cannot_certify_a_fresh_device_id_before_or_after_its_revocation() {
    use crate::KeychainError;
    let founder = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // The device under test: same account, admitted by pairing.
    let paired = Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(T0))),
        seeded_rng(),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [2u8; 32],
        )),
    );
    let mut dba = db_root(ROOT);
    let paired_id = paired.keychain.device_id();
    trust(&founder, &mut dba, &paired);

    // The fresh id the attack needs, with real keys behind it.
    let fresh = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let attempt = |e: &Engine| {
        e.keychain.issue_cert_for(
            fresh.keychain.device_id(),
            fresh.keychain.device_signing_pub(),
            fresh.keychain.device_dh_pub(),
            "a fresh id",
            "test",
            T0,
        )
    };

    // Before: the founder can, the paired device cannot. Both are members
    // of the same account and both hold valid certs of their own.
    assert!(attempt(&founder).is_ok(), "the control: the creator can");
    assert!(matches!(
        attempt(&paired),
        Err(KeychainError::IdentitySigningKeyAbsent)
    ));

    // The revocation, from the founder, which rotates the identity.
    let head_before = founder.current_identity(dba.conn()).unwrap().identity_id;
    founder
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, paired_id),
                reason: RevokeReason::Lost,
            },
        )
        .unwrap();
    assert_ne!(
        founder.current_identity(dba.conn()).unwrap().identity_id,
        head_before,
        "the creator holds ID_S_priv, so the revocation still rotates"
    );

    // After: unchanged, and that is the point. The revoked device's
    // inability to certify anything is a property of the device, not a
    // consequence of the rotation — so it survives the *next* revocation
    // too, which is what the remedy could not do.
    assert!(matches!(
        attempt(&paired),
        Err(KeychainError::IdentitySigningKeyAbsent)
    ));
}

/// A revocation run from a device that cannot rotate still cuts the reads.
///
/// Only the account's creator holds `ID_S_priv` now, so a paired device can
/// revoke and cannot rotate the identity. Refusing the command outright
/// would be worse than not rotating — the device a user is revoking is
/// often the one they lost, and the creator may be the one they lost — so
/// the revocation completes and says what it did not do.
///
/// What it gives up is bounded: a revoked device that was itself paired
/// holds no signing key either way, so there is nothing for the rotation to
/// have retired. The case that genuinely needs it is revoking the creator.
#[test]
fn a_revocation_from_a_paired_device_cuts_the_keys_without_rotating_the_identity() {
    let paired = Engine::from_clock(
        Arc::new(FakeClock(PLMutex::new(T0))),
        seeded_rng(),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [1u8; 32],
        )),
    );
    let victim = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let victim_id = victim.keychain.device_id();
    trust(&paired, &mut db, &victim);

    paired
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "before the cut".into(),
                ..Default::default()
            }),
        )
        .unwrap();
    let head_before = paired.current_identity(db.conn()).unwrap().identity_id;
    let sealed_before = envelopes_to(&paired, &db, &victim_id).len();

    paired
        .apply(
            &mut db,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, victim_id),
                reason: RevokeReason::Stolen,
            },
        )
        .expect("a device with no signing key can still revoke");

    assert!(
        paired.is_revoked(db.conn(), &victim_id).unwrap(),
        "the cut is recorded"
    );
    assert_eq!(
        envelopes_to(&paired, &db, &victim_id).len(),
        sealed_before,
        "and the rotation sealed the new epochs to everyone except it"
    );
    assert_eq!(
        paired.current_identity(db.conn()).unwrap().identity_id,
        head_before,
        "the identity did not move, because this device cannot sign a transition"
    );

    // ...and asking for the rotation on its own says so, rather than
    // failing with something storage-shaped.
    let err = paired
        .apply(
            &mut db,
            Command::RotateIdentity {
                keep_recovery_code: true,
            },
        )
        .expect_err("a paired device cannot rotate the account identity");
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains("admitted by pairing")),
        "expected a named refusal, got {err:?}"
    );
}

/// Read `devices.(cert_blob, d_d_pub, identity_id)` for one device.
fn device_key_row(db: &Db, id: &[u8; 16]) -> (Vec<u8>, Option<Vec<u8>>, Vec<u8>) {
    db.conn()
        .query_row(
            "SELECT cert_blob, d_d_pub, identity_id FROM devices WHERE device_id = ?",
            params![&id[..]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// **#281.** A device's own republish cannot rebind its `d_d_pub`.
///
/// `a_device_cannot_publish_a_cert_naming_another_device` stops a *sibling*
/// redirecting C's envelopes; it does not stop C's own id doing it, because
/// `device_id` is derived from `D_S_pub` alone. A holder of `ID_S_priv` and
/// C's `D_S_priv` can mint a cert for C's id under a fresh `D_D_pub`, and
/// before this the upsert took it and the backfill on the next line sealed
/// every held epoch to the new key.
#[test]
fn a_republished_cert_cannot_rebind_the_device_key() {
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ec);
    let c_id = ec.keychain.device_id();
    let before = device_key_row(&dbb, &c_id);
    let ops_before = op_count(&dbb);

    let rebound = ec
        .keychain
        .issue_cert_for(
            c_id,
            ec.keychain.device_signing_pub(),
            [0x42; 32],
            "device",
            "macos",
            T0,
        )
        .expect("issue the rebinding cert");
    apply_control_at(
        &eb,
        &mut dbb,
        &InnerOp::DeviceCertPublish(rebound),
        &c_id,
        Hlc::at(T0),
        1,
    );

    assert_eq!(
        device_key_row(&dbb, &c_id),
        before,
        "the whole cert is refused, so cert_blob and d_d_pub still agree"
    );
    assert_eq!(
        before.1.as_deref(),
        Some(&ec.keychain.device_dh_pub()[..]),
        "and C's envelopes are still sealed to C's own key"
    );
    assert_eq!(op_count(&dbb), ops_before, "and nothing was backfilled");

    // The honest republish, same key, still lands.
    trust_at(&eb, &mut dbb, &ec, T0 + 1);
    assert_eq!(device_key_row(&dbb, &c_id).1, before.1);
}

/// A NULL `d_d_pub` is filled rather than refused: that is the legacy-adoption
/// row, and filling it is what `DeviceCertPublish` is for.
#[test]
fn a_cert_fills_a_null_device_key() {
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ec);
    let c_id = ec.keychain.device_id();
    dbb.conn()
        .execute(
            "UPDATE devices SET d_d_pub = NULL WHERE device_id = ?",
            params![&c_id[..]],
        )
        .unwrap();

    trust_at(&eb, &mut dbb, &ec, T0 + 1);
    assert_eq!(
        device_key_row(&dbb, &c_id).1.as_deref(),
        Some(&ec.keychain.device_dh_pub()[..])
    );
}

/// The roster is the other upsert of `d_d_pub`, and it holds the same rule: an
/// entry carrying a different key for a device this vault holds is skipped,
/// which leaves the device on its old row exactly as an omission would.
#[test]
fn a_roster_entry_cannot_rebind_the_device_key() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();
    let before = device_key_row(&dba, &b_id);

    let successor = ea.keychain.mint_successor_identity(&SystemRng);
    let entry = |d_d_pub: [u8; 32]| RosterEntry {
        cert: Keychain::issue_roster_cert(
            &successor,
            b_id,
            eb.keychain.device_signing_pub(),
            d_d_pub,
            "device",
            "test",
            T0,
        )
        .expect("roster cert"),
    };
    let to = successor.identity_id();

    dba.with_tx(|tx| ea.apply_roster(tx, &[entry([0x42; 32])], &to))
        .unwrap();
    assert_eq!(
        device_key_row(&dba, &b_id),
        before,
        "a rebinding entry moves nothing"
    );

    dba.with_tx(|tx| ea.apply_roster(tx, &[entry(eb.keychain.device_dh_pub())], &to))
        .unwrap();
    let after = device_key_row(&dba, &b_id);
    assert_eq!(after.1, before.1);
    assert_eq!(
        after.2,
        to.to_vec(),
        "an honest entry still moves the device"
    );
}
