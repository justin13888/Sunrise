//! Identity transitions past the cap, and forged siblings of a predecessor.

use super::identity_rotation::{apply_control_at, build_transition};
use super::*;

/// A complete, valid transition whose roster and share list name `devices`
/// synthetic survivors.
///
/// [`build_transition`] can only name engines that exist, and the point of the
/// size cap is a payload far larger than any real account. This mints the
/// successor the same way `rotate_identity` does and issues one genuine cert
/// and one genuine HPKE share per synthetic device, so the only thing wrong
/// with the result is how many there are of them.
fn build_transition_naming(emitter: &Engine, devices: usize, now_ms: u64) -> InnerOp {
    use rand_core::SeedableRng;
    use sunrise_crypto::keys::{DeviceDhKeyPair, DeviceSigningKeyPair};

    let successor = emitter.keychain.mint_successor_identity(&SystemRng);
    // The device keys are throwaway: nothing verifies a signature by them here,
    // only that the HPKE seals target distinct real X25519 points and the certs
    // carry distinct real Ed25519 ones.
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xD06F);
    let mut entries: Vec<RosterEntry> = Vec::with_capacity(devices);
    let mut shares: Vec<KeyShare> = Vec::with_capacity(devices);
    for i in 0..devices {
        let d_s = DeviceSigningKeyPair::generate(&mut rng);
        let d_d = DeviceDhKeyPair::generate(&mut rng);
        let mut device_id = [0u8; 16];
        device_id[..8].copy_from_slice(&(i as u64).to_be_bytes());
        entries.push(RosterEntry {
            cert: Keychain::issue_roster_cert(
                &successor,
                device_id,
                d_s.public_bytes(),
                d_d.public_bytes(),
                "device",
                "test",
                now_ms,
            )
            .expect("roster cert"),
        });
        shares.push(KeyShare {
            device_id,
            hpke_ciphertext: emitter
                .keychain
                .seal_successor_device_share(
                    &successor,
                    &d_d.public_bytes(),
                    &device_id,
                    &SystemRng,
                )
                .expect("device share"),
        });
    }
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
        shares_digest: sunrise_crypto::shares_digest(&share_refs, None).expect("shares digest"),
    };
    let sigs = emitter
        .keychain
        .sign_transition(&body, &successor)
        .expect("sign");
    InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
        from_identity_id: body.from_identity_id,
        to_identity_id: body.to_identity_id,
        to_id_s_pub: body.to_id_s_pub,
        to_id_d_pub: body.to_id_d_pub,
        roster: entries,
        device_shares: shares,
        identity_share: None,
        prev_sig: sigs.prev_sig,
        next_sig: sigs.next_sig,
    }))
}

/// The state `MAX_TRANSITION_CHAIN = 64` could put an account into and never
/// let it out of.
///
/// The fold walked at most 64 links. Any holder of `ID_S_priv` -- which is
/// every paired device, and every device that ever was one -- could reach that
/// by rotating, and past it the head was pinned forever: every later transition
/// was a row the walk never reached, so no rotation took effect, so no
/// *revocation* took effect either, because a revocation is a rotation. Nothing
/// recovered from it and nothing reported it; the head was a real identity with
/// a real set of current devices under it.
///
/// Seventy links, which is past the old cap by six. Each one is a genuine
/// transition -- a minted successor, a roster signed by it, real HPKE shares,
/// both signatures taken by the keychain -- applied through the same
/// `apply_control_op` arm a peer's delivery reaches, so what is being folded is
/// what production writes.
///
/// The roster names **A as well as B**, because A is the emitter and
/// `rotate_identity` puts this device in its own survivor set unconditionally.
/// Without it A opens its share, finds no cert in the roster to adopt under,
/// and keeps its old `identity_id` -- so every later transition is emitted
/// from the *same* predecessor and what this builds is seventy siblings of
/// genesis rather than a chain of seventy links. That is not the shape the
/// assertions below are about, and it is not a shape production can emit;
/// see `build_transition` for the invariant.
///
/// The assertion that matters is the last one: after seventy rotations the
/// account can still rotate again, and the new head is the one it just moved
/// to. That is "cannot reach a state it cannot leave", stated as the thing a
/// frozen account would fail.
#[test]
fn a_chain_past_the_old_cap_still_folds_and_can_still_be_extended() {
    const LINKS: usize = 70;

    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let genesis = ea.current_identity(dba.conn()).unwrap().identity_id;

    let mut last = genesis;
    for i in 0..LINKS {
        let (inner, to) = build_transition(&ea, &[&ea, &eb], true, T0);
        apply_control_at(
            &ea,
            &mut dba,
            &inner,
            &a_id,
            Hlc::at(T0 + i as u64 + 1),
            u32::try_from(i).unwrap() + 1,
        );
        assert_eq!(
            ea.current_identity(dba.conn()).unwrap().identity_id,
            to,
            "rotation {i} did not move the head; the fold stopped short of it"
        );
        last = to;
    }

    let chain = ea.chain_identities(dba.conn()).unwrap();
    assert_eq!(
        chain.len(),
        LINKS + 1,
        "the fold walked {} of {} links",
        chain.len(),
        LINKS + 1
    );
    assert_eq!(chain.first().unwrap().0, genesis);
    assert_eq!(chain.last().unwrap().0, last);

    // The point of the whole test: the account is not frozen. One more
    // rotation, and it takes effect like the seventy before it.
    let (inner, to) = build_transition(&ea, &[&ea, &eb], true, T0);
    apply_control_at(
        &ea,
        &mut dba,
        &inner,
        &a_id,
        Hlc::at(T0 + LINKS as u64 + 1),
        u32::try_from(LINKS).unwrap() + 1,
    );
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        to,
        "an account past the old cap can no longer rotate: it is in a state it \
         cannot leave, which is the defect this test exists for"
    );
}

