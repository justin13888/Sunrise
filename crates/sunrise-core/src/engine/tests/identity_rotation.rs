//! Identity transitions: the verification rule and the rotation chain.

use super::*;

// ---- the verification rule (ADR-0032, #105) ----

/// Build a complete, valid `identity_transition` from `emitter` onto a
/// fresh successor, with `roster` as the surviving devices.
///
/// Assembled by hand rather than through a command, because the command is
/// the next commit's. Every byte of it is what a real emitter produces —
/// the roster is signed by the successor, the shares are real HPKE seals,
/// and the two signatures are taken by the keychain over the real digests —
/// so a test that applies one is exercising the production path and not a
/// fixture.
///
/// # `roster` must name `emitter` for the emitter to adopt
///
/// `roster` is a free parameter here and is **not** free in production:
/// [`Engine::rotate_identity`] pushes this device into the survivor set
/// unconditionally, right after the `devices` query that deliberately skips
/// it, so a real emitter is always in its own roster. Since `#105`,
/// `recompute_identity_head` takes the adopting device's cert *from* the
/// roster rather than re-issuing it — most devices hold no `ID_S_priv` and
/// could not sign one — so an emitter left out of `roster` opens its share,
/// finds no cert to adopt under, and keeps its old `identity_id`.
///
/// That matters to any caller that rotates more than once, because
/// `from_identity_id` below is `emitter.keychain.identity_id()`: an emitter
/// that never adopts emits every later transition from the *same*
/// predecessor, so what accumulates is a fan of siblings off one identity
/// rather than a chain — and the fan hits
/// [`MAX_SIBLINGS_PER_PREDECESSOR`] at ingest on the seventeenth.
/// A caller building a chain must pass `emitter` in `roster`. A caller
/// deliberately testing a losing branch, a rejected payload or a single
/// transition need not, and several below do not.
pub(super) fn build_transition(
    emitter: &Engine,
    roster: &[&Engine],
    carry: bool,
    now_ms: u64,
) -> (InnerOp, [u8; 16]) {
    let successor = emitter.keychain.mint_successor_identity(&SystemRng);
    let mut entries: Vec<RosterEntry> = Vec::new();
    let mut shares: Vec<KeyShare> = Vec::new();
    for e in roster {
        let cert = Keychain::issue_roster_cert(
            &successor,
            e.keychain.device_id(),
            e.keychain.device_signing_pub(),
            e.keychain.device_dh_pub(),
            "device",
            "test",
            now_ms,
        )
        .expect("roster cert");
        entries.push(RosterEntry { cert });
        shares.push(KeyShare {
            device_id: e.keychain.device_id(),
            hpke_ciphertext: emitter
                .keychain
                .seal_successor_device_share(
                    &successor,
                    &e.keychain.device_dh_pub(),
                    &e.keychain.device_id(),
                    &SystemRng,
                )
                .expect("device share"),
        });
    }
    let identity_share = carry.then(|| {
        serde_bytes::ByteBuf::from(
            emitter
                .keychain
                .seal_successor_carry_share(&successor, &SystemRng)
                .expect("carry share"),
        )
    });

    let certs: Vec<&[u8]> = entries.iter().map(|e| e.cert.as_slice()).collect();
    let share_refs: Vec<([u8; 16], &[u8])> = shares
        .iter()
        .map(|s| (s.device_id, s.hpke_ciphertext.as_slice()))
        .collect();
    let body = sunrise_crypto::IdentityTransitionBody {
        from_identity_id: emitter.keychain.identity_id(),
        to_identity_id: successor.identity_id(),
        to_id_s_pub: successor.id_s_pub(),
        to_id_d_pub: successor.id_d_pub(),
        roster_digest: sunrise_crypto::roster_digest(&certs).expect("roster digest"),
        shares_digest: sunrise_crypto::shares_digest(
            &share_refs,
            identity_share.as_ref().map(|b| b.as_slice()),
        )
        .expect("shares digest"),
    };
    let sigs = emitter
        .keychain
        .sign_transition(&body, &successor)
        .expect("sign");
    let to = successor.identity_id();
    (
        InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
            from_identity_id: body.from_identity_id,
            to_identity_id: body.to_identity_id,
            to_id_s_pub: body.to_id_s_pub,
            to_id_d_pub: body.to_id_d_pub,
            roster: entries,
            device_shares: shares,
            identity_share,
            prev_sig: sigs.prev_sig,
            next_sig: sigs.next_sig,
        })),
        to,
    )
}

