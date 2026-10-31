//! Revocation against the apply cursor: what advances it and what does not.

use super::*;

/// A revoked device's ops still **apply** at a receiving replica, and its
/// cursor still advances.
///
/// This half is deliberately not a gate, and there is now a write bound
/// elsewhere for it to defer to. `Command::RevokeDevice` records a
/// `relay_revocation_intents` row in the same transaction as the op and the
/// sync driver drains it into `DELETE /api/v1/devices/{device_id}`, after which
/// the relay refuses the device's uploads and tears down its live session
/// ([#80](https://github.com/justin13888/Sunrise/issues/80), closed). That
/// bound is where an *entity* write is stopped. The gate ADR-0041 adds here is
/// over control ops only, and this test is the boundary of it: a task the
/// revoked device wrote still applies.
///
/// Refusing here instead is a live hazard, and six review rounds produced
/// six defects that were all this shape. Refusing a device's ops freezes
/// its sync cursor while the relay keeps accepting its uploads, and
/// retention then latches a permanent data-loss warning on every peer.
/// Refusing at apply time is also not convergent: a replica that applied an
/// op before the revocation arrived cannot un-apply it, and there is no
/// projection rebuild in this engine to make it, so two replicas with the
/// same op set would disagree forever. Because nothing refuses, they do
/// not: delivery order is not an input to the materialized state, and a
/// cut correction is lossless. Decided in ADR-0034
/// (`docs/11-adr/0034-revocation-bounds-reads-not-writes.md`), which closed
/// #78; #82 is where a convergent form belongs, after the relay bound, and
/// ADR-0041 (`docs/11-adr/0041-peer-side-revocation-is-a-fold.md`) is that
/// form — scoped to the control ops whose effect can be re-derived, which a
/// task edit's cannot.
///
/// What *is* enforced here is reads, and that is the test below.
#[test]
fn a_revoked_devices_ops_still_apply_at_the_replica() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);

    // A is revoked before it writes anything, at a cut before every op
    // below — the strongest form of the premise.
    revoke(&eb, &mut dbb, &eb, ea.keychain.device_id(), T0);
    assert!(eb.is_revoked(dbb.conn(), &ea.keychain.device_id()).unwrap());

    set_clock(&ca, T0 + 10_000);
    let task = ea
        .apply(
            &mut dba,
            Command::CreateTask(TaskDraft {
                title: "written after the cut".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    // B is given the vault-meta key, which is what lets it read control
    // ops at all; every Stream key below still has to arrive by envelope.
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);

    // Its Stream keys are absorbed, so the op it sealed can be opened.
    for env in key_envelope_envs(&dba) {
        eb.apply_remote_all(&mut dbb, &env)
            .expect("a revoked device's key envelope applies");
    }
    assert!(
        !eb.keychain
            .stream_keys_at(&sunrise_domain::INBOX_STREAM_BYTES, 1)
            .is_empty(),
        "the key it distributed was absorbed"
    );

    // And the op itself is applied, materialized, and passed by the cursor.
    let env = env_bytes(&dba, &task.op_id);
    let head = sunrise_cbor::decode_envelope_header(&env).unwrap();
    eb.apply_remote(&mut dbb, &env)
        .expect("a revoked device's op applies")
        .expect("and produces its event");
    assert_eq!(
        read_task_t(&eb, &dbb, task.entity).title,
        "written after the cut"
    );
    assert_eq!(
        cursor_for(&dbb, &head.stream_id, &head.device_id),
        1,
        "the cursor advances, so nothing stalls and nothing replays forever"
    );
}

/// The refusal that survives on the apply path still counts the op it refused.
///
/// `apply_control_op` refuses a `device_revoke` that names its own sender —
/// `core.device.revoke_refused`, `reason = "self"` — because the register is
/// last-writer-wins and a device that can rewrite its own row can undo
/// somebody else's revocation of it. What it refuses is the **register
/// write**, not the delivery: the op row went in at `apply_remote_all`'s
/// idempotence gate before the control op was dispatched, so
/// `upsert_sync_cursor` still runs afterwards with that row in the log. The
/// delivery here is seq 1, so the contiguous prefix is that one op and the
/// stored number becomes 1.
/// `a_self_refused_revoke_out_of_order_leaves_the_cursor_short` is the same
/// refusal with a gap below it, where the number does not move at all — the
/// refusal is not what decides it either way.
///
/// Two further refusals live in the revocation machinery and neither is on
/// this path: `Engine::revoke_device` refuses a self-revocation, and refuses
/// a target with no `devices` row. Both are local command guards that never
/// produce an op, so neither is what a reader arrives here holding.
///
/// That is the opposite of what `upsert_sync_cursor`'s doc claimed until
/// [#253](https://github.com/justin13888/Sunrise/issues/253) — that a refusal
/// leaves the cursor where it is and the op applies if it is resent. Only
/// prose ever said so, which is how four other copies of the same claim
/// reached #240's review uncorrected and this one outlived them, so this
/// holds both halves: the register is untouched and the cursor advances.
///
/// `a_device_cannot_move_its_own_revocation_cut` covers the register half
/// through `apply_control_op` directly. It cannot see this one, because that
/// path has no envelope, no op row and therefore no cursor.
#[test]
fn a_self_refused_revoke_still_advances_the_cursor() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();

    // B has admitted A, so `apply_remote_all`'s step b finds the sender's row
    // and the delivery is an ordinary one rather than a rejected stranger.
    trust(&eb, &mut dbb, &ea);

    // A seals a `device_revoke` naming **itself**. `Command::RevokeDevice`
    // refuses that locally for an unrelated reason — rotating every key away
    // from the only device holding them is not recoverable — so the op is
    // built at the op-log seam instead, which is all a peer running older or
    // hostile code has to do.
    let inner = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
        revoked_device_id: a_id,
        reason_code: RevokeReason::Lost,
    }))
    .expect("encode the inner op");
    let (seq, env) = dba
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            // The epoch first, then the seq: minting can emit `key_envelope`
            // ops into this very stream, and each would spend a seq a number
            // read beforehand had already claimed.
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            let seq = ea.next_seq_tx(tx, &META_STREAM)?;
            let env = ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    ea.hlc.send(),
                    &inner,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under A's own live meta epoch");
            Ok((seq, env))
        })
        .unwrap();
    assert_eq!(
        seq, 1,
        "the cursor is the contiguous prefix from seq 1, so this op has to be it"
    );

    // Captured, because a paragraph in `upsert_sync_cursor`'s doc rests on this
    // event by name and names both of its emitters. Asserting only the
    // absent register row would leave the log line deletable with nothing red.
    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(eb.apply_remote_all(&mut dbb, &env));
    });
    let events = delivered
        .expect("the capture ran the delivery")
        .expect("a self-naming revoke is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");
    assert!(
        logged.iter().any(|ev| ev == "core.device.revoke_refused"),
        "the refusal is announced, not silent: replace the `tracing::warn!` in \
         `apply_device_revoke` with nothing and this is what fails; got {logged:?}"
    );

    // The register refused it.
    assert_eq!(
        revocation_row(&dbb, &a_id),
        None,
        "a device must not be able to write the register entry about itself"
    );
    assert!(
        !eb.is_revoked(dbb.conn(), &a_id).unwrap(),
        "and it is not revoked by its own op"
    );

    // And the cursor advanced past it regardless, which is the half nothing
    // else in this suite holds.
    assert_eq!(
        cursor_for(&dbb, &META_STREAM, &a_id),
        seq,
        "the refusal is the register's, not the delivery's: the op row is in, \
         so the cursor covers it"
    );

    // Resending recovers nothing, and the arm is never entered a second time.
    // `remote_op_id` is derived from `(stream_id, device_id, seq)`, so the
    // same coordinates carry the same op-log primary key whatever the payload
    // says: the insert collides, `tx.changes()` is 0, and `apply_remote_all`
    // returns before `apply_control_op` is reached at all.
    //
    // So the resend carries a **different** inner op under those same
    // coordinates -- a revoke naming a third device, which the self guard
    // would not refuse and which would write a register row the instant the
    // arm ran. No such row is the observation that the arm was not entered.
    // Resending the identical bytes and asserting nothing changed cannot make
    // it: an idempotent second run of the refusal looks exactly the same.
    let third = [9u8; 16];
    let other = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
        revoked_device_id: third,
        reason_code: RevokeReason::Lost,
    }))
    .expect("encode a different inner op");
    let resend = dba
        .with_tx(|tx| -> rusqlite::Result<Vec<u8>> {
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            Ok(ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    ea.hlc.send(),
                    &other,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal a different payload at the same seq"))
        })
        .unwrap();
    assert!(
        eb.apply_remote_all(&mut dbb, &resend)
            .expect("a resend is not an error")
            .is_empty(),
        "the second delivery is not re-evaluated"
    );
    assert_eq!(
        revocation_row(&dbb, &third),
        None,
        "the idempotence gate returned before `apply_control_op`, so the arm never \
         saw the payload the resend carried"
    );
    assert_eq!(
        revocation_row(&dbb, &a_id),
        None,
        "and a resend does not get the register a second look"
    );
    assert_eq!(
        cursor_for(&dbb, &META_STREAM, &a_id),
        seq,
        "and it moves nothing"
    );
}