/// An oversized roster is refused, and refused *before* the verification it
/// would have paid for.
///
/// `roster` and `device_shares` are `Vec`s in a payload, so their length is
/// chosen by whoever wrote the op, and each roster entry costs a `DeviceCert`
/// decode plus an Ed25519 verification. Nothing bounded either, so one op could
/// buy arbitrary verification work on every peer in the account.
///
/// The oversized transition is **completely valid** apart from its size:
/// [`build_transition_naming`] mints the successor itself, so every roster
/// entry is a distinct cert signed by it, every share is a real HPKE seal to a
/// distinct key, both digests are recomputed over the real contents and both
/// signatures are genuine. Nothing but the length check can refuse it — which
/// is the only way to tell "refused for its size" apart from "refused because a
/// padded payload is malformed", and the only construction under which removing
/// the check turns this test red.
#[test]
fn a_transition_naming_more_devices_than_the_cap_is_refused() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    let oversized = build_transition_naming(&ea, MAX_ROSTER_ENTRIES + 1, T0);
    apply_control_at(&ea, &mut dba, &oversized, &a_id, Hlc::at(T0 + 1), 1);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "the oversized transition moved the head"
    );
    let rows: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0, "the oversized transition was stored");

    // And the cap is a cap rather than a refusal of everything: the same
    // transition at the cap is accepted. Without this the test above would pass
    // against a build that refused every transition.
    let (ok, to) = build_transition(&ea, &[&eb], true, T0);
    apply_control_at(&ea, &mut dba, &ok, &a_id, Hlc::at(T0 + 2), 1);
    assert_eq!(ea.current_identity(dba.conn()).unwrap().identity_id, to);
}

/// The fold is willing to look at every row ingest will store.
///
/// `apply_control_op` is the only writer of `identity_transitions`, and its
/// sibling cap lets a predecessor reach exactly
/// [`MAX_SIBLINGS_PER_PREDECESSOR`] rows. If the fold's `LIMIT` were smaller
/// than that, the rows past it would be ones the walk could never reach on a
/// replica that had already stored them — no rotation recorded there could
/// ever take effect, which is `MAX_TRANSITION_CHAIN = 64` all over again, one
/// level down.
///
/// So the load-bearing property of [`MAX_SIBLING_CANDIDATES`] is not its value
/// but this inequality, and this is the test that makes the constant causal:
/// lowering it below the ingest cap turns this red. Its *behaviour* — scanning
/// past a row that does not verify — is not exercised anywhere, because every
/// row stored under an established predecessor had `prev_sig` checked at
/// ingest and therefore verifies here too. See `MAX_SIBLING_CANDIDATES` for
/// the measurement and the one path that could store a row the fold must skip.
#[test]
fn the_fold_looks_at_every_row_ingest_will_store() {
    let ingest_cap = usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR)
        .expect("the sibling cap is a small positive number");
    assert!(
        MAX_SIBLING_CANDIDATES >= ingest_cap,
        "the fold verifies at most {MAX_SIBLING_CANDIDATES} candidates per link but ingest \
         will store {ingest_cap} rows under one predecessor, so {} of them are rows the walk \
         can never reach and the transitions they carry can never take effect",
        ingest_cap - MAX_SIBLING_CANDIDATES
    );
}