/// Apply `inner` to `receiver` as `sender`, at a chosen meta epoch.
pub(super) fn apply_control_at(
    receiver: &Engine,
    db: &mut Db,
    inner: &InnerOp,
    sender: &[u8; 16],
    hlc: Hlc,
    meta_epoch: u32,
) {
    db.with_tx(|tx| {
        receiver
            .apply_control_op(tx, inner, sender, hlc, hlc.physical_ms, meta_epoch)
            .map(|_| ())
    })
    .unwrap();
}

/// A valid transition moves the head, and the emitter adopts it.
#[test]
fn an_applied_transition_moves_the_head() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let before = ea.current_identity(dba.conn()).unwrap();

    let (inner, to) = build_transition(&ea, &[&ea, &eb], true, T0);
    apply_control_at(
        &ea,
        &mut dba,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let head = ea.current_identity(dba.conn()).unwrap();
    assert_eq!(head.identity_id, to);
    assert_ne!(head.identity_id, before.identity_id);
    assert_eq!(
        ea.chain_identities(dba.conn()).unwrap(),
        vec![
            (before.identity_id, before.id_s_pub),
            (head.identity_id, head.id_s_pub)
        ],
        "the chain is genesis then successor, in that order"
    );
    // The emitter holds the outgoing ID_D_priv, so it opens the carry share
    // and keeps the account's unwrapping key across the rotation.
    assert_eq!(ea.keychain.identity_id(), to);
    assert!(ea.keychain.holds_only_copy_of_identity_key());
}

/// Re-delivery is a no-op: the row is keyed on `to_identity_id`.
#[test]
fn applying_the_same_transition_twice_changes_nothing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let (inner, to) = build_transition(&ea, &[&eb], true, T0);
    let a_id = ea.keychain.device_id();
    apply_control_at(&ea, &mut dba, &inner, &a_id, Hlc::at(T0), 1);
    apply_control_at(&ea, &mut dba, &inner, &a_id, Hlc::at(T0 + 5), 1);
    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(ea.current_identity(dba.conn()).unwrap().identity_id, to);
}

/// A device left out of the roster does not adopt, and reads itself as no
/// longer speaking for the account. This is the mechanism, not a failure.
#[test]
fn a_device_left_out_of_the_roster_does_not_adopt() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    // A rotates, and B is not in the roster.
    let (inner, to) = build_transition(&ea, &[], false, T0);
    apply_control_at(
        &eb,
        &mut dbb,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        to,
        "B agrees who the account now is"
    );
    assert_ne!(
        eb.keychain.identity_id(),
        to,
        "...and knows it is not itself: no share, no adoption"
    );
}

/// Structural rejections are logged and dropped, never returned, and leave
/// the head where it was. Each of the four is a different way to be
/// malformed and none of them is a delivery failure.
#[test]
fn a_malformed_transition_is_dropped_and_moves_nothing() {
    /// One named way to malform a transition. Declared before the first
    /// statement, because `clippy::items_after_statements` is denied.
    type Case<'a> = (
        &'static str,
        Box<dyn Fn(&mut IdentityTransitionPayload) + 'a>,
    );

    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    let a_id = ea.keychain.device_id();

    let mutate = |f: &dyn Fn(&mut IdentityTransitionPayload)| {
        let (inner, _) = build_transition(&ea, &[&eb], true, T0);
        let InnerOp::IdentityTransition(mut p) = inner else {
            unreachable!()
        };
        f(&mut p);
        InnerOp::IdentityTransition(p)
    };

    let cases: Vec<Case<'_>> = vec![
        // The id is a derivation of the key, so a chosen one is refused.
        (
            "id_derivation",
            Box::new(|p: &mut IdentityTransitionPayload| p.to_identity_id[0] ^= 0x01),
        ),
        // A roster entry that is not a cert.
        (
            "roster",
            Box::new(|p: &mut IdentityTransitionPayload| {
                p.roster[0].cert = b"not a cert".to_vec();
            }),
        ),
        // A share of the wrong width: the digest refuses to be computed
        // rather than hashing an ambiguous concatenation.
        (
            "shares",
            Box::new(|p: &mut IdentityTransitionPayload| {
                p.device_shares[0].hpke_ciphertext.truncate(60);
            }),
        ),
        // A cert that is a valid cert, but not one the successor issued.
        (
            "roster_binding",
            Box::new(|p: &mut IdentityTransitionPayload| {
                p.roster[0].cert = eb.keychain.cert_blob();
            }),
        ),
    ];

    for (name, f) in cases {
        let inner = mutate(f.as_ref());
        apply_control_at(&ea, &mut dba, &inner, &a_id, Hlc::at(T0), 1);
        assert_eq!(
            ea.current_identity(dba.conn()).unwrap().identity_id,
            head,
            "`{name}` must not move the head"
        );
        let n: i64 = dba
            .conn()
            .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "`{name}` must not become a link");
    }
}

