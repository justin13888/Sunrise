//! Per-device op chains, fork evidence and the stream digest (ADR-0043,
//! issue #325).
//!
//! Each test drives real engines: the writer seals through the op-log path
//! that stamps fields 14 and 15, and the receiver applies through
//! `apply_remote_all`. A relay that omits, reorders or equivocates is played
//! by the test choosing which envelopes to hand over and in what order.

use super::testutil::*;
use super::*;
use crate::engine::chain::{digest_payload, ForkKind};
use crate::ChainIntegrity;
use sunrise_crypto::{decode_envelope, op_hash, verify_envelope, FrontierEntry};
use sunrise_domain::INBOX_STREAM_BYTES;

const INBOX: [u8; 16] = INBOX_STREAM_BYTES;

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock(PLMutex::new(T0)))
}

/// A pair of replicas of one account that trust each other.
fn pair(seed_a: u8, seed_b: u8) -> (Engine, Db, Engine, Db) {
    let ea = engine_seeded(ROOT, [seed_a; 32], clock());
    let eb = engine_seeded(ROOT, [seed_b; 32], clock());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    (ea, dba, eb, dbb)
}

/// `device`'s envelopes in `stream`, by seq.
fn envs(db: &Db, stream: &[u8; 16], device: &[u8; 16]) -> Vec<Vec<u8>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT envelope FROM ops WHERE stream_id = ? AND device_id = ? ORDER BY seq")
        .unwrap();
    stmt.query_map(params![&stream[..], &device[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn integrity(e: &Engine, db: &Db) -> ChainIntegrity {
    e.chain_integrity(db).unwrap()
}

fn cursor(db: &Db, stream: &[u8; 16], device: &[u8; 16]) -> i64 {
    db.conn()
        .query_row(
            "SELECT last_applied_seq FROM sync_cursors WHERE stream_id = ? AND device_id = ?",
            params![&stream[..], &device[..]],
            |r| r.get(0),
        )
        .unwrap_or(0)
}

fn digest_of(e: &Engine, db: &mut Db, stream: &[u8; 16]) -> [u8; 32] {
    let now = e.clock.now_ms();
    db.with_tx(|tx| digest_payload(tx, stream, now))
        .unwrap()
        .expect("the replica holds ops in the stream")
        .digest
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    op_hash(&decode_envelope(bytes).unwrap()).unwrap()
}

fn evidence_kinds(db: &Db) -> Vec<(i64, String)> {
    let mut stmt = db
        .conn()
        .prepare("SELECT seq, kind FROM fork_evidence ORDER BY seq, kind")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// ADR-0043 §1–§2: each op names its writer's previous op, the first names
/// none, and field 15 lists another device's tip once, the first time the
/// writer has seen it advance.
#[test]
fn a_writer_links_each_op_to_its_predecessor_and_lists_what_it_saw() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    let b = eb.keychain.device_id();
    new_task(&ea, &mut dba, "one");
    new_task(&ea, &mut dba, "two");
    let a_envs = envs(&dba, &INBOX, &a);
    assert_eq!(a_envs.len(), 2);
    let first = decode_envelope(&a_envs[0]).unwrap();
    let second = decode_envelope(&a_envs[1]).unwrap();
    assert_eq!(first.prev_hash, None, "seq 1 has no predecessor");
    assert_eq!(second.prev_hash, Some(hash(&a_envs[0])));
    assert!(first.heads.is_empty() && second.heads.is_empty());

    for env in &a_envs {
        eb.apply_remote(&mut dbb, env).unwrap();
    }
    new_task(&eb, &mut dbb, "three");
    new_task(&eb, &mut dbb, "four");
    let b_envs = envs(&dbb, &INBOX, &b);
    let third = decode_envelope(&b_envs[0]).unwrap();
    let fourth = decode_envelope(&b_envs[1]).unwrap();
    assert_eq!(
        third.heads,
        vec![sunrise_crypto::ChainHead {
            device_id: a,
            seq: 2,
            op_hash: hash(&a_envs[1]),
        }],
        "B lists A's tip, which it had applied"
    );
    assert!(fourth.heads.is_empty(), "a delta: nothing advanced since");
    assert_eq!(fourth.prev_hash, Some(hash(&b_envs[0])));

    // A checks both links and B's head and finds nothing to record.
    for env in &b_envs {
        ea.apply_remote(&mut dba, env).unwrap();
    }
    assert_eq!(integrity(&ea, &dba), ChainIntegrity::default());
}

/// An omitted op is a known missing op as soon as the op after it arrives,
/// and arriving later checks it against the hash its successor named.
#[test]
fn an_omitted_op_is_expected_and_checked_when_it_arrives() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    for t in ["one", "two", "three"] {
        new_task(&ea, &mut dba, t);
    }
    let a_envs = envs(&dba, &INBOX, &a);
    eb.apply_remote(&mut dbb, &a_envs[0]).unwrap();
    eb.apply_remote(&mut dbb, &a_envs[2])
        .unwrap()
        .expect("an op above a gap is applied, not parked");
    assert_eq!(cursor(&dbb, &INBOX, &a), 1);
    assert_eq!(
        integrity(&eb, &dbb),
        ChainIntegrity {
            wanted: 1,
            ..Default::default()
        },
        "seq 3 named seq 2, which B does not hold"
    );

    eb.apply_remote(&mut dbb, &a_envs[1]).unwrap();
    assert_eq!(cursor(&dbb, &INBOX, &a), 3);
    assert_eq!(integrity(&eb, &dbb), ChainIntegrity::default());
    assert_eq!(
        digest_of(&ea, &mut dba, &INBOX),
        digest_of(&eb, &mut dbb, &INBOX)
    );
}

/// Delivery order reaches neither the evidence nor the digest.
#[test]
fn a_reordered_chain_links_up_and_digests_agree() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    for t in ["one", "two", "three", "four"] {
        new_task(&ea, &mut dba, t);
    }
    let a_envs = envs(&dba, &INBOX, &a);
    for i in [3, 1, 0, 2] {
        eb.apply_remote(&mut dbb, &a_envs[i]).unwrap();
    }
    assert_eq!(integrity(&eb, &dbb), ChainIntegrity::default());
    assert_eq!(
        digest_of(&ea, &mut dba, &INBOX),
        digest_of(&eb, &mut dbb, &INBOX)
    );
}

/// One device's seq signed twice — a device restored from an old backup
/// does exactly this. The second envelope is kept as verifiable evidence and
/// the first stays the applied one; an op that names the other branch as its
/// predecessor is applied, and its broken link is recorded too.
#[test]
fn a_forked_chain_is_kept_as_evidence_and_never_refused() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    // The same device, from a vault that diverged before seq 1.
    let ea2 = engine_seeded(ROOT, [1; 32], clock());
    let mut dba2 = db_root(ROOT);
    let a = ea.keychain.device_id();
    assert_eq!(ea2.keychain.device_id(), a);

    let kept = new_task(&ea, &mut dba, "kept");
    let forked = new_task(&ea2, &mut dba2, "forked");
    new_task(&ea2, &mut dba2, "after the fork");
    let one = envs(&dba, &INBOX, &a);
    let other = envs(&dba2, &INBOX, &a);

    eb.apply_remote(&mut dbb, &one[0]).unwrap();
    assert!(
        eb.apply_remote(&mut dbb, &other[0]).unwrap().is_none(),
        "the second envelope at seq 1 is not materialized"
    );
    eb.apply_remote(&mut dbb, &other[1])
        .unwrap()
        .expect("seq 2 is a new position and is applied");

    assert_eq!(
        evidence_kinds(&dbb),
        vec![
            (1, ForkKind::Link.as_str().to_owned()),
            (1, ForkKind::Seq.as_str().to_owned()),
        ]
    );
    assert_eq!(integrity(&eb, &dbb).forks, 2);
    assert_eq!(read_task_t(&eb, &dbb, kept).title, "kept");
    assert!(eb.query(&dbb, Query::EntityById(forked)).is_err());

    // The evidence is a proof: the kept envelope verifies under A's key.
    let (held, other_hash, kept_env): (Vec<u8>, Vec<u8>, Vec<u8>) = dbb
        .conn()
        .query_row(
            "SELECT held_hash, other_hash, other_envelope FROM fork_evidence WHERE kind = 'seq'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(held, hash(&one[0]).to_vec());
    assert_eq!(other_hash, hash(&other[0]).to_vec());
    let proof = decode_envelope(&kept_env).unwrap();
    let d_s_pub = DeviceCert::from_cbor(&ea.keychain.cert_blob())
        .unwrap()
        .body
        .d_s_pub;
    verify_envelope(&proof, &d_s_pub).unwrap();

    // A re-delivery of either half adds nothing.
    eb.apply_remote(&mut dbb, &other[0]).unwrap();
    eb.apply_remote(&mut dbb, &one[0]).unwrap();
    assert_eq!(integrity(&eb, &dbb).forks, 2);
}

/// A relay that withholds the *last* op of a sequence leaves no gap. The
/// writer's digest is what shows the receiver it is behind, and catching up
/// clears it without a disagreement.
#[test]
fn a_digest_reveals_a_withheld_tail_and_clears_once_it_arrives() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    new_task(&ea, &mut dba, "one");
    new_task(&ea, &mut dba, "two");
    let a_envs = envs(&dba, &INBOX, &a);
    eb.apply_remote(&mut dbb, &a_envs[0]).unwrap();
    assert_eq!(
        integrity(&eb, &dbb),
        ChainIntegrity::default(),
        "nothing B holds names seq 2"
    );

    assert!(ea.publish_stream_digest(&mut dba, &INBOX).unwrap());
    let digest_env = envs(&dba, &INBOX, &a).pop().unwrap();
    eb.apply_remote(&mut dbb, &digest_env).unwrap();
    let wanted = integrity(&eb, &dbb);
    assert_eq!(wanted.divergences, 0);
    assert!(
        wanted.wanted >= 1,
        "B now knows it lacks A's seq 2: {wanted:?}"
    );

    eb.apply_remote(&mut dbb, &a_envs[1]).unwrap();
    assert_eq!(integrity(&eb, &dbb), ChainIntegrity::default());
    assert_eq!(cursor(&dbb, &INBOX, &a), 3);
    assert_eq!(
        digest_of(&ea, &mut dba, &INBOX),
        digest_of(&eb, &mut dbb, &INBOX)
    );
}

