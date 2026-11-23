//! `share_grant`'s unit tests: the codec, the seal and open, what the
//! signature and the AAD bind, expiry, and the re-seal a rotation emits. The
//! byte-exact freeze is `tests/frozen_share_grant.rs`.

use super::*;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;

struct Party {
    id_s: IdentitySigningKeyPair,
    id_d: IdentityDhKeyPair,
    identity_id: [u8; 16],
}

fn identity(seed: u8) -> Party {
    let id_s = IdentitySigningKeyPair::from_secret_bytes(&[seed; 32]);
    let id_d = IdentityDhKeyPair::from_secret_bytes([seed.wrapping_add(1); 32]);
    let identity_id = identity_id_from_pub(&id_s.public_bytes());
    Party {
        id_s,
        id_d,
        identity_id,
    }
}

fn metadata() -> StreamMetadata {
    StreamMetadata {
        name: "Garden".into(),
        color: "green".into(),
    }
}

fn terms(to: &Party, expires_at_ms: Option<u64>) -> ShareGrantTerms {
    ShareGrantTerms {
        stream_id: [0x5a; 16],
        epoch: 4,
        recipient_identity_id: to.identity_id,
        role: ShareRole::Viewer,
        expires_at_ms,
    }
}

fn grant(from: &Party, to: &Party, expires_at_ms: Option<u64>) -> ShareGrant {
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    seal_share_grant(
        &from.id_s,
        terms(to, expires_at_ms),
        &to.id_d.public_bytes(),
        &StreamKey::from_bytes([0x33; 32]),
        &metadata(),
        &mut rng,
    )
    .expect("seal")
}

fn open(
    g: &ShareGrant,
    from: &Party,
    to: &Party,
    now_ms: u64,
) -> Result<OpenedShareGrant, ShareGrantError> {
    g.open(&from.id_s.public_bytes(), &to.identity_id, &to.id_d, now_ms)
}

/// The encoding with one field replaced, re-encoded canonically.
fn with_field(g: &ShareGrant, key: u64, value: Option<Value>) -> Vec<u8> {
    let Value::Map(mut entries) = sunrise_cbor::decode_canonical(&g.encode().unwrap()).unwrap()
    else {
        unreachable!("a grant is a map");
    };
    entries.retain(|(k, _)| k.as_integer() != Some(Integer::from(key)));
    if let Some(v) = value {
        entries.push((int(key), v));
    }
    sunrise_cbor::encode_canonical(&Value::Map(entries)).unwrap()
}

#[test]
fn a_grant_round_trips_and_opens_for_its_recipient() {
    let (alice, bob) = (identity(1), identity(2));
    let g = grant(&alice, &bob, Some(10_000));
    let bytes = g.encode().unwrap();
    let decoded = decode_share_grant(&bytes).unwrap();
    assert_eq!(decoded, g);
    assert_eq!(decoded.encode().unwrap(), bytes);
    assert_eq!(
        decoded.verify(&alice.id_s.public_bytes(), &bob.id_d.public_bytes()),
        Ok(())
    );
    let opened = open(&decoded, &alice, &bob, 9_999).unwrap();
    assert_eq!(opened.key, StreamKey::from_bytes([0x33; 32]));
    assert_eq!(opened.metadata, metadata());
}

#[test]
fn a_grant_that_never_expires_omits_field_5() {
    let (alice, bob) = (identity(1), identity(2));
    let g = grant(&alice, &bob, None);
    let bytes = g.encode().unwrap();
    assert_eq!(with_field(&g, 5, None), bytes);
    assert_eq!(
        decode_share_grant(&bytes).unwrap().terms().expires_at_ms,
        None
    );
    assert!(open(&g, &alice, &bob, u64::MAX).is_ok());
}

