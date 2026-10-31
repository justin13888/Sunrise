//! Identity transition (ADR-0032): the successor and its shares.

use super::*;

#[test]
fn a_successor_is_a_new_identity_carrying_the_accounts_age() {
    let root = VaultRootKey::from_bytes([0x70; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let successor = kc.mint_successor_identity(&SystemRng);

    assert_ne!(successor.identity_id, kc.identity_id());
    assert_ne!(successor.signing.public_bytes(), kc.identity_signing_pub());
    assert_ne!(successor.dh_pub, kc.identity_dh_pub());
    assert_eq!(
        successor.identity_id,
        identity_id_from_pub(&successor.signing.public_bytes()),
        "the id is a derivation, never a chosen field"
    );
    assert!(
        successor.dh_secret.is_some(),
        "the minter holds both halves"
    );
    // A rotation does not begin a new account, and field 6 of the recovery
    // blob means when the account began.
    assert_eq!(
        successor.created_at_ms,
        kc.identity.lock().identity.created_at_ms
    );
}

/// The per-device share: 80 bytes, openable only by the device it names,
/// and yielding a successor that holds `ID_S_priv` and not `ID_D_priv`.
#[test]
fn a_device_share_round_trips_to_exactly_one_device() {
    let (_root, _da, ka, _db2, kb) = paired_pair();
    let successor = ka.mint_successor_identity(&SystemRng);
    let sealed = ka
        .seal_successor_device_share(&successor, &kb.device_dh_pub(), &kb.device_id(), &SystemRng)
        .unwrap();
    assert_eq!(
        sealed.len(),
        sunrise_crypto::DEVICE_SHARE_LEN,
        "the digest in the signed body checks this width"
    );

    let opened = kb
        .open_successor_device_share(&publics(&successor), &sealed)
        .unwrap();
    assert_eq!(opened.identity_id, successor.identity_id);
    assert_eq!(
        opened.signing.public_bytes(),
        successor.signing.public_bytes()
    );
    assert!(
        !opened.holds_signing_key(),
        "handing every device the successor's ID_S_priv would undo #105 in one op: \
             the device pairing refused a signing key would get one from the next rotation"
    );
    assert_eq!(opened.dh_pub, successor.dh_pub);
    assert!(
        opened.dh_secret.is_none(),
        "handing every device ID_D_priv would undo #76 in one op"
    );

    // The founder is not the named device, so its own D_D does not open it.
    assert!(matches!(
        ka.open_successor_device_share(&publics(&successor), &sealed),
        Err(KeychainError::Hpke(_))
    ));
}

/// The share's `info` binds the successor id, so a share cannot be
/// replayed from one rotation into another.
#[test]
fn a_device_share_does_not_replay_across_transitions() {
    let (_root, _da, ka, _db2, kb) = paired_pair();
    let first = ka.mint_successor_identity(&SystemRng);
    let second = ka.mint_successor_identity(&SystemRng);
    let sealed = ka
        .seal_successor_device_share(&first, &kb.device_dh_pub(), &kb.device_id(), &SystemRng)
        .unwrap();
    assert!(matches!(
        kb.open_successor_device_share(&publics(&second), &sealed),
        Err(KeychainError::Hpke(_))
    ));
}

/// A body that names one key beside a share carrying another is refused.
/// HPKE Base authenticates no sender, so this check is the whole of what
/// stops a share being swapped for a key the attacker holds.
#[test]
fn a_share_that_does_not_match_the_signed_body_is_refused() {
    let (_root, _da, ka, _db2, kb) = paired_pair();
    let successor = ka.mint_successor_identity(&SystemRng);
    let sealed = ka
        .seal_successor_device_share(&successor, &kb.device_dh_pub(), &kb.device_id(), &SystemRng)
        .unwrap();
    // Same `info` — so the blob still opens — but a body naming a
    // different signing key.
    let impostor = ka.mint_successor_identity(&SystemRng);
    let lying = SuccessorPublics {
        identity_id: successor.identity_id,
        id_s_pub: impostor.signing.public_bytes(),
        id_d_pub: successor.dh_pub,
    };
    assert!(matches!(
        kb.open_successor_device_share(&lying, &sealed),
        Err(KeychainError::IdentityIdMismatch)
    ));
}

/// The carry-forward share: 112 bytes, sealed to the **outgoing**
/// `ID_D_pub`, and openable only where that key lives.
#[test]
fn the_carry_share_reaches_the_identity_key_and_nowhere_else() {
    let (_root, _da, ka, _db2, kb) = paired_pair();
    let successor = ka.mint_successor_identity(&SystemRng);
    let sealed = ka
        .seal_successor_carry_share(&successor, &SystemRng)
        .unwrap();
    assert_eq!(sealed.len(), sunrise_crypto::IDENTITY_SHARE_LEN);

    let opened = ka
        .open_successor_carry_share(&publics(&successor), &sealed)
        .unwrap();
    assert_eq!(opened.identity_id, successor.identity_id);
    assert_eq!(
        opened
            .dh_secret
            .as_ref()
            .map(IdentityDhKeyPair::secret_bytes),
        successor
            .dh_secret
            .as_ref()
            .map(IdentityDhKeyPair::secret_bytes),
        "this is the one path by which ID_D_priv survives a rotation"
    );

    // A paired device holds no outgoing ID_D_priv, so it cannot open it —
    // and cannot seal one either, because the successor it received has no
    // ID_D_priv to put inside.
    assert!(matches!(
        kb.open_successor_carry_share(&publics(&successor), &sealed),
        Err(KeychainError::IdentitySecretAbsent)
    ));
    let received = kb
        .open_successor_device_share(
            &publics(&successor),
            &ka.seal_successor_device_share(
                &successor,
                &kb.device_dh_pub(),
                &kb.device_id(),
                &SystemRng,
            )
            .unwrap(),
        )
        .unwrap();
    assert!(matches!(
        kb.seal_successor_carry_share(&received, &SystemRng),
        Err(KeychainError::IdentitySecretAbsent)
    ));
}

/// Adoption on the minting device: the row is re-wrapped under the new id,
/// the cert is re-issued, the device secrets are untouched, and the vault
/// re-opens.
#[test]
fn adopting_a_successor_rewraps_the_identity_and_leaves_the_device_keys_alone() {
    let root = VaultRootKey::from_bytes([0x72; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let before_id = kc.identity_id();
    let genesis_before = kc.genesis_identity_id(&d).unwrap();
    let device_secrets = |d: &Db| -> (Vec<u8>, Option<Vec<u8>>) {
        d.conn()
            .query_row(
                "SELECT signing_secret_wrapped, dh_secret_wrapped
                     FROM local_identity WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    let secrets_before = device_secrets(&d);

    let successor = kc.mint_successor_identity(&SystemRng);
    let successor_id = successor.identity_id;
    let roster = roster_cert(&successor, &kc, 1_700_000_001_000);
    let cert = d
        .with_tx(|tx| {
            Ok(kc
                .adopt_successor_identity(tx, successor, roster, &SystemRng)
                .expect("adopt the successor"))
        })
        .unwrap();

    // The live keychain signs under the successor from here on — the whole
    // point of the lock.
    assert_eq!(kc.identity_id(), successor_id);
    assert_ne!(kc.identity_id(), before_id);
    assert_eq!(kc.cert_blob(), cert);
    DeviceCert::from_cbor(&cert)
        .unwrap()
        .verify_binding(&kc.identity_signing_pub(), &successor_id)
        .expect("the re-issued cert binds to the successor");

    // D_S and D_D are unchanged: their AAD binds to device_id, not to the
    // identity, so re-wrapping them could only go wrong.
    assert_eq!(device_secrets(&d), secrets_before);

    // The anchor does not move, and neither does the account's age.
    assert_eq!(kc.genesis_identity_id(&d).unwrap(), genesis_before);
    assert_eq!(genesis_before, Some(before_id));

    // ...and the re-wrap really is under the new id's AAD, which is what
    // re-opening proves: the old ciphertext would not unwrap.
    let reopened = open(&mut d, &root);
    assert_eq!(reopened.identity_id(), successor_id);
    assert_eq!(reopened.identity_signing_pub(), kc.identity_signing_pub());
    assert!(
        reopened.holds_only_copy_of_identity_key(),
        "the minting device keeps ID_D_priv, and 0019's guard has to agree"
    );
    assert_eq!(reopened.cert_blob(), cert);
}

/// Adoption on a device that received the transition rather than minting
/// it: no `ID_D_priv`, so no `minted_by_device_id`, which is the same rule
/// migration 0019 enforces for a paired device.
#[test]
fn a_device_adopting_a_successor_it_did_not_mint_stores_no_identity_key() {
    let (root, _da, ka, mut db2, kb) = paired_pair();
    let successor = ka.mint_successor_identity(&SystemRng);
    let sealed = ka
        .seal_successor_device_share(&successor, &kb.device_dh_pub(), &kb.device_id(), &SystemRng)
        .unwrap();
    let received = kb
        .open_successor_device_share(&publics(&successor), &sealed)
        .unwrap();
    assert!(
        !received.holds_signing_key(),
        "a device that did not mint the successor must not end up holding its \
             signing key: that is the rotation handing back what pairing withheld (#105)"
    );
    let successor_id = received.identity_id;
    let roster = roster_cert(&successor, &kb, 1_700_000_001_000);
    db2.with_tx(|tx| {
        Ok(kb
            .adopt_successor_identity(tx, received, roster, &SystemRng)
            .expect("adopt the successor"))
    })
    .unwrap();

    assert_eq!(kb.identity_id(), successor_id);
    assert!(!kb.holds_only_copy_of_identity_key());
    let (minted_by, wrapped_d): (Option<Vec<u8>>, Vec<u8>) = db2
        .conn()
        .query_row(
            "SELECT minted_by_device_id, id_d_priv_wrapped FROM identity WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(minted_by, None);
    assert!(wrapped_d.is_empty(), "an absent secret is an empty blob");

    let reopened = open(&mut db2, &root);
    assert_eq!(reopened.identity_id(), successor_id);
    assert!(!reopened.holds_only_copy_of_identity_key());
}

/// A re-issued roster cert carries the device's real labels, not a
/// placeholder. A transition that renamed every device would otherwise be
/// a perfectly valid transition that renamed the user's device list.
#[test]
fn a_reissued_cert_keeps_the_devices_own_labels() {
    let root = VaultRootKey::from_bytes([0x73; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let sibling = [0x9e; 16];
    let blob = kc
        .issue_cert_for(
            sibling,
            [0x11; 32],
            [0x22; 32],
            "Work laptop",
            "macos15",
            1_700_000_002_000,
        )
        .unwrap();
    let cert = DeviceCert::from_cbor(&blob).unwrap();
    assert_eq!(cert.body.device_id, sibling);
    assert_eq!(cert.body.nickname, "Work laptop");
    assert_eq!(cert.body.platform, "macos15");
    cert.verify_binding(&kc.identity_signing_pub(), &kc.identity_id())
        .expect("issued under this account identity");
}