/// A relay that served two replicas different halves of a fork: each holds a
/// consistent chain, so neither sees evidence of its own. A third replica's
/// digest is what shows the disagreement, and it names the device and seq.
#[test]
fn a_digest_detects_a_replica_that_holds_different_ops() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let ec = engine_seeded(ROOT, [3; 32], clock());
    let mut dbc = db_root(ROOT);
    trust(&ec, &mut dbc, &ea);
    trust(&eb, &mut dbb, &ec);
    let ea2 = engine_seeded(ROOT, [1; 32], clock());
    let mut dba2 = db_root(ROOT);
    let a = ea.keychain.device_id();
    let c = ec.keychain.device_id();

    new_task(&ea, &mut dba, "what B got");
    new_task(&ea2, &mut dba2, "what C got");
    eb.apply_remote(&mut dbb, &envs(&dba, &INBOX, &a)[0])
        .unwrap();
    ec.apply_remote(&mut dbc, &envs(&dba2, &INBOX, &a)[0])
        .unwrap();

    assert!(ec.publish_stream_digest(&mut dbc, &INBOX).unwrap());
    let digest_env = envs(&dbc, &INBOX, &c).pop().unwrap();
    eb.apply_remote(&mut dbb, &digest_env).unwrap();

    assert_eq!(integrity(&eb, &dbb).divergences, 1);
    let (device, peer, seq): (Vec<u8>, Vec<u8>, i64) = dbb
        .conn()
        .query_row(
            "SELECT device_id, peer_device_id, seq FROM chain_divergence",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((device, peer, seq), (a.to_vec(), c.to_vec(), 1));
}

