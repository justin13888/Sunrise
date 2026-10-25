//! The account's key bundle on a replica (ADR-0069, issue #580).
//!
//! Real random keys throughout, so a key a replica was never handed is one it
//! genuinely cannot open: the publisher is a device admitted by pairing, which
//! holds `ID_D_pub` and not `ID_D_priv`, and the reader is a vault restored
//! from the recovery code, which holds `ID_D_priv` and nothing else.

use super::testutil::*;
use super::*;
use crate::engine::PinnedKeyBundle;
use sunrise_domain::INBOX_STREAM_BYTES;

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock(PLMutex::new(T0)))
}

/// A device admitted by pairing: same account, no `ID_D_priv`.
fn paired(seed: u8) -> Engine {
    Engine::from_clock(
        clock(),
        Arc::new(SystemRng),
        Arc::new(Keychain::for_test_paired(
            VaultRootKey::from_bytes(ROOT),
            [seed; 32],
        )),
    )
}

/// A paired publisher whose keys have rotated past the genesis: the account
/// state a recovery cannot reach from the identity alone once the envelopes
/// are trimmed.
fn rotated_publisher() -> (Engine, Db) {
    let p = paired(2);
    let mut db = db_root(ROOT);
    p.ensure_base_epochs(&mut db).unwrap();
    for stream in [META_STREAM, INBOX_STREAM_BYTES] {
        p.apply(
            &mut db,
            Command::RotateStreamKey {
                stream: EntityRef::new(EntityKind::Stream, stream),
            },
        )
        .unwrap();
    }
    (p, db)
}

fn bundle_of(p: &Engine, db: &mut Db) -> (Vec<u8>, crate::engine::KeyBundlePlan) {
    let plan = p
        .key_bundle_plan(db)
        .unwrap()
        .expect("a current device publishes");
    (p.seal_key_bundle(&plan).unwrap(), plan)
}

fn bound(db: &mut Db, device: &[u8; 16]) {
    db.with_tx(|tx| {
        tx.execute(
            "INSERT INTO device_read_bounds (device_id, first_bound_at_ms) VALUES (?, 0)",
            params![&device[..]],
        )
        .map(|_| ())
    })
    .unwrap();
}

/// The whole route: a paired device publishes every key it holds, sealed to
/// an identity key it does not hold, and a vault restored from the recovery
/// code opens it and holds each of them.
#[test]
fn a_recovered_vault_takes_every_key_from_a_paired_devices_bundle() {
    let (p, mut pdb) = rotated_publisher();
    let (bytes, plan) = bundle_of(&p, &mut pdb);
    assert!(
        plan.keys
            .iter()
            .any(|k| k.stream_id == META_STREAM && k.epoch == 2),
        "the rotated vault-meta key is in the bundle"
    );

    let r = engine_random_keys(ROOT, [9; 32], clock());
    let mut rdb = db_root(ROOT);
    let (applied, _) = r.apply_key_bundle(&mut rdb, &bytes).unwrap();
    assert_eq!(applied.carried, plan.keys.len());
    assert_eq!(applied.absorbed, plan.keys.len());
    for k in &plan.keys {
        assert!(
            r.keychain
                .stream_keys_at(&k.stream_id, k.epoch)
                .contains(&k.key),
            "the recovered vault holds {k:?}"
        );
    }
    // Absorbing it again changes nothing: a bundle is a join.
    let (again, _) = r.apply_key_bundle(&mut rdb, &bytes).unwrap();
    assert_eq!(again.absorbed, 0);
}

/// Sealing needs only `ID_D_pub`; opening needs `ID_D_priv`, which no device
/// pairing admitted holds. A revoked paired device therefore reads nothing out
/// of the bundle the relay pins.
#[test]
fn a_device_without_the_identity_key_cannot_open_a_bundle() {
    let (p, mut pdb) = rotated_publisher();
    let (bytes, _) = bundle_of(&p, &mut pdb);
    let other = paired(3);
    let mut odb = db_root(ROOT);
    assert!(matches!(
        other.apply_key_bundle(&mut odb, &bytes),
        Err(EngineError::Keychain(_))
    ));
}

#[test]
fn a_tampered_bundle_is_refused() {
    let (p, mut pdb) = rotated_publisher();
    let (mut bytes, _) = bundle_of(&p, &mut pdb);
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let r = engine_random_keys(ROOT, [9; 32], clock());
    let mut rdb = db_root(ROOT);
    assert!(matches!(
        r.apply_key_bundle(&mut rdb, &bytes),
        Err(EngineError::Invalid(_))
    ));
}

/// Every key is credited to the publisher. Once the reader learns the
/// publisher was revoked, the keys are still read but never written under,
/// exactly as an envelope from that device would be (ADR-0041 §4); and the
/// publisher's pinned bundle is one an honest device replaces.
#[test]
fn a_bounded_publishers_keys_are_read_and_not_written_under() {
    let (p, mut pdb) = rotated_publisher();
    let (bytes, _) = bundle_of(&p, &mut pdb);
    let r = engine_random_keys(ROOT, [9; 32], clock());
    let mut rdb = db_root(ROOT);
    r.apply_key_bundle(&mut rdb, &bytes).unwrap();
    let live = |db: &mut Db| {
        db.with_tx(|tx| r.keychain.current_epoch_tx(tx, &INBOX_STREAM_BYTES))
            .unwrap()
    };
    assert_eq!(live(&mut rdb), Some(2));
    assert_eq!(
        r.judge_pinned_key_bundle(&rdb, &bytes).unwrap(),
        PinnedKeyBundle::Valid(sunrise_crypto::manifest_of(
            &p.key_bundle_plan(&mut pdb).unwrap().unwrap().keys
        ))
    );

    bound(&mut rdb, &p.keychain.device_id());
    assert_ne!(
        live(&mut rdb),
        Some(2),
        "a bounded publisher's key is not live"
    );
    assert!(!r.keychain.stream_keys_at(&INBOX_STREAM_BYTES, 2).is_empty());
    assert_eq!(
        r.judge_pinned_key_bundle(&rdb, &bytes).unwrap(),
        PinnedKeyBundle::Refused("publisher_bounded")
    );
}

/// A bundle never launders a key a bounded device delivered into one its
/// reader writes under: the publisher leaves such keys out.
#[test]
fn a_bundle_leaves_out_keys_a_bounded_device_delivered() {
    let (p, mut pdb) = rotated_publisher();
    let stranger = [0x66; 16];
    let foreign = StreamKey::from_bytes([0x77; 32]);
    pdb.with_tx(|tx| {
        p.keychain
            .absorb_stream_key(
                tx,
                &INBOX_STREAM_BYTES,
                3,
                &foreign,
                KeySource::Envelope { sender: stranger },
                &SystemRng,
                T0,
            )
            .map(|_| ())
    })
    .unwrap();
    let held = |p: &Engine, db: &mut Db| {
        p.key_bundle_plan(db)
            .unwrap()
            .unwrap()
            .keys
            .iter()
            .any(|k| k.key == foreign)
    };
    assert!(
        held(&p, &mut pdb),
        "the control: an unbounded sender's key is carried"
    );
    bound(&mut pdb, &stranger);
    assert!(!held(&p, &mut pdb));
}

/// A device this replica holds as revoked publishes nothing.
#[test]
fn a_bounded_device_publishes_no_bundle() {
    let (p, mut pdb) = rotated_publisher();
    bound(&mut pdb, &p.keychain.device_id());
    assert!(p.key_bundle_plan(&mut pdb).unwrap().is_none());
}