/// **A read-bounded sender's third-party recipient claim is refused, and the
/// cursor counts the op anyway.**
///
/// This is the delivery half of the gate at
/// `crates/sunrise-core/src/engine/sync.rs:989#apply_control_op`. The two
/// units that reach that gate today —
/// `a_revoked_devices_third_party_envelope_claim_is_not_recorded` and
/// `an_unwound_devices_third_party_envelope_claim_is_not_recorded` — call
/// `apply_control_op` directly inside a test transaction, so there is no
/// envelope, no op row and no cursor, and the only thing either can observe is
/// the absent hint row.
///
/// `upsert_sync_cursor`'s doc names this site in its enumeration of the
/// refusals that survive, and what it claims there is exactly the half a
/// direct call cannot see: this refusal declines a `key_envelope_recipients`
/// hint row and nothing else. The op row went in at the idempotence gate
/// before the control op was dispatched, so it stands, and `upsert_sync_cursor`
/// runs afterwards at step g and counts it. The delivery is seq 1, so the
/// contiguous prefix is that one op and the stored number becomes 1.
///
/// All three are asserted, because no two of them constrain the third. An
/// absent hint row is also what a delivery that never arrived looks like; a
/// cursor at 1 is also what an *accepted* claim writes. The refusal's own log
/// line is observed rather than assumed, for the reason
/// `a_self_refused_revoke_still_advances_the_cursor` gives: asserting only the
/// absent row would leave the `tracing::warn!` deletable with nothing red.
#[test]
fn a_read_bounded_senders_recipient_claim_is_refused_and_the_cursor_counts_the_op() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let victim = [0x77u8; 16];

    // B has admitted A, so `apply_remote_all`'s step b finds the sender's row
    // and the delivery is an ordinary one rather than a rejected stranger.
    trust(&eb, &mut dbb, &ea);

    // C retires A at B. The gate reads `device_read_bounds` and not the
    // register, so the premise is asserted on the table the gate reads.
    revoke(&eb, &mut dbb, &ec, a_id, T0);
    assert!(
        eb.is_read_bounded(dbb.conn(), &a_id).unwrap(),
        "the gate asks `is_read_bounded`, so that is what has to hold going in"
    );

    // A claims a third device already holds the key at (vault-meta, 1). That
    // is the withhold the gate exists to refuse: a device that can file these
    // rows and cannot read what they suppress.
    let inner = encode_inner_op(&InnerOp::KeyEnvelope(KeyEnvelopePayload {
        stream_id: META_STREAM,
        epoch: 1,
        recipient: Recipient::Device(victim),
        key_id: [0u8; 8],
        hpke_ciphertext: vec![0u8; 48],
    }))
    .expect("encode the inner op");
    let (seq, env) = dba
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            // The epoch first, then the seq, for the reason
            // `a_self_refused_revoke_still_advances_the_cursor` states.
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            let seq = ea.next_seq_tx(tx, &META_STREAM)?;
            let env = ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    ea.hlc.send(),
                    &inner,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under A's own live meta epoch");
            Ok((seq, env))
        })
        .unwrap();
    assert_eq!(
        seq, 1,
        "the cursor is the contiguous prefix from seq 1, so this op has to be it"
    );

    let filed = |db: &Db| -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM key_envelope_recipients
                 WHERE stream_id = ? AND epoch = ? AND recipient = ?",
                params![&META_STREAM[..], 1u32, &victim[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(filed(&dbb), 0, "nothing has filed a hint row yet");

    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(eb.apply_remote_all(&mut dbb, &env));
    });
    let events = delivered
        .expect("the capture ran the delivery")
        .expect("a refused recipient claim is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");
    assert!(
        logged
            .iter()
            .any(|ev| ev == "core.key.recipient_claim_refused"),
        "the refusal is announced, not silent: replace the `tracing::warn!` at \
         the `is_read_bounded` gate with nothing and this is what fails; got \
         {logged:?}"
    );

    // What the refusal took: the hint row, and only the hint row.
    assert_eq!(
        filed(&dbb),
        0,
        "a read-bounded device must not be able to say a key has already been \
         delivered"
    );

    // What it left behind: the op row, in at the idempotence gate before the
    // control arm ever ran.
    assert_eq!(
        ops_at(dbb.conn(), &META_STREAM, &a_id, seq),
        1,
        "the refusal is the hint table's, not the delivery's: the op row is in"
    );

    // And the cursor counted it, which is the claim
    // `upsert_sync_cursor`'s doc makes about this site.
    assert_eq!(
        cursor_for(&dbb, &META_STREAM, &a_id),
        seq,
        "`upsert_sync_cursor` runs at step g with that row in the log, so the \
         contiguous prefix covers it"
    );
}