/// A digest whose `digest` is not the digest of its own frontier is damaged,
/// and is read for nothing.
#[test]
fn a_digest_that_does_not_match_its_frontier_is_ignored() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    new_task(&ea, &mut dba, "one");
    let now = ea.clock.now_ms();
    let mut payload = dba
        .with_tx(|tx| digest_payload(tx, &INBOX, now))
        .unwrap()
        .unwrap();
    // Claim a longer prefix than the digest covers.
    payload.frontier[0].1 = 9;
    let inner = InnerOp::StreamDigest(payload);
    let key = dba
        .with_tx(|tx| ea.keychain.current_stream_key_tx(tx, &INBOX))
        .unwrap()
        .unwrap();
    let env = ea
        .keychain
        .seal_op_at(
            INBOX,
            2,
            ea.hlc.send(),
            &encode_inner_op(&inner).unwrap(),
            ea.rng.as_ref(),
            key.0,
            &key.1,
        )
        .unwrap();
    eb.apply_remote(&mut dbb, &envs(&dba, &INBOX, &a)[0])
        .unwrap();
    eb.apply_remote(&mut dbb, &env).unwrap();
    assert_eq!(integrity(&eb, &dbb), ChainIntegrity::default());
}

/// Ops written before chaining carry no field 14 or 15. They are applied,
/// assert nothing, and are still folded into the chain root, so a digest
/// covers them.
#[test]
fn legacy_ops_without_chain_fields_are_applied_and_folded() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    new_task(&ea, &mut dba, "chained");
    let chained = envs(&dba, &INBOX, &a);
    // Re-seal both of A's inbox ops without chain fields, as a build from
    // before ADR-0043 would have written them.
    let key = dba
        .with_tx(|tx| ea.keychain.current_stream_key_tx(tx, &INBOX))
        .unwrap()
        .unwrap();
    let inner = ea.keychain.open_op(&chained[0]).unwrap();
    let legacy: Vec<Vec<u8>> = (1..=2)
        .map(|seq| {
            ea.keychain
                .seal_op_at(
                    INBOX,
                    seq,
                    ea.hlc.send(),
                    &inner,
                    ea.rng.as_ref(),
                    key.0,
                    &key.1,
                )
                .unwrap()
        })
        .collect();
    for env in legacy.iter().rev() {
        let decoded = decode_envelope(env).unwrap();
        assert!(decoded.prev_hash.is_none() && decoded.heads.is_empty());
        eb.apply_remote(&mut dbb, env).unwrap();
    }
    assert_eq!(integrity(&eb, &dbb), ChainIntegrity::default());
    assert_eq!(cursor(&dbb, &INBOX, &a), 2);
    let root: Vec<u8> = dbb
        .conn()
        .query_row(
            "SELECT chain_root FROM ops WHERE stream_id = ? AND device_id = ? AND seq = 2",
            params![&INBOX[..], &a[..]],
            |r| r.get(0),
        )
        .unwrap();
    let expected = sunrise_crypto::chain_root_step(
        &sunrise_crypto::chain_root_step(
            &sunrise_crypto::chain_root_init(&INBOX, &a),
            &hash(&legacy[0]),
        ),
        &hash(&legacy[1]),
    );
    assert_eq!(root, expected.to_vec());
}