/// A whole, genuinely signed transition belonging to **another account**.
///
/// Built from `sunrise_crypto` directly rather than from a keychain, because
/// every engine a test can construct shares this vault root and therefore this
/// account's identity — a stranger needs a predecessor key no keychain here
/// holds. The shares are the right width and nothing else: the apply path
/// recomputes their digest and never opens them, and what this fixture is for
/// is the path, not the payload.
fn stranger_transition(for_device: &Engine, now_ms: u64) -> InnerOp {
    use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};
    use sunrise_crypto::{DeviceCertInner, DEVICE_SHARE_LEN};

    let from = IdentitySigningKeyPair::from_secret_bytes(&[0x5e; 32]);
    let to = IdentitySigningKeyPair::from_secret_bytes(&[0x7a; 32]);
    let to_dh = IdentityDhKeyPair::from_secret_bytes([0x7b; 32]);
    let to_identity_id = sunrise_crypto::identity_id_from_pub(&to.public_bytes());

    let cert = DeviceCert::issue(
        DeviceCertInner {
            v: 1,
            device_id: for_device.keychain.device_id(),
            d_s_pub: for_device.keychain.device_signing_pub(),
            d_d_pub: for_device.keychain.device_dh_pub(),
            identity_id: to_identity_id,
            created_at_ms: now_ms,
            nickname: "stranger".into(),
            platform: "test".into(),
        },
        &to,
    )
    .expect("issue the stranger's roster cert")
    .to_cbor()
    .expect("cbor");

    let share = vec![0x11u8; DEVICE_SHARE_LEN];
    let body = sunrise_crypto::IdentityTransitionBody {
        from_identity_id: sunrise_crypto::identity_id_from_pub(&from.public_bytes()),
        to_identity_id,
        to_id_s_pub: to.public_bytes(),
        to_id_d_pub: to_dh.public_bytes(),
        roster_digest: sunrise_crypto::roster_digest(std::slice::from_ref(&cert))
            .expect("roster digest"),
        shares_digest: sunrise_crypto::shares_digest(
            &[(for_device.keychain.device_id(), share.as_slice())],
            None,
        )
        .expect("shares digest"),
    };
    let sigs = sunrise_crypto::sign_identity_transition(&body, &from, &to).expect("sign");
    InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
        from_identity_id: body.from_identity_id,
        to_identity_id: body.to_identity_id,
        to_id_s_pub: body.to_id_s_pub,
        to_id_d_pub: body.to_id_d_pub,
        roster: vec![RosterEntry { cert }],
        device_shares: vec![KeyShare {
            device_id: for_device.keychain.device_id(),
            hpke_ciphertext: share,
        }],
        identity_share: None,
        prev_sig: sigs.prev_sig,
        next_sig: sigs.next_sig,
    }))
}

/// A transition signed by somebody who is not the predecessor is stored —
/// applying is unconditional — and is never a link, because the fold checks
/// `prev_sig` under the identity it actually claims to succeed.
#[test]
fn a_transition_from_a_stranger_is_stored_and_never_folded() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    let inner = stranger_transition(&ea, T0);
    apply_control_at(
        &ea,
        &mut dba,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1, "it is structurally well formed, so it is stored");
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "and it extends nothing: it does not succeed this account's head"
    );
}

/// **A body edited after it was signed never reaches the table.**
///
/// The row is keyed on `to_identity_id` and written with `INSERT OR IGNORE`, so
/// whichever copy of a transition arrives first owns that key for good. Before
/// the successor signature was checked here, anyone who saw an honest
/// transition could re-publish it with the roster or the shares substituted and
/// take the key: the forged row never verifies at fold time, so it is never
/// *believed* — but it is never *replaced* either, and the honest rotation can
/// then never be recorded on that replica. A revoked device could suppress its
/// own removal from every peer's view that way.
///
/// `next_sig` is checkable without knowing the predecessor, which is what makes
/// the check possible this early. Here the tamper is on `from_identity_id`, so
/// the same `body_hash` no longer exists and neither signature covers what
/// arrived.
#[test]
fn a_transition_edited_after_signing_is_refused_rather_than_stored() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let (inner, _) = build_transition(&ea, &[&eb], true, T0);
    let InnerOp::IdentityTransition(mut p) = inner else {
        unreachable!()
    };
    p.from_identity_id = [0x5e; 16];
    apply_control_at(
        &ea,
        &mut dba,
        &InnerOp::IdentityTransition(p),
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0, "an altered body does not get to occupy the key");
}