/// The apply path reads the **read bound**, and what that read decides is
/// keys — not whether the delivery applies.
///
/// This walks the trace `upsert_sync_cursor`'s doc describes, as a delivery
/// and end to end: `apply_remote_all` dispatches the control op into
/// `apply_control_op`, whose `DeviceCertPublish` arm calls
/// `backfill_key_envelopes`, which asks `is_read_bounded` over
/// `device_read_bounds` and returns before it seals anything.
///
/// It is **not** the revocation register, and since ADR-0041 the difference
/// is the point: the register is a fold that can hand a device back, and
/// `device_read_bounds` keeps a certified one. Revoking writes both, so the
/// premise here is set by the ordinary command and then asserted on the
/// table the gate actually reads.
///
/// Nothing else in this suite walks that trace. Every other
/// `InnerOp::DeviceCertPublish` here — the `trust` and `trust_at` helpers,
/// and the impostor-cert cases — calls `apply_control_op` directly inside a
/// test transaction, so there is no envelope, no op row and no cursor, and
/// the reachability of the bound check from a remote delivery goes unpinned.
/// That gap is how the doc came to describe this trace against the wrong
/// table for a whole release: nothing disagreed.
///
/// So this holds all three halves. The read-bounded device is sealed nothing,
/// which is the security-relevant end of ADR-0034's "revocation bounds
/// reads". Its cert is recorded and this replica's cursor covers the delivery
/// anyway, which is the "not writes" end. And an unbounded device taking the
/// identical route **is** sealed, without which the first assertion would
/// hold for the trivial reason that this vault had no key to seal to anyone.
#[test]
fn a_revoked_device_cert_through_apply_remote_seals_no_keys() {
    // Real random Stream keys, so `stream_keys` holds rows and
    // `Keychain::held_epochs_tx` — the set a backfill walks — is not empty.
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    let c_id = ec.keychain.device_id();

    // A mints the vault-meta and Inbox epochs, so a backfill has keys to seal.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before either peer was known".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(
        held.len() >= 2,
        "the capture minted at least the meta and Inbox epochs, or there is nothing \
         a backfill could seal and the assertions below would hold vacuously"
    );

    // A revokes B. B is a device A has never seen, which is the ordinary case
    // rather than a contrived one: a cert and a revocation naming it travel in
    // the same stream with no ordering guarantee between them.
    revoke(&ea, &mut dba, &ea, b_id, T0);
    assert!(
        ea.is_read_bounded(dba.conn(), &b_id).unwrap(),
        "the premise, on the table the gate reads: revoking B bounded its reads \
         before the delivery below"
    );

    // The meta epoch A is on. A peer seals under the key a `key_envelope`
    // handed it; reading the same bytes out of A is that key, and keeps the
    // delivery about the register rather than about key distribution.
    let (meta_epoch, meta_key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("A's live vault-meta epoch");

    // Seal a `device_cert_publish` the way a peer does and hand the bytes to
    // A. The op is self-authenticating, so A needs no prior row for the
    // sender — which is the whole point of the arm this reaches.
    let publish = |e: &Engine, db: &mut Db| -> (u64, Vec<u8>) {
        let inner = encode_inner_op(&InnerOp::DeviceCertPublish(e.keychain.cert_blob()))
            .expect("encode the cert publish");
        db.with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = e.next_seq_tx(tx, &META_STREAM)?;
            let env = e
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    e.hlc.send(),
                    &inner,
                    e.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch A holds");
            Ok((seq, env))
        })
        .unwrap()
    };

    let (b_seq, b_env) = publish(&eb, &mut dbb);
    let events = ea
        .apply_remote_all(&mut dba, &b_env)
        .expect("a revoked device's cert is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");

    // The bound was consulted, and it stopped the backfill.
    assert!(
        envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a read-bounded device republished its cert through a delivery and was handed \
         the keys the revocation had just rotated away"
    );

    // It did not stop the op. The cert is recorded and the cursor covers it —
    // step b found the sender like any other, and the early return inside
    // `backfill_key_envelopes` leaves both alone.
    let recorded: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE device_id = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        recorded, 1,
        "the backfill is refused, the delivery is not: the cert belongs in `devices` \
         whatever the bound says"
    );
    assert_eq!(
        cursor_for(&dba, &META_STREAM, &b_id),
        b_seq,
        "and the cursor advances over it like any other applied op"
    );

    // The control. An unrevoked device down the identical route is sealed, so
    // the emptiness above is the register's answer and not this vault having
    // nothing to give.
    let (_, c_env) = publish(&ec, &mut dbc);
    ea.apply_remote_all(&mut dba, &c_env)
        .expect("an unrevoked device's cert is a well-formed delivery too");
    let mut sealed = envelopes_to(&ea, &dba, &c_id);
    sealed.sort_unstable();
    let mut want = held;
    want.sort_unstable();
    assert_eq!(
        sealed, want,
        "the same delivery route seals every epoch A holds to an unbounded device, \
         which is what makes B's empty list the bound's doing"
    );
}

