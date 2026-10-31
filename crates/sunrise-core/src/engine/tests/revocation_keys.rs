//! What a revocation does to stream keys and envelopes.

use super::identity_rotation::{apply_control_at, build_transition};
use super::*;

/// An absorbed epoch far above this vault's own is refused.
///
/// `MAX(epoch)` is what makes a key live, so one `key_envelope` at
/// `u32::MAX` strands `mint_epoch` at its saturation point *and* redirects
/// every op this device seals afterwards to a key only the sender holds.
#[test]
fn a_key_envelope_far_above_the_live_epoch_is_refused() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let stream = [0x5c; 16];

    let mut absorb = |epoch: u32, key_byte: u8| {
        let key = StreamKey::from_bytes([key_byte; 32]);
        let sealed = eb
            .keychain
            .seal_key_envelope(
                &eb.keychain.identity_dh_pub(),
                &stream,
                epoch,
                &key,
                eb.rng.as_ref(),
            )
            .unwrap();
        let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
            stream_id: stream,
            epoch,
            recipient: Recipient::Identity(eb.keychain.identity_id()),
            key_id: stream_key_id(&key),
            hpke_ciphertext: sealed,
        });
        dbb.with_tx(|tx| eb.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0, 1))
            .unwrap()
    };

    assert_eq!(
        absorb(1, 0xa1),
        vec![(stream, 1)],
        "an ordinary first epoch is absorbed"
    );
    assert_eq!(
        absorb(1 + MAX_EPOCH_LEAP, 0xa2),
        vec![(stream, 1 + MAX_EPOCH_LEAP)],
        "the far edge of the window is still inside it"
    );
    assert!(
        absorb(u32::MAX, 0xa3).is_empty(),
        "and an absurd epoch is refused"
    );
    assert!(
        eb.keychain.stream_keys_at(&stream, u32::MAX).is_empty(),
        "the refused key is not written, so it cannot become the live epoch"
    );
}

/// **A revoked device's third-party recipient claim is not recorded at all.**
///
/// The other half of the bound below, and the second thing ADR-0041 takes away
/// from a revoked device. `key_envelope_recipients` is what
/// `backfill_key_envelopes` reads to decide a device has already been served,
/// so a claim filed on nobody's behalf is a way to withhold a key — and the
/// comment in `apply_control_op` names the exact sender class it could not
/// argue away: a revoked device's reads are bounded by the rotation and (before
/// this) its writes by nothing, so it could file these rows and could not read
/// what they withhold.
///
/// This gate is free of the convergence question the register's fold is about,
/// because the table is a hint rather than state: a replica that declines the
/// row finds no row and emits the backfill, so the gate can only ever cause
/// *more* key distribution, never less.
#[test]
fn a_revoked_devices_third_party_envelope_claim_is_not_recorded() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_random_keys(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let victim = [0x77u8; 16];
    let stream = [0x5d; 16];

    let claim = |db: &mut Db| {
        let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
            stream_id: stream,
            epoch: 1,
            recipient: Recipient::Device(victim),
            key_id: [0u8; 8],
            hpke_ciphertext: vec![0u8; 48],
        });
        db.with_tx(|tx| er.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0, 1))
            .unwrap();
    };
    let filed = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                 WHERE stream_id = ? AND recipient = ?",
                params![&stream[..], &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };

    // While A is a member the claim is recorded, unchecked, as it always was.
    claim(&mut db);
    assert_eq!(filed(&db), 1);

    // Revoked, and a claim about a *different* epoch is refused outright.
    revoke(&er, &mut db, &eb, a_id, T0);
    let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
        stream_id: stream,
        epoch: 2,
        recipient: Recipient::Device(victim),
        key_id: [0u8; 8],
        hpke_ciphertext: vec![0u8; 48],
    });
    db.with_tx(|tx| er.apply_control_op(tx, &inner, &a_id, Hlc::at(T0 + 1), T0 + 1, 1))
        .unwrap();
    assert_eq!(
        filed(&db),
        1,
        "a revoked device must not be able to say a key has already been delivered"
    );
}

/// **An *unwound* device's third-party recipient claim is not recorded
/// either.**
///
/// The gate in `apply_control_op` asks `is_read_bounded` and not `is_revoked`,
/// and this is the case that separates the two. Its own justification names the
/// sender class it exists to catch — a device whose *reads are bounded* by the
/// rotation, so it could file these rows and could not read what they withhold
/// — and since migration 0028 that class is `device_read_bounds`. A device the
/// fold rehabilitated is `read_bounded: true, revoked: false`: it receives no
/// key this vault mints, which is exactly the standing the comment describes,
/// and under `is_revoked` it passed.
///
/// Widening the gate cannot withhold a key, for the reason
/// `a_revoked_devices_third_party_envelope_claim_is_not_recorded` gives: a
/// replica that declines the row finds no row, so `backfill_key_envelopes`
/// emits the envelope. More senders refused means more backfill, never less.
#[test]
fn an_unwound_devices_third_party_envelope_claim_is_not_recorded() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_random_keys(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());
    let victim = [0x77u8; 16];
    let stream = [0x5d; 16];

    let claim = |db: &mut Db, epoch: u32| {
        let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
            stream_id: stream,
            epoch,
            recipient: Recipient::Device(victim),
            key_id: [0u8; 8],
            hpke_ciphertext: vec![0u8; 48],
        });
        db.with_tx(|tx| er.apply_control_op(tx, &inner, &c_id, Hlc::at(T0 + 2), T0 + 2, 1))
            .unwrap();
    };
    let filed = |db: &Db, epoch: u32| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                 WHERE stream_id = ? AND epoch = ? AND recipient = ?",
                params![&stream[..], epoch, &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };

    // A retires laptop C; months later B retires A. The fold stops believing
    // A's op, so C leaves the register — and stays in `device_read_bounds`.
    revoke(&er, &mut db, &ea, c_id, T0);
    revoke(&er, &mut db, &eb, a_id, T0 + 60_000);
    assert_eq!(
        revocation_row(&db, &c_id),
        None,
        "the register no longer calls C revoked: this is the unwind"
    );
    assert!(
        read_bound_row(&db, &c_id).is_some(),
        "and the bound is still C's, which is the whole of what 0028 buys"
    );

    claim(&mut db, 1);
    assert_eq!(
        filed(&db, 1),
        0,
        "a device that receives no key must not be able to say a key has \
         already been delivered, whether or not the register still names it"
    );
}