#[test]
fn an_expired_grant_does_not_open() {
    let (alice, bob) = (identity(1), identity(2));
    let g = grant(&alice, &bob, Some(1_000));
    assert!(!g.terms().is_expired_at(999));
    assert!(g.terms().is_expired_at(1_000));
    assert!(open(&g, &alice, &bob, 999).is_ok());
    assert_eq!(open(&g, &alice, &bob, 1_000), Err(ShareGrantError::Expired));
    assert_eq!(open(&g, &alice, &bob, 1_001), Err(ShareGrantError::Expired));
}

#[test]
fn another_identity_cannot_open_it() {
    let (alice, bob, carol) = (identity(1), identity(2), identity(3));
    let g = grant(&alice, &bob, None);
    assert_eq!(
        open(&g, &alice, &carol, 0),
        Err(ShareGrantError::OtherRecipient)
    );
    // Carol claiming Bob's identity still holds only her own ID_D, which is
    // not the key Alice signed the grant as sealed to.
    assert_eq!(
        g.open(&alice.id_s.public_bytes(), &bob.identity_id, &carol.id_d, 0),
        Err(ShareGrantError::BadSignature)
    );
}

/// The `ID_D_pub` the grant is sealed to is under the signature, though not
/// on the wire: the same grant beside any other key does not verify, so
/// nobody can name another key as the one the granter sealed to.
#[test]
fn the_key_it_is_sealed_to_is_signed() {
    let (alice, bob, carol) = (identity(1), identity(2), identity(3));
    let g = grant(&alice, &bob, None);
    let pk = alice.id_s.public_bytes();
    assert_eq!(g.verify(&pk, &bob.id_d.public_bytes()), Ok(()));
    assert_eq!(
        g.verify(&pk, &carol.id_d.public_bytes()),
        Err(ShareGrantError::BadSignature)
    );
}

/// Every field is under the signature: changing any one of them fails
/// verification.
#[test]
fn every_field_is_signed() {
    let (alice, bob) = (identity(1), identity(2));
    let g = grant(&alice, &bob, Some(5));
    let pk = alice.id_s.public_bytes();
    let to = bob.id_d.public_bytes();
    let mut ct = g.hpke_ct.clone();
    ct[40] ^= 1;
    for bytes in [
        with_field(&g, 1, Some(Value::Bytes(vec![0x5b; 16]))),
        with_field(&g, 2, Some(int(5))),
        with_field(&g, 3, Some(Value::Bytes(vec![9; 16]))),
        with_field(&g, 5, Some(int(6))),
        with_field(&g, 5, None),
        with_field(&g, 6, Some(Value::Bytes(ct))),
    ] {
        let altered = decode_share_grant(&bytes).unwrap();
        assert_eq!(altered.verify(&pk, &to), Err(ShareGrantError::BadSignature));
    }
    assert_eq!(
        g.verify(&bob.id_s.public_bytes(), &to),
        Err(ShareGrantError::BadSignature)
    );
}

/// A third identity that re-signs the owner's ciphertext as its own grant
/// produces a grant that verifies under the impostor and does not open:
/// the granter's identity is in the AAD.
#[test]
fn a_re_signed_ciphertext_does_not_open_under_the_impostor() {
    let (alice, bob, mallory) = (identity(1), identity(2), identity(4));
    let g = grant(&alice, &bob, None);
    let forged = ShareGrant {
        identity_sig: mallory
            .id_s
            .sign(&signed_message(&g.terms, &g.hpke_ct, &bob.id_d.public_bytes()).unwrap()),
        ..g.clone()
    };
    assert_eq!(
        forged.verify(&mallory.id_s.public_bytes(), &bob.id_d.public_bytes()),
        Ok(())
    );
    assert_eq!(open(&forged, &mallory, &bob, 0), Err(ShareGrantError::Open));
    assert!(open(&g, &alice, &bob, 0).is_ok());
}

