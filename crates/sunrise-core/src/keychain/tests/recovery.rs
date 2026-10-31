//! Who holds the identity key, and what a recovery blob restores.

use super::*;

/// The sole-copy condition is *observable*, on the device it applies to and
/// on the ones it does not.
///
/// Dropping `ID_D_priv` from `PairingPayload` moved the account identity's
/// unwrapping key from "on every device" to "on exactly one", and the
/// second copy it is supposed to have — the recovery blob — is not built.
/// So for every vault created from now on there is a window in which one
/// machine holds a key that cannot be reconstructed from anywhere else, and
/// nothing in the tree said so. A caller cannot warn about a condition it
/// cannot ask about, which is what this pins.
#[test]
fn only_the_account_creator_holds_the_identity_key() {
    let root = VaultRootKey::from_bytes([0x6f; 32]);
    let mut creator_db = db(&root);
    let creator = open(&mut creator_db, &root);
    assert!(
        creator.holds_only_copy_of_identity_key(),
        "the account's creator holds `ID_D_priv`, and today holds the only copy"
    );

    let payload = creator
        .pair_device_in_process(
            &creator_db,
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
    assert!(
        !paired.holds_only_copy_of_identity_key(),
        "a paired device holds no copy at all, so it cannot hold the only one"
    );

    // And the answer survives a reopen on both sides: it is a property of
    // what the vault stores, not of how it was constructed this session.
    drop(creator);
    drop(paired);
    assert!(open(&mut creator_db, &root).holds_only_copy_of_identity_key());
    assert!(!open(&mut paired_db, &root).holds_only_copy_of_identity_key());
}

/// The recovery blob is the second copy of `ID_D_priv`, and this is the
/// whole of it: the creator seals one, the code that opens it recovers the
/// identity byte for byte, and a wrong code recovers nothing.
///
/// `sunrise_client_core::recovery::recover_identity` is the other end and asserts the
/// same thing against a hand-built payload; what this adds is that the
/// bytes come out of a *real vault* rather than out of a fixture, which is
/// the step nothing exercised before — `seal_recovery_blob` had no caller
/// outside `sunrise-crypto`'s own tests.
#[test]
fn the_creators_recovery_blob_restores_its_identity() {
    let root = VaultRootKey::from_bytes([0x7a; 32]);
    let mut d = db(&root);
    let kc = open(&mut d, &root);

    let seed = *sunrise_crypto::bip39::decode_recovery_code(
        sunrise_crypto::bip39::encode_recovery_code(&[0x11; 32]).reveal(),
    )
    .expect("a code this workspace produced decodes");
    let blob = kc.seal_recovery_blob(&seed, &SystemRng).unwrap();

    let payload = sunrise_crypto::unseal_recovery_blob(&blob, &seed, &kc.identity_id()).unwrap();
    assert_eq!(payload.id_s_pub, kc.identity_signing_pub());
    assert_eq!(payload.id_d_pub, kc.identity_dh_pub());
    assert_eq!(payload.identity_id, kc.identity_id());
    assert_eq!(
        payload.created_at_ms,
        clock().now_ms(),
        "field 6 is the account's creation time, read from the identity row"
    );

    // The identity keys really are the vault's: an `ID_D_priv` that does
    // not match the `ID_D_pub` every `key_envelope` was sealed to would
    // restore an account that reads nothing.
    assert_eq!(
        sunrise_crypto::keys::IdentityDhKeyPair::from_secret_bytes(payload.id_d_priv)
            .public_bytes(),
        kc.identity_dh_pub()
    );

    let wrong = [0u8; 32];
    assert!(sunrise_crypto::unseal_recovery_blob(&blob, &wrong, &kc.identity_id()).is_err());
}

/// A paired device cannot seal a recovery blob at all.
///
/// It holds `ID_D_pub` and not `ID_D_priv` (#76), so the blob it produced
/// would restore an identity that opens none of the identity-addressed
/// `key_envelope` ops the recovery path exists to replay. A recovery code
/// that decrypts to a useless key is worse than none, because the user
/// believes they have one — so this refuses rather than sealing something
/// partial.
#[test]
fn a_paired_device_cannot_seal_a_recovery_blob() {
    let root = VaultRootKey::from_bytes([0x7b; 32]);
    let mut creator_db = db(&root);
    let creator = open(&mut creator_db, &root);
    let payload = creator
        .pair_device_in_process(
            &creator_db,
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

    assert!(matches!(
        paired.seal_recovery_blob(&[9u8; 32], &SystemRng),
        Err(KeychainError::IdentitySecretAbsent)
    ));
    assert!(creator.seal_recovery_blob(&[9u8; 32], &SystemRng).is_ok());
}

// ---- recovery -------------------------------------------------------

/// The whole of what `Unlock::RecoveryCode` could not express before: a
/// vault seeded from an opened blob **keeps** `ID_D_priv`.
///
/// Without it the vault can seal nothing and — far worse — can open none
/// of the identity-addressed `key_envelope` ops that are its only route to
/// the account's Stream keys. The mode was routed through `PairingPayload`,
/// which stopped carrying that key at `#76`, so it came up with
/// `dh_secret: None` and read nothing.
#[test]
fn a_vault_recovered_from_a_blob_holds_the_identity_key() {
    let founding_root = VaultRootKey::from_bytes([0x71; 32]);
    let mut founding_db = db(&founding_root);
    let founder = open(&mut founding_db, &founding_root);
    assert!(founder.holds_only_copy_of_identity_key());
    let payload = recovered_payload(&founder, &[0x42; 32]);

    // A different root, because the blob carries no vault root and a
    // recovering device has nobody to be handed one by.
    let recovered_root = VaultRootKey::from_bytes([0x17; 32]);
    let mut recovered_db = db(&recovered_root);
    let recovered = Keychain::open(
        &mut recovered_db,
        recovered_root.clone(),
        &clock(),
        &SystemRng,
        &IdentitySeed::Recovered(Box::new(payload)),
    )
    .unwrap();

    assert!(
        recovered.holds_only_copy_of_identity_key(),
        "a recovered vault holds ID_D_priv, or it opens no identity envelope"
    );
    assert_eq!(recovered.identity_id(), founder.identity_id());
    assert_eq!(
        recovered.identity_dh_pub(),
        founder.identity_dh_pub(),
        "the same identity, so the envelopes already sealed to it are this vault's"
    );
    assert_ne!(
        recovered.device_id(),
        founder.device_id(),
        "a recovery re-keys the device: fresh D_S/D_D under the restored identity"
    );
    assert!(!recovered.cert_blob().is_empty());

    // And it survives a reopen, which is what `minted_by_device_id`
    // decides: `unwrap_identity` hands `ID_D_priv` back only to the device
    // the row names, so a recovery that did not record itself there would
    // hold the key exactly once and lose it at the next launch.
    let reopened = Keychain::open(
        &mut recovered_db,
        recovered_root,
        &clock(),
        &SystemRng,
        &IdentitySeed::Own,
    )
    .unwrap();
    assert!(
        reopened.holds_only_copy_of_identity_key(),
        "the restored ID_D_priv has to survive closing the app"
    );
    assert_eq!(reopened.device_id(), recovered.device_id());
}

/// The account's creation time comes off the blob rather than off the
/// clock the recovery ran on, so a blob re-sealed afterwards reports the
/// same `created_at` as the one that produced it.
#[test]
fn a_recovered_vault_keeps_the_accounts_creation_time() {
    let founding_root = VaultRootKey::from_bytes([0x72; 32]);
    let mut founding_db = db(&founding_root);
    let founder = open(&mut founding_db, &founding_root);
    let payload = recovered_payload(&founder, &[0x43; 32]);
    let created_at = payload.created_at_ms;

    let recovered_root = VaultRootKey::from_bytes([0x18; 32]);
    let mut recovered_db = db(&recovered_root);
    let recovered = Keychain::open(
        &mut recovered_db,
        recovered_root,
        &clock(),
        &SystemRng,
        &IdentitySeed::Recovered(Box::new(payload)),
    )
    .unwrap();

    let resealed = recovered
        .seal_recovery_blob(&[0x44; 32], &SystemRng)
        .unwrap();
    let reopened =
        sunrise_crypto::unseal_recovery_blob(&resealed, &[0x44; 32], &founder.identity_id())
            .unwrap();
    assert_eq!(reopened.created_at_ms, created_at);
    assert_eq!(reopened.id_d_pub, founder.identity_dh_pub());
}

/// `sunrise.meta_genesis_key.v1`, anchored to a frozen literal.
///
/// The test below proves two vaults *agree* with each other, which is a
/// property they keep while both have drifted away from what every vault
/// already written holds. This one pins the bytes: the derivation is the
/// one Stream key in the hierarchy that is computed rather than drawn, and
/// it is computed from a secret and an id that never travel together, so
/// nothing on the wire or on disk would reveal that a build derives it
/// differently. A device restored from the recovery code would simply read
/// nothing, for ever, while looking healthy — because every `key_envelope`
/// op lives in the stream this key opens.
///
/// The expectation is a literal in `sunrise-crypto-test-vectors`, a crate
/// with no dependencies, so renaming [`META_GENESIS_CONTEXT`] cannot be
/// absorbed by editing the assertion beside it.
#[test]
fn the_vault_meta_genesis_key_is_byte_exact() {
    use sunrise_crypto_test_vectors::at_rest::meta_genesis as m;

    let root = VaultRootKey::from_bytes([0x7a; 32]);
    let mut d = db(&root);
    let id_s = IdentitySigningKeyPair::from_secret_bytes(&m::ID_S_PRIV);
    let id_d = IdentityDhKeyPair::from_secret_bytes(m::ID_D_PRIV);
    let payload = sunrise_crypto::recovery::RecoveryPayload {
        id_s_priv: m::ID_S_PRIV,
        id_d_priv: m::ID_D_PRIV,
        id_s_pub: id_s.public_bytes(),
        id_d_pub: id_d.public_bytes(),
        identity_id: identity_id_from_pub(&id_s.public_bytes()),
        created_at_ms: 1_700_000_000_000,
        account_id_s_pub: None,
        meta_genesis_keys: Vec::new(),
    };
    assert_eq!(
        payload.identity_id,
        m::IDENTITY_ID,
        "the frozen identity id is not the one this seed derives"
    );

    let kc = Keychain::open(
        &mut d,
        root,
        &clock(),
        &SystemRng,
        &IdentitySeed::Recovered(Box::new(payload)),
    )
    .expect("open a recovered vault");

    let key = kc
        .meta_genesis_key(&m::META_STREAM, m::GENESIS_EPOCH)
        .expect("a recovered vault holds ID_D_priv and derives the key");
    assert_eq!(
        key.as_bytes(),
        &m::KEY,
        "the sunrise.meta_genesis_key.v1 derivation drifted — every vault \
             restored from a recovery code would read nothing"
    );
}

/// The cycle [`Keychain::meta_genesis_key`] exists to break, asserted as
/// the agreement it produces: a recovering vault that has never met the
/// founder computes the founder's vault-meta genesis key.
///
/// Everything else in a recovery hangs off this. Every `key_envelope` op
/// lives in the vault-meta stream, so a device that cannot open that
/// stream's first epoch cannot open the op that would have given it the
/// key — and reads nothing, for ever, while looking perfectly healthy.
#[test]
fn a_recovered_vault_agrees_with_the_founder_on_the_vault_meta_genesis_key() {
    let founding_root = VaultRootKey::from_bytes([0x73; 32]);
    let mut founding_db = db(&founding_root);
    let founder = open(&mut founding_db, &founding_root);
    let payload = recovered_payload(&founder, &[0x45; 32]);

    let founder_key = founding_db
        .with_tx(|tx| founder.mint_epoch(tx, &crate::engine::META_STREAM, &SystemRng, 1))
        .unwrap()
        .1;

    let recovered_root = VaultRootKey::from_bytes([0x19; 32]);
    let mut recovered_db = db(&recovered_root);
    let recovered = Keychain::open(
        &mut recovered_db,
        recovered_root,
        &clock(),
        &SystemRng,
        &IdentitySeed::Recovered(Box::new(payload)),
    )
    .unwrap();
    let recovered_key = recovered_db
        .with_tx(|tx| recovered.mint_epoch(tx, &crate::engine::META_STREAM, &SystemRng, 1))
        .unwrap()
        .1;

    assert_eq!(
        founder_key, recovered_key,
        "two vaults sharing no root and no device must still agree on the \
             vault-meta genesis key, or a pure recovery reads nothing"
    );

    // And the derivation is confined to that one slot: epoch 2 of the same
    // stream, and epoch 1 of any other, are still drawn at random.
    let later = founding_db
        .with_tx(|tx| founder.mint_epoch(tx, &crate::engine::META_STREAM, &SystemRng, 2))
        .unwrap();
    assert_eq!(later.0, 2);
    assert_ne!(later.1, founder_key);
    let other = founding_db
        .with_tx(|tx| founder.mint_epoch(tx, &[9u8; 16], &SystemRng, 3))
        .unwrap();
    assert_eq!(other.0, 1);
    assert_ne!(other.1, founder_key);
}

/// A paired device derives nothing here, because it holds no `ID_D_priv`
/// — and it must not, or withholding an epoch from a revoked device would
/// withhold nothing.
#[test]
fn a_paired_device_does_not_derive_the_vault_meta_genesis_key() {
    let root = VaultRootKey::from_bytes([0x74; 32]);
    let mut founding_db = db(&root);
    let founder = open(&mut founding_db, &root);
    let payload = founder
        .pair_device_in_process(
            &founding_db,
            "joiner".into(),
            "test".into(),
            [0x76; 32],
            [0x77; 32],
            1_700_000_000_000,
        )
        .unwrap();

    let mut paired_db = db(&root);
    let paired = Keychain::open(
        &mut paired_db,
        root,
        &clock(),
        &SystemRng,
        &seed_of(&payload),
    )
    .unwrap();
    assert!(!paired.holds_only_copy_of_identity_key());
    assert!(paired
        .meta_genesis_key(&crate::engine::META_STREAM, 1)
        .is_none());
    assert!(founder
        .meta_genesis_key(&crate::engine::META_STREAM, 1)
        .is_some());
}

/// A blob whose public halves disagree with its private ones is refused at
/// the door, rather than becoming a vault that silently reads nothing.
#[test]
fn a_malformed_recovery_payload_is_refused() {
    type Payload = sunrise_crypto::recovery::RecoveryPayload;
    let founding_root = VaultRootKey::from_bytes([0x75; 32]);
    let mut founding_db = db(&founding_root);
    let founder = open(&mut founding_db, &founding_root);

    let mutations: [fn(&mut Payload); 3] = [
        |p| p.id_d_pub = [0x01; 32],
        |p| p.id_s_pub = [0x02; 32],
        |p| p.identity_id = [0x03; 16],
    ];
    for mutate in mutations {
        let mut payload = recovered_payload(&founder, &[0x46; 32]);
        mutate(&mut payload);
        let root = VaultRootKey::from_bytes([0x1a; 32]);
        let mut d = db(&root);
        let err = Keychain::open(
            &mut d,
            root,
            &clock(),
            &SystemRng,
            &IdentitySeed::Recovered(Box::new(payload)),
        )
        .unwrap_err();
        assert!(
            matches!(err, KeychainError::IdentityIdMismatch),
            "got {err:?}"
        );
    }
}