/// The same, with the tamper on the part an attacker actually wants to change.
///
/// Substituting the roster is the whole point of taking the key: it is what
/// decides who lands on the successor identity. The recomputed digest then
/// differs from the signed one, so the body differs, so `next_sig` does not
/// verify — without needing to know the predecessor at all.
#[test]
fn a_transition_whose_roster_was_substituted_is_refused_rather_than_stored() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);

    let (inner, _) = build_transition(&ea, &[&ea, &eb], true, T0);
    let InnerOp::IdentityTransition(mut p) = inner else {
        unreachable!()
    };
    assert_eq!(p.roster.len(), 2, "the honest roster carries both devices");
    p.roster.truncate(1);
    apply_control_at(
        &ea,
        &mut dba,
        &InnerOp::IdentityTransition(p),
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );

    let n: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 0, "a roster the successor did not sign is not stored");
}

/// A vault that has taken no transition has a one-link chain, and the head
/// is the identity it has always had. The base case every other assertion
/// below is a departure from.
#[test]
fn an_unrotated_account_has_a_one_link_chain() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let dba = db_root(ROOT);
    let head = ea.current_identity(dba.conn()).unwrap();
    assert_eq!(head.identity_id, ea.keychain.identity_id());
    assert_eq!(head.id_s_pub, ea.keychain.identity_signing_pub());
    assert_eq!(
        ea.chain_identities(dba.conn()).unwrap(),
        vec![(head.identity_id, head.id_s_pub)]
    );
}

/// The rule, stated without the machinery that will produce it: a device
/// row naming an identity that is not the head is admitted, keeps its cert,
/// and receives nothing.
///
/// The stale `identity_id` is written directly here rather than produced by
/// a rotation, and deliberately: this asserts the *guard*, which is the
/// half that has to exist before anything can emit a transition. The
/// end-to-end version — where the staleness comes from a real rotation the
/// device could not follow — is the flipped `#105` test.
#[test]
fn a_device_certified_under_a_non_head_identity_receives_nothing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);

    // B is a member and a recipient while its row names the head.
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert!(
        !envelopes_to(&ea, &dba, &b_id).is_empty(),
        "a member is a recipient, or this test proves nothing"
    );
    let before = envelopes_to(&ea, &dba, &b_id).len();

    // Its cert is untouched and its row stays; only the identity that
    // certified it is no longer the one in force.
    dba.conn()
        .execute(
            "UPDATE devices SET identity_id = ? WHERE device_id = ?",
            params![&[0x9fu8; 16][..], &b_id[..]],
        )
        .unwrap();
    assert!(
        !ea.is_revoked(dba.conn(), &b_id).unwrap(),
        "nothing revoked it; membership is the only thing that moved"
    );

    // No new epoch reaches it...
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert_eq!(
        envelopes_to(&ea, &dba, &b_id).len(),
        before,
        "the recipient query bounds every subsequent epoch, which is what a \
             one-shot check at admission could not do"
    );
}