/// A third party's recipient claim is bounded in epoch before it is
/// recorded.
///
/// The `Recipient::Device(other)` arm files a `key_envelope_recipients`
/// row on another device's word alone — it holds no key for that
/// ciphertext, so nothing about the claim is checkable — and that row is
/// what `backfill_key_envelopes` reads to decide a device already has the
/// key. Unbounded, it is a way to make a *future* epoch undeliverable:
/// file rows for e+1..e+k, wait for the rotation that reaches one, and the
/// backfill finds a row, emits nothing, and leaves a device with no key
/// while its ops park in `deferred_ops` until the TTL drops them with
/// nothing surfacing.
#[test]
fn a_third_party_envelope_claim_is_bounded_in_epoch() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let victim = [0x77u8; 16];
    let stream = [0x5d; 16];

    // The bytes are garbage on purpose. This path never opens them, which
    // is precisely why the claim needs a bound that does not depend on
    // opening them.
    let claim = |db: &mut Db, epoch: u32| {
        let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
            stream_id: stream,
            epoch,
            recipient: Recipient::Device(victim),
            key_id: [0u8; 8],
            hpke_ciphertext: vec![0u8; 48],
        });
        db.with_tx(|tx| eb.apply_control_op(tx, &inner, &a_id, Hlc::at(T0), T0, 1))
            .unwrap();
    };
    let filed = |db: &Db, epoch: u32| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                     WHERE stream_id = ? AND epoch = ? AND recipient = ?",
                params![&stream[..], epoch, &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };

    // This vault holds no key for the stream, so its live epoch is 0.
    claim(&mut dbb, MAX_EPOCH_LEAP);
    assert_eq!(
        filed(&dbb, MAX_EPOCH_LEAP),
        1,
        "the far edge of the window is still inside it"
    );

    claim(&mut dbb, MAX_EPOCH_LEAP + 1);
    assert_eq!(
        filed(&dbb, MAX_EPOCH_LEAP + 1),
        0,
        "a claim one epoch past the window must not be recorded"
    );
    claim(&mut dbb, u32::MAX);
    assert_eq!(
        filed(&dbb, u32::MAX),
        0,
        "and an absurd epoch must not poison every rotation this account will ever do"
    );
}

/// A revocation that cannot rotate every stream succeeds **and says which it
/// could not**.
///
/// `rotation_set` drops a `stream_id` column that is not 16 bytes, because
/// there is genuinely no stream to mint an epoch for and padding one out would
/// file a stranger's key against the vault-meta stream. What was wrong is that
/// it dropped it *silently*: the revoked device goes on holding whatever key it
/// was last given for that row, and `revoke_device` returned a plain success.
///
/// Raising is not the alternative — the device being revoked is often the one
/// that is gone, so failing the whole operation is the worse answer. The third
/// option is the one issue #160 already took one level up for the relay half:
/// succeed partially and disclose it.
///
/// So this asserts both halves, and the first is what stops it from passing
/// against a build that simply refuses the revocation.
#[test]
fn a_revocation_that_cannot_rotate_every_stream_says_which_it_could_not() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    // A real stream with a real key, then its id truncated in place. The row
    // survives, names nothing, and is what the rotation cannot reach.
    let doomed = [0x4du8; 16];
    dba.with_tx(|tx| {
        ea.keychain.mint_epoch(tx, &doomed, &SystemRng, T0)?;
        Ok(())
    })
    .expect("mint the stream that is about to be corrupted");
    dba.conn()
        .execute(
            "UPDATE stream_keys SET stream_id = X'00aabb' WHERE stream_id = ?",
            params![&doomed[..]],
        )
        .unwrap();

    let before = meta_epoch_now(&dba);
    let out = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, b_id),
                reason: RevokeReason::Stolen,
            },
        )
        .expect("the revocation must still succeed: the device that is gone is the scenario");

    // Half one: the rest of the revocation really happened.
    assert!(
        meta_epoch_now(&dba) > before,
        "the vault-meta stream was not rotated, so this is not a partial success \
         but a failure wearing one"
    );

    // Half two: and it is not quiet about the row it could not reach.
    assert_eq!(
        out.unrotated_streams,
        vec!["00aabb".to_string()],
        "the revocation reported success without naming the stream it could not \
         rotate, which the revoked device can still read"
    );
}

/// The vault-meta stream's live epoch, as a revocation moves it.
fn meta_epoch_now(db: &Db) -> i64 {
    db.conn()
        .query_row(
            "SELECT COALESCE(MAX(epoch), 0) FROM stream_keys WHERE stream_id = ?",
            params![&META_STREAM[..]],
            |r| r.get(0),
        )
        .unwrap()
}

