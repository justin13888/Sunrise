//! The read bound beside the register, and a gated revocation doing nothing.

use super::*;

// ---------------------------------------------------------------------------
// The read bound is not the register (migration 0028, ADR-0041 §"What a user
// sees" item 5).
// ---------------------------------------------------------------------------

/// **An unwound device is not a key recipient again.**
///
/// The chain from
/// `a_revocation_written_before_the_senders_own_cut_is_unwound_when_the_sender_is_revoked`,
/// asked the question that test does not. That one asserts C leaves the
/// *register*, and the register was also what `emit_key_envelopes`
/// anti-joined — so the unwind released the read bound and C re-entered the
/// recipient set of every epoch the vault minted from then on. No adversary is
/// involved: the sequence is retiring an old laptop from the desktop and,
/// months later, the desktop from the phone, which is what migration 0027's
/// own seed traces.
///
/// The fix is that the four key-distribution sites read `device_read_bounds`,
/// which the fold only ever adds to. Point the anti-join back at
/// `device_revocations` and this goes red.
#[test]
fn an_unwound_revocation_does_not_make_the_device_a_key_recipient_again() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, b_id, c_id) = (
        ea.keychain.device_id(),
        eb.keychain.device_id(),
        ec.keychain.device_id(),
    );
    // Everyone is in the device list, so everyone is a candidate recipient.
    trust(&ea, &mut db, &eb);
    trust(&ea, &mut db, &ec);

    // A retires C.
    revoke(&ea, &mut db, &ea, c_id, T0);
    assert!(revocation_row(&db, &c_id).is_some());
    assert!(
        read_bound_row(&db, &c_id).is_some(),
        "the bound is taken by the same fold that writes the register"
    );

    // Later, B retires A, which unwinds A's revocation of C.
    revoke(&ea, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "the register is a fold, so C is called current again"
    );
    assert!(
        read_bound_row(&db, &c_id).is_some(),
        "but the read bound is a ratchet and C is still bounded"
    );

    // The consequence the register alone could not hold: mint a fresh epoch
    // now and see who it is sealed to.
    let before = envelopes_to(&ea, &db, &c_id).len();
    let stream = [0x3c; 16];
    db.with_tx(|tx| {
        let (epoch, key) = ea
            .keychain
            .mint_epoch(tx, &stream, ea.rng.as_ref(), T0 + 120_000)?;
        ea.emit_key_envelopes(tx, &stream, epoch, &key, T0 + 120_000, None)
    })
    .unwrap();

    assert_eq!(
        envelopes_to(&ea, &db, &c_id).len(),
        before,
        "an unwound device must not be sealed an epoch minted after its bound"
    );
    assert!(
        !envelopes_to(&ea, &db, &b_id).is_empty(),
        "and an unrevoked sibling must still be sealed one, or this asserts nothing"
    );
}

