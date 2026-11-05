//! Creating a vault, reopening it, and the wrong root.

use super::*;

#[test]
fn first_open_creates_identity_and_rows() {
    let root = VaultRootKey::from_bytes([0xab; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);

    let (dev, cert): (Vec<u8>, Vec<u8>) = d
        .conn()
        .query_row(
            "SELECT device_id, cert_blob FROM local_identity WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(&dev[..], &kc.device_id()[..]);
    assert_eq!(cert, kc.cert_blob());

    let n: i64 = d
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE device_id = ?",
            params![&kc.device_id()[..]],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);

    // The cert is signed by the ACCOUNT identity, not by the device.
    let parsed = DeviceCert::from_cbor(&kc.cert_blob()).unwrap();
    parsed
        .verify_binding(&kc.identity_signing_pub(), &kc.identity_id())
        .expect("identity-signed");
    assert!(
        parsed.verify(&kc.device_signing_pub()).is_err(),
        "a self-signed cert is exactly what ADR-0024 removes"
    );
    assert_eq!(parsed.body.device_id, kc.device_id());
    assert_eq!(parsed.body.d_d_pub, kc.device_dh_pub());
}

#[test]
fn second_open_loads_same_identity() {
    let root = VaultRootKey::from_bytes([0xcd; 32]);
    let mut d = db(&root);
    let first = open(&mut d, &root);
    let first_id = first.device_id();
    let first_cert = first.cert_blob();
    let first_identity = first.identity_id();
    let first_id_d = first.identity_dh_pub();
    drop(first);

    let second = open(&mut d, &root);
    assert_eq!(second.device_id(), first_id);
    assert_eq!(second.cert_blob(), first_cert);
    assert_eq!(second.identity_id(), first_identity);
    assert_eq!(second.identity_dh_pub(), first_id_d);
}

#[test]
fn wrong_vault_root_fails_cleanly() {
    let root = VaultRootKey::from_bytes([1u8; 32]);
    let mut d = db(&root);
    let _ = open(&mut d, &root);

    let bad = VaultRootKey::from_bytes([2u8; 32]);
    let err = Keychain::open(&mut d, bad, &clock(), &SystemRng, &IdentitySeed::Own).unwrap_err();
    assert!(matches!(err, KeychainError::VaultRootMismatch));
}

/// The property the derivation used to give for free, and which envelopes
/// now have to earn: two devices sharing a vault root no longer agree on a
/// Stream key, because the key is random.
#[test]
fn two_devices_on_one_root_do_not_share_keys_by_accident() {
    let root = VaultRootKey::from_bytes([9u8; 32]);
    let mut da = db(&root);
    let mut db2 = db(&root);
    let ka = open(&mut da, &root);
    let kb = open(&mut db2, &root);
    assert_ne!(ka.device_id(), kb.device_id());

    let sid = [5u8; 16];
    let a_key = da
        .with_tx(|tx| ka.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    let b_key = db2
        .with_tx(|tx| kb.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    assert_eq!(a_key.0, 1);
    assert_eq!(b_key.0, 1);
    assert_ne!(
        a_key.1, b_key.1,
        "random keys must not collide across devices"
    );
}