/// The same refusal, delivered with a gap below it: the cursor does not move.
///
/// `a_self_refused_revoke_still_advances_the_cursor` delivers the refused op
/// at seq 1 and asserts the cursor becomes 1. That is true, and it says
/// nothing about refusals — at seq 1 the op *is* the whole contiguous prefix,
/// so the assertion holds for every op that reaches the log. This is the case
/// that separates the two. B holds nothing from A, A's self-naming
/// `device_revoke` arrives stamped seq 2, and `ops_run_end` starts its run at
/// seq 1: with seq 1 absent the `ELSE ?3 - 1` arm returns 0, so the row
/// `upsert_sync_cursor` writes says 0 and the refused op sits outside it.
///
/// So "a refused op advances the cursor past it" is not a fact about
/// refusals. The control at the end delivers an **accepted** op through the
/// same gap and gets the same 0: the number is decided by the seqs below and
/// never by the answer the control op got. Without this, both the prose in
/// `upsert_sync_cursor` and the assertion in the test above read as promising
/// that a delivery moves the cursor, which is the shape of claim
/// [#253](https://github.com/justin13888/Sunrise/issues/253) was opened about.
#[test]
fn a_self_refused_revoke_out_of_order_leaves_the_cursor_short() {
    // The frame carrying seq 1 is the one that was dropped. What the receiver
    // acts on is the seq in the envelope, so the sender's own log is beside
    // the point and the op is sealed at 2 directly.
    const GAPPED_SEQ: u64 = 2;

    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    let a_id = ea.keychain.device_id();
    let c_id = ec.keychain.device_id();

    trust(&eb, &mut dbb, &ea);

    let inner = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
        revoked_device_id: a_id,
        reason_code: RevokeReason::Lost,
    }))
    .expect("encode the inner op");
    let env = dba
        .with_tx(|tx| -> rusqlite::Result<Vec<u8>> {
            let (epoch, key) = ea.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            Ok(ea
                .keychain
                .seal_op_at(
                    META_STREAM,
                    GAPPED_SEQ,
                    ea.hlc.send(),
                    &inner,
                    ea.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under A's own live meta epoch"))
        })
        .unwrap();

    let events = eb
        .apply_remote_all(&mut dbb, &env)
        .expect("a self-naming revoke with a gap below it is still a well-formed delivery");
    assert!(events.is_empty(), "a control op materializes no entity");

    // The delivery applied: the refusal is the register's, not the op's.
    assert_eq!(
        ops_at(dbb.conn(), &META_STREAM, &a_id, GAPPED_SEQ),
        1,
        "the op row is in the log, which is the premise the cursor claim is made from"
    );
    assert_eq!(
        revocation_row(&dbb, &a_id),
        None,
        "a device must not be able to write the register entry about itself"
    );

    // And the cursor says 0, not 2. `cursor_for` reports 0 for a missing row
    // too, so the row itself is read: one *was* written, and it says 0.
    assert_eq!(
        cursor_row(&dbb, &META_STREAM, &a_id),
        Some(0),
        "seq 1 never arrived, so the contiguous prefix from 1 is empty and the cursor \
         is written below the op that was just delivered"
    );

    // The control. An accepted op through the identical gap writes the same 0,
    // so the 0 above is the gap's doing and not the refusal's. A cert publish
    // is self-authenticating, so C needs no prior row here either.
    let cert = encode_inner_op(&InnerOp::DeviceCertPublish(ec.keychain.cert_blob()))
        .expect("encode the cert publish");
    let cert_env = dbc
        .with_tx(|tx| -> rusqlite::Result<Vec<u8>> {
            let (epoch, key) = ec.ensure_stream_epoch(tx, &META_STREAM, T0)?;
            Ok(ec
                .keychain
                .seal_op_at(
                    META_STREAM,
                    GAPPED_SEQ,
                    ec.hlc.send(),
                    &cert,
                    ec.rng.as_ref(),
                    epoch,
                    &key,
                )
                .expect("seal under C's own live meta epoch"))
        })
        .unwrap();
    eb.apply_remote_all(&mut dbb, &cert_env)
        .expect("an accepted op with a gap below it is a well-formed delivery too");
    assert_eq!(
        ops_at(dbb.conn(), &META_STREAM, &c_id, GAPPED_SEQ),
        1,
        "the control's op row is in the log as well"
    );
    assert_eq!(
        cursor_row(&dbb, &META_STREAM, &c_id),
        Some(0),
        "an accepted op through the same gap writes the same 0: what the cursor reports \
         is the run below the op, never the answer the op got"
    );
}

/// A backfill that fails is logged, not raised — and the delivery still lands.
///
/// `upsert_sync_cursor`'s doc says the cursor is reached on every answer the
/// register read can give, and names the failing backfill as one of them.
/// Nothing provoked one. The `DeviceCertPublish` arm swallows the `Err` into
/// `core.device.backfill_failed` deliberately, because the cert is valid and
/// belongs in `devices` whatever happens next, and that arm's own comment
/// says so — but a build that returned the error instead would fail the whole
/// delivery, roll the op row back, and leave the relay resending an op this
/// replica had already decided. Every other unit here goes down the success
/// path or the revoked early return, so nothing disagreed with the change.
///
/// The fault is a trigger on the one table `backfill_key_envelopes` writes,
/// which is what makes it a *storage* failure and not a rearranged input:
/// the arm reaches the backfill exactly as it does in production.
#[test]
fn a_failed_backfill_is_logged_and_the_cert_delivery_still_applies() {
    // Real random Stream keys, so `held_epochs_tx` is non-empty and the
    // backfill has something to try to record.
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let b_id = eb.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before the peer was known".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    let held = dba.with_tx(Keychain::held_epochs_tx).expect("held epochs");
    assert!(
        held.len() >= 2,
        "the capture minted at least the meta and Inbox epochs, or the backfill below \
         has nothing to fail at"
    );

    let (meta_epoch, meta_key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("A's live vault-meta epoch");

    // The fault. `record_envelope_recipient` is the last statement of each
    // backfill iteration, so the loop gets as far as sealing and emitting and
    // then fails on storage — the shape the log event exists for.
    dba.conn()
        .execute_batch(
            "CREATE TEMP TRIGGER no_recipient_rows
               BEFORE INSERT ON main.key_envelope_recipients
               BEGIN SELECT RAISE(ABORT, 'key_envelope_recipients is unavailable'); END;",
        )
        .expect("install the storage fault");

    let inner = encode_inner_op(&InnerOp::DeviceCertPublish(eb.keychain.cert_blob()))
        .expect("encode the cert publish");
    let (b_seq, b_env) = dbb
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = eb.next_seq_tx(tx, &META_STREAM)?;
            let env = eb
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    eb.hlc.send(),
                    &inner,
                    eb.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch A holds");
            Ok((seq, env))
        })
        .unwrap();

    // Captured, so the name of this unit is a claim it proves. Without it the
    // `tracing::warn!` in the `DeviceCertPublish` arm could be deleted and
    // every assertion below would still pass — the partial seal, the recorded
    // cert and the cursor are all true of a backfill that failed silently.
    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(ea.apply_remote_all(&mut dba, &b_env));
    });
    let events = delivered
        .expect("the capture ran the delivery")
        .expect("a backfill error is logged, not raised: the delivery is not failed by it");
    assert!(events.is_empty(), "a control op materializes no entity");
    assert!(
        logged.iter().any(|ev| ev == "core.device.backfill_failed"),
        "the arm swallows the `Err` into a log line, and the log line is the whole \
         of what the operator gets; got {logged:?}"
    );

    // The backfill did fail, and part-way: it emitted for the epoch it was on
    // and never reached the rest. Were this list complete, the trigger had not
    // fired and the test would be asserting nothing.
    let sealed = envelopes_to(&ea, &dba, &b_id);
    assert!(
        !sealed.is_empty() && sealed.len() < held.len(),
        "the backfill stopped at the first epoch it tried to record, which is the \
         partial state the error is logged about"
    );

    // And the delivery stands: the cert is recorded and the cursor covers it.
    let recorded: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE device_id = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        recorded, 1,
        "the cert belongs in `devices` whatever the backfill did"
    );
    assert_eq!(
        cursor_for(&dba, &META_STREAM, &b_id),
        b_seq,
        "and the cursor is reached on this answer like any other"
    );
}

