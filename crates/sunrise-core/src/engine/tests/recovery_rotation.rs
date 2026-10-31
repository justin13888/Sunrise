//! A recovery code across an identity rotation (ADR-0070, issue #583).
//!
//! Real vaults throughout — `Keychain::open` writes the `identity` row, so the
//! genesis anchor, the recovery blob and the carry shares are the ones a
//! shipping build produces — and real random Stream keys, so a key a replica
//! was never handed is one it cannot open.
//!
//! The founder A writes, rotates, and writes again. R is a vault restored from
//! a recovery code; which code, and whether the rotation carried it, is what
//! each test is about.

use super::support::*;
use super::*;
use crate::control_op::Recipient;
use crate::unlock::IdentitySeed;
use sunrise_crypto::recovery::RecoveryPayload;
use sunrise_crypto::{CarryWalk, IdentityChain};

const ROOT_A: [u8; 32] = [0x51; 32];
const ROOT_R: [u8; 32] = [0x52; 32];
const SEED: [u8; 32] = [0x5a; 32];
const NEW_SEED: [u8; 32] = [0x5b; 32];

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock(PLMutex::new(T0)))
}

/// A vault as `Core::open` leaves one: the keychain, its base epochs, its
/// published cert.
fn open_vault(root: [u8; 32], seed: &IdentitySeed) -> (Engine, Db) {
    let mut db = db_root(root);
    let c = clock();
    let kc = Keychain::open(
        &mut db,
        VaultRootKey::from_bytes(root),
        c.as_ref(),
        &SystemRng,
        seed,
    )
    .unwrap();
    let e = Engine::from_clock(c, Arc::new(SystemRng), Arc::new(kc));
    e.ensure_base_epochs(&mut db).unwrap();
    e.publish_device_cert(&mut db).unwrap();
    (e, db)
}

/// What the 24 words open: the blob `e` seals under `seed`, opened as a
/// recovery opens it, with the account record's identity as the AAD.
fn code_opens(e: &Engine, db: &Db, seed: &[u8; 32]) -> RecoveryPayload {
    let blob = e.seal_recovery_blob(db, seed).unwrap();
    let (genesis, _) = e.keychain.genesis_pair(db).unwrap();
    sunrise_crypto::unseal_recovery_blob(&blob, seed, &genesis).unwrap()
}

fn recover(payload: RecoveryPayload) -> (Engine, Db) {
    open_vault(ROOT_R, &IdentitySeed::Recovered(Box::new(payload)))
}