/// A revocation with nothing wrong reports nothing, so the field above is a
/// signal rather than noise every caller learns to ignore.
#[test]
fn an_ordinary_revocation_reports_no_unrotated_streams() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let out = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, eb.keychain.device_id()),
                reason: RevokeReason::Lost,
            },
        )
        .expect("revoke");
    assert!(out.unrotated_streams.is_empty());
    assert!(
        !out.revocation_gated,
        "and an ordinary revocation is not reported as discarded"
    );
}

/// **A revocation the fold discards is not reported as a success.**
///
/// `apply_control_op` returns `Ok(())` whether the op was folded into the
/// register or stored and skipped, so until this read `revoke_device` could
/// not tell the two apart and neither could its caller. The failure that
/// followed was not a missing log line. With no `device_revocations` row,
/// `emit_key_envelopes`' anti-join does not exclude the target, so the fresh
/// epoch this command mints for every stream is sealed *to the device it
/// claims to have revoked* — while the relay intent drained and the relay
/// 401'd it. The user was told it worked, every device list went on showing
/// the device as current, it kept receiving new keys, and the relay refused
/// it. Three surfaces disagreeing about one act.
///
/// The op is still emitted, still stored, and still re-judged whenever another
/// revocation lands, and the rotation still runs — the target is a recipient
/// of the new epochs either way, the register saying it is current. What
/// changes is the claim and the relay's half. `log-events.md`'s own
/// `core.device.revoke_incomplete` row states the rule: an operator's NDJSON
/// is not sufficient, the same facts must reach the caller.
#[test]
fn revoke_device_reports_that_the_fold_discarded_its_own_op() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&ea, &mut dba, &ec);
    let (a_id, c_id) = (ea.keychain.device_id(), ec.keychain.device_id());

    // This vault is itself revoked, by an op it has absorbed from B.
    revoke(&ea, &mut dba, &eb, a_id, T0);
    assert!(ea.is_revoked(dba.conn(), &a_id).unwrap());

    // And it now tries to revoke C anyway, which is the whole scenario: a
    // compromised device expelling the rest of the account.
    let out = ea
        .apply(
            &mut dba,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, c_id),
                reason: RevokeReason::Compromised,
            },
        )
        .expect("the command still succeeds: the op is emitted before the fold judges it");

    assert!(
        out.revocation_gated,
        "a command whose op the account discarded must not report a plain success"
    );
    assert!(
        !ea.is_revoked(dba.conn(), &c_id).unwrap(),
        "and nothing was actually revoked"
    );
    assert!(
        !pending_relay_revocations(&dba).contains(&c_id),
        "telling the relay to cut a device every replica still shows as current is \
         the disclosure failure #160 fixed in the other direction"
    );
}