/// A local emit that fails leaves the cursor exactly where it was, by either
/// of the two routes it can fail.
///
/// `ops_insert_at` seals, inserts into `ops`, enqueues the outbox row and only
/// then calls `upsert_sync_cursor`. Nothing held what happens when the first
/// of those does not land: the `Err` arm on `OpLog::insert` had no test at
/// all, and neither did the collision that arm cannot see.
///
/// **The collision.** `OpLog::insert` is `INSERT OR IGNORE`, so an op at a
/// `seq` this device has already written is dropped without a word and
/// without an error — the failure `emit_control_op`'s doc calls "the op would
/// be ignored, and its outbox row would then fail its foreign key, which is
/// exactly how this was found". The `Err` arm never runs. What turns the
/// silence into an error is `outbox.op_id REFERENCES ops (op_id)`, one table
/// away, so this half also pins that foreign key: drop it and a spent seq
/// becomes a silent no-op that still advertises a cursor.
///
/// **The storage failure.** A fault at the `ops` insert itself, which is the
/// arm that maps `OpLogError` and returns. Here the outbox is never reached
/// at all.
///
/// Both halves read **inside the transaction**, before the rollback `with_tx`
/// performs. Looking afterwards would pass on the rollback alone. And the
/// cursor is held twice over in each: `upsert_sync_cursor` is not reached,
/// and could not have moved the number if it were, because it recomputes the
/// prefix out of `ops` rather than taking it off the op being written — which
/// is precisely what the high-water mark it replaced did do.
#[test]
fn a_failed_local_emit_leaves_the_cursor_where_it_was() {
    let e = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let self_id = e.keychain.device_id();

    // One op through the same seam first, so there is a stored cursor to leave
    // alone rather than an absent row that would prove nothing, and a spent
    // seq for the first half to collide with.
    let cert = InnerOp::DeviceCertPublish(e.keychain.cert_blob());
    db.with_tx(|tx| e.emit_control_op(tx, &cert, T0, None))
        .expect("the first emit goes through");
    let spent = cursor_for(&db, &META_STREAM, &self_id);
    assert!(
        spent >= 1,
        "the emit above put at least one op in this device's own log"
    );
    let queued_before = outbox_rows(&db);
    let inner = encode_inner_op(&cert).expect("encode the cert publish");

    // --- the collision ---
    let collided = db.with_tx(|tx| -> rusqlite::Result<()> {
        let (epoch, key) = e.ensure_stream_epoch(tx, &META_STREAM, T0)?;
        let err = e
            .ops_insert_at(
                tx,
                &e.fresh_op_id(T0),
                &META_STREAM,
                spent,
                e.hlc.send(),
                &inner,
                cert.inner_kind(),
                cert.target_kind(),
                None,
                Some(T0),
                None,
                T0,
                &[],
                epoch,
                &key,
            )
            .expect_err("a spent seq does not reach the caller as a success");

        assert_eq!(
            ops_at(tx, &META_STREAM, &self_id, spent),
            1,
            "`INSERT OR IGNORE` kept the op already at this seq and dropped the new one"
        );
        assert_eq!(
            outbox_count(tx),
            queued_before,
            "and the foreign key rejected the outbox row for the op that was dropped, \
             which is the only thing that made the drop visible at all"
        );
        assert_eq!(
            cursor_in_tx(tx, &META_STREAM, &self_id),
            i64::try_from(spent).unwrap(),
            "so `upsert_sync_cursor` was never reached and the cursor still reports the \
             run the log actually holds"
        );
        Err(err)
    });
    assert!(
        collided.is_err(),
        "the collision reaches the caller rather than being swallowed"
    );

    // --- the storage failure ---
    let faulted = db.with_tx(|tx| -> rusqlite::Result<()> {
        // A fault at the one statement `OpLog::insert` runs, and at nothing
        // before it, so what fails is the insert and not the seal.
        tx.execute_batch(
            "CREATE TEMP TRIGGER no_op_rows
               BEFORE INSERT ON main.ops
               BEGIN SELECT RAISE(ABORT, 'ops is unavailable'); END;",
        )?;
        let err = e
            .emit_control_op(tx, &cert, T0, None)
            .expect_err("a failed op-log insert is returned, not swallowed");

        assert_eq!(
            outbox_count(tx),
            queued_before,
            "the `Err` arm returned before `Outbox::enqueue`, so this route does not even \
             reach the foreign key the other one leans on"
        );
        assert_eq!(
            cursor_in_tx(tx, &META_STREAM, &self_id),
            i64::try_from(spent).unwrap(),
            "and the cursor is untouched here too"
        );
        Err(err)
    });
    assert!(
        faulted.is_err(),
        "the storage failure reaches the caller as well"
    );

    // Both transactions rolled back, which is belt to the braces above.
    assert_eq!(
        cursor_for(&db, &META_STREAM, &self_id),
        spent,
        "and the rollback leaves the cursor where the code path already had it"
    );
    assert_eq!(outbox_rows(&db), queued_before, "the outbox likewise");
}