/// The republish route is closed on the bound too.
///
/// `backfill_key_envelopes` is the larger of the two failures, because it does
/// not hand back the epochs minted from now on — it hands back **every epoch
/// this vault holds**. Against the register, one `DeviceCertPublish` from an
/// unwound device pulled all of them.
#[test]
fn an_unwound_device_republishing_its_cert_is_handed_nothing() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    trust(&ea, &mut db, &eb);
    trust(&ea, &mut db, &ec);

    revoke(&ea, &mut db, &ea, c_id, T0);
    revoke(&ea, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(revocation_row(&db, &c_id), None, "C reads current again");

    // An epoch minted *after* the bound, on a stream C has never been served.
    // `key_envelope_recipients` therefore holds no row saying C has it, which
    // is the whole of what makes a backfill emit anything at all — without
    // this the call is a no-op for reasons that have nothing to do with
    // revocation, and the test would pass against either table.
    let stream = [0x7b; 16];
    db.with_tx(|tx| {
        ea.keychain
            .mint_epoch(tx, &stream, ea.rng.as_ref(), T0 + 90_000)
            .map(|_| ())
    })
    .unwrap();
    let served_before: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM key_envelope_recipients
             WHERE stream_id = ? AND recipient = ?",
            params![&stream[..], &c_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(served_before, 0, "nothing has served C this stream yet");

    let before = envelopes_to(&ea, &db, &c_id).len();
    let c_pub = ec.keychain.device_dh_pub();
    db.with_tx(|tx| ea.backfill_key_envelopes(tx, &c_id, &c_pub, T0 + 120_000))
        .unwrap();

    assert_eq!(
        envelopes_to(&ea, &db, &c_id).len(),
        before,
        "the backfill must read the bound, not the register: against the register a \
         republished cert hands an unwound device every held epoch of every stream"
    );
}

/// The starve attack, which is the shape with no honest cause.
///
/// A revoked device X emits one ordinary `device_revoke` naming each current
/// device. Every one is correctly gated, so the register records nothing — and
/// the bound must record nothing either, or X takes the whole account's key
/// access away with N ops and no crafted input at all.
///
/// The bound is written from `register` and not from the ledger, which is what
/// makes this hold: a gated row never reaches `register`, so it never reaches
/// `device_read_bounds`. Seed the bound from `device_revoke_ops` instead and
/// this goes red.
#[test]
fn a_revoked_device_emitting_gated_revocations_bounds_nobody() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let et = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, d_id, t_id) = (
        ex.keychain.device_id(),
        ed.keychain.device_id(),
        et.keychain.device_id(),
    );

    // O expels X. X is now gated for every third party.
    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(read_bound_row(&db, &x_id).is_some(), "X is bounded");

    // X names every remaining device, one ordinary op each.
    revoke(&er, &mut db, &ex, d_id, T0 + 1_000);
    revoke(&er, &mut db, &ex, t_id, T0 + 2_000);

    for (name, id) in [("D", &d_id), ("T", &t_id)] {
        assert_eq!(
            revocation_row(&db, id),
            None,
            "{name}'s revocation by a revoked device must be gated"
        );
        assert_eq!(
            read_bound_row(&db, id),
            None,
            "and a gated revocation must not bound {name} either -- otherwise a revoked \
             device takes the account's key access with one op per device"
        );
    }
}

/// The discount rehabilitates the *gate* and never the bound.
///
/// ADR-0041 §Decision 1's discount pass takes a revoker back out of a device's
/// revoker set once somebody else has expelled that revoker, so X stops being
/// gated and revokes third parties again. What it must not do is give X its
/// keys back: the bound was taken when the account believed X was out, and
/// nothing in the tree releases one. This pins the asymmetry the device list's
/// `read_bounded` column exists to show.
#[test]
fn the_discount_rehabilitates_the_gate_and_never_the_read_bound() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (x_id, o_id, d_id) = (
        ex.keychain.device_id(),
        eo.keychain.device_id(),
        ed.keychain.device_id(),
    );

    // O revokes X; a third party P then revokes O.
    revoke(&er, &mut db, &eo, x_id, T0);
    assert!(read_bound_row(&db, &x_id).is_some());
    revoke(&er, &mut db, &ep, o_id, T0 + 10_000);

    assert_eq!(
        revocation_row(&db, &x_id),
        None,
        "O's revocation of X is unwound, because O was itself revoked"
    );
    assert!(
        read_bound_row(&db, &x_id).is_some(),
        "and X keeps its read bound, which is the whole of migration 0028"
    );

    // X is no longer gated -- the discount took O out of X's revoker set --
    // so this op lands. That is the recorded residual, not a defect.
    revoke(&er, &mut db, &ex, d_id, T0 + 20_000);
    assert!(
        revocation_row(&db, &d_id).is_some(),
        "the discount ungates X, which is ADR-0041 §\"What a user sees\" item 4's residual"
    );
    assert!(
        read_bound_row(&db, &x_id).is_some(),
        "X revoking somebody else does not give X its own keys back"
    );
}

// ---------------------------------------------------------------------------
// A gated revocation does nothing at all (ADR-0041 §Consequences).
// ---------------------------------------------------------------------------