/// **The relay half follows the register through an unwind, both ways (#257).**
///
/// `revoke_device` guards the intent insert so the two halves of a revocation
/// cannot disagree about whether there is one. The fold then made the register
/// retroactive and never touched the intent table, which reopened the same
/// disagreement from the other side. The chain is the issue's own:
///
/// 1. X revokes C while offline: effective, so C's intent is queued.
/// 2. O revokes X. The re-fold gates `X -> C`, and C unwinds to current.
/// 3. X revokes C again. Gated, so nothing new is queued — but step 1's row is
///    still there.
///
/// The intent must not be owed while the register calls C current, or the
/// relay refuses a device every replica shows as a member. And it must not be
/// lost either, which is what clearing it on the unwind would do:
///
/// 4. P revokes O. The discount takes O out of X's revoker set, `X -> C` lands
///    again, and C is revoked — so the relay is owed that revocation, and the
///    row step 1 queued is what carries it.
#[test]
fn a_relay_intent_is_owed_only_while_the_register_calls_its_device_revoked() {
    // The intents the drain would send: `pending_relay_revocations` less every
    // row the register no longer agrees with, read through the one definition
    // `Core::pending_relay_revocations` reads.
    fn owed_relay_revocations(db: &Db) -> Vec<[u8; 16]> {
        let conn = db.conn();
        let mut stmt = conn
            .prepare(crate::relay_intents::OWED_RELAY_REVOCATIONS_SQL)
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows.into_iter()
            .filter_map(|id| <[u8; 16]>::try_from(id.as_slice()).ok())
            .collect()
    }

    let ex = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ep = engine_random_keys(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbx = db_root(ROOT);
    trust(&ex, &mut dbx, &eo);
    trust(&ex, &mut dbx, &ec);
    trust(&ex, &mut dbx, &ep);
    let (x_id, o_id, c_id) = (
        ex.keychain.device_id(),
        eo.keychain.device_id(),
        ec.keychain.device_id(),
    );
    let revoke_c = |dbx: &mut Db| {
        ex.apply(
            dbx,
            Command::RevokeDevice {
                device_id: EntityRef::new(EntityKind::Device, c_id),
                reason: RevokeReason::Lost,
            },
        )
        .expect("revoke C")
    };

    // 1.
    let first = revoke_c(&mut dbx);
    assert!(!first.revocation_gated);
    assert_eq!(owed_relay_revocations(&dbx), vec![c_id]);

    // 2.
    revoke(&ex, &mut dbx, &eo, x_id, T0 + 10_000);
    assert!(!ex.is_revoked(dbx.conn(), &c_id).unwrap(), "C unwound");
    assert_eq!(
        pending_relay_revocations(&dbx),
        vec![c_id],
        "the fold does not write the intent table"
    );
    assert!(
        owed_relay_revocations(&dbx).is_empty(),
        "an intent for a device the register calls current is not owed"
    );

    // 3.
    let again = revoke_c(&mut dbx);
    assert!(again.revocation_gated);
    assert!(
        owed_relay_revocations(&dbx).is_empty(),
        "a gated revocation tells the relay nothing, even over an earlier row"
    );

    // 4.
    revoke(&ex, &mut dbx, &ep, o_id, T0 + 20_000);
    assert!(ex.is_revoked(dbx.conn(), &c_id).unwrap(), "C revoked again");
    assert_eq!(
        owed_relay_revocations(&dbx),
        vec![c_id],
        "the relay is owed the revocation the register now holds"
    );
}

/// `Command::RotateStreamKey` is the one-stream form of the mint-and-distribute
/// chain `revoke_device` keeps a revoked device out of, and it refuses the same
/// device outright.
///
/// Without the guard, a device its own register calls revoked mints a fresh
/// epoch into its own `stream_keys` and seals it to every unbounded peer, and
/// each peer that has not yet bounded it seals its next write under that key.
/// A peer that has is covered by
/// `a_key_a_revoked_device_seals_to_a_peer_is_absorbed_and_never_written_under`. The
/// refusal is checked before anything can mint, so the failed command leaves
/// no key, no op, and no envelope behind. The positive half — an unrevoked
/// device rotates and every current peer is sealed the new epoch — is
/// `a_requested_rotation_keeps_every_current_device` and its neighbours.
#[test]
fn a_revoked_device_cannot_rotate_a_stream_key() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let b_id = eb.keychain.device_id();

    // This vault is itself revoked, by an op it has absorbed from B.
    revoke(&ea, &mut dba, &eb, a_id, T0);
    assert!(ea.is_revoked(dba.conn(), &a_id).unwrap());

    let inbox_keys = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM stream_keys WHERE stream_id = ?",
                params![&INBOX_STREAM_BYTES[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    let ops = |db: &Db| -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
            .unwrap()
    };
    let keys_before = inbox_keys(&dba);
    let ops_before = ops(&dba);
    let to_b_before = envelopes_to(&ea, &dba, &b_id).len();

    let err = ea
        .apply(
            &mut dba,
            Command::RotateStreamKey {
                stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
            },
        )
        .expect_err("a revoked device must not mint an epoch every honest peer adopts");
    assert!(
        matches!(err, EngineError::Invalid(_)),
        "refused as a policy violation, not a storage failure: {err:?}"
    );

    assert_eq!(
        inbox_keys(&dba),
        keys_before,
        "no epoch was minted into this device's own keychain"
    );
    assert_eq!(ops(&dba), ops_before, "and nothing was emitted");
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        to_b_before,
        "so no honest peer was sealed a key this device chose"
    );
}