/// A stranger's cert through `apply_remote_all` is refused at step b, and the
/// refusal costs the cursor nothing because there is no op row yet.
///
/// `upsert_sync_cursor`'s doc says admission is settled at step b — by the
/// `devices` lookup, or for the `DeviceCertPublish` family by
/// `self_authenticating_signer` — and never by the revocation register. Only
/// the *accepting* half of that function was reached through a delivery:
/// `a_revoked_device_cert_through_apply_remote_seals_no_keys` walks an
/// unknown sender whose cert does verify. The refusing half was reached by
/// nothing. The impostor-cert cases next to this one call `apply_control_op`
/// directly, which is past the point this function is consulted at.
///
/// The sender here is a device this vault has never admitted, publishing
/// **another device's** cert. Both halves are deliberate. A sender already in
/// `devices` never reaches this function at all — step b's lookup answers
/// first — and a sender publishing its own genuine cert is admitted, which is
/// the half already covered. What is left is the impostor test: a well-formed
/// cert that names somebody other than the envelope's signer, which is the
/// rebinding `Command::TrustDevice` could not refuse.
///
/// The cursor half is the reason this unit sits here rather than beside the
/// other cert cases. A refusal at step b happens **before** the `with_tx`
/// block, so the op row is never inserted and `upsert_sync_cursor` is never
/// reached — unlike the refusals inside `apply_control_op`, which run after
/// the row is in. The doc's two kinds of refusal are only distinguishable by
/// what they leave behind, and this is the one that leaves nothing.
#[test]
fn a_stranger_cert_through_apply_remote_is_refused_and_writes_no_cursor() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    // Neither of these is in A's `devices`, so the envelope's sender falls
    // through step b's lookup into `self_authenticating_signer`.
    let stranger = engine_seeded(ROOT, [9u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let victim = engine_seeded(ROOT, [8u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbs = db_root(ROOT);
    let s_id = stranger.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "written before the stranger knocked".into(),
            ..Default::default()
        }),
    )
    .unwrap();

    // The meta key A holds, handed to the stranger the way a leaked or
    // retained key reaches one. This is what gets the delivery past step d and
    // into the cert check, which is the whole subject.
    let (meta_epoch, meta_key) = dba
        .with_tx(|tx| ea.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("A's live vault-meta epoch");

    // The cert of a *third* device, published under the stranger's own
    // envelope. Genuine bytes, wrong signer.
    let inner = encode_inner_op(&InnerOp::DeviceCertPublish(victim.keychain.cert_blob()))
        .expect("encode the cert publish");
    let (s_seq, s_env) = dbs
        .with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = stranger.next_seq_tx(tx, &META_STREAM)?;
            let env = stranger
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    stranger.hlc.send(),
                    &inner,
                    stranger.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch A holds");
            Ok((seq, env))
        })
        .unwrap();

    let err = ea
        .apply_remote_all(&mut dba, &s_env)
        .expect_err("a cert naming another device is refused");
    assert!(
        matches!(&err, EngineError::RemoteOpInvalid(m)
            if m.contains("published cert names another device")),
        "refused by the impostor test in `self_authenticating_signer`, which is the \
         half no delivery reached before; got {err:?}"
    );

    // Nothing was admitted.
    let recorded: i64 = dba
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE device_id = ?",
            params![&s_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 0, "and the stranger is not in `devices`");

    // And the cursor was never reached: this refusal is the kind that runs
    // before the op row exists, so there is no row for the prefix to count and
    // no cursor row at all.
    assert_eq!(
        ops_at(dba.conn(), &META_STREAM, &s_id, s_seq),
        0,
        "a step-b refusal returns before the `with_tx` block, so no op row was \
         inserted"
    );
    assert_eq!(
        cursor_row(&dba, &META_STREAM, &s_id),
        None,
        "and `upsert_sync_cursor` was never reached, so there is no row — which is \
         what tells this refusal apart from the ones inside `apply_control_op`"
    );
}

