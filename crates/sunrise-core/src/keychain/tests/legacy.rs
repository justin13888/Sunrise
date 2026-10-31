//! Adopting a vault written before ADR-0024.

use super::*;

/// A vault written by the pre-ADR-0024 code path opens, gains an identity,
/// keeps its device id, and can still read the ops it already had.
#[test]
fn a_legacy_vault_is_adopted_once() {
    let root = VaultRootKey::from_bytes([0x77; 32]);
    let mut d = db(&root);
    let sid = [8u8; 16];

    // Build the pre-0017 state by hand: a `local_identity` row with no DH
    // secret and no `identity` row, plus an op sealed under the derived key.
    let device_signing = DeviceSigningKeyPair::from_secret_bytes(&[0x41; 32]);
    let device_id = device_id_from_pub(&device_signing.public_bytes());
    let legacy_identity = IdentitySigningKeyPair::from_secret_bytes(&[0x41; 32]);
    let legacy_cert = DeviceCert::issue(
        DeviceCertInner {
            v: 1,
            device_id,
            d_s_pub: device_signing.public_bytes(),
            d_d_pub: [0x42; 32],
            identity_id: identity_id_from_pub(&device_signing.public_bytes()),
            created_at_ms: 1,
            nickname: "old laptop".into(),
            platform: "macos".into(),
        },
        &legacy_identity,
    )
    .unwrap()
    .to_cbor()
    .unwrap();
    let mut secret = device_signing.secret_bytes();
    let wrapped = wrap_secret(&root, &secret, &device_aad(&device_id), &SystemRng);
    secret.zeroize();
    d.with_tx(|tx| {
        tx.execute(
            "INSERT INTO local_identity
                 (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
                 VALUES (1, ?, ?, ?, 1)",
            params![&device_id[..], wrapped, legacy_cert],
        )?;
        tx.execute(
            "INSERT INTO streams
                 (stream_id, head_root, last_op_seq, created_at_ms, updated_at_ms)
                 VALUES (?, X'00', 0, 0, 0)",
            params![&sid[..]],
        )?;
        Ok(())
    })
    .unwrap();

    // An op sealed under exactly the key the old derivation produced.
    let derived = legacy_derived_stream_key(&root, &sid, LEGACY_EPOCH);
    let legacy_env = sunrise_crypto::encode_envelope(
        b"a pre-0017 op",
        sid,
        device_id,
        1,
        sunrise_cbor::hlc::Hlc::at(1),
        AeadAlgId::XChaCha20Poly1305,
        LEGACY_EPOCH,
        [0x11; AEAD_NONCE_LEN],
        Some(&derived),
        &device_signing,
    )
    .unwrap();

    let kc = open(&mut d, &root);
    assert_eq!(kc.device_id(), device_id, "the device keeps its id");
    assert_eq!(
        kc.open_op(&legacy_env).unwrap(),
        b"a pre-0017 op",
        "adoption must not lose the ops the vault already had"
    );
    let parsed = DeviceCert::from_cbor(&kc.cert_blob()).unwrap();
    parsed
        .verify_binding(&kc.identity_signing_pub(), &kc.identity_id())
        .expect("re-issued under the new account identity");
    assert_eq!(parsed.body.nickname, "old laptop", "the name is kept");
    assert_eq!(parsed.body.d_d_pub, kc.device_dh_pub());
    // Meta and Inbox are in the adopted set even with no rows naming them.
    assert_eq!(
        kc.stream_keys_at(&crate::engine::META_STREAM, LEGACY_EPOCH)
            .len(),
        1
    );
    assert_eq!(
        kc.stream_keys_at(&PRIVATE_DOMAIN_BYTES, LEGACY_EPOCH).len(),
        1
    );

    // Idempotent: a second open changes nothing.
    let identity_id = kc.identity_id();
    let cert = kc.cert_blob();
    drop(kc);
    let again = open(&mut d, &root);
    assert_eq!(again.identity_id(), identity_id);
    assert_eq!(again.cert_blob(), cert);
    assert_eq!(again.open_op(&legacy_env).unwrap(), b"a pre-0017 op");
}

/// A row whose blob is the wrong length is a corrupt row, and is refused
/// or skipped rather than zero-padded into a valid-looking value.
///
/// Padding is what made this dangerous rather than merely wrong: it turns a
/// corrupt row into a value that looks well-formed everywhere it is later
/// used. A truncated `stream_id` becomes a stream of its own that the next
/// revocation mints an epoch for, and a truncated `identity_id` still
/// compares equal to itself.
///
/// Each assertion is on a path where padding is *observable*. The first
/// version of this test asserted the key cache, which cannot tell the two
/// apart — a padded id is also the wrong AAD, so the wrap fails to open and
/// the row is skipped either way — and it passed with the padding still in
/// place.
#[test]
fn a_wrong_length_blob_is_refused_rather_than_padded() {
    let root = VaultRootKey::from_bytes([0x79; 32]);

    // A `stream_keys` row whose id is the wrong length must not become a
    // stream. Asserted through `rotation_set` rather than through the key
    // cache, because the cache cannot tell the two apart: a padded id is
    // also the wrong AAD, so the wrap fails to open and the row is dropped
    // either way. `rotation_set` does no crypto — a padded `X'0011'`
    // becomes a 16-byte id naming nothing, and the next revocation mints an
    // epoch for it and seals `key_envelope` ops to a stream that does not
    // exist.
    let sid: [u8; 16] = [0x3c; 16];
    let mut d = db(&root);
    {
        let kc = open(&mut d, &root);
        d.with_tx(|tx| {
            kc.mint_epoch(tx, &sid, &SystemRng, 1)?;
            Ok(())
        })
        .unwrap();
    }
    d.with_tx(|tx| {
        tx.execute(
            "UPDATE stream_keys SET stream_id = X'0011' WHERE stream_id = ?",
            params![&sid[..]],
        )?;
        Ok(())
    })
    .unwrap();
    let kc = open(&mut d, &root);
    let set = d.with_tx(|tx| kc.rotation_set(tx)).unwrap();
    assert_eq!(
        set.streams,
        vec![crate::engine::META_STREAM, PRIVATE_DOMAIN_BYTES],
        "a truncated stream id must not be padded into a stream of its own"
    );
    // ...and it is reported rather than dropped, because the stream it
    // stands for is one a revocation did not rotate.
    assert_eq!(
        set.unrotatable,
        vec![vec![0x00u8, 0x11]],
        "the row that could not be rotated must reach the caller"
    );

    // A short `identity.identity_id` is fatal, and says which column.
    let mut d2 = db(&root);
    drop(open(&mut d2, &root));
    d2.with_tx(|tx| {
        tx.execute(
            "UPDATE identity SET identity_id = X'00112233' WHERE id = 1",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let err = Keychain::open(
        &mut d2,
        root.clone(),
        &clock(),
        &SystemRng,
        &IdentitySeed::Own,
    )
    .expect_err("a truncated identity_id is not an identity");
    assert!(
        matches!(err, KeychainError::CorruptRow("identity.identity_id")),
        "got {err:?}"
    );

    // As is a short `id_s_pub`, which `to32` used to pad.
    let mut d3 = db(&root);
    drop(open(&mut d3, &root));
    d3.with_tx(|tx| {
        tx.execute(
            "UPDATE identity SET id_s_pub = X'00112233' WHERE id = 1",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let err = Keychain::open(&mut d3, root, &clock(), &SystemRng, &IdentitySeed::Own)
        .expect_err("a truncated id_s_pub is not a key");
    assert!(
        matches!(err, KeychainError::CorruptRow("identity.id_s_pub")),
        "got {err:?}"
    );
}

/// A pre-0017 vault that already had two devices is refused rather than
/// adopted.
///
/// Adoption mints an account identity from *this* device's RNG. Two devices
/// sharing one vault root that both upgrade therefore mint two different
/// accounts, each writing `d_d_pub` only for its own `devices` row — so
/// `emit_key_envelopes`, which selects `d_d_pub IS NOT NULL`, never makes
/// the peer a recipient of a new epoch, and each device's cert fails the
/// other's `verify_binding`. The account splits in two and neither screen
/// says so. An error naming re-pairing is the only honest outcome.
#[test]
fn a_legacy_vault_with_two_devices_is_refused() {
    let root = VaultRootKey::from_bytes([0x78; 32]);
    let mut d = db(&root);

    let device_signing = DeviceSigningKeyPair::from_secret_bytes(&[0x43; 32]);
    let device_id = device_id_from_pub(&device_signing.public_bytes());
    let mut secret = device_signing.secret_bytes();
    let wrapped = wrap_secret(&root, &secret, &device_aad(&device_id), &SystemRng);
    secret.zeroize();
    d.with_tx(|tx| {
        tx.execute(
            "INSERT INTO local_identity
                 (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
                 VALUES (1, ?, ?, X'00', 1)",
            params![&device_id[..], wrapped],
        )?;
        // This device and one peer, exactly as a pre-0017 two-device
        // account records them.
        for (id, name) in [(device_id, "laptop"), ([0x99; 16], "phone")] {
            tx.execute(
                "INSERT INTO devices
                     (device_id, cert_blob, nickname, platform, created_at_ms)
                     VALUES (?, X'00', ?, 'macos', 1)",
                params![&id[..], name],
            )?;
        }
        Ok(())
    })
    .unwrap();

    let err = Keychain::open(
        &mut d,
        root.clone(),
        &clock(),
        &SystemRng,
        &IdentitySeed::Own,
    )
    .expect_err("a two-device legacy vault must not be adopted");
    assert!(
        matches!(err, KeychainError::LegacyMultiDevice(2)),
        "got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains("Re-pair"),
        "the error has to name the way forward: {message}"
    );

    // And nothing was written on the way out: a refused adoption must not
    // leave half an identity behind for the next open to load.
    let identities: i64 = d
        .conn()
        .query_row("SELECT count(*) FROM identity", [], |r| r.get(0))
        .unwrap();
    assert_eq!(identities, 0, "the refusal is total");
}