/// **The peer-side half: a key a read-bounded device seals to this device is
/// absorbed and never written under** (issue #280, ADR-0041 §Decision 4).
///
/// `a_revoked_device_cannot_rotate_a_stream_key` binds an unmodified build.
/// This is the modified client that guard cannot reach: X seals a fresh epoch
/// straight to A's public `D_D`, one above A's live one, through the ordinary
/// `key_envelope` arm. The envelope lands *before* A learns X is revoked, so
/// the test also pins that the standing is read when A chooses a key to write
/// under, and not when the key arrived.
///
/// A does not fall back to its own `honest_epoch` either: X was trusted when
/// A minted it, so X holds it. Pre-revocation keys stay live only while they
/// are the highest epoch a replica holds, and the revoked device holds them
/// until the rotation's envelope arrives.
#[test]
fn a_key_a_revoked_device_seals_to_a_peer_is_absorbed_and_never_written_under() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ex = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eo = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &ex);
    trust(&ea, &mut dba, &eo);
    let (a_id, x_id) = (ea.keychain.device_id(), ex.keychain.device_id());
    let inbox = INBOX_STREAM_BYTES;

    let (honest_epoch, honest_key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &inbox, T0))
        .unwrap();

    // X, running a client that ignores its own revocation, mints the next
    // epoch and seals it to A.
    let planted = StreamKey::from_bytes([0x6b; 32]);
    let planted_epoch = honest_epoch + 1;
    let sealed = ex
        .keychain
        .seal_key_envelope(
            &ea.keychain.device_dh_pub(),
            &inbox,
            planted_epoch,
            &planted,
            ex.rng.as_ref(),
        )
        .unwrap();
    let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
        stream_id: inbox,
        epoch: planted_epoch,
        recipient: Recipient::Device(a_id),
        key_id: stream_key_id(&planted),
        hpke_ciphertext: sealed,
    });
    let learned = dba
        .with_tx(|tx| ea.apply_control_op(tx, &inner, &x_id, Hlc::at(T0), T0, 1))
        .unwrap();
    assert_eq!(
        learned,
        vec![(inbox, planted_epoch)],
        "the key is absorbed, so every op sealed under it opens here"
    );
    let live = |db: &mut Db| {
        db.with_tx(|tx| ea.keychain.current_stream_key_tx(tx, &inbox))
            .unwrap()
            .expect("a live inbox key")
    };
    assert_eq!(
        stream_key_id(&live(&mut dba).1),
        stream_key_id(&planted),
        "while X is a member here, its epoch is live like anybody's"
    );

    // O revokes X, and A applies it.
    revoke(&ea, &mut dba, &eo, x_id, T0 + 1_000);
    assert!(read_bound_row(&dba, &x_id).is_some(), "A bounded X");

    // Not X's epoch, which X delivered. Nor `honest_epoch` below it: A minted
    // that while X was trusted, so X was sealed it and holds it too.
    assert!(
        dba.with_tx(|tx| ea.keychain.current_stream_key_tx(tx, &inbox))
            .unwrap()
            .is_none(),
        "A holds no inbox key it may write under"
    );
    assert!(
        ea.keychain
            .stream_keys_at(&inbox, planted_epoch)
            .iter()
            .any(|k| stream_key_id(k) == stream_key_id(&planted)),
        "and still holds X's key, so what was sealed under it stays readable"
    );
    assert!(
        ea.keychain
            .stream_keys_at(&inbox, honest_epoch)
            .iter()
            .any(|k| stream_key_id(k) == stream_key_id(&honest_key)),
        "as it does its own pre-revocation key"
    );

    // So A's next write mints above both, and the fresh epoch is delivered to
    // O and not to X.
    let task = new_task(&ea, &mut dba, "written after the revocation");
    let env = create_env_for(&dba, task.bytes());
    let sealed_at = sunrise_crypto::decode_envelope(&env)
        .expect("the op this engine just wrote decodes")
        .epoch;
    assert_eq!(
        sealed_at,
        planted_epoch + 1,
        "the task was sealed under epoch {sealed_at}; X holds epochs \
         {honest_epoch} and {planted_epoch}"
    );
    let o_id = eo.keychain.device_id();
    assert!(envelopes_to(&ea, &dba, &o_id).contains(&(inbox, sealed_at)));
    assert!(
        !envelopes_to(&ea, &dba, &x_id).contains(&(inbox, sealed_at)),
        "X was not delivered the epoch A wrote under"
    );
    ea.keychain
        .open_op(&env)
        .expect("and A reads its own write");

    // A stream X keyed alone — one it created after its revocation — has no
    // key A may write under, so A mints its own above X's rather than
    // writing under X's.
    let lone = [0x5f; 16];
    let lone_key = StreamKey::from_bytes([0x6c; 32]);
    let sealed = ex
        .keychain
        .seal_key_envelope(
            &ea.keychain.device_dh_pub(),
            &lone,
            1,
            &lone_key,
            ex.rng.as_ref(),
        )
        .unwrap();
    let inner = InnerOp::KeyEnvelope(KeyEnvelopePayload {
        stream_id: lone,
        epoch: 1,
        recipient: Recipient::Device(a_id),
        key_id: stream_key_id(&lone_key),
        hpke_ciphertext: sealed,
    });
    dba.with_tx(|tx| ea.apply_control_op(tx, &inner, &x_id, Hlc::at(T0), T0, 1))
        .unwrap();
    let (epoch, key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &lone, T0 + 2_000))
        .unwrap();
    assert_eq!(epoch, 2, "minted above the epoch X holds");
    assert_ne!(stream_key_id(&key), stream_key_id(&lone_key));
}

/// The epoch separation the ordering argument rests on, asserted against the
/// **production** path rather than against hand-chosen epochs.
///
/// `a_higher_meta_epoch_beats_any_hlc` below applies one transition at epoch 1
/// and the other at epoch 2 and shows the fold prefers the higher. That pins
/// the comparison and nothing else: it says what happens *if* the honest
/// rotation outranks the revoked device's, and takes the "if" as given. The
/// "if" is the security claim.
///
/// Nothing in this engine's types produces it. It is an emergent property of
/// two transactions in `revoke_device`: the register is written, every stream
/// in `rotation_set` — the vault-meta stream among them — is minted forward and
/// sealed to the unrevoked devices only, that transaction commits, and *then*
/// `rotate_identity` runs and seals the transition under whatever epoch is live,
/// which is now E+1. Reorder those two and the argument collapses silently: the
/// transition ties the revoked device's own possible rows on `meta_epoch`, and
/// `hlc_physical_ms` decides — a value that device chooses freely inside
/// `MAX_DRIFT_MS`.
///
/// The comment in `rotate_identity` asserted the collapsed version ("sealed
/// under the epoch the departing device still shares") until this test was
/// written, which is exactly how much a comment is worth here.
#[test]
fn a_revocations_transition_is_sealed_above_the_epoch_the_cut_device_holds() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .expect("the revocation runs");

    // Read the epoch each op was sealed under off its own envelope, which is
    // what a peer reads.
    let sealed_at = |kind: &str| -> u32 {
        let env: Vec<u8> = dba
            .conn()
            .query_row(
                "SELECT envelope FROM ops WHERE inner_kind = ?1 AND stream_id = ?2",
                params![kind, &META_STREAM[..]],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("one {kind} op in the vault-meta stream: {e}"));
        sunrise_crypto::decode_envelope(&env)
            .expect("the op this engine just wrote decodes")
            .epoch
    };

    // The revocation itself is deliberately sealed at the epoch the departing
    // device still holds -- it has to be readable by every peer that has not
    // yet received the new one. That is the ceiling on what the cut device can
    // ever seal under.
    let cut_epoch = sealed_at("device.revoke");
    let transition_epoch = sealed_at("identity.transition");
    assert!(
        transition_epoch > cut_epoch,
        "the rotation that makes the revocation stick was sealed at meta epoch \
         {transition_epoch}, which does not outrank the epoch {cut_epoch} the cut device \
         still holds; ADR-0037 §4's ordering argument needs it strictly above"
    );

    // And the ceiling is real: the revoked device was sealed no key at or above
    // the epoch the transition sits at, so it cannot produce a competing row
    // there however it dates its HLC.
    let served_at_or_above: Vec<u32> = key_envelope_envs(&dba)
        .into_iter()
        .filter_map(|env| ea.keychain.open_op(&env).ok())
        .filter_map(|cbor| decode_inner_op(&cbor).ok())
        .filter_map(|inner| match inner {
            InnerOp::KeyEnvelope(p)
                if p.stream_id == META_STREAM
                    && p.recipient == Recipient::Device(b_id)
                    && p.epoch >= transition_epoch =>
            {
                Some(p.epoch)
            }
            _ => None,
        })
        .collect();
    assert!(
        served_at_or_above.is_empty(),
        "the revoked device was handed vault-meta keys at {served_at_or_above:?}, at or \
         above the epoch its own revocation's rotation was sealed under"
    );
}