/// One predecessor may accumulate only so many successors, and a re-delivery
/// of a row already held is never what the cap turns away.
///
/// The fold verifies at most `MAX_SIBLING_CANDIDATES` rows per link, so a
/// seventeenth row under one predecessor would be one the walk could never
/// reach -- the same shape the chain cap had, one level down. Ingest holds the
/// two numbers equal so that cannot arise.
#[test]
fn a_predecessor_accumulates_only_as_many_successors_as_the_fold_will_verify() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();

    // Every one of these succeeds the *same* predecessor: `build_transition`
    // reads `emitter.keychain.identity_id()`, and that only moves when this
    // device *adopts*, which since #105 needs a cert for itself in the
    // transition's roster. The roster here is B alone, so A never adopts and
    // every call forks from genesis -- which is exactly the fan this test
    // wants, and is why the roster deliberately does not name A the way
    // `a_chain_past_the_old_cap_still_folds_and_can_still_be_extended` does.
    let mut built = Vec::new();
    for i in 0..MAX_SIBLINGS_PER_PREDECESSOR + 4 {
        let (inner, to) = build_transition(&ea, &[&eb], true, T0);
        built.push((inner, to, i));
    }
    // A fresh replica, so the emitter's own adoption does not move the
    // predecessor between deliveries.
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbc = db_root(ROOT);
    trust(&ec, &mut dbc, &ea);
    trust(&ec, &mut dbc, &eb);
    for (inner, _, i) in &built {
        apply_control_at(
            &ec,
            &mut dbc,
            inner,
            &a_id,
            Hlc::at(T0 + u64::try_from(*i).unwrap() + 1),
            1,
        );
    }

    let rows: i64 = dbc
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        rows, MAX_SIBLINGS_PER_PREDECESSOR,
        "a predecessor accumulated {rows} successors; the fold verifies at most {MAX_SIBLINGS_PER_PREDECESSOR}"
    );

    // Re-delivering a row already held is not a new sibling and must not be
    // turned away, or evict anything, at a full register -- the insert is OR
    // IGNORE and the row occupies no fresh place. The *last* one, because these
    // were delivered by ascending HLC and a full register now keeps the
    // greatest sixteen by the fold's order rather than the first sixteen to
    // arrive (#232): `built[0]` is the one the cap dropped, so re-delivering it
    // would assert nothing about re-delivery.
    let (last, last_to, last_i) = built.last().expect("twenty transitions were built");
    let held_before: i64 = dbc
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
            params![&last_to[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        held_before, 1,
        "the row about to be re-delivered is not held"
    );
    apply_control_at(
        &ec,
        &mut dbc,
        last,
        &a_id,
        Hlc::at(T0 + u64::try_from(*last_i).unwrap() + 1),
        1,
    );
    let after: i64 = dbc
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(after, rows, "a re-delivery changed the row count");
    let held_after: i64 = dbc
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
            params![&last_to[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        held_after, 1,
        "a re-delivery evicted the row it re-delivered"
    );
}

/// **[`SiblingRank`] and [`SIBLING_ORDER_DESC`] are the same order.**
///
/// Ingest decides in Rust, over a `derive(Ord)` whose field order is the
/// comparison; the fold decides in SQL, over a string. The whole safety
/// argument for [`MAX_SIBLING_CANDIDATES`] is that the row ingest discards is
/// one the fold would never have reached, and that argument is only true while
/// the two agree — a reordered field or a reworded `ORDER BY` would break it
/// with no other symptom than a stored row the walk cannot see.
///
/// Six rows off a predecessor this replica will never establish, differing in
/// one rank component each, sorted by SQLite and by `Ord` and compared. The
/// emitter and the HLC are free parameters of `apply_control_op`, so every
/// component but `to_identity_id` — which is minted inside the fixture — is
/// varied deliberately rather than incidentally.
#[test]
fn the_rank_type_and_the_sql_order_agree() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    // Never a link on this replica, so `prev_sig` is not checked and no row is
    // ever swept: the table keeps exactly what is put in it.
    let absent = [0x7Eu8; 16];

    let variants: [(u32, u64, u32, [u8; 16]); 6] = [
        (1, T0, 0, [0x11; 16]),
        (1, T0, 0, [0x22; 16]),
        (1, T0, 1, [0x11; 16]),
        (1, T0 + 1, 0, [0x11; 16]),
        (2, T0 - 5, 0, [0x00; 16]),
        (2, T0, 0, [0xFF; 16]),
    ];
    for (i, (meta_epoch, physical_ms, logical, emitter)) in variants.iter().enumerate() {
        let (inner, _) = forge_sibling(absent, 0x5A47 + u64::try_from(i).unwrap());
        apply_control_at(
            &ea,
            &mut dba,
            &inner,
            emitter,
            Hlc {
                physical_ms: *physical_ms,
                logical: *logical,
            },
            *meta_epoch,
        );
    }

    let by_sqlite: Vec<SiblingRank> = {
        let conn = dba.conn();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT meta_epoch, hlc_physical_ms, hlc_logical, emitter_device_id,
                        to_identity_id
                 FROM identity_transitions
                 WHERE from_identity_id = ?1
                 ORDER BY {SIBLING_ORDER_DESC}"
            ))
            .unwrap();
        let rows = stmt
            .query_map(params![&absent[..]], |r| {
                Ok(SiblingRank {
                    meta_epoch: r.get(0)?,
                    hlc_physical_ms: r.get(1)?,
                    hlc_logical: r.get(2)?,
                    emitter_device_id: r.get(3)?,
                    to_identity_id: r.get(4)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    };
    assert_eq!(
        by_sqlite.len(),
        variants.len(),
        "the fixture did not store one row per variant"
    );

    let mut by_ord = by_sqlite.clone();
    by_ord.sort();
    by_ord.reverse();
    assert_eq!(
        by_sqlite, by_ord,
        "SQLite's `ORDER BY {SIBLING_ORDER_DESC}` and `SiblingRank`'s derived `Ord` \
         disagree, so ingest can discard a row the fold would have read"
    );
}

/// A sibling of `from_identity_id` whose `prev_sig` is **arbitrary** — the row
/// a peer implementing the published wire format can write for a predecessor it
/// holds no key for.
///
/// Deliberately not built through [`build_transition`] or
/// `sunrise_crypto::sign_identity_transition`. Both take the *predecessor's*
/// keypair and refuse a body whose `from_identity_id` is not its derivation, so
/// neither can express the one payload that matters here. Everything else an
/// emitter needs is public: `docs/03-crypto/key-rotation.md` gives the two
/// signing inputs verbatim, and this reconstructs `next_sig`'s from that
/// document rather than from the code under test — so the fixture is not
/// self-consistent with the ingest path it is aimed at, and a change to the
/// domain string turns it red against the format document.
///
/// The roster and the share list are **empty**, which is what makes this the
/// cheapest row an adversary can place: no cert to issue, no HPKE seal to
/// compute, one Ed25519 signature over about two hundred bytes.
fn forge_sibling(from_identity_id: [u8; 16], seed: u64) -> (InnerOp, [u8; 16]) {
    use rand_core::SeedableRng;
    use sunrise_crypto::keys::{IdentityDhKeyPair, IdentitySigningKeyPair};

    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let to_s = IdentitySigningKeyPair::generate(&mut rng);
    let to_d = IdentityDhKeyPair::generate(&mut rng);
    let to_identity_id = sunrise_crypto::identity_id_from_pub(&to_s.public_bytes());
    let body = sunrise_crypto::IdentityTransitionBody {
        from_identity_id,
        to_identity_id,
        to_id_s_pub: to_s.public_bytes(),
        to_id_d_pub: to_d.public_bytes(),
        roster_digest: sunrise_crypto::roster_digest::<&[u8]>(&[]).expect("empty roster digest"),
        shares_digest: sunrise_crypto::shares_digest::<&[u8]>(&[], None)
            .expect("empty shares digest"),
    };
    // Chosen freely. Nothing the receiving replica can check covers it while
    // the predecessor is unknown, and `next_sig` below is taken *over* it, so
    // the payload is internally consistent whatever it says.
    let prev_sig = [0xA5u8; 64];
    let mut input = Vec::from(&b"sunrise.identity_transition.succ.v1"[..]);
    input.extend_from_slice(
        &sunrise_crypto::identity_transition::body_hash(&body).expect("body hash"),
    );
    input.extend_from_slice(&prev_sig);
    let next_sig = to_s.sign(&input);
    (
        InnerOp::IdentityTransition(Box::new(IdentityTransitionPayload {
            from_identity_id,
            to_identity_id,
            to_id_s_pub: body.to_id_s_pub,
            to_id_d_pub: body.to_id_d_pub,
            roster: Vec::new(),
            device_shares: Vec::new(),
            identity_share: None,
            prev_sig,
            next_sig,
        })),
        to_identity_id,
    )
}

/// A replica two links behind, its predecessor's places filled with forgeries,
/// and the honest successor that must still land.
///
/// Returns `(replica, db, forged_ids, honest_transition, honest_id, establishing
/// _transition, predecessor_id)`. The forged rows are sealed at meta epoch 1 and
/// the honest successor at 2, which is the shape `revoke_device` produces and
/// the only thing that separates them: the cut device holds no key above the
/// epoch it was cut at, so it cannot raise `meta_epoch` however it dates its
/// HLC (ADR-0037 §4, pinned by
/// `a_revocations_transition_is_sealed_above_the_epoch_the_cut_device_holds`).
struct SuppressionFixture {
    replica: Engine,
    db: Db,
    emitter: [u8; 16],
    forged: Vec<[u8; 16]>,
    establishing: InnerOp,
    predecessor: [u8; 16],
    honest: InnerOp,
    honest_id: [u8; 16],
}

/// Build [`SuppressionFixture`], asserting the fixture is the shape it claims.
///
/// The assertions are the point. `build_transition` reads
/// `emitter.keychain.identity_id()`, which only moves when the emitter
/// *adopts*, so a caller that forgets to put the emitter in its own roster gets
/// a fan of siblings off genesis and calls it a chain — the failure its own doc
/// comment warns about and one this suite has shipped before. So this checks
/// what the two transitions actually name rather than what they were meant to.
fn suppression_fixture() -> SuppressionFixture {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let genesis = ea.current_identity(dba.conn()).unwrap().identity_id;

    // G -> P, applied to the emitter so it adopts P and its next transition
    // succeeds P rather than G.
    let (establishing, predecessor) = build_transition(&ea, &[&ea, &eb], true, T0);
    apply_control_at(&ea, &mut dba, &establishing, &a_id, Hlc::at(T0 + 1), 1);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        predecessor,
        "the emitter did not adopt the predecessor, so the fixture is a fan and not a chain"
    );

    // P -> Q, the honest successor. On the revocation path this is the op that
    // makes a cut stick.
    let (honest, honest_id) = build_transition(&ea, &[&ea, &eb], true, T0);
    let InnerOp::IdentityTransition(p) = &honest else {
        unreachable!("build_transition returns an identity transition")
    };
    assert_eq!(
        p.from_identity_id, predecessor,
        "the honest successor does not name the predecessor the forgeries target"
    );
    assert_ne!(predecessor, genesis, "the predecessor is genesis");

    // A replica that has seen neither link. It knows both devices, so their
    // envelopes verify, and it is at genesis, so `P` is a predecessor it cannot
    // check a `prev_sig` against — the ordinary state of a replica catching up
    // on a chain it is receiving out of order.
    let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dbc = db_root(ROOT);
    trust(&ec, &mut dbc, &ea);
    trust(&ec, &mut dbc, &eb);
    assert_eq!(
        ec.current_identity(dbc.conn()).unwrap().identity_id,
        genesis,
        "the receiving replica already knows the predecessor"
    );

    let forged = (0..MAX_SIBLINGS_PER_PREDECESSOR)
        .map(|i| {
            let (inner, to) = forge_sibling(predecessor, 0x5EED + u64::try_from(i).unwrap());
            apply_control_at(
                &ec,
                &mut dbc,
                &inner,
                &a_id,
                Hlc::at(T0 + 100 + u64::try_from(i).unwrap()),
                1,
            );
            to
        })
        .collect::<Vec<_>>();
    let stored: i64 = dbc
        .conn()
        .query_row(
            "SELECT count(*) FROM identity_transitions WHERE from_identity_id = ?",
            params![&predecessor[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, MAX_SIBLINGS_PER_PREDECESSOR,
        "the forged siblings did not reach the table, so this fixture is not \
         reproducing the suppression at all"
    );

    SuppressionFixture {
        replica: ec,
        db: dbc,
        emitter: a_id,
        forged,
        establishing,
        predecessor,
        honest,
        honest_id,
    }
}

/// **Sixteen forged siblings of a predecessor this replica has not established
/// cannot turn the honest successor away.**
///
/// The `prev_sig` check at ingest runs only when the predecessor is already on
/// this replica's chain — it has to, because a replica must accept a chain out
/// of order and has no key to check against until it catches up. The sibling
/// cap below it counted *rows*, so every row admitted in that window, verified
/// or not, spent one of a predecessor's sixteen places. Sixteen of the cheapest
/// possible forgeries filled them, and the real successor was refused; the
/// refusal was permanent, because the op is recorded in `ops` and never
/// re-offered, and the account could then never adopt a successor — which, since
/// a revocation *is* a rotation, means it could never cut a device again either.
///
/// What replaces the count is a *rank*: the places are held by the sixteen
/// greatest rows in the fold's own order, and a seventeenth is admitted by
/// displacing the weakest. The honest successor is sealed at the meta epoch its
/// rotation minted, which the forger provably does not hold, so it outranks all
/// sixteen and takes a place whatever order the rows arrive in.
#[test]
fn forged_siblings_cannot_suppress_the_successor_of_an_unestablished_predecessor() {
    let mut f = suppression_fixture();

    // The honest successor, at the epoch its own rotation minted.
    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.honest,
        &f.emitter,
        Hlc::at(T0 + 2),
        2,
    );
    let admitted: i64 =
        f.db.conn()
            .query_row(
                "SELECT count(*) FROM identity_transitions WHERE to_identity_id = ?",
                params![&f.honest_id[..]],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        admitted, 1,
        "the honest successor was refused by a register full of rows nothing had verified; \
         this replica can never adopt it, and never cut a device again"
    );

    // And it is the account's identity once the missing link lands.
    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.establishing,
        &f.emitter,
        Hlc::at(T0 + 3),
        1,
    );
    assert_eq!(
        f.replica.current_identity(f.db.conn()).unwrap().identity_id,
        f.honest_id,
        "the chain did not reach the honest successor"
    );
}

/// **Establishing a predecessor deletes the rows stored under it that verify
/// against nothing, and frees their places.**
///
/// The converging half. A replica that admitted forgeries while it was behind
/// repairs itself the moment it catches up: `from_identity_id` determines the
/// key those rows have to verify under — the id is the derivation of that key —
/// so a row that fails once fails forever, and deleting it loses nothing the
/// fold could ever have used. Nothing survives the catch-up, and a successor
/// delivered *after* it is judged against an empty register.
///
/// Without this the sixteen dead rows would sit under the predecessor for the
/// life of the account, costing the fold sixteen Ed25519 verifications at that
/// link on every call rather than the one an honest link costs.
#[test]
fn establishing_a_predecessor_clears_the_siblings_that_verify_against_nothing() {
    let mut f = suppression_fixture();

    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.establishing,
        &f.emitter,
        Hlc::at(T0 + 3),
        1,
    );
    assert_eq!(
        f.replica.current_identity(f.db.conn()).unwrap().identity_id,
        f.predecessor,
        "the missing link did not establish the predecessor"
    );
    let survivors: Vec<Vec<u8>> = {
        let conn = f.db.conn();
        let mut stmt = conn
            .prepare("SELECT to_identity_id FROM identity_transitions WHERE from_identity_id = ?")
            .unwrap();
        let rows = stmt
            .query_map(params![&f.predecessor[..]], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        rows
    };
    for forged in &f.forged {
        assert!(
            !survivors.iter().any(|s| s.as_slice() == &forged[..]),
            "a row that verifies against the established predecessor's key under no \
             signature is still holding one of its places"
        );
    }
    assert!(
        survivors.is_empty(),
        "the predecessor kept {} rows none of which it signed",
        survivors.len()
    );

    // And the register it freed is a register the honest successor fits in.
    apply_control_at(
        &f.replica,
        &mut f.db,
        &f.honest,
        &f.emitter,
        Hlc::at(T0 + 4),
        2,
    );
    assert_eq!(
        f.replica.current_identity(f.db.conn()).unwrap().identity_id,
        f.honest_id,
        "the honest successor could not land after the catch-up that should have \
         freed every place the forgeries took"
    );
}

/// **What ingest keeps is what the fold reads, and neither depends on arrival
/// order.**
///
/// The replacement for a count. `MAX_SIBLING_CANDIDATES >= MAX_SIBLINGS_PER_PREDECESSOR`
/// says the fold is *willing* to look at every row ingest will store, which is
/// necessary and was never sufficient: while ingest kept the first sixteen rows
/// to arrive and the fold read the sixteen greatest, two replicas holding the
/// same ops could hold different rows and fold to different heads. They now use
/// one order, so the set ingest retains is exactly the set the fold reads, and
/// it is a function of the op set rather than of the delivery order.
///
/// Twenty valid transitions off one predecessor, delivered worst-rank-first and
/// then best-rank-first, must leave the same sixteen rows both times — the four
/// weakest gone in both.
#[test]
fn what_ingest_keeps_is_what_the_fold_reads_whatever_order_it_arrives_in() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();

    // A fan off genesis: the roster names B alone, so A never adopts and every
    // call succeeds the same predecessor. Ranked by HLC, which is the component
    // an emitter chooses.
    let offered = usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR).unwrap() + 4;
    let built: Vec<(InnerOp, [u8; 16])> = (0..offered)
        .map(|_| build_transition(&ea, &[&eb], true, T0))
        .collect();

    let retained = |order: Vec<usize>| -> BTreeSet<Vec<u8>> {
        let ec = engine_random_keys(ROOT, [3u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut dbc = db_root(ROOT);
        trust(&ec, &mut dbc, &ea);
        trust(&ec, &mut dbc, &eb);
        for i in order {
            apply_control_at(
                &ec,
                &mut dbc,
                &built[i].0,
                &a_id,
                Hlc::at(T0 + 1 + u64::try_from(i).unwrap()),
                1,
            );
        }
        let conn = dbc.conn();
        let mut stmt = conn
            .prepare("SELECT to_identity_id FROM identity_transitions")
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .unwrap();
        rows
    };

    let ascending = retained((0..offered).collect());
    let descending = retained((0..offered).rev().collect());
    assert_eq!(
        ascending.len(),
        usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR).unwrap(),
        "the predecessor kept a number of rows the fold's LIMIT does not match"
    );
    assert_eq!(
        ascending, descending,
        "two replicas holding the same twenty transitions kept different sixteen of \
         them, so which identity the account folds to depends on delivery order"
    );
    // The four the cap turned away are the four weakest, in both orders.
    let expected: BTreeSet<Vec<u8>> = built
        [offered - usize::try_from(MAX_SIBLINGS_PER_PREDECESSOR).unwrap()..]
        .iter()
        .map(|(_, to)| to.to_vec())
        .collect();
    assert_eq!(
        ascending, expected,
        "the rows kept are not the greatest sixteen in the order the fold reads them"
    );
}