/// What the cursor means across a re-fold: nothing the fold does reaches it.
///
/// Since ADR-0041 the revocation register is derived, and it **shrinks**:
/// applying one `device_revoke` can unwind another already believed, and
/// `core.device.revocation_unwound` is a routine event rather than an alarm.
/// `upsert_sync_cursor`'s doc asserts the cursor is a fact about `ops` and
/// never about any judgement passed on one, so the re-fold is the sharpest
/// case that claim has: a delivery that *retracts* a decision this replica
/// had already made.
///
/// Nothing pinned it. Every unwind unit in this suite drives
/// `apply_control_op` directly through the `revoke` helper, so there is no
/// envelope, no op row and no cursor to observe; every cursor unit here
/// delivers something the fold does not disturb.
///
/// So this delivers both revocations as remote ops and holds three things at
/// once. The register did unwind — C is off the list and the event fired.
/// The read bound did not, because a ledger row still names C: the
/// asymmetry `Engine::is_read_bounded` exists for. And both cursors stand
/// exactly where the op rows put them: the fold rewrote `device_revocations`
/// and touched neither `ops` nor `sync_cursors`.
#[test]
fn a_refold_that_unwinds_a_revocation_moves_no_cursor() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_seeded(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let er = engine_seeded(ROOT, [4u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbr = db_root(ROOT);
    let (a_id, b_id, c_id) = (
        ea.keychain.device_id(),
        eb.keychain.device_id(),
        ec.keychain.device_id(),
    );

    // R has admitted both senders, so step b finds each row and the deliveries
    // are ordinary ones.
    trust(&er, &mut dbr, &ea);
    trust(&er, &mut dbr, &eb);

    // The meta epoch R is on; both peers seal under the key R holds.
    let (meta_epoch, meta_key) = dbr
        .with_tx(|tx| er.ensure_stream_epoch(tx, &META_STREAM, T0))
        .expect("R's live vault-meta epoch");

    let revoke_env = |e: &Engine, db: &mut Db, target: [u8; 16]| -> (u64, Vec<u8>) {
        let inner = encode_inner_op(&InnerOp::DeviceRevoke(DeviceRevokePayload {
            revoked_device_id: target,
            reason_code: RevokeReason::Lost,
        }))
        .expect("encode the revoke");
        db.with_tx(|tx| -> rusqlite::Result<(u64, Vec<u8>)> {
            let seq = e.next_seq_tx(tx, &META_STREAM)?;
            let env = e
                .keychain
                .seal_op_at(
                    META_STREAM,
                    seq,
                    e.hlc.send(),
                    &inner,
                    e.rng.as_ref(),
                    meta_epoch,
                    &meta_key,
                )
                .expect("seal under the meta epoch R holds");
            Ok((seq, env))
        })
        .unwrap()
    };

    // A revokes C, delivered. R believes it.
    let (a_seq, a_env) = revoke_env(&ea, &mut dba, c_id);
    er.apply_remote_all(&mut dbr, &a_env)
        .expect("a revoke is a well-formed delivery");
    assert!(
        er.is_revoked(dbr.conn(), &c_id).unwrap(),
        "the premise: R believes C is revoked before the second delivery"
    );
    assert_eq!(
        cursor_for(&dbr, &META_STREAM, &a_id),
        a_seq,
        "and the cursor covers A's op"
    );

    // Then B revokes A. The fold discounts every row A wrote, so C comes back
    // off the register — the retraction this unit is about.
    let (b_seq, b_env) = revoke_env(&eb, &mut dbb, a_id);
    let mut delivered = None;
    let logged = events_emitted_by(|| {
        delivered = Some(er.apply_remote_all(&mut dbr, &b_env));
    });
    delivered
        .expect("the capture ran the delivery")
        .expect("revoking the revoker is a well-formed delivery too");

    assert!(
        er.is_revoked(dbr.conn(), &a_id).unwrap(),
        "B's revocation of A stands"
    );
    assert_eq!(
        revocation_row(&dbr, &c_id),
        None,
        "and A's revocation of C was unwound by the fold, which is the premise the \
         rest of this unit needs"
    );
    assert!(
        logged
            .iter()
            .any(|ev| ev == "core.device.revocation_unwound"),
        "a device leaving the revoked list is announced; got {logged:?}"
    );

    // The read bound did not unwind. It is the monotone half, which is why the
    // four key-distribution sites ask it and not the register.
    assert!(
        er.is_read_bounded(dbr.conn(), &c_id).unwrap(),
        "C stays read-bounded however the register folds: `device_read_bounds` has \
         no `DELETE`"
    );

    // And neither cursor moved with the register. This is the answer the doc
    // owed a reader: the cursor is the contiguous prefix of `ops`, the fold
    // rewrites `device_revocations`, and the two never meet.
    assert_eq!(
        cursor_for(&dbr, &META_STREAM, &a_id),
        a_seq,
        "A's op is still in the log and still counted, though the decision it \
         carried has been retracted"
    );
    assert_eq!(
        cursor_for(&dbr, &META_STREAM, &b_id),
        b_seq,
        "and B's op is counted like any other applied op"
    );
}