/// **`meta_epoch` sorts first, and that is the security component.**
///
/// Two transitions succeed the same identity. The attacker's carries an
/// HLC far ahead of anything honest — the shape a forward-dated op takes,
/// bounded only by `MAX_DRIFT_MS` — and is sealed under the meta epoch it
/// still holds. The honest one carries a small HLC and a higher epoch.
///
/// The honest one wins, and not because it is honest: it wins because a
/// revoked device provably cannot raise `meta_epoch`. `revoke_device`
/// writes the revocation register *before* it mints, so
/// `emit_key_envelopes`' anti-join excludes that device from every epoch
/// minted in the same transaction, and it holds no key above the one it was
/// cut at. An HLC is a claim; an epoch is a key you either have or do not.
#[test]
fn a_higher_meta_epoch_beats_any_hlc() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let (attacker, attacker_to) = build_transition(&ea, &[&eb], false, T0);
    let (honest, honest_to) = build_transition(&ea, &[&eb], true, T0);
    let a_id = ea.keychain.device_id();

    // Forward-dated to just inside the drift gate, at the old epoch.
    apply_control_at(
        &ea,
        &mut dba,
        &attacker,
        &a_id,
        Hlc::at(T0 + sunrise_cbor::hlc::MAX_DRIFT_MS - 1),
        1,
    );
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        attacker_to,
        "with nothing to compete against it, it does win"
    );

    // Backdated ten days, at the next epoch. The epoch decides.
    apply_control_at(&ea, &mut dba, &honest, &a_id, Hlc::at(T0 - 864_000_000), 2);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        honest_to,
        "a higher meta_epoch wins however far the loser's clock was pushed"
    );
}

/// Two competing transitions, applied to two fresh replicas in **opposite
/// orders**, converge byte for byte.
///
/// The property the whole design rests on and the reason nothing is refused
/// at apply time: standing is a pure function of the op set, so arrival
/// order cannot decide who the account is.
#[test]
fn competing_transitions_converge_whatever_order_they_arrive_in() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let (first, _) = build_transition(&ea, &[&eb], true, T0);
    let (second, _) = build_transition(&ea, &[&eb], true, T0);
    let a_id = ea.keychain.device_id();

    // Each op carries its *own* stamp, fixed before either is delivered.
    // Deriving it from arrival position instead would make the input a
    // function of the order and the test vacuous — which is what the first
    // version of this did, and what it caught.
    let a = (&first, Hlc::at(T0 + 1));
    let b = (&second, Hlc::at(T0 + 2));

    let chain_after = |order: [(&InnerOp, Hlc); 2]| -> Vec<([u8; 16], [u8; 32])> {
        // A fresh engine per run: the keychain adopts as it folds, and a
        // reused one would carry the first run's adoption into the second.
        let e = engine_random_keys(ROOT, [9u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut db = db_root(ROOT);
        trust(&e, &mut db, &ea);
        trust(&e, &mut db, &eb);
        for (inner, hlc) in order {
            apply_control_at(&e, &mut db, inner, &a_id, hlc, 1);
        }
        e.chain_identities(db.conn()).unwrap()
    };

    let forwards = chain_after([a, b]);
    let backwards = chain_after([b, a]);
    assert_eq!(forwards.len(), 2, "one of the two won, and only one");
    assert_eq!(
        forwards, backwards,
        "the chain is a function of the op set, not of arrival order"
    );
}

/// The read half of revocation: a rotation seals **no** device envelope to
/// the device it just revoked, and there is no identity copy that device
/// could open instead.
///
/// Both halves are asserted, because either alone is vacuous. Excluding the
/// device from the recipient list withholds nothing while a `PairingPayload`
/// hands it `ID_D_priv` — that was #76, and it is why the exclusion was
/// removed rather than repaired. Dropping `ID_D_priv` from the payload
/// withholds nothing while the device is still on the recipient list. The
/// property only exists when both are true at once.
#[test]
fn a_revoked_device_gets_no_key_for_an_epoch_minted_after_its_cut() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    let before = key_envelope_envs(&dba).len();
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();
    let minted: Vec<InnerOp> = key_envelope_envs(&dba)
        .into_iter()
        .skip(before)
        .filter_map(|env| ea.keychain.open_op(&env).ok())
        .filter_map(|cbor| decode_inner_op(&cbor).ok())
        .collect();
    assert!(
        !minted.is_empty(),
        "the revocation rotated something, or this test proves nothing"
    );

    assert!(
        !minted.iter().any(|inner| matches!(
            inner,
            InnerOp::KeyEnvelope(p) if p.recipient == Recipient::Device(b_id)
        )),
        "the rotation sealed a device envelope to the device it just revoked"
    );

    // The identity copy is still emitted -- recovery depends on it -- and
    // is exactly what the revoked device can no longer open, because
    // `PairingPayload` no longer carries `ID_D_priv`. `Identity::dh_secret`
    // is `None` on every device pairing admitted, so the type system is
    // what holds this rather than a runtime check.
    assert!(
        minted.iter().any(|inner| matches!(
            inner,
            InnerOp::KeyEnvelope(p) if matches!(p.recipient, Recipient::Identity(_))
        )),
        "the identity copy must survive: it is the recovery path"
    );
}