#[test]
fn a_role_this_build_does_not_grant_is_refused() {
    let (alice, bob) = (identity(1), identity(2));
    let g = grant(&alice, &bob, None);
    assert_eq!(
        decode_share_grant(&with_field(&g, 4, Some(Value::Text("editor".into())))),
        Err(ShareGrantError::UnsupportedRole)
    );
    assert_eq!(
        decode_share_grant(&with_field(&g, 4, Some(int(0)))),
        Err(ShareGrantError::Malformed("role"))
    );
}

#[test]
fn a_malformed_grant_is_refused() {
    let (alice, bob) = (identity(1), identity(2));
    let g = grant(&alice, &bob, Some(1));
    let bytes = g.encode().unwrap();
    let malformed = |b: &[u8]| matches!(decode_share_grant(b), Err(ShareGrantError::Malformed(_)));
    assert!(malformed(&with_field(&g, 8, Some(int(1)))), "unknown field");
    for missing in [1, 2, 3, 4, 6, 7] {
        assert!(malformed(&with_field(&g, missing, None)), "field {missing}");
    }
    assert!(malformed(&with_field(
        &g,
        1,
        Some(Value::Bytes(vec![0; 15]))
    )));
    assert!(malformed(&with_field(
        &g,
        2,
        Some(int(u64::from(u32::MAX) + 1))
    )));
    assert!(malformed(&with_field(
        &g,
        5,
        Some(Value::Text("soon".into()))
    )));
    assert!(malformed(&with_field(
        &g,
        7,
        Some(Value::Bytes(vec![0; 63]))
    )));
    assert!(malformed(&bytes[..bytes.len() - 1]), "truncated");
    assert!(
        malformed(&[bytes.as_slice(), &[0]].concat()),
        "trailing byte"
    );
    assert!(
        malformed(&vec![0u8; MAX_SHARE_GRANT_BYTES + 1]),
        "too large"
    );
    // The same map with its keys out of canonical order.
    let Value::Map(mut entries) = sunrise_cbor::decode_canonical(&bytes).unwrap() else {
        unreachable!("a grant is a map");
    };
    entries.rotate_left(1);
    let mut reordered = Vec::new();
    ciborium::into_writer(&Value::Map(entries), &mut reordered).unwrap();
    assert!(malformed(&reordered), "non-canonical key order");
}

/// A plaintext that is not exactly `{1: key, 2: {1: name, 2: color}}` does not
/// open, even sealed and signed by the granter itself.
#[test]
fn a_plaintext_of_the_wrong_shape_does_not_open() {
    let (alice, bob) = (identity(1), identity(2));
    let t = terms(&bob, None);
    let meta = map(&[
        (1, Value::Text("Garden".into())),
        (2, Value::Text("green".into())),
    ]);
    let key = Value::Bytes(vec![0x33; 32]);
    for plaintext in [
        map(&[(1, key.clone())]),
        map(&[(2, meta.clone())]),
        map(&[(1, key.clone()), (2, meta.clone()), (3, int(0))]),
        map(&[(1, Value::Bytes(vec![0x33; 31])), (2, meta.clone())]),
        map(&[
            (1, key.clone()),
            (2, map(&[(1, Value::Text("Garden".into()))])),
        ]),
        map(&[
            (1, key.clone()),
            (
                2,
                map(&[
                    (
                        1,
                        Value::Text("n".repeat(MAX_SHARE_METADATA_TEXT_BYTES + 1)),
                    ),
                    (2, Value::Text("green".into())),
                ]),
            ),
        ]),
        Value::Array(vec![key.clone(), meta.clone()]),
    ] {
        let mut rng = ChaCha20Rng::seed_from_u64(7);
        let hpke_ct = hpke_seal(
            &bob.id_d.public_bytes(),
            &share_grant_info(&t.stream_id, t.epoch, &t.recipient_identity_id),
            &encode(&plaintext).unwrap(),
            &aad(&alice.identity_id, &t).unwrap(),
            &mut rng,
        )
        .unwrap();
        let identity_sig = alice
            .id_s
            .sign(&signed_message(&t, &hpke_ct, &bob.id_d.public_bytes()).unwrap());
        let g = ShareGrant {
            terms: t.clone(),
            hpke_ct,
            identity_sig,
        };
        assert_eq!(open(&g, &alice, &bob, 0), Err(ShareGrantError::Open));
    }
}