/// Membership is **derived, never stored as a decision** — the property
/// that makes the rule convergent, asserted through the one path that
/// recomputes it.
///
/// Republishing a cert rewrites `devices.identity_id` from whichever chain
/// identity actually verified it. Here that is still the head, because
/// nothing has rotated, so the device becomes current again with no other
/// change: the stale row was a fact about a past publish and not a verdict
/// the account had recorded.
///
/// That is also precisely why the real fix works. In the rotated case the
/// same recomputation writes the *superseded* issuer, so the republish that
/// restores membership here restores nothing there — and the difference is
/// entirely in the cert, which is the one thing a departed device cannot
/// change.
#[test]
fn membership_is_recomputed_from_the_issuer_at_every_publish() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    dba.conn()
        .execute(
            "UPDATE devices SET identity_id = ? WHERE device_id = ?",
            params![&[0x9fu8; 16][..], &b_id[..]],
        )
        .unwrap();
    let stale = envelopes_to(&ea, &dba, &b_id).len();
    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert_eq!(envelopes_to(&ea, &dba, &b_id).len(), stale);

    // The cert is republished unchanged. It still verifies under the head,
    // so the row is rewritten to the head and the device is current again.
    trust_at(&ea, &mut dba, &eb, T0 + 1);
    let restored: Vec<u8> = dba
        .conn()
        .query_row(
            "SELECT identity_id FROM devices WHERE device_id = ?",
            params![&b_id[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        restored,
        head.to_vec(),
        "the column records which identity verified the cert, recomputed \
             each time rather than carried forward"
    );
    assert!(
        envelopes_to(&ea, &dba, &b_id).len() > stale,
        "and the backfill that runs behind the publish now reaches it"
    );
}

/// A user-requested rotation keeps everybody: every current device is in
/// the roster, adopts, and goes on receiving keys.
///
/// The counterpart to the revocation case, and the one that says the fix is
/// a *rotation* rather than a way to lock devices out. Nothing needs
/// re-pairing and nothing goes quiet.
#[test]
fn a_requested_rotation_keeps_every_current_device() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);
    let before_identity = ea.current_identity(dba.conn()).unwrap().identity_id;
    let before_envelopes = envelopes_to(&ea, &dba, &b_id).len();

    ea.apply(
        &mut dba,
        Command::RotateIdentity {
            keep_recovery_code: true,
        },
    )
    .unwrap();

    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    assert_ne!(head, before_identity, "the identity moved");
    assert_eq!(
        ea.keychain.identity_id(),
        head,
        "and the emitter adopted it"
    );

    // B's row was moved onto the successor by the roster, so it is current
    // and still a recipient.
    let QueryResult::Devices(rows) = ea.query(&dba, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    let b = rows.iter().find(|r| r.device_id == b_id).expect("listed");
    assert!(b.current, "a rotation that keeps everybody keeps everybody");
    assert!(!b.revoked);

    ea.apply(
        &mut dba,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, INBOX_STREAM_BYTES),
        },
    )
    .unwrap();
    assert!(
        envelopes_to(&ea, &dba, &b_id).len() > before_envelopes,
        "and it goes on receiving keys minted after the rotation"
    );
}

/// Revoking the device that holds `ID_D_priv` **must not** carry the
/// recovery code forward.
///
/// The carry share is sealed to the outgoing `ID_D_pub`, and the only
/// holder of the matching private half is the account's creator. Carrying
/// it while revoking that creator would seal the successor to the very key
/// the rotation exists to exclude: every check would pass and the excluded
/// device would hold the new identity. So the request is overridden, the
/// user's code stops working, and a client has to say so.
///
/// The `identity` row is written by hand because `Keychain::for_test*`
/// builds none; `minted_by_device_id` is the column that names the holder,
/// and it is the one this branch reads.
#[test]
fn revoking_the_recovery_key_holder_refuses_to_carry_the_code() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let b_id = eb.keychain.device_id();
    trust(&ea, &mut dba, &eb);

    let id = ea.keychain.identity_id();
    let pk = ea.keychain.identity_signing_pub();
    dba.conn()
        .execute(
            "INSERT INTO identity
                 (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped,
                  id_d_priv_wrapped, created_at_ms, minted_by_device_id,
                  genesis_identity_id, genesis_id_s_pub)
                 VALUES (1, ?1, ?2, X'00', X'00', X'00', 0, ?3, ?1, ?2)",
            params![&id[..], &pk[..], &b_id[..]],
        )
        .unwrap();

    // B holds ID_D_priv, and B is the device being revoked.
    let out = ea.rotate_identity(&mut dba, Some(b_id), true).unwrap();
    assert!(
        !out.carried_recovery_code,
        "carrying it would seal the successor to the key being excluded"
    );

    // The same rotation excluding somebody else does carry it.
    let out = ea
        .rotate_identity(&mut dba, Some([0x77; 16]), true)
        .unwrap();
    assert!(out.carried_recovery_code);
}