/// The same property, on a replica whose HLC sits ahead of its wall clock.
///
/// This is the deterministic form of the leak the exclusion above only
/// *looked* like it provided. `revoke_device` reads `now_ms` off the wall
/// clock before the transaction opens; the cut it writes is
/// `hlc.send().physical_ms`, and once `observe()` has absorbed a peer up to
/// `MAX_DRIFT_MS` ahead that stamp stays above `now_ms` for as long as the
/// skew lasts. The anti-join asked `cut_ms <= now_ms`, which is then false,
/// so the revoked device was sealed a `key_envelope` for every stream its
/// own revocation rotated -- to its own `D_D_pub`, in ops sealed under the
/// *pre*-rotation meta epoch it still holds, with a
/// `key_envelope_recipients` row filed to say it had been served.
///
/// A frozen clock cannot detect this. `Engine::from_clock` derives the HLC
/// from the same `Clock`, so `hlc.physical_ms == now_ms` by construction
/// and the comparison held by coincidence. The `observe` below is what
/// breaks the coincidence, and it is an ordinary event: any peer whose
/// clock leads by any amount inside the drift gate causes it.
///
/// The repair that survives is not a better comparison but no comparison:
/// a recorded row excludes, full stop. This test is kept because it is the
/// regression the original defect deserves — a cut stamped ahead of local
/// time must be effective — and it now passes for a reason no clock can
/// take away. See `a_recorded_cut_survives_a_restart` for the case that
/// killed the comparison outright.
#[test]
fn a_revoked_device_is_excluded_when_the_hlc_leads_the_wall_clock() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b_id = eb.keychain.device_id();

    // A peer 200 s ahead: inside `MAX_DRIFT_MS`, so the drift gate accepts
    // it and A's HLC keeps it. Everything A stamps afterwards sits at
    // T0 + 200 s while `clock.now_ms()` still reads T0.
    ea.hlc.observe(Hlc::at(T0 + 200_000)).unwrap();
    assert!(
        ea.hlc.peek().physical_ms > ea.clock.now_ms(),
        "the premise of this test is an HLC ahead of the wall clock"
    );

    let before = key_envelope_envs(&dba).len();
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    // The cut really did land ahead of the wall clock. Without this the
    // anti-join would exclude B for the ordinary reason and the test would
    // prove nothing about the skewed case.
    let (cut_ms, _, _, _) = revocation_row(&dba, &b_id).expect("the cut was recorded");
    assert!(
        u64::try_from(cut_ms).unwrap() > ea.clock.now_ms(),
        "the cut must be ahead of `now_ms`, or this test is the frozen-clock one again"
    );

    let minted: Vec<InnerOp> = key_envelope_envs(&dba)
        .into_iter()
        .skip(before)
        .filter_map(|env| ea.keychain.open_op(&env).ok())
        .filter_map(|cbor| decode_inner_op(&cbor).ok())
        .collect();
    assert!(
        !minted.is_empty(),
        "the revocation rotated something, or this test proves nothing"
    );
    assert!(
        !minted.iter().any(|inner| matches!(
            inner,
            InnerOp::KeyEnvelope(p) if p.recipient == Recipient::Device(b_id)
        )),
        "a revocation stamped by an HLC ahead of the wall clock sealed the new epoch to \
             the device it was revoking"
    );
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "no envelope in the whole log may name the revoked device"
    );

    // And nothing filed a row claiming it had been served, which is what
    // made the leak silent: the row is what `backfill_key_envelopes` reads
    // to decide a device already has the key.
    let served: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM key_envelope_recipients WHERE recipient = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        served, 0,
        "a recipient row was filed for the device the transaction was revoking"
    );
}

/// The republish route is closed on the same timeline, not only the mint.
///
/// `backfill_key_envelopes` skips a revoked device on the same cut test the
/// recipient query uses, so leaving that one on the wall clock while fixing
/// the anti-join would only move the leak: the revoked device re-sends the
/// cert it already has and is handed every key back in one round trip.
#[test]
fn a_revoked_device_is_not_backfilled_when_the_hlc_leads_the_wall_clock() {
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
    // A cut 200 s beyond the wall clock -- what a peer whose clock leads
    // writes, and what the drift gate admits.
    let cut = T0 + 200_000;
    ea.hlc.observe(Hlc::at(cut)).unwrap();
    revoke(&ea, &mut dba, &ea, b_id, cut);
    assert!(
        u64::try_from(revocation_row(&dba, &b_id).expect("the cut was recorded").0).unwrap()
            > ea.clock.now_ms(),
        "the premise: this cut sits ahead of the wall clock, which is what used to read \
             as 'not revoked yet'"
    );
    assert!(
        ea.is_revoked(dba.conn(), &b_id).unwrap(),
        "and it is effective anyway, because the row is the whole test"
    );

    trust_at(&ea, &mut dba, &eb, T0);
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a revoked device recovered its keys by re-sending its cert, because the cut was \
             compared against a wall clock instead of the timeline it was written on"
    );
}