/// **A revoked device's gated revocation mints no key.**
///
/// The party doing the minting is the revoked one. `revoke_device` ran
/// `rotation_set` → `mint_epoch` → `emit_key_envelopes` whether or not the
/// fold believed its op, so an expelled device running
/// `sunrise devices revoke <anything>` drew a fresh key for every stream in
/// the account, wrote it into its **own** `stream_keys`, and sealed it to every
/// honest peer — whose next writes it could then read, because
/// `current_epoch_tx` was `MAX(epoch)`. It was told, correctly, that it had
/// revoked nothing. A peer that has bounded it no longer writes under such a
/// key (ADR-0041 §Decision 4); a peer that has not still does, and this guard
/// is what protects it.
///
/// Move the rotation loop back outside the `if effective` block in
/// `revoke_device` and this goes red.
#[test]
fn a_gated_revocation_mints_no_epoch_for_the_revoked_device_that_asked() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // X's own vault: X is the device running the command.
    let mut dbx = db_root(ROOT);
    let (x_id, d_id) = (ex.keychain.device_id(), ed.keychain.device_id());
    trust(&ex, &mut dbx, &eo);
    trust(&ex, &mut dbx, &ed);

    // O expels X, and X's own replica applies it.
    revoke(&ex, &mut dbx, &eo, x_id, T0);
    assert!(ex.is_revoked(dbx.conn(), &x_id).unwrap());

    let before = held_epochs(&dbx);
    let out = ex
        .apply(
            &mut dbx,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, d_id),
                reason: RevokeReason::Stolen,
            },
        )
        .unwrap();

    assert!(
        out.revocation_gated,
        "the fold must discard a revoked device's revocation of a third party"
    );
    assert_eq!(
        revocation_row(&dbx, &d_id),
        None,
        "and D stays current, which is what makes the rotation pointless"
    );
    assert_eq!(
        held_epochs(&dbx),
        before,
        "a gated revocation must mint nothing: every key it drew would land in the \
         revoked device's own stream_keys and be sealed to every honest peer"
    );
    assert!(
        out.unrotated_streams.is_empty(),
        "nothing was left unrotated, because nothing was rotated"
    );
    assert!(
        pending_relay_revocations(&dbx).is_empty(),
        "and the relay is told nothing, which was already true"
    );
}

/// The other side of the same predicate: `effective` is **true** when the
/// target was already revoked by a surviving row, and the rotation still runs.
///
/// This is the case the guard must not swallow. The command's own op is gated
/// — its sender is revoked — while the account does record a revocation of the
/// target, so the command is not a no-op and re-revoking is a legitimate,
/// harmless act. Guarding on "was this op gated" rather than on `effective`
/// would break it.
#[test]
fn a_gated_op_whose_target_is_already_revoked_still_rotates() {
    let ex = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbx = db_root(ROOT);
    let (x_id, d_id) = (ex.keychain.device_id(), ed.keychain.device_id());
    trust(&ex, &mut dbx, &eo);
    trust(&ex, &mut dbx, &ed);

    // O expels D first, and then expels X.
    revoke(&ex, &mut dbx, &eo, d_id, T0);
    revoke(&ex, &mut dbx, &eo, x_id, T0 + 1_000);
    assert!(
        revocation_row(&dbx, &d_id).is_some(),
        "D is out on O's word"
    );

    let before = held_epochs(&dbx);
    let out = ex
        .apply(
            &mut dbx,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, d_id),
                reason: RevokeReason::Stolen,
            },
        )
        .unwrap();

    assert!(
        !out.revocation_gated,
        "`effective` reads the register after the fold, and a surviving revocation of \
         the target makes this command's claim true whoever wrote it"
    );
    assert_ne!(
        held_epochs(&dbx),
        before,
        "so the rotation still runs: the predicate is the account's belief about the \
         target, not this op's own fate"
    );
}