/// Every op `from` wrote, delivered to `to`, in each stream's order.
fn deliver(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
    let device = from.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY stream_id, seq")
            .unwrap();
        stmt.query_map(params![&device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        // An op refused here is the case under test, not the harness's.
        let _ = to.apply_remote_all(to_db, &env);
    }
}

fn rotate(e: &Engine, db: &mut Db, keep: bool) {
    e.apply(
        db,
        Command::RotateIdentity {
            keep_recovery_code: keep,
        },
    )
    .unwrap();
}

fn head(e: &Engine, db: &Db) -> [u8; 16] {
    e.current_identity(db.conn()).unwrap().identity_id
}

fn chain_of(e: &Engine, db: &Db) -> Vec<u8> {
    e.identity_chain_plan(db)
        .unwrap()
        .expect("a rotated account has a chain to publish")
}

fn bundle_of(e: &Engine, db: &mut Db) -> Vec<u8> {
    let plan = e.key_bundle_plan(db).unwrap().expect("a current device");
    e.seal_key_bundle(&plan).unwrap()
}

/// This device's row: whether it is current, under the identity in force.
fn current(e: &Engine, db: &Db) -> bool {
    let QueryResult::Devices(rows) = e.query(db, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    rows.iter()
        .find(|r| r.device_id == e.keychain.device_id())
        .is_some_and(|r| r.current)
}

/// The issue's case, with every op retained: a code made before two rotations
/// that carried it restores a vault that walks both links, certifies itself
/// under the identity in force, keeps the unwrapping key, and reads what was
/// written before and after each rotation.
///
/// The keys come from the key bundle, as the sync driver takes them: once the
/// vault signs under the head, the identity copies sealed to the genesis are
/// sealed to a key it no longer holds.
#[test]
fn a_code_carried_across_two_rotations_reaches_the_head_from_the_ops() {
    let (a, mut adb) = open_vault(ROOT_A, &IdentitySeed::Own);
    let words = code_opens(&a, &adb, &SEED);
    let before = new_task(&a, &mut adb, "before");
    rotate(&a, &mut adb, true);
    let between = new_task(&a, &mut adb, "between");
    rotate(&a, &mut adb, true);
    let after = new_task(&a, &mut adb, "after");
    let head_a = head(&a, &adb);
    assert!(
        a.keychain.holds_only_copy_of_identity_key(),
        "the founder keeps the successor's unwrapping key"
    );

    let bundle = bundle_of(&a, &mut adb);

    let (r, mut rdb) = recover(words);
    // Twice: the first pass meets A's earliest envelopes before A's cert.
    deliver(&a, &adb, &r, &mut rdb);
    deliver(&a, &adb, &r, &mut rdb);

    assert_eq!(head(&r, &rdb), head_a);
    assert_eq!(
        r.keychain.identity_id(),
        head_a,
        "R walked both carry shares"
    );
    assert!(r.keychain.holds_only_copy_of_identity_key());
    assert!(r.keychain.can_rotate_identity());
    assert!(current(&r, &rdb), "R certified itself under the head");
    r.apply_key_bundle(&mut rdb, &bundle)
        .expect("the head's bundle opens under the walked identity");
    deliver(&a, &adb, &r, &mut rdb);
    let tasks = live_task_ids(&rdb);
    for t in [before, between, after] {
        assert!(tasks.contains(t.bytes()), "R reads {t:?}");
    }

    // And A, meeting R's cert, holds it as a current device under the head:
    // R is a member, not a device that merely reads.
    deliver(&r, &rdb, &a, &mut adb);
    let QueryResult::Devices(rows) = a.query(&adb, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    let r_row = rows
        .iter()
        .find(|row| row.device_id == r.keychain.device_id())
        .expect("A knows R");
    assert!(r_row.current, "{r_row:?}");
}

/// The same, with no op at all: the chain record alone takes the code to the
/// head, and the key bundle sealed to the head then opens. This is the path a
/// recovery takes once the relay has trimmed the transitions.
#[test]
fn the_chain_record_alone_takes_a_carried_code_to_the_head_and_its_bundle() {
    let (a, mut adb) = open_vault(ROOT_A, &IdentitySeed::Own);
    let words = code_opens(&a, &adb, &SEED);
    rotate(&a, &mut adb, true);
    rotate(&a, &mut adb, true);
    let head_a = head(&a, &adb);
    let chain = chain_of(&a, &adb);
    let bundle = bundle_of(&a, &mut adb);

    let (r, mut rdb) = recover(words);
    let seeded = r.seed_identity_chain(&mut rdb, &chain).unwrap();
    assert_eq!(seeded.stored, 2);
    assert!(seeded.at_head);
    assert_eq!(r.keychain.identity_id(), head_a);

    let (applied, _) = r.apply_key_bundle(&mut rdb, &bundle).unwrap();
    assert!(applied.absorbed > 0, "the bundle sealed to the head opens");

    // The transitions' own ops, arriving later, replace the seeded rows'
    // missing roster and change nothing about the head.
    deliver(&a, &adb, &r, &mut rdb);
    assert_eq!(head(&r, &rdb), head_a);
    let links: i64 = rdb
        .conn()
        .query_row("SELECT count(*) FROM identity_transitions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(links, 2);
    assert!(rdb
        .conn()
        .prepare("SELECT payload FROM identity_transitions")
        .unwrap()
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .all(|p| !super::super::identity_chain::is_seeded_payload(&p.unwrap())));
}

/// A rotation that retires the code: the founder keeps the successor's
/// unwrapping key, the vault-meta epoch the transition is sealed under has no
/// identity copy for the outgoing identity, and every Stream moved.
#[test]
fn a_rotation_that_retires_the_code_keeps_its_holder_out_of_every_new_key() {
    let (a, mut adb) = open_vault(ROOT_A, &IdentitySeed::Own);
    let genesis = a.keychain.identity_id();
    let meta_before = a.keychain.stream_keys_at(&META_STREAM, 1).len();
    assert!(meta_before > 0);
    let inbox = sunrise_domain::INBOX_STREAM_BYTES;
    let inbox_before = adb
        .with_tx(|tx| a.keychain.current_epoch_tx(tx, &inbox))
        .unwrap();

    rotate(&a, &mut adb, false);
    let to = head(&a, &adb);
    assert_ne!(to, genesis);
    assert_eq!(a.keychain.identity_id(), to);
    assert!(
        a.keychain.holds_only_copy_of_identity_key(),
        "without it no identity copy written from now on could ever be opened"
    );

    let transition_epoch: i64 = adb
        .conn()
        .query_row(
            "SELECT meta_epoch FROM identity_transitions WHERE to_identity_id = ?",
            params![&to[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        transition_epoch > 1,
        "sealed under a fresh vault-meta epoch"
    );
    let epoch = u32::try_from(transition_epoch).unwrap();
    let identity_copies: Vec<Recipient> = {
        let mut stmt = adb
            .conn()
            .prepare("SELECT envelope FROM ops WHERE inner_kind = 'key.envelope'")
            .unwrap();
        let envs: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        envs.iter()
            .filter_map(|env| a.keychain.open_op(env).ok())
            .filter_map(|inner| match crate::inner_op::decode_inner_op(&inner) {
                Ok(InnerOp::KeyEnvelope(k)) if k.stream_id == META_STREAM && k.epoch == epoch => {
                    Some(k.recipient)
                }
                _ => None,
            })
            .collect()
    };
    assert!(
        identity_copies.contains(&Recipient::Identity(to)),
        "the successor holds the new vault-meta epoch"
    );
    assert!(
        !identity_copies.contains(&Recipient::Identity(genesis)),
        "and the outgoing identity, which every holder of the old code is, does not"
    );
    let inbox_after = adb
        .with_tx(|tx| a.keychain.current_epoch_tx(tx, &inbox))
        .unwrap();
    assert!(inbox_after > inbox_before, "every Stream moved");
}

/// After a rotation that retired it, the old code opens its blob and reaches
/// nothing: the chain names the link that retired it, and a vault seeded
/// anyway stays under the genesis, not current, holding no key of the head.
#[test]
fn a_retired_code_reaches_nothing_the_account_writes_after() {
    let (a, mut adb) = open_vault(ROOT_A, &IdentitySeed::Own);
    let old = code_opens(&a, &adb, &SEED);
    rotate(&a, &mut adb, false);
    new_task(&a, &mut adb, "after");
    let chain = chain_of(&a, &adb);
    let record = IdentityChain::decode(&chain).unwrap();
    record.verify(&old.identity_id).unwrap();
    assert_eq!(
        record.carry_walk(&old.identity_id, &old.id_d_priv),
        CarryWalk::Retired { at_link: 0 },
        "what the recovery flow reports before it creates a vault"
    );
    let bundle = bundle_of(&a, &mut adb);

    let (r, mut rdb) = recover(old);
    let seeded = r.seed_identity_chain(&mut rdb, &chain).unwrap();
    assert!(!seeded.at_head);
    assert!(!current(&r, &rdb));
    assert!(
        r.apply_key_bundle(&mut rdb, &bundle).is_err(),
        "the head's bundle does not open under the retired identity"
    );
}

/// The code a retiring rotation issues names the genesis, carries the
/// vault-meta genesis key, and restores a vault that folds from the genesis,
/// is current under the head, and reads the account through its bundle.
#[test]
fn the_reissued_code_restores_a_current_vault_of_the_same_account() {
    let (a, mut adb) = open_vault(ROOT_A, &IdentitySeed::Own);
    let genesis = a.keychain.identity_id();
    let before = new_task(&a, &mut adb, "before");
    rotate(&a, &mut adb, false);
    let after = new_task(&a, &mut adb, "after");
    let words = code_opens(&a, &adb, &NEW_SEED);
    assert_eq!(words.account_identity_id(), genesis);
    assert_eq!(words.identity_id, head(&a, &adb));
    assert!(!words.meta_genesis_keys.is_empty());
    let chain = chain_of(&a, &adb);
    let bundle = bundle_of(&a, &mut adb);

    let (r, mut rdb) = recover(words);
    assert_eq!(r.keychain.genesis_pair(&rdb).unwrap().0, genesis);
    let seeded = r.seed_identity_chain(&mut rdb, &chain).unwrap();
    assert!(seeded.at_head);
    r.apply_key_bundle(&mut rdb, &bundle).unwrap();
    deliver(&a, &adb, &r, &mut rdb);
    assert!(current(&r, &rdb));
    let tasks = live_task_ids(&rdb);
    assert!(tasks.contains(before.bytes()) && tasks.contains(after.bytes()));

    // R's first write was sealed under the account's vault-meta genesis key,
    // which it was handed rather than derived, so A opens it.
    deliver(&r, &rdb, &a, &mut adb);
    let QueryResult::Devices(rows) = a.query(&adb, Query::DeviceList).unwrap() else {
        panic!("expected Devices")
    };
    assert!(rows
        .iter()
        .any(|row| row.device_id == r.keychain.device_id() && row.current));
}

/// A chain record of another account is refused before anything is stored.
#[test]
fn a_record_from_another_account_is_refused() {
    let (a, mut adb) = open_vault(ROOT_A, &IdentitySeed::Own);
    rotate(&a, &mut adb, true);
    let chain = chain_of(&a, &adb);
    let (stranger, mut sdb) = open_vault([0x53; 32], &IdentitySeed::Own);
    assert!(matches!(
        stranger.seed_identity_chain(&mut sdb, &chain),
        Err(EngineError::Invalid(_))
    ));
}