#[test]
fn metadata_past_its_bound_is_not_sealed() {
    let (alice, bob) = (identity(1), identity(2));
    let mut rng = ChaCha20Rng::seed_from_u64(7);
    let long = StreamMetadata {
        name: "n".repeat(MAX_SHARE_METADATA_TEXT_BYTES + 1),
        color: "green".into(),
    };
    assert_eq!(
        seal_share_grant(
            &alice.id_s,
            terms(&bob, None),
            &bob.id_d.public_bytes(),
            &StreamKey::from_bytes([1; 32]),
            &long,
            &mut rng,
        ),
        Err(ShareGrantError::Malformed("metadata too long"))
    );
}

fn recipient(who: &Party, expires_at_ms: Option<u64>) -> ShareRecipient {
    ShareRecipient {
        identity_id: who.identity_id,
        id_d_pub: who.id_d.public_bytes(),
        expires_at_ms,
    }
}

/// A rotation seals the new epoch to every recipient but the revoked and
/// the expired, in identity order, each opening to the new key.
#[test]
fn a_rotation_reseals_to_every_recipient_but_the_revoked_and_the_expired() {
    let alice = identity(1);
    let kept_a = identity(2);
    let revoked = identity(3);
    let expired = identity(5);
    let kept_b = identity(6);
    let key = StreamKey::from_bytes([0x44; 32]);
    let meta = metadata();
    let shared = SharedEpoch {
        stream_id: [0x5a; 16],
        epoch: 9,
        key: &key,
        metadata: &meta,
    };
    let recipients = [
        recipient(&kept_b, Some(2_000)),
        recipient(&expired, Some(1_000)),
        recipient(&revoked, None),
        recipient(&kept_a, None),
    ];
    let mut rng = ChaCha20Rng::seed_from_u64(3);
    let grants = reseal_share_grants(
        &alice.id_s,
        shared,
        &recipients,
        &[revoked.identity_id],
        1_000,
        &mut rng,
    )
    .unwrap();
    let mut expected = vec![kept_a.identity_id, kept_b.identity_id];
    expected.sort_unstable();
    assert_eq!(
        grants
            .iter()
            .map(|g| g.terms().recipient_identity_id)
            .collect::<Vec<_>>(),
        expected
    );
    for (who, expires) in [(&kept_a, None), (&kept_b, Some(2_000))] {
        let g = grants
            .iter()
            .find(|g| g.terms().recipient_identity_id == who.identity_id)
            .unwrap();
        assert_eq!(g.terms().epoch, 9);
        assert_eq!(g.terms().expires_at_ms, expires);
        assert_eq!(g.terms().role, ShareRole::Viewer);
        assert_eq!(open(g, &alice, who, 1_000).unwrap().key, key);
    }
}

#[test]
fn a_rotation_refuses_a_recipient_named_twice() {
    let (alice, bob, carol) = (identity(1), identity(2), identity(3));
    let key = StreamKey::from_bytes([0x44; 32]);
    let meta = metadata();
    let shared = SharedEpoch {
        stream_id: [0x5a; 16],
        epoch: 9,
        key: &key,
        metadata: &meta,
    };
    let twice = [
        recipient(&bob, None),
        recipient(&carol, None),
        ShareRecipient {
            id_d_pub: carol.id_d.public_bytes(),
            ..recipient(&bob, Some(5))
        },
    ];
    let mut rng = ChaCha20Rng::seed_from_u64(3);
    assert_eq!(
        reseal_share_grants(&alice.id_s, shared, &twice, &[], 0, &mut rng),
        Err(ShareGrantError::DuplicateRecipient)
    );
}