/// A vault from before migration 0034 has no stored hash or root. The first
/// fold computes both from the envelopes it holds.
#[test]
fn a_vault_from_before_chaining_folds_its_existing_ops() {
    let (ea, mut dba, _eb, _dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    new_task(&ea, &mut dba, "one");
    new_task(&ea, &mut dba, "two");
    let before = digest_of(&ea, &mut dba, &INBOX);
    dba.conn()
        .execute("UPDATE ops SET op_hash = NULL, chain_root = NULL", [])
        .unwrap();
    assert_eq!(digest_of(&ea, &mut dba, &INBOX), before);
    let missing: i64 = dba
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ops WHERE stream_id = ? AND device_id = ? AND chain_root IS NULL",
            params![&INBOX[..], &a[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(missing, 0);
}

/// The cadence: a first digest as soon as the stream holds anything, then
/// none until enough ops or enough time have passed.
#[test]
fn digests_are_published_on_their_cadence() {
    let c = clock();
    let ea = engine_seeded(ROOT, [1; 32], Arc::clone(&c));
    let mut dba = db_root(ROOT);
    new_task(&ea, &mut dba, "one");
    let first = ea.publish_due_stream_digests(&mut dba).unwrap();
    assert!(first >= 1, "the inbox and the meta stream hold ops");
    assert_eq!(ea.publish_due_stream_digests(&mut dba).unwrap(), 0);

    new_task(&ea, &mut dba, "two");
    assert_eq!(
        ea.publish_due_stream_digests(&mut dba).unwrap(),
        0,
        "one op is not enough"
    );
    set_clock(&c, T0 + crate::engine::chain::DIGEST_EVERY_MS);
    assert_eq!(
        ea.publish_due_stream_digests(&mut dba).unwrap(),
        1,
        "a day later the inbox, which moved, is due; the meta stream is not"
    );
}

/// The count rule: with the clock held still, the 256th op since this
/// device's last digest makes one due, and the 255th does not.
#[test]
fn a_digest_is_due_after_256_ops_without_waiting_a_day() {
    use crate::engine::chain::DIGEST_EVERY_OPS;
    let ea = engine_seeded(ROOT, [1; 32], clock());
    let mut dba = db_root(ROOT);
    new_task(&ea, &mut dba, "first");
    assert!(ea.publish_due_stream_digests(&mut dba).unwrap() >= 1);
    let a = ea.keychain.device_id();
    let held = envs(&dba, &INBOX, &a).len();

    for i in 1..DIGEST_EVERY_OPS {
        new_task(&ea, &mut dba, &format!("task {i}"));
    }
    assert_eq!(
        envs(&dba, &INBOX, &a).len() - held,
        usize::try_from(DIGEST_EVERY_OPS - 1).unwrap(),
        "one inbox op per task"
    );
    assert_eq!(
        ea.publish_due_stream_digests(&mut dba).unwrap(),
        0,
        "255 ops are not enough"
    );
    new_task(&ea, &mut dba, "the 256th");
    assert_eq!(
        ea.publish_due_stream_digests(&mut dba).unwrap(),
        1,
        "256 ops are, the same instant"
    );
}

/// A peer's digest is not activity: two idle replicas do not answer each
/// other's digests once a day forever.
#[test]
fn a_peer_digest_does_not_make_a_digest_due() {
    let c = clock();
    let ea = engine_seeded(ROOT, [1; 32], Arc::clone(&c));
    let eb = engine_seeded(ROOT, [2; 32], clock());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let b = eb.keychain.device_id();
    new_task(&ea, &mut dba, "one");
    ea.publish_due_stream_digests(&mut dba).unwrap();

    new_task(&eb, &mut dbb, "theirs");
    for env in envs(&dbb, &INBOX, &b) {
        ea.apply_remote(&mut dba, &env).unwrap();
    }
    set_clock(&c, T0 + crate::engine::chain::DIGEST_EVERY_MS);
    assert_eq!(
        ea.publish_due_stream_digests(&mut dba).unwrap(),
        1,
        "B's task is activity in the inbox"
    );

    assert!(eb.publish_stream_digest(&mut dbb, &INBOX).unwrap());
    let digest = envs(&dbb, &INBOX, &b).pop().unwrap();
    ea.apply_remote(&mut dba, &digest).unwrap();
    set_clock(&c, T0 + 2 * crate::engine::chain::DIGEST_EVERY_MS);
    assert_eq!(
        ea.publish_due_stream_digests(&mut dba).unwrap(),
        0,
        "B's digest alone is not"
    );
}

/// A fresh replica that trusts every engine in `senders`.
fn trusting(seed: u8, senders: &[&Engine]) -> (Engine, Db) {
    let e = engine_seeded(ROOT, [seed; 32], clock());
    let mut db = db_root(ROOT);
    for s in senders {
        trust(&e, &mut db, s);
    }
    (e, db)
}

/// Field 15 checked three ways (ADR-0043 §3). C applied one branch of A's
/// fork and lists it as A's head. A replica holding the other branch keeps
/// C's op as `head` evidence; one holding nothing of A expects the hash C
/// named, and the branch that arrives later either meets it or becomes
/// `head` evidence in its turn.
#[test]
fn a_listed_head_is_checked_against_the_held_op_or_expected() {
    let ea = engine_seeded(ROOT, [1; 32], clock());
    let mut dba = db_root(ROOT);
    let ea2 = engine_seeded(ROOT, [1; 32], clock());
    let mut dba2 = db_root(ROOT);
    let a = ea.keychain.device_id();
    new_task(&ea, &mut dba, "branch X");
    new_task(&ea2, &mut dba2, "branch Y");
    let x = envs(&dba, &INBOX, &a).remove(0);
    let y = envs(&dba2, &INBOX, &a).remove(0);
    assert_ne!(hash(&x), hash(&y));

    let (ec, mut dbc) = trusting(3, &[&ea]);
    let c = ec.keychain.device_id();
    ec.apply_remote(&mut dbc, &y).unwrap();
    new_task(&ec, &mut dbc, "C saw Y");
    let c_op = envs(&dbc, &INBOX, &c).remove(0);
    assert_eq!(
        decode_envelope(&c_op).unwrap().heads,
        vec![sunrise_crypto::ChainHead {
            device_id: a,
            seq: 1,
            op_hash: hash(&y),
        }]
    );

    // Holds X, then reads C's claim that A's seq 1 is Y.
    let (eb, mut dbb) = trusting(2, &[&ea, &ec]);
    eb.apply_remote(&mut dbb, &x).unwrap();
    eb.apply_remote(&mut dbb, &c_op)
        .unwrap()
        .expect("an op whose head disagrees is still applied");
    assert_eq!(
        evidence_kinds(&dbb),
        vec![(1, ForkKind::Head.as_str().to_owned())]
    );
    assert_eq!(integrity(&eb, &dbb).wanted, 0);

    // Holds nothing of A: C's head is an expectation, reason `head`.
    let (ed, mut dbd) = trusting(4, &[&ea, &ec]);
    ed.apply_remote(&mut dbd, &c_op).unwrap();
    let (device, seq, expected): (Vec<u8>, i64, Vec<u8>) = dbd
        .conn()
        .query_row(
            "SELECT device_id, seq, op_hash FROM chain_expected",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((device, seq, expected), (a.to_vec(), 1, hash(&y).to_vec()));
    assert_eq!(
        integrity(&ed, &dbd),
        ChainIntegrity {
            wanted: 1,
            ..Default::default()
        }
    );
    // X arrives where Y was expected, named by an op of another device.
    ed.apply_remote(&mut dbd, &x).unwrap();
    assert_eq!(
        evidence_kinds(&dbd),
        vec![(1, ForkKind::Head.as_str().to_owned())]
    );
    assert_eq!(
        integrity(&ed, &dbd),
        ChainIntegrity {
            forks: 1,
            ..Default::default()
        }
    );

    // Y arrives where Y was expected: nothing to record.
    let (ee, mut dbe) = trusting(5, &[&ea, &ec]);
    ee.apply_remote(&mut dbe, &c_op).unwrap();
    ee.apply_remote(&mut dbe, &y).unwrap();
    assert_eq!(integrity(&ee, &dbe), ChainIntegrity::default());
}

/// `sealer`'s `StreamDigest` op in the inbox at `seq`, over `frontier`,
/// whatever `sealer` holds. A peer's claim is only as good as its frontier;
/// this is how a test makes one that disagrees.
fn digest_op(sealer: &Engine, db: &mut Db, seq: u64, frontier: &[FrontierEntry]) -> Vec<u8> {
    let payload = crate::inner_op::StreamDigestPayload {
        digest: sunrise_crypto::stream_digest(&INBOX, frontier),
        frontier: frontier
            .iter()
            .map(|e| crate::inner_op::FrontierWire(e.device_id, e.seq, e.root))
            .collect(),
        unknown: sunrise_domain::Unknowns::new(),
    };
    let key = db
        .with_tx(|tx| sealer.keychain.current_stream_key_tx(tx, &INBOX))
        .unwrap()
        .expect("the sealer holds an inbox key");
    sealer
        .keychain
        .seal_op_at(
            INBOX,
            seq,
            sealer.hlc.send(),
            &encode_inner_op(&InnerOp::StreamDigest(payload)).unwrap(),
            sealer.rng.as_ref(),
            key.0,
            &key.1,
        )
        .unwrap()
}

fn root_through(device: &[u8; 16], envelopes: &[Vec<u8>]) -> [u8; 32] {
    envelopes
        .iter()
        .fold(sunrise_crypto::chain_root_init(&INBOX, device), |r, e| {
            sunrise_crypto::chain_root_step(&r, &hash(e))
        })
}

/// A recorded divergence is cleared by a later digest from the same peer
/// that agrees at the same seq, because roots chain.
#[test]
fn an_agreeing_digest_clears_a_recorded_divergence() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    new_task(&ea, &mut dba, "one");
    let a_envs = envs(&dba, &INBOX, &a);
    eb.apply_remote(&mut dbb, &a_envs[0]).unwrap();

    let (ec, mut dbc) = trusting(3, &[&ea]);
    ec.apply_remote(&mut dbc, &a_envs[0]).unwrap();
    trust(&eb, &mut dbb, &ec);
    let agreeing = FrontierEntry {
        device_id: a,
        seq: 1,
        root: root_through(&a, &a_envs),
    };
    let disagreeing = FrontierEntry {
        root: [0xd1; 32],
        ..agreeing
    };
    let good = digest_op(&ec, &mut dbc, 1, &[agreeing]);
    let bad = digest_op(&ec, &mut dbc, 2, &[disagreeing]);

    eb.apply_remote(&mut dbb, &bad).unwrap();
    assert_eq!(integrity(&eb, &dbb).divergences, 1);
    eb.apply_remote(&mut dbb, &good).unwrap();
    assert_eq!(
        integrity(&eb, &dbb).divergences,
        0,
        "an agreement at the recorded seq clears it"
    );
}

/// A claim this replica cannot check yet waits as a `chain_claims` row, and
/// becomes a divergence once the prefix reaches it with a different root.
#[test]
fn a_deferred_claim_becomes_a_divergence_when_the_prefix_reaches_it() {
    let (ea, mut dba, eb, mut dbb) = pair(1, 2);
    let a = ea.keychain.device_id();
    new_task(&ea, &mut dba, "one");
    new_task(&ea, &mut dba, "two");
    let a_envs = envs(&dba, &INBOX, &a);
    eb.apply_remote(&mut dbb, &a_envs[0]).unwrap();

    let (ec, mut dbc) = trusting(3, &[&ea]);
    ec.apply_remote(&mut dbc, &a_envs[0]).unwrap();
    trust(&eb, &mut dbb, &ec);
    let c = ec.keychain.device_id();
    let claim = digest_op(
        &ec,
        &mut dbc,
        1,
        &[FrontierEntry {
            device_id: a,
            seq: 2,
            root: [0xd2; 32],
        }],
    );
    eb.apply_remote(&mut dbb, &claim).unwrap();
    assert_eq!(
        integrity(&eb, &dbb),
        ChainIntegrity {
            wanted: 1,
            ..Default::default()
        },
        "B holds A through 1 and cannot check a claim at 2"
    );

    eb.apply_remote(&mut dbb, &a_envs[1]).unwrap();
    assert_eq!(
        integrity(&eb, &dbb),
        ChainIntegrity {
            divergences: 1,
            ..Default::default()
        },
        "the claim was checked and dropped, and it disagreed"
    );
    let (device, peer, seq): (Vec<u8>, Vec<u8>, i64) = dbb
        .conn()
        .query_row(
            "SELECT device_id, peer_device_id, seq FROM chain_divergence",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((device, peer, seq), (a.to_vec(), c.to_vec(), 2));
}