/// **A recorded cut survives a restart.** This is the case that took the
/// time comparison out entirely.
///
/// Any comparison of `cut_ms` against a local reading needs a local reading
/// that is at or above every cut this replica has absorbed. The HLC is
/// exactly that *within one process* — and only there.
/// [`MonotonicHlc`](crate::config::MonotonicHlc) is deliberately not
/// persisted, [`Engine::from_clock`] builds a fresh one at `Hlc::default()`,
/// and `observe()` runs from one place: `apply_remote_all`. Nothing primes
/// the HLC from the op log or from `device_revocations` at open. So a
/// restart leaves the HLC reading 0 against `cut_ms` rows that survived,
/// and any horizon derived from it collapses to the bare wall clock.
///
/// The scenario is ordinary rather than adversarial: a peer inside
/// `MAX_DRIFT_MS` writes a cut ahead of local time, and a backgrounded
/// mobile app is killed and reopened before local time catches up. The
/// revoked device then re-sends the cert it already holds and
/// `backfill_key_envelopes` seals it the current epoch of every stream —
/// without the revoked device having to restart, or do anything but wait.
///
/// The second engine below is that restart: a new `Engine` over the *same*
/// database, which is what `Core::open` constructs.
#[test]
fn a_recorded_cut_survives_a_restart() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], clock.clone());
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

    // A cut 200 s ahead of the wall clock: inside `MAX_DRIFT_MS`, so an
    // ordinary peer writes it and the drift gate admits it.
    let cut = T0 + 200_000;
    ea.hlc.observe(Hlc::at(cut)).unwrap();
    revoke(&ea, &mut dba, &ea, b_id, cut);
    drop(ea);

    // The restart. Same database, same wall clock, brand-new HLC.
    let ea2 = engine_random_keys(ROOT, [1u8; 32], clock.clone());
    assert_eq!(
        ea2.hlc.peek().physical_ms,
        0,
        "the premise: HLC state is not persisted, so a reopened engine starts at zero"
    );
    assert!(
        u64::try_from(
            revocation_row(&dba, &b_id)
                .expect("the cut is still recorded")
                .0
        )
        .unwrap()
            > ea2.clock.now_ms(),
        "and the persisted cut is still ahead of the wall clock"
    );

    // Every comparison that could have been built out of this engine's own
    // state is now below the cut, so any of them would report "not revoked".
    assert!(
        ea2.is_revoked(dba.conn(), &b_id).unwrap(),
        "a recorded revocation must not become ineffective because the process restarted"
    );

    // And the route that actually exploits it stays shut.
    trust_at(&ea2, &mut dba, &eb, T0);
    assert!(
        envelopes_to(&ea2, &dba, &b_id).is_empty(),
        "a revoked device recovered every key by re-sending its cert after the revoking \
             device restarted"
    );
}

/// The revocation's *own* transaction excludes the revokee even on the
/// path that mints the vault-meta stream's first epoch.
///
/// `revoke_device` calls `ensure_stream_epoch(META)` to find the epoch its
/// ops are sealed under, and on an account whose meta stream has no key yet
/// that call **mints** one and emits a `key_envelope` per recipient. That
/// mint happens inside the revoking transaction and does not go through the
/// rotation loop, so it is a second, easily-missed way for the revoked
/// device to be handed a key by its own revocation.
///
/// What makes it safe is ordering, and only ordering: the register row is
/// written before anything can mint, so the recipient query already sees it.
/// This test exists because that ordering is a property nothing else pins —
/// every other revocation test starts from a vault that already has a meta
/// key, so `ensure_stream_epoch` returns early and the mint never runs.
#[test]
fn the_first_meta_mint_inside_a_revocation_excludes_the_revokee() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    // Admit B and write nothing else, so the meta stream still has no key
    // and the first mint will happen inside the revocation below.
    trust(&ea, &mut dba, &eb);
    let meta_keys: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM stream_keys WHERE stream_id = ?",
            params![&META_STREAM[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        meta_keys, 0,
        "the premise: the vault-meta stream has no key yet, so revoking mints its first"
    );

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, b_id),
            reason: RevokeReason::Lost,
        },
    )
    .unwrap();

    assert!(
        dba.conn()
            .query_row(
                "SELECT count(*) FROM stream_keys WHERE stream_id = ?",
                params![&META_STREAM[..]],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0,
        "the revocation did mint the meta key, or this test proves nothing"
    );
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "the first meta mint inside a revocation sealed an envelope to the device the \
             transaction exists to revoke"
    );
}

/// Revoking a device leaves `devices.revoked_at_ms` NULL, because the
/// register lives in `device_revocations` and that column is superseded.
///
/// Migration 0017 says so in a comment. This makes it a fact: anything that
/// starts writing the old column again fails here rather than creating a
/// second, silently disagreeing source of truth.
#[test]
fn revocation_does_not_touch_the_superseded_devices_column() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
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

    assert!(
        revocation_row(&dba, &target).is_some(),
        "the register is where the revocation lives"
    );
    let legacy: Option<i64> = dba
        .conn()
        .query_row(
            "SELECT revoked_at_ms FROM devices WHERE device_id = ?",
            params![&target[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy, None, "the 0013 column is superseded and unwritten");
    // And the device list reads the register, not the dead column.
    match ea.query(&dba, Query::DeviceList).unwrap() {
        QueryResult::Devices(rows) => assert!(
            rows.iter().find(|d| d.device_id == target).unwrap().revoked,
            "the device list still shows it revoked"
        ),
        other => panic!("expected Devices, got {other:?}"),
    }
}
