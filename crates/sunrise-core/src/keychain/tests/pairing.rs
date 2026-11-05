//! What a device admitted by pairing receives, and what it cannot do.

use super::*;

/// A device admitted by pairing cannot open an identity-sealed envelope,
/// and survives a reopen still unable to.
///
/// This is the half of `#76` that the recipient filter cannot express. The
/// filter decides who gets a `Recipient::Device` copy; this decides whether
/// the `Recipient::Identity` copy — which is emitted for *every* epoch,
/// because recovery depends on it — is a way around that filter. It was,
/// for as long as `PairingPayload` carried `ID_D_priv`.
///
/// The reopen matters as much as the first open: the identity row is
/// written from what pairing supplied, so a paired device that persisted a
/// secret it was never given would get it back on the next unlock and the
/// property would hold for one session only.
#[test]
fn a_paired_device_cannot_open_the_identity_copy() {
    let root = VaultRootKey::from_bytes([0x5e; 32]);

    // The inviting device: it created the account, so it holds `ID_D_priv`
    // and can open an identity-sealed envelope.
    let mut inviter_db = db(&root);
    let inviter = open(&mut inviter_db, &root);
    let sid = [0x21; 16];
    let key = StreamKey::from_bytes([0x64; 32]);
    let sealed = inviter
        .seal_key_envelope(&inviter.identity_dh_pub(), &sid, 4, &key, &SystemRng)
        .unwrap();
    assert_eq!(
        inviter
            .open_key_envelope(EnvelopeRecipient::Identity, &sid, 4, &sealed)
            .unwrap(),
        key,
        "the account's creator can still open the recovery copy"
    );

    // The paired device, opened from that device's own payload.
    let payload = inviter
        .pair_device_in_process(
            &inviter_db,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .unwrap();
    let mut paired_db = db(&root);
    let paired = Keychain::open(
        &mut paired_db,
        root.clone(),
        &clock(),
        &SystemRng,
        &seed_of(&payload),
    )
    .unwrap();
    assert_eq!(
        paired.identity_dh_pub(),
        inviter.identity_dh_pub(),
        "it joined the same account, so it can still seal to the identity"
    );
    assert!(
        matches!(
            paired.open_key_envelope(EnvelopeRecipient::Identity, &sid, 4, &sealed),
            Err(KeychainError::IdentitySecretAbsent)
        ),
        "a paired device must not hold the identity's unwrapping key"
    );

    // And it did not quietly persist one to find again next time.
    drop(paired);
    let reopened = open(&mut paired_db, &root);
    assert!(
        matches!(
            reopened.open_key_envelope(EnvelopeRecipient::Identity, &sid, 4, &sealed),
            Err(KeychainError::IdentitySecretAbsent)
        ),
        "nor recover it on the next unlock"
    );
}

/// A device that already has `ID_D_priv` in its own `identity` row — the
/// state STORAGE_V 17 left every paired device in — does not get to use it.
///
/// Issue #87. Dropping the key from `PairingPayload` fixed the devices
/// paired afterwards and did nothing for the ones paired before, and
/// migration 0018 could not clear the column because on the account's
/// creator that same column holds the only copy in existence. 0019 clears
/// it from the rows it can prove are not the creator's, and
/// `minted_by_device_id` is the runtime half of the same test: the blob is
/// ignored unless the row names *this* device as the identity's minter, so
/// a copy that reaches the column by any route the migration did not
/// anticipate is still inert.
///
/// The blob planted below is the inviter's own, byte for byte. Both vaults
/// share a root and an `identity_id` in this test, which are exactly the
/// two inputs to the wrap — so this is not an approximation of what 17 did,
/// it is the same bytes in the same column.
#[test]
fn a_wrapped_identity_key_on_a_paired_device_is_inert() {
    let root = VaultRootKey::from_bytes([0x3c; 32]);
    let mut inviter_db = db(&root);
    let inviter = open(&mut inviter_db, &root);
    let sid = [0x22; 16];
    let key = StreamKey::from_bytes([0x65; 32]);
    let sealed = inviter
        .seal_key_envelope(&inviter.identity_dh_pub(), &sid, 4, &key, &SystemRng)
        .unwrap();

    let payload = inviter
        .pair_device_in_process(
            &inviter_db,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .unwrap();
    let mut paired_db = db(&root);
    drop(
        Keychain::open(
            &mut paired_db,
            root.clone(),
            &clock(),
            &SystemRng,
            &seed_of(&payload),
        )
        .unwrap(),
    );

    // Reproduce the 17-era row: the wrapped secret present, and nothing
    // saying which device minted the identity.
    let stolen: Vec<u8> = inviter_db
        .conn()
        .query_row(
            "SELECT id_d_priv_wrapped FROM identity WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!stolen.is_empty(), "the creator's row carries the key");
    paired_db
        .conn()
        .execute(
            "UPDATE identity
                 SET id_d_priv_wrapped = ?, minted_by_device_id = NULL
                 WHERE id = 1",
            rusqlite::params![stolen],
        )
        .unwrap();

    let reopened = open(&mut paired_db, &root);
    assert!(
        !reopened.holds_only_copy_of_identity_key(),
        "a paired device does not hold the identity key however the column got filled"
    );
    assert!(
        matches!(
            reopened.open_key_envelope(EnvelopeRecipient::Identity, &sid, 4, &sealed),
            Err(KeychainError::IdentitySecretAbsent)
        ),
        "and it must not read through a revocation using the identity copy"
    );
}

/// A device opened with a pairing payload joins the *sender's* account and
/// inherits its keys, rather than minting an account of its own.
#[test]
fn a_paired_device_adopts_the_senders_identity_and_keys() {
    let root = VaultRootKey::from_bytes([0x55; 32]);
    let mut da = db(&root);
    let ka = open(&mut da, &root);
    let sid = [0x0c; 16];
    let (epoch, key) = da
        .with_tx(|tx| ka.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    let payload = ka
        .pair_device_in_process(
            &da,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .unwrap();

    let mut db2 = db(&root);
    let kb = Keychain::open(
        &mut db2,
        root.clone(),
        &clock(),
        &SystemRng,
        &seed_of(&payload),
    )
    .unwrap();

    assert_eq!(kb.identity_id(), ka.identity_id());
    assert_eq!(kb.identity_signing_pub(), ka.identity_signing_pub());
    assert_ne!(kb.device_id(), ka.device_id(), "a distinct device");
    assert_ne!(kb.device_dh_pub(), ka.device_dh_pub());
    assert_eq!(kb.stream_keys_at(&sid, epoch), vec![key.clone()]);

    // B's cert verifies under the shared account identity, which is what
    // makes it a member rather than a stranger.
    DeviceCert::from_cbor(&kb.cert_blob())
        .unwrap()
        .verify_binding(&ka.identity_signing_pub(), &ka.identity_id())
        .expect("B's cert is issued under A's identity");

    // ...and an op A sealed is readable on B.
    let env = ka
        .seal_op_at(
            sid,
            1,
            sunrise_cbor::hlc::Hlc::at(1),
            b"from A",
            &SystemRng,
            epoch,
            &key,
        )
        .unwrap();
    let decoded = decode_envelope(&env).unwrap();
    let opened = open_envelope(
        &decoded,
        &ka.device_signing_pub(),
        Some(&kb.stream_keys_at(&sid, epoch)[0]),
    )
    .unwrap();
    assert_eq!(opened, b"from A");
}

/// A device paired **after** a rotation anchors its fold at the account's
/// genesis, not at the identity it happened to join at.
///
/// The divergence this closes is silent, which is what makes it worth a
/// test: each replica stays internally consistent, and the two simply
/// disagree about who the account is. Before fields 10 and 11 the joining
/// device recorded whatever identity was in force when it arrived, so a
/// peer that had been there since the beginning folded from link 0 while
/// the newcomer folded from link 1 — and a cert issued under link 0 was a
/// stranger's cert to the newcomer.
#[test]
fn a_device_paired_after_a_rotation_anchors_at_the_genesis() {
    let root = VaultRootKey::from_bytes([0x74; 32]);
    let mut da = db(&root);
    let ka = open(&mut da, &root);
    let genesis = ka.identity_id();
    let genesis_pub = ka.identity_signing_pub();

    // A rotates. Its own row moves to the successor; its anchor does not.
    let successor = ka.mint_successor_identity(&SystemRng);
    let successor_id = successor.identity_id();
    let cert = roster_cert(&successor, &ka, 1);
    da.with_tx(|tx| {
        Ok(ka
            .adopt_successor_identity(tx, successor, cert, &SystemRng)
            .expect("adopt"))
    })
    .unwrap();
    assert_eq!(ka.identity_id(), successor_id);
    assert_eq!(ka.genesis_pair(&da).unwrap(), (genesis, genesis_pub));

    // B pairs from A now, and gets the anchor rather than the head.
    let payload = ka
        .pair_device_in_process(
            &da,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(payload.identity_id, successor_id, "the identity in force");
    assert_eq!(
        payload.genesis_identity_id, genesis,
        "and the anchor behind it"
    );
    assert_eq!(payload.genesis_id_s_pub, genesis_pub);

    let mut db2 = db(&root);
    let kb = Keychain::open(
        &mut db2,
        root.clone(),
        &clock(),
        &SystemRng,
        &seed_of(&payload),
    )
    .unwrap();
    assert_eq!(kb.identity_id(), successor_id);
    assert_eq!(
        kb.genesis_pair(&db2).unwrap(),
        (genesis, genesis_pub),
        "B folds from the same point A does, which is the whole property"
    );
}

/// A payload whose `identity_id` does not derive from the `ID_S_pub` beside
/// it names one account and signs under another. It used to be provoked by
/// rewriting `id_s_priv`; the field is gone, so the public half is rewritten
/// instead — which is the only half there is now, and the only thing the
/// receiver can check.
#[test]
fn a_payload_from_another_account_is_refused() {
    let root = VaultRootKey::from_bytes([0x56; 32]);
    let mut da = db(&root);
    let ka = open(&mut da, &root);
    let mut payload = ka
        .pair_device_in_process(
            &da,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .unwrap();
    payload.id_s_pub = IdentitySigningKeyPair::from_secret_bytes(&[0x01; 32]).public_bytes();

    let mut db2 = db(&root);
    let err = Keychain::open(&mut db2, root, &clock(), &SystemRng, &seed_of(&payload)).unwrap_err();
    assert!(matches!(err, KeychainError::IdentityIdMismatch));
}

/// **The #105 test.** A device admitted by pairing holds no `ID_S_priv`,
/// and cannot issue a `DeviceCert` for a fresh device id.
///
/// This is the property the whole change exists for, and it is asserted
/// three ways that fail independently:
///
/// 1. **Behaviourally**, which is what actually matters. `issue_cert_for`
///    is the *only* way to produce a cert, and the paired device is handed
///    exactly the arguments issue #105 describes — a device id it just
///    minted, with keys it holds — and refused. The sponsor is handed the
///    same arguments and succeeds, so the refusal is attributable to the
///    absent key rather than to a malformed request.
/// 2. **At rest.** The paired vault's `identity` row stores an empty
///    `id_s_priv_wrapped`, so there is no ciphertext a later build could
///    decide to unwrap. A struct field is a fact about today's source; a
///    zero-length column is a fact about the database.
/// 3. **Across a restart**, because the first two would both pass on a
///    device that held the key in memory and simply declined to use it.
///    Re-opening reconstructs the keychain from that row alone.
///
/// Note what is *not* asserted: that the struct has no field. That would be
/// grepping the type, and the type can be changed by the same commit that
/// breaks the property.
#[test]
fn a_paired_device_cannot_issue_a_cert_for_a_fresh_device_id() {
    let (root, da, sponsor, mut db2, paired) = paired_pair();

    // The fresh device id a revoked device used to mint for itself: real
    // keys, derived id, nothing forged about it.
    let (fresh_signing, fresh_dh, fresh_id) = mint_device_keys(&SystemRng);

    // 1. The sponsor can. This is the control, and it is what makes the
    //    refusal below mean "no key" rather than "bad arguments".
    let issued = sponsor
        .issue_cert_for(
            fresh_id,
            fresh_signing.public_bytes(),
            fresh_dh.public_bytes(),
            "a fresh id",
            "test",
            1_700_000_002_000,
        )
        .expect("the account's creator holds ID_S_priv and can certify anything");
    DeviceCert::from_cbor(&issued)
        .unwrap()
        .verify_binding(&sponsor.identity_signing_pub(), &sponsor.identity_id())
        .expect("and what it produces is a genuinely valid cert");

    // ...and the paired device cannot, for the same id and the same keys.
    let err = paired
        .issue_cert_for(
            fresh_id,
            fresh_signing.public_bytes(),
            fresh_dh.public_bytes(),
            "a fresh id",
            "test",
            1_700_000_002_000,
        )
        .expect_err("a paired device must not be able to certify anything");
    assert!(matches!(err, KeychainError::IdentitySigningKeyAbsent));

    // Nor can it speak for the account any other way. Both of these are
    // signatures under `ID_S_priv` wearing different names.
    assert!(!paired.can_rotate_identity());
    assert!(matches!(
        paired.issue_pairing_grant(
            &sunrise_pairing::PairingRequest {
                device_id: fresh_id,
                d_s_pub: fresh_signing.public_bytes(),
                d_d_pub: fresh_dh.public_bytes(),
                identity_id: paired.identity_id(),
                nickname: "a fresh id".into(),
                platform: "test".into(),
            },
            BTreeSet::new(),
            1_700_000_002_000
        ),
        Err(KeychainError::IdentitySigningKeyAbsent)
    ));
    assert!(matches!(
        paired.seal_recovery_blob(&[0x99; 32], &SystemRng),
        Err(KeychainError::IdentitySecretAbsent | KeychainError::IdentitySigningKeyAbsent)
    ));

    // 2. At rest: no wrapped signing secret to unwrap, on the paired vault
    //    and — the control again — one on the sponsor's.
    let wrapped = |d: &Db| -> Vec<u8> {
        d.conn()
            .query_row(
                "SELECT id_s_priv_wrapped FROM identity WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(
        wrapped(&db2).is_empty(),
        "a paired vault must store no ciphertext of the account signing key"
    );
    assert!(
        !wrapped(&da).is_empty(),
        "the creator's vault does, or the assertion above is vacuous"
    );

    // 3. Across a restart, so this is not a keychain that holds the key and
    //    declines to use it.
    let reopened = open(&mut db2, &root);
    assert!(!reopened.can_rotate_identity());
    assert_eq!(
        reopened.identity_signing_pub(),
        sponsor.identity_signing_pub(),
        "it still knows whose account it is — it just cannot sign for it"
    );
    assert!(matches!(
        reopened.issue_cert_for(
            fresh_id,
            fresh_signing.public_bytes(),
            fresh_dh.public_bytes(),
            "a fresh id",
            "test",
            1_700_000_002_000,
        ),
        Err(KeychainError::IdentitySigningKeyAbsent)
    ));
}

/// A payload whose cert does not name the keys beside it is refused at
/// `Keychain::create`, not at the first op the device tries to publish.
///
/// `PairingGrant::accept` makes the same checks one crate over, and this is
/// not the same test: a `PairingPayload` reaches this function across a
/// *seam* — a `paired_bundle` from Swift, a file on disk — so it arrives
/// having been decoded rather than assembled, and the only thing standing
/// between "these bytes decoded" and "this device has a usable identity" is
/// the check here.
///
/// Each mutation below is a different way for a grant to be wrong, and each
/// produces the same silent failure without the check: a vault that opens
/// fine and whose every op every peer refuses, because the cert it publishes
/// names a key it cannot sign with.
#[test]
fn a_payload_whose_cert_does_not_match_its_keys_is_refused_at_open() {
    let root = VaultRootKey::from_bytes([0x73; 32]);
    let mut da = db(&root);
    let sponsor = open(&mut da, &root);
    let honest = || {
        sponsor
            .pair_device_in_process(
                &da,
                "joiner".into(),
                "test".into(),
                [0x71; 32],
                [0x72; 32],
                1_700_000_000_000,
            )
            .unwrap()
    };

    // The control: the honest payload opens a vault.
    let mut good_db = db(&root);
    Keychain::open(
        &mut good_db,
        root.clone(),
        &clock(),
        &SystemRng,
        &IdentitySeed::Paired(Box::new(honest())),
    )
    .expect("an honest grant opens the vault");

    // A cert issued by a well-formed identity that is not this account's.
    // It verifies against its own key, which is why the check has to name
    // the identity rather than merely verify the signature.
    let stranger = IdentitySigningKeyPair::from_secret_bytes(&[0x99; 32]);
    let mut p = honest();
    let cert = DeviceCert::from_cbor(&p.device_cert).unwrap();
    let mut body = cert.body.clone();
    body.identity_id = identity_id_from_pub(&stranger.public_bytes());
    p.device_cert = DeviceCert::issue(body, &stranger)
        .unwrap()
        .to_cbor()
        .unwrap();

    // ...and a cert for a different device's keys, signed by the real
    // account. This is the one a confused sponsor produces.
    let other = DeviceSigningKeyPair::from_secret_bytes(&[0x51; 32]);
    let mut q = honest();
    q.d_s_priv = other.secret_bytes();

    for (name, payload) in [("another identity", p), ("another device", q)] {
        let mut d = db(&root);
        let err = Keychain::open(
            &mut d,
            root.clone(),
            &clock(),
            &SystemRng,
            &IdentitySeed::Paired(Box::new(payload)),
        )
        .expect_err("a mismatched grant must not open a vault");
        assert!(
            matches!(err, KeychainError::BadGrantCert(_)),
            "expected a named refusal for {name}, got {err:?}"
        );
    }
}

/// A paired device is a **member** — it just cannot speak for the account.
///
/// The other half of the test above, and the one that stops "cannot issue a
/// cert" from being achieved by breaking the pairing. Everything a device
/// is supposed to be able to do, it still does.
#[test]
fn a_paired_device_is_a_full_member_of_the_account_it_cannot_speak_for() {
    // Built inline rather than from `paired_pair`, because this one needs a
    // Stream key to exist *before* the pairing: a bare keychain mints none
    // — `Core::open` is what mints the base epochs — so the fixture's
    // grant would carry an empty map and the last assertion below would
    // pass on nothing.
    let root = VaultRootKey::from_bytes([0x72; 32]);
    let mut da = db(&root);
    let sponsor = open(&mut da, &root);
    let sid = [0x5a_u8; 16];
    da.with_tx(|tx| sponsor.mint_epoch(tx, &sid, &SystemRng, 0))
        .expect("mint a stream key for the grant to carry");

    let payload = sponsor
        .pair_device_in_process(
            &da,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .expect("the creator can sponsor");
    let mut db2 = db(&root);
    let paired = Keychain::open(
        &mut db2,
        root.clone(),
        &clock(),
        &SystemRng,
        &IdentitySeed::Paired(Box::new(payload)),
    )
    .unwrap();

    assert_eq!(paired.identity_id(), sponsor.identity_id());
    assert_eq!(
        paired.identity_signing_pub(),
        sponsor.identity_signing_pub()
    );
    assert_eq!(paired.identity_dh_pub(), sponsor.identity_dh_pub());
    assert_eq!(
        paired.genesis_pair(&db2).unwrap(),
        sponsor.genesis_pair(&da).unwrap(),
        "it folds the identity chain from the same anchor"
    );
    assert_ne!(
        paired.device_id(),
        sponsor.device_id(),
        "with its own device id, minted here and never sent"
    );

    // Its cert was issued by the sponsor and verifies under the account.
    let cert = DeviceCert::from_cbor(&paired.cert_blob()).expect("its cert parses");
    cert.verify_binding(&sponsor.identity_signing_pub(), &sponsor.identity_id())
        .expect("and is signed by the account identity, not by itself");
    assert_eq!(cert.body.device_id, paired.device_id());
    assert_eq!(cert.body.d_s_pub, paired.device_signing_pub());
    assert_eq!(cert.body.d_d_pub, paired.device_dh_pub());
    assert_eq!(
        cert.body.nickname, "joiner",
        "the labels it asked for, which the account signed"
    );

    // And it holds the keys it needs to read the account's history.
    assert_eq!(paired.held_stream_keys(), sponsor.held_stream_keys());
    assert!(
        !paired.held_stream_keys().is_empty(),
        "or the comparison above is vacuous"
    );
}