/// The unwind reaches the caller, and not only an operator's NDJSON.
///
/// `docs/10-cross-cutting/log-events.md` states the rule on
/// `core.device.revoke_incomplete`: a fact that changes what the account
/// believes comes back on `CommandResult` and a client must disclose it. The
/// unwind has a strictly larger consequence than an unrotated stream and was
/// reaching a `tracing::warn!` and nothing else.
#[test]
fn a_revocation_reports_the_devices_the_account_stopped_calling_revoked() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ed = engine_seeded(ROOT, [5u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id, d_id) = (
        ea.keychain.device_id(),
        ec.keychain.device_id(),
        ed.keychain.device_id(),
    );
    trust(&eb, &mut db, &ea);
    trust(&eb, &mut db, &ec);
    trust(&eb, &mut db, &ed);

    // A retires C; then B retires A, unwinding A's op. C is now bounded and
    // not revoked -- invisible to any caller reading the register alone.
    revoke(&eb, &mut db, &ea, c_id, T0);
    revoke(&eb, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(revocation_row(&db, &c_id), None);

    // B's next revocation is where a user finds out.
    let out = eb
        .apply(
            &mut db,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, d_id),
                reason: RevokeReason::Retired,
            },
        )
        .unwrap();

    assert!(
        !out.revocation_gated,
        "B is current and its revocation lands"
    );
    assert!(
        out.revocation_unwound.iter().any(|h| {
            let mut want = String::new();
            for b in &c_id {
                use std::fmt::Write as _;
                let _ = write!(want, "{b:02x}");
            }
            *h == want
        }),
        "the device the account stopped calling revoked must reach the caller; got {:?}",
        out.revocation_unwound
    );
    assert!(
        !out.revocation_unwound
            .iter()
            .any(|h| h.contains(&format!("{:02x}{:02x}", d_id[0], d_id[1]))
                && read_bound_row(&db, &d_id).is_some()
                && revocation_row(&db, &d_id).is_some()),
        "a device that is both bounded and revoked is not unwound and must not be listed"
    );
}

/// The device list carries both facts, because they can disagree.
///
/// `revoked` is the derived register and `read_bounded` is the ratchet. A row
/// reading `revoked: false, read_bounded: true` is a device the account calls
/// current while giving it nothing, and before this column a user had no way
/// to see it at all.
#[test]
fn the_device_list_shows_a_device_that_is_bounded_without_being_revoked() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    trust(&eb, &mut db, &ea);
    trust(&eb, &mut db, &ec);

    revoke(&eb, &mut db, &ea, c_id, T0);
    revoke(&eb, &mut db, &eb, a_id, T0 + 60_000);

    let QueryResult::Devices(rows) = eb.query(&db, Query::DeviceList).unwrap() else {
        panic!("expected a device list");
    };
    let c = rows
        .iter()
        .find(|r| r.device_id == c_id)
        .expect("C is in the list");
    assert!(
        !c.revoked,
        "the register no longer names C, which is the fold working as designed"
    );
    assert!(
        c.read_bounded,
        "and the list must say it receives nothing, or the state is invisible"
    );

    let a = rows
        .iter()
        .find(|r| r.device_id == a_id)
        .expect("A is in the list");
    assert!(a.revoked && a.read_bounded, "A is out on both counts");
}

/// The fourth key-distribution site: the roster `rotate_identity` builds.
///
/// This one hands out HPKE shares of the **successor `ID_S_priv`**, which is a
/// strictly larger grant than a stream key — a device on the roster follows the
/// account's identity chain. Against the derived register, an unwound device
/// was back on the roster of every transition from then on, so a revocation
/// that had already taken effect was undone by a later, unrelated one.
///
/// Point the survivor query back at `device_revocations` and this goes red.
#[test]
fn an_unwound_device_is_not_on_the_next_identity_rotations_roster() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // B's vault: B is the device that will rotate.
    let mut dbb = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    trust(&eb, &mut dbb, &ea);
    trust(&eb, &mut dbb, &ec);

    let baseline = eb.rotate_identity(&mut dbb, None, true).unwrap();
    assert_eq!(
        baseline.devices_kept, 3,
        "B, A and C are all on the roster before anything is revoked"
    );

    // A retires C, then B retires A -- which unwinds A's revocation of C.
    revoke(&eb, &mut dbb, &ea, c_id, T0);
    revoke(&eb, &mut dbb, &eb, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&dbb, &c_id),
        None,
        "the fold no longer calls C revoked"
    );
    assert!(
        read_bound_row(&dbb, &c_id).is_some(),
        "and C is still bounded"
    );

    let after = eb.rotate_identity(&mut dbb, None, true).unwrap();
    assert_eq!(
        after.devices_kept, 1,
        "only B survives: A is revoked and C is read-bounded. A roster built from the \
         register would carry C, and C would hold a share of the successor ID_S_priv"
    );
}