/// The same bypass, seen by a **peer** rather than by the device that did
/// the revoking.
///
/// The flipped test above runs on the revoker's own replica, where the
/// rotation and the attacker's cert land in one process. This is the case
/// that actually matters for convergence: B applies the revocation and the
/// rotation as ordinary remote ops, then meets the fresh cert. B has no
/// special knowledge — it did not choose the successor and cannot tell an
/// attacker's fresh id from an honest new pairing — and still gives it
/// nothing, because the only question it asks is which chain identity
/// signed the cert.
#[test]
fn a_peer_gives_nothing_to_a_fresh_id_certified_under_a_retired_identity() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec2 = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbb = db_root(ROOT);
    let c2_id = ec2.keychain.device_id();

    // B knows A. A rotates, keeping B, and B applies the transition as an
    // ordinary remote control op.
    trust(&eb, &mut dbb, &ea);
    let (inner, to) = build_transition(&ea, &[&eb], true, T0);
    apply_control_at(
        &eb,
        &mut dbb,
        &inner,
        &ea.keychain.device_id(),
        Hlc::at(T0),
        1,
    );
    assert_eq!(eb.current_identity(dbb.conn()).unwrap().identity_id, to);
    assert_eq!(
        eb.keychain.identity_id(),
        to,
        "B is in the roster, so it adopts"
    );

    // The attacker's fresh id, certified under the identity it kept.
    let before = envelopes_to(&eb, &dbb, &c2_id).len();
    trust_at(&eb, &mut dbb, &ec2, T0 + 1);
    assert_eq!(
        envelopes_to(&eb, &dbb, &c2_id).len(),
        before,
        "B has no way to recognise the attacker and does not need one"
    );
    let QueryResult::Devices(rows) = eb.query(&dbb, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    assert!(
        !rows
            .iter()
            .find(|r| r.device_id == c2_id)
            .expect("listed")
            .current
    );
}

/// Seal `inner` into a real envelope under a key the *attacker* minted.
///
/// The whole point of the epoch-forgery cases: an attacker cannot raise
/// `meta_epoch` by asserting a number, because the number names a key. It
/// can mint a key of its own and claim any epoch it likes — this builds
/// exactly that, with a real signature over real ciphertext, so the
/// receiver's refusal is a cryptographic one and not a parse failure.
fn forged_envelope_at(
    attacker: &Engine,
    attacker_db: &mut Db,
    inner: &InnerOp,
    mints: u32,
    hlc: Hlc,
) -> Vec<u8> {
    let mut minted = (0u32, StreamKey::from_bytes([0u8; 32]));
    for _ in 0..mints {
        minted = attacker_db
            .with_tx(|tx| {
                attacker
                    .keychain
                    .mint_epoch(tx, &META_STREAM, &SystemRng, T0)
            })
            .expect("the attacker mints its own epoch");
    }
    attacker
        .keychain
        .seal_op_at(
            META_STREAM,
            99,
            hlc,
            &encode_inner_op(inner).expect("encode"),
            &SystemRng,
            minted.0,
            &minted.1,
        )
        .expect("seal under the self-minted key")
}

/// A forged transition sealed under a **self-minted `e+1`** is refused, not
/// applied and not parked.
///
/// The revoked device can mint keys — nothing stops it doing arithmetic in
/// its own vault — so it can claim the epoch the honest rotation just
/// reached. What it cannot do is produce *the* key the survivors hold at
/// that epoch, because that one was random and was never sealed to it. So
/// the survivors have a non-empty key list at `e+1` and none of them opens
/// this envelope, which is the one case `apply_remote` treats as an error
/// rather than as a missing key.
///
/// That distinction is the whole test. "No key" parks, because the
/// `key_envelope` carrying it may simply be in flight; "keys, none of which
/// opens it" is ciphertext nobody in this account wrote.
#[test]
fn a_transition_forged_under_a_self_minted_next_epoch_is_refused() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    trust(&ea, &mut dba, &ec);
    trust(&ec, &mut dbc, &ea);
    let c_id = ec.keychain.device_id();

    // The revocation rotates the meta stream, so A now holds a *random*
    // key at the next epoch.
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Compromised,
        },
    )
    .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    let live = dba
        .with_tx(|tx| ea.keychain.current_epoch_tx(tx, &META_STREAM))
        .unwrap()
        .expect("the meta stream has an epoch");
    assert!(
        !ea.keychain.stream_keys_at(&META_STREAM, live).is_empty(),
        "the premise: the survivor holds a key at the epoch being forged"
    );

    // C forges a transition off the identity it kept and seals it under an
    // epoch it minted itself.
    //
    // **Twice**, and that is load-bearing. The vault-meta stream's genesis
    // epoch is *derived* — `meta_genesis_key`, from `ID_D_priv ‖ identity_id`,
    // which is what lets a recovered vault reach the epochs that predate it —
    // so the first mint on any vault holding `ID_D_priv` produces the key every
    // other such vault already has. Sealing there is not forging under a
    // self-minted key at all; it is using the shared one. The second mint is
    // the first that is genuinely C's own.
    let (inner, _) = build_transition(&ec, &[&ec], false, T0);
    let forged = forged_envelope_at(&ec, &mut dbc, &inner, 2, Hlc::at(T0 + 1));
    let err = ea.apply_remote_all(&mut dba, &forged).unwrap_err();
    assert!(
        matches!(&err, EngineError::RemoteOpInvalid(m)
                if m.contains("no key at this (stream, epoch) opens the envelope")),
        "expected a decrypt refusal, got {err:?}"
    );
    assert_eq!(deferred_rows(&dba), 0, "refused, not parked");
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "and the head did not move"
    );
}