/// A transition carrying another transition's signature pair occupies no row.
///
/// The fifth structural refusal, alongside the four in
/// `a_malformed_transition_is_dropped_and_moves_nothing`: `next_sig` covers
/// `body_hash || prev_sig`, so a pair lifted whole from a different transition
/// is internally consistent and still fits no other body. Without the check the
/// row is occupiable by anyone — `INSERT OR IGNORE` is keyed on
/// `to_identity_id`, so a forged copy published first would discard the honest
/// one for good — and with the sibling cap it would also consume one of the
/// predecessor's sixteen places.
#[test]
fn a_transition_carrying_another_transitions_signatures_is_not_stored() {
    let ea = engine_random_keys(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_random_keys(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let a_id = ea.keychain.device_id();
    let head = ea.current_identity(dba.conn()).unwrap().identity_id;

    let (inner, _) = build_transition(&ea, &[&eb], true, T0);
    let InnerOp::IdentityTransition(mut p) = inner else {
        unreachable!()
    };
    // Both signatures from a *different* transition of the same shape: the pair
    // is internally consistent and belongs to another body, which is the shape
    // a substitution attack takes.
    let (other, _) = build_transition(&ea, &[&eb], true, T0);
    let InnerOp::IdentityTransition(other) = other else {
        unreachable!()
    };
    p.prev_sig = other.prev_sig;
    p.next_sig = other.next_sig;
    let forged = InnerOp::IdentityTransition(p);

    apply_control_at(&ea, &mut dba, &forged, &a_id, Hlc::at(T0 + 1), 1);
    assert_eq!(
        ea.current_identity(dba.conn()).unwrap().identity_id,
        head,
        "the forged transition moved the head"
    );
    let rows: i64 = dba
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        rows, 0,
        "a transition signed for another body was stored anyway"
    );
}
