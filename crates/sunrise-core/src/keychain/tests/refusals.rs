//! Failures that must surface as errors rather than as benign-looking values.

use super::*;

/// The audit's fail-open, stated as what the caller does with the answer.
///
/// A live Stream key that will not open under the vault root used to come
/// back as `Ok(None)` — the same value the function returns when the stream
/// has genuinely never had a key. `Engine::ensure_stream_epoch` answers
/// `None` by minting, and `mint_epoch` reads `MAX(epoch)` and bumps it, so
/// the "repair" was a full unannounced rotation: a new epoch broadcast to
/// every peer, every subsequent op sealed under it, every peer's ops at the
/// old epoch parked until the deferred TTL dropped them, and the corrupt
/// row still sitting there — never re-read, because `MAX(epoch)` now
/// pointed past it. No revocation, no operator action, no error.
///
/// The row here is corrupted by flipping one byte of `wrapped`, which is
/// exactly what the AEAD tag exists to catch, and nothing else about the
/// vault is touched.
#[test]
fn a_live_stream_key_that_will_not_open_is_an_error_and_not_an_absence() {
    let root = VaultRootKey::from_bytes([0x1f; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let stream = [0x77u8; 16];

    // A real key, minted and stored the way production does.
    d.with_tx(|tx| {
        kc.mint_epoch(tx, &stream, &SystemRng, 1_700_000_000_000)
            .map(|_| ())
    })
    .expect("mint");

    // The control: it opens, and the answer is the epoch it was minted at.
    let found = d
        .with_tx(|tx| kc.current_stream_key_tx(tx, &stream))
        .expect("a live key reads back");
    assert!(found.is_some(), "the fixture did not store a live key");

    // One byte of the wrap, flipped. Nothing else changes.
    d.conn()
        .execute(
            "UPDATE stream_keys SET wrapped = ?1 WHERE stream_id = ?2",
            params![
                {
                    let mut w: Vec<u8> = d
                        .conn()
                        .query_row(
                            "SELECT wrapped FROM stream_keys WHERE stream_id = ?",
                            params![&stream[..]],
                            |r| r.get(0),
                        )
                        .unwrap();
                    w[0] ^= 0x01;
                    w
                },
                &stream[..]
            ],
        )
        .unwrap();

    // A fresh keychain, so the cached copy from the mint is gone and the
    // read has to come off the row.
    let kc = open(&mut d, &root);
    let answer = d.with_tx(|tx| kc.current_stream_key_tx(tx, &stream));
    assert!(
        answer.is_err(),
        "an unopenable live Stream key came back as {:?}, which every caller \
             reads as \"this stream has never had a key\" and answers by minting a \
             new epoch",
        answer.map(|o| o.map(|(e, _)| e))
    );
}

/// The same distinction one column over: an epoch SQLite hands back outside
/// `u32` is refused rather than read as the genesis epoch.
///
/// `unwrap_or(1)` was a valid-looking answer. It sent the key lookup to a
/// row that is not there — which fed the fail-open above — and it sent
/// `mint_epoch` to write epoch 2 over a stream that may be far past it.
#[test]
fn an_epoch_outside_u32_is_refused_rather_than_read_as_the_genesis_epoch() {
    let root = VaultRootKey::from_bytes([0x2f; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let stream = [0x88u8; 16];
    d.with_tx(|tx| {
        kc.mint_epoch(tx, &stream, &SystemRng, 1_700_000_000_000)
            .map(|_| ())
    })
    .expect("mint");
    d.conn()
        .execute(
            "UPDATE stream_keys SET epoch = -3 WHERE stream_id = ?",
            params![&stream[..]],
        )
        .unwrap();

    let answer = d.with_tx(|tx| kc.current_epoch_tx(tx, &stream));
    assert!(
        answer.is_err(),
        "a negative epoch read back as {answer:?} instead of being refused"
    );
}

/// A row whose epoch does not fit `u32` is skipped at load, not clamped
/// into epoch 0.
///
/// `unwrap_or(0)` did not skip the row, it *misfiled* it: epoch 0 is a real
/// slot on a real stream, and a key filed there looks to every reader like
/// a key that belongs there. `load_cache`'s other two skips are deliberate
/// and documented; this one was not a skip at all.
///
/// The fixture has to wrap at epoch 0 and then rewrite the column, because
/// the wrap binds `(stream_id, epoch)`: a key wrapped at epoch 1 whose
/// column is clamped to 0 simply fails to open and is dropped by the
/// documented skip below it. So the clamp could only misfile a row whose
/// column and whose wrap disagree — which is the corrupt row this is about,
/// and the only shape that tells the clamp and the skip apart.
#[test]
fn a_row_whose_epoch_does_not_fit_is_skipped_rather_than_filed_at_epoch_zero() {
    let root = VaultRootKey::from_bytes([0x5f; 32]);
    let mut d = db(&root);
    // Opened once to create the vault's rows; the keychain itself is not
    // needed, because this fixture writes the `stream_keys` row directly.
    let _ = open(&mut d, &root);
    let stream = [0xaau8; 16];
    let key = StreamKey::from_bytes([0x5a; 32]);
    d.with_tx(|tx| {
        rows::insert_stream_key_row(tx, &root, &stream, 0, &key, KeySource::Local, &SystemRng, 0)
            .map(|_| ())
    })
    .expect("store a key wrapped at epoch 0");

    // The control: as stored, it loads and is found at epoch 0.
    let reloaded = open(&mut d, &root);
    assert_eq!(
        reloaded.stream_keys_at(&stream, 0).len(),
        1,
        "the fixture did not store a loadable key at epoch 0"
    );

    // Now the column disagrees with the wrap.
    d.conn()
        .execute(
            "UPDATE stream_keys SET epoch = -7 WHERE stream_id = ?",
            params![&stream[..]],
        )
        .unwrap();
    let reloaded = open(&mut d, &root);
    assert!(
        reloaded.stream_keys_at(&stream, 0).is_empty(),
        "a key whose row declares a negative epoch was filed at epoch 0, where \
             nothing put it and every reader will treat it as belonging"
    );
}

/// A present-but-wrong-width genesis column is a corrupt row, not "this
/// vault has never rotated".
///
/// The `?`-on-`Option` chain folded the width failure in with the NULLs, so
/// a corrupted anchor byte answered with the identity *in force* — the
/// moving value the anchor exists not to be — and `export_pairing_payload`
/// ships that answer to every newly paired device.
/// `genesis_identity_id` already raised `CorruptRow` for the same blob.
#[test]
fn a_short_genesis_column_is_corrupt_rather_than_the_identity_in_force() {
    let root = VaultRootKey::from_bytes([0x3f; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);

    // The control: a well-formed row answers, and answers the genesis.
    let (id, _) = kc.genesis_pair(&d).expect("a well-formed row reads back");
    assert_eq!(
        id,
        kc.identity_id(),
        "an unrotated vault is its own genesis"
    );

    d.conn()
        .execute(
            "UPDATE identity SET genesis_identity_id = ? WHERE id = 1",
            params![&[0xaau8; 8][..]],
        )
        .unwrap();
    assert!(
        matches!(
            kc.genesis_pair(&d),
            Err(KeychainError::CorruptRow("identity.genesis_identity_id"))
        ),
        "a truncated fold anchor read as the identity in force"
    );
}

/// A forged signature on an op attributed to this device is a signature
/// failure, not a missing key.
///
/// `open_envelope` verifies the signature *and* opens the AEAD, and the
/// loop that tried every key discarded which of the two failed — so a
/// forgery came back as `NoStreamKey`, whose documented handling is "park
/// it in `deferred_ops` and wait for a `key_envelope`". It was retried
/// indefinitely as a transient absence.
#[test]
fn a_forged_signature_is_not_reported_as_a_missing_key() {
    let root = VaultRootKey::from_bytes([0x4f; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    let stream = [0x99u8; 16];
    let (epoch, key) = d
        .with_tx(|tx| kc.mint_epoch(tx, &stream, &SystemRng, 1_700_000_000_000))
        .expect("mint");

    let sealed = kc
        .seal_op_at(
            stream,
            1,
            sunrise_cbor::hlc::Hlc::at(42),
            b"inner op bytes",
            &SystemRng,
            epoch,
            &key,
        )
        .expect("seal");

    // The control: untouched, it opens.
    kc.open_op(&sealed).expect("an honest envelope opens");

    // Flip a bit of the signature. The key is still held and still right,
    // so the only thing wrong with this envelope is who signed it.
    let mut forged = sealed.clone();
    let n = forged.len();
    forged[n - 1] ^= 0x01;
    match kc.open_op(&forged) {
        Err(KeychainError::Envelope(_)) => {}
        Err(KeychainError::NoStreamKey) => panic!(
            "a forged signature was reported as a missing Stream key, which the \
                 deferral path retries instead of refusing"
        ),
        other => panic!("expected a signature failure, got {other:?}"),
    }
}

// ---- refusal arms (issue #604) ---------------------------------------

fn count(d: &Db, table: &str) -> i64 {
    d.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn stored_identity_id(d: &Db) -> Vec<u8> {
    d.conn()
        .query_row("SELECT identity_id FROM identity", [], |r| r.get(0))
        .unwrap()
}

/// A vault that already holds one account's `identity` row, and no device
/// of its own yet: the state [`Keychain::create`] is entered in with
/// `existing` set.
fn vault_with_identity_and_no_device(byte: u8) -> (VaultRootKey, Db, Keychain) {
    let root = VaultRootKey::from_bytes([byte; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);
    d.conn().execute("DELETE FROM local_identity", []).unwrap();
    (root, d, kc)
}

/// A pairing payload for one account cannot be adopted into a vault whose
/// identity row is another account's, and nothing is written on the way
/// out. The control opens the same vault with its own account's payload,
/// so the refusal is the identity comparison and not the missing device.
#[test]
fn a_payload_for_another_account_conflicts_with_the_vaults_identity() {
    let (root, mut d, ka) = vault_with_identity_and_no_device(0x61);
    let own_payload = ka
        .pair_device_in_process(
            &d,
            "joiner".into(),
            "test".into(),
            [0x71; 32],
            [0x72; 32],
            1_700_000_000_000,
        )
        .unwrap();

    let other_root = VaultRootKey::from_bytes([0x62; 32]);
    let mut other_db = db(&other_root);
    let kb = open(&mut other_db, &other_root);
    assert_ne!(kb.identity_id(), ka.identity_id());
    let foreign_payload = kb
        .pair_device_in_process(
            &other_db,
            "joiner".into(),
            "test".into(),
            [0x73; 32],
            [0x74; 32],
            1_700_000_000_000,
        )
        .unwrap();

    let err = Keychain::open(
        &mut d,
        root.clone(),
        &clock(),
        &SystemRng,
        &seed_of(&foreign_payload),
    )
    .unwrap_err();
    assert!(
        matches!(err, KeychainError::IdentityConflict),
        "got {err:?}"
    );
    assert_eq!(
        count(&d, "local_identity"),
        0,
        "the refusal writes no device"
    );
    assert_eq!(stored_identity_id(&d), ka.identity_id().to_vec());

    let joined =
        Keychain::open(&mut d, root, &clock(), &SystemRng, &seed_of(&own_payload)).unwrap();
    assert_eq!(joined.identity_id(), ka.identity_id());
}

/// The recovery arm of the same comparison: a blob for another account is
/// refused by a vault already holding an identity row, and one for this
/// vault's own account is adopted.
#[test]
fn a_recovery_blob_for_another_account_conflicts_with_the_vaults_identity() {
    let (root, mut d, ka) = vault_with_identity_and_no_device(0x63);
    let own_blob = recovered_payload(&ka, &[0x47; 32]);

    let other_root = VaultRootKey::from_bytes([0x64; 32]);
    let mut other_db = db(&other_root);
    let kb = open(&mut other_db, &other_root);
    let foreign_blob = recovered_payload(&kb, &[0x48; 32]);

    let err = Keychain::open(
        &mut d,
        root.clone(),
        &clock(),
        &SystemRng,
        &IdentitySeed::Recovered(Box::new(foreign_blob)),
    )
    .unwrap_err();
    assert!(
        matches!(err, KeychainError::IdentityConflict),
        "got {err:?}"
    );
    assert_eq!(
        count(&d, "local_identity"),
        0,
        "the refusal writes no device"
    );
    assert_eq!(stored_identity_id(&d), ka.identity_id().to_vec());

    let recovered = Keychain::open(
        &mut d,
        root,
        &clock(),
        &SystemRng,
        &IdentitySeed::Recovered(Box::new(own_blob)),
    )
    .unwrap();
    assert_eq!(recovered.identity_id(), ka.identity_id());
}

/// A sponsor refuses to spend a grant on a request answering another
/// account's offer. The control is the account that made the offer, which
/// grants the same request.
#[test]
fn a_sponsor_refuses_a_request_that_answers_another_accounts_offer() {
    let root_a = VaultRootKey::from_bytes([0x65; 32]);
    let mut da = db(&root_a);
    let ka = open(&mut da, &root_a);
    let root_b = VaultRootKey::from_bytes([0x66; 32]);
    let mut db_b = db(&root_b);
    let kb = open(&mut db_b, &root_b);

    let joiner = sunrise_pairing::PairingJoiner::new(
        kb.export_pairing_offer(&db_b).unwrap(),
        "joiner".into(),
        "test".into(),
        [0x75; 32],
        [0x76; 32],
    );

    let err = ka
        .issue_pairing_grant(joiner.request(), BTreeSet::new(), 1_700_000_000_000)
        .unwrap_err();
    assert!(
        matches!(err, KeychainError::IdentityConflict),
        "got {err:?}"
    );

    kb.issue_pairing_grant(joiner.request(), BTreeSet::new(), 1_700_000_000_000)
        .expect("the account that made the offer grants it");
}

/// A known vault whose stored signing secret is not the key its
/// `device_id` derives from is refused at load, rather than opening as a
/// device that signs under someone else's id.
#[test]
fn a_stored_signing_key_that_is_not_the_devices_is_refused_at_load() {
    let root = VaultRootKey::from_bytes([0x67; 32]);
    let mut d = db(&root);
    let device_id = open(&mut d, &root).device_id();

    // Wrapped under this device's AAD, so the unwrap succeeds and only the
    // derivation check can catch it.
    let other = DeviceSigningKeyPair::from_secret_bytes(&[0x5a; 32]);
    assert_ne!(device_id_from_pub(&other.public_bytes()), device_id);
    let mut secret = other.secret_bytes();
    let wrapped = wrap_secret(&root, &secret, &device_aad(&device_id), &SystemRng);
    secret.zeroize();
    d.conn()
        .execute(
            "UPDATE local_identity SET signing_secret_wrapped = ?",
            params![wrapped],
        )
        .unwrap();

    let err = Keychain::open(&mut d, root, &clock(), &SystemRng, &IdentitySeed::Own).unwrap_err();
    assert!(
        matches!(err, KeychainError::DeviceIdMismatch),
        "got {err:?}"
    );
}

/// The legacy adoption path makes the same check before minting an
/// identity, and a refused adoption leaves no identity row behind.
#[test]
fn a_legacy_vault_whose_device_id_is_not_its_keys_is_refused() {
    let root = VaultRootKey::from_bytes([0x68; 32]);
    let mut d = db(&root);
    let device_signing = DeviceSigningKeyPair::from_secret_bytes(&[0x44; 32]);
    let claimed_id = [0x99u8; 16];
    assert_ne!(
        device_id_from_pub(&device_signing.public_bytes()),
        claimed_id
    );
    let mut secret = device_signing.secret_bytes();
    let wrapped = wrap_secret(&root, &secret, &device_aad(&claimed_id), &SystemRng);
    secret.zeroize();
    d.conn()
        .execute(
            "INSERT INTO local_identity
                 (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
                 VALUES (1, ?, ?, X'00', 1)",
            params![&claimed_id[..], wrapped],
        )
        .unwrap();

    let err = Keychain::open(&mut d, root, &clock(), &SystemRng, &IdentitySeed::Own).unwrap_err();
    assert!(
        matches!(err, KeychainError::DeviceIdMismatch),
        "got {err:?}"
    );
    assert_eq!(count(&d, "identity"), 0, "no identity is minted");
}

/// `open_op` opens only this device's own envelopes. A sibling that holds
/// the very key the op was sealed under still gets `UnknownDevice`, so the
/// refusal is the device check and not a missing key.
#[test]
fn open_op_refuses_another_devices_envelope_even_with_its_key() {
    let root = VaultRootKey::from_bytes([0x69; 32]);
    let mut da = db(&root);
    let ka = open(&mut da, &root);
    let sid = [0x0d; 16];
    let (epoch, key) = da
        .with_tx(|tx| ka.mint_epoch(tx, &sid, &SystemRng, 0))
        .unwrap();
    let payload = ka
        .pair_device_in_process(
            &da,
            "joiner".into(),
            "test".into(),
            [0x77; 32],
            [0x78; 32],
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
    assert_eq!(kb.stream_keys_at(&sid, epoch), vec![key.clone()]);

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
    assert_eq!(ka.open_op(&env).unwrap(), b"from A");
    let err = kb.open_op(&env).unwrap_err();
    assert!(matches!(err, KeychainError::UnknownDevice), "got {err:?}");
}