/// **The vault-meta genesis epoch is shared by every holder of `ID_D_priv`,
/// and that is the weakest place to seal anything.**
///
/// `meta_genesis_key` derives epoch 1 of the vault-meta stream from
/// `ID_D_priv ‖ identity_id` rather than minting it at random. That is what
/// makes recovery work at all — a restored vault has the identity key and
/// nothing else, so a random genesis would leave every epoch that predates the
/// recovery unreadable — and its cost is that a *revoked* device which held
/// `ID_D_priv` keeps the ability to seal and open at that one epoch forever.
///
/// It does not get such a device anything, and this pins why: `meta_epoch` is
/// the first component of the fold's ordering key, and the genesis is the
/// lowest epoch there is. Anything sealed there sorts below every honest
/// rotation by construction. The envelope opens; the head does not move.
#[test]
fn an_op_sealed_at_the_derived_genesis_epoch_opens_but_outranks_nothing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    trust(&ea, &mut dba, &ec);
    trust(&ec, &mut dbc, &ea);
    let c_id = ec.keychain.device_id();

    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Compromised,
        },
    )
    .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    // One mint lands on the derived genesis, which A holds too.
    let (inner, _) = build_transition(&ec, &[&ec], false, T0);
    let at_genesis = forged_envelope_at(&ec, &mut dbc, &inner, 1, Hlc::at(T0 + 1));
    ea.apply_remote_all(&mut dba, &at_genesis)
        .expect("A can open what was sealed under the epoch both derive");

    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "the genesis epoch is the bottom of the ordering key, so it moves nothing"
    );
}

/// The same forgery at **`e+2`** parks instead of erroring, is never
/// applied, and ages out.
///
/// Parking is correct here and is not a weakness: from the receiver's side
/// "I hold no key at epoch 3" is indistinguishable from an honest op that
/// overtook its `key_envelope`, and refusing would hold the device's cursor
/// below that op until the relay replayed it, and lose it for good once the
/// relay's retention window (30 days or 256 MiB per channel) evicted the frame.
/// What matters is that parking is *inert*:
/// the op never applies, the head never moves, and the row is swept at
/// [`DEFERRED_TTL_MS`] rather than sitting there forever waiting for a key
/// that cannot exist, because no honest device ever minted that epoch.
#[test]
fn a_transition_forged_two_epochs_ahead_parks_and_is_never_applied() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_random_keys(ROOT, [1u8; 32], ca.clone());
    let eb = engine_random_keys(ROOT, [2u8; 32], cb.clone());
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let mut dbc = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);
    trust(&eb, &mut dbb, &ec);
    trust(&ec, &mut dbc, &ea);
    // B is the replica under attack, and it has to be a *replica* rather
    // than the emitter: the TTL sweep runs inside `drain_deferred`, which
    // only a remotely absorbed key reaches.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    let head = eb.current_identity(dbb.conn()).unwrap().identity_id;

    let (inner, to) = build_transition(&ec, &[&ec], false, T0);
    let forged = forged_envelope_at(&ec, &mut dbc, &inner, 2, Hlc::at(T0 + 1));
    assert!(eb.apply_remote_all(&mut dbb, &forged).unwrap().is_empty());
    assert_eq!(deferred_rows(&dbb), 1, "no key at that epoch, so it parks");
    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        head,
        "parked is not applied"
    );
    let n: i64 = dbb
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
            params![&to[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "and it never became a link");

    // A month later some *other* key reaches B — an ordinary new Stream —
    // and the drain it triggers sweeps by age. The forged row is swept
    // rather than released, because the epoch it claims was never minted by
    // anyone in this account and no envelope for it can ever arrive.
    set_clock(&ca, T0 + DEFERRED_TTL_MS + 1);
    set_clock(&cb, T0 + DEFERRED_TTL_MS + 1);
    let stream = ea
        .apply(
            &mut dba,
            Command::CreateStream(StreamDraft {
                name: "later".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "in the new stream".into(),
            stream_id: Some(stream),
            ..Default::default()
        }),
    )
    .unwrap();
    for env in key_envelope_envs(&dba) {
        let _ = eb.apply_remote_all(&mut dbb, &env);
    }
    assert_eq!(deferred_rows(&dbb), 0, "swept at the TTL, not lingering");
    assert_eq!(eb.current_identity(dbb.conn()).unwrap().identity_id, head);
}

/// **A device that was offline across the rotation comes back and catches
/// up, with no recovery code and no re-pairing.**
///
/// This is the half of #143 that decides whether any of it is usable. The
/// transition is sealed under the meta epoch the rotation *minted*, so a
/// device that missed that epoch cannot read the op that would tell it the
/// identity moved — which looks circular and is not, because the
/// `key_envelope` carrying the new meta epoch is itself sealed under the
/// **old** one. The device can always open that.
///
/// So the order is: park the transition, absorb the envelope, drain, apply,
/// and take the re-issued cert and the `ID_S_priv` share out of the roster.
/// Each step is asserted here, because the failure mode is a device that
/// silently never becomes current again and can only be fixed by re-pairing
/// it — which is exactly the outcome a rotation is supposed to avoid.
#[test]
fn a_device_offline_across_a_rotation_catches_up_without_re_pairing() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    let c_id = ec.keychain.device_id();

    // A knows B and C; B knows A. B then goes offline.
    trust(&ea, &mut dba, &eb);
    trust(&ea, &mut dba, &ec);
    trust(&eb, &mut dbb, &ea);
    // Something has to be written before the vault-meta stream has a key
    // to hand over at all.
    ea.apply(
        &mut dba,
        Command::CreateTask(TaskDraft {
            title: "before B went away".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    hand_over_key(&eb, &mut dbb, &ea, &mut dba, &META_STREAM);
    let before = eb.current_identity(dbb.conn()).unwrap().identity_id;

    // A revokes C while B is away. B is in the roster; C is not.
    ea.apply(
        &mut dba,
        Command::RevokeDevice {
            device_id: EntityRef::new(EntityKind::Device, c_id),
            reason: RevokeReason::Stolen,
        },
    )
    .unwrap();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;
    assert_ne!(
        head, before,
        "the premise: the identity moved while B was away"
    );

    // B comes back and meets the transition first. It is sealed under the
    // meta epoch the rotation minted, which B does not hold.
    let transition = env_for_kind(&dba, &head, "identity.transition");
    assert!(eb
        .apply_remote_all(&mut dbb, &transition)
        .unwrap()
        .is_empty());
    assert_eq!(deferred_rows(&dbb), 1, "parked, because the epoch is new");
    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        before,
        "and nothing has moved yet"
    );

    // Now the `key_envelope` that carries that epoch. It was sealed under
    // the OLD meta epoch, so B can open it — this is what breaks the
    // apparent circularity.
    let mut drained = false;
    for env in key_envelope_envs(&dba) {
        if eb.apply_remote_all(&mut dbb, &env).is_ok() {
            drained = true;
        }
    }
    assert!(drained, "B absorbed at least one envelope");
    assert_eq!(
        deferred_rows(&dbb),
        0,
        "and the drain released the transition"
    );

    // B is current again, on its own, with everything it needs.
    assert_eq!(
        eb.current_identity(dbb.conn()).unwrap().identity_id,
        head,
        "B agrees who the account is"
    );
    assert_eq!(
        eb.keychain.identity_id(),
        head,
        "B opened its share and adopted: no recovery code, no re-pairing"
    );
    DeviceCert::from_cbor(&eb.keychain.cert_blob())
        .unwrap()
        .verify_binding(&eb.keychain.identity_signing_pub(), &head)
        .expect("B holds a cert under the identity in force");

    // ...and C, which was excluded, is not current on B's replica either.
    let QueryResult::Devices(rows) = eb.query(&dbb, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    if let Some(c) = rows.iter().find(|r| r.device_id == c_id) {
        assert!(!c.current, "the excluded device is not current anywhere");
    }
}
