//! `key_envelope` HPKE seals, from the outside.
//!
//! ADR-0024 decision 4 makes these blobs the whole distribution mechanism for
//! Stream keys, which puts three properties on the critical path: only the
//! named recipient opens one, the `(stream, epoch)` a blob was sealed for
//! cannot be changed after the fact, and a truncated blob is rejected as such
//! rather than mistaken for a wrong key.

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_crypto::keys::{DeviceDhKeyPair, IdentityDhKeyPair, StreamKey};
use sunrise_crypto::{
    hpke_open, hpke_open_identity, hpke_seal, key_envelope_info, HpkeError, HPKE_ENC_LEN,
    HPKE_TAG_LEN,
};

const STREAM: [u8; 16] = [0x5a; 16];

fn rng(seed: u64) -> ChaCha20Rng {
    ChaCha20Rng::seed_from_u64(seed)
}

#[test]
fn a_device_opens_the_epoch_it_was_sealed() {
    let mut r = rng(1);
    let device = DeviceDhKeyPair::generate(&mut r);
    let key = StreamKey::from_bytes([0x31; 32]);
    let info = key_envelope_info(&STREAM, 4);

    let sealed = hpke_seal(&device.public_bytes(), &info, key.as_bytes(), b"", &mut r).unwrap();
    assert_eq!(sealed.len(), HPKE_ENC_LEN + 32 + HPKE_TAG_LEN);
    let opened = hpke_open(&device, &info, &sealed, b"").unwrap();
    assert_eq!(opened.as_slice(), key.as_bytes());
}

/// The recovery half: the same key sealed a second time to the identity, so a
/// recovery code with no surviving device still reaches the content.
#[test]
fn the_identity_opens_its_own_copy() {
    let mut r = rng(2);
    let identity = IdentityDhKeyPair::generate(&mut r);
    let key = StreamKey::from_bytes([0x32; 32]);
    let info = key_envelope_info(&STREAM, 1);

    let sealed = hpke_seal(&identity.public_bytes(), &info, key.as_bytes(), b"", &mut r).unwrap();
    assert_eq!(
        hpke_open_identity(&identity, &info, &sealed, b"").unwrap(),
        key.as_bytes()
    );
}

/// Revocation is only meaningful if this holds: a device that is not a
/// recipient of an epoch's envelope cannot open it, even holding the vault
/// root and the whole op log.
#[test]
fn another_device_cannot_open_it() {
    let mut r = rng(3);
    let intended = DeviceDhKeyPair::generate(&mut r);
    let revoked = DeviceDhKeyPair::generate(&mut r);
    let info = key_envelope_info(&STREAM, 7);

    let sealed = hpke_seal(&intended.public_bytes(), &info, &[9u8; 32], b"", &mut r).unwrap();
    assert_eq!(
        hpke_open(&revoked, &info, &sealed, b""),
        Err(HpkeError::Open)
    );
}

/// The `info` string binds the blob to one `(stream_id, epoch)`. Without this
/// an envelope for epoch 3 could be replayed as the envelope for epoch 4, and
/// a rotation would distribute the key it just replaced.
#[test]
fn a_blob_cannot_be_replayed_into_another_epoch_or_stream() {
    let mut r = rng(4);
    let device = DeviceDhKeyPair::generate(&mut r);
    let sealed = hpke_seal(
        &device.public_bytes(),
        &key_envelope_info(&STREAM, 3),
        &[1u8; 32],
        b"",
        &mut r,
    )
    .unwrap();

    for wrong in [
        key_envelope_info(&STREAM, 4),
        key_envelope_info(&[0x5b; 16], 3),
    ] {
        assert_eq!(
            hpke_open(&device, &wrong, &sealed, b""),
            Err(HpkeError::Open)
        );
    }
}

/// A short blob is `TooShort`, not `Open`: the two are told apart because one
/// is a malformed frame and the other is a failed authentication, and a caller
/// that logs them the same way cannot tell a bug from an attack.
#[test]
fn a_truncated_blob_is_refused_as_truncated() {
    let mut r = rng(5);
    let device = DeviceDhKeyPair::generate(&mut r);
    let info = key_envelope_info(&STREAM, 1);
    let sealed = hpke_seal(&device.public_bytes(), &info, &[2u8; 32], b"", &mut r).unwrap();

    for len in [0, HPKE_ENC_LEN, HPKE_ENC_LEN + HPKE_TAG_LEN - 1] {
        assert_eq!(
            hpke_open(&device, &info, &sealed[..len], b""),
            Err(HpkeError::TooShort),
            "a {len}-byte blob cannot be an HPKE output"
        );
    }
    // One byte past the floor is long enough to *parse* and must then fail
    // authentication, not length.
    assert_eq!(
        hpke_open(&device, &info, &sealed[..HPKE_ENC_LEN + HPKE_TAG_LEN], b""),
        Err(HpkeError::Open)
    );
}

/// A flipped bit anywhere — in the encapsulated key or in the ciphertext —
/// fails. HPKE gives no partial results.
#[test]
fn tampering_fails_everywhere() {
    let mut r = rng(6);
    let device = DeviceDhKeyPair::generate(&mut r);
    let info = key_envelope_info(&STREAM, 2);
    let sealed = hpke_seal(&device.public_bytes(), &info, &[3u8; 32], b"", &mut r).unwrap();

    for idx in [0, HPKE_ENC_LEN, sealed.len() - 1] {
        let mut bad = sealed.clone();
        bad[idx] ^= 0x01;
        assert!(
            hpke_open(&device, &info, &bad, b"").is_err(),
            "a flipped bit at {idx} must not open"
        );
    }
}

/// The AAD is authenticated even though it is not encrypted.
#[test]
fn the_aad_is_bound() {
    let mut r = rng(7);
    let device = DeviceDhKeyPair::generate(&mut r);
    let info = key_envelope_info(&STREAM, 1);
    let sealed = hpke_seal(&device.public_bytes(), &info, &[4u8; 32], b"bound", &mut r).unwrap();
    assert!(hpke_open(&device, &info, &sealed, b"bound").is_ok());
    assert_eq!(
        hpke_open(&device, &info, &sealed, b"other"),
        Err(HpkeError::Open)
    );
}

/// Two seals of the same plaintext to the same recipient differ, because the
/// KEM key is ephemeral. A deterministic output here would leak that two
/// envelopes carry the same key.
#[test]
fn seals_are_randomised() {
    let mut r = rng(8);
    let device = DeviceDhKeyPair::generate(&mut r);
    let info = key_envelope_info(&STREAM, 1);
    let a = hpke_seal(&device.public_bytes(), &info, &[5u8; 32], b"", &mut r).unwrap();
    let b = hpke_seal(&device.public_bytes(), &info, &[5u8; 32], b"", &mut r).unwrap();
    assert_ne!(a, b, "the ephemeral KEM key must not repeat");
    assert_eq!(hpke_open(&device, &info, &a, b"").unwrap(), vec![5u8; 32]);
    assert_eq!(hpke_open(&device, &info, &b, b"").unwrap(), vec![5u8; 32]);
}

/// A non-canonical recipient key is refused at the boundary rather than
/// producing a blob nobody can open.
#[test]
fn a_malformed_recipient_key_is_refused() {
    // All-zero is a valid 32-byte encoding but a low-order point; HPKE's
    // encapsulation refuses it because the DH result is all zeros.
    let mut r = rng(9);
    let info = key_envelope_info(&STREAM, 1);
    assert_eq!(
        hpke_seal(&[0u8; 32], &info, &[6u8; 32], b"", &mut r),
        Err(HpkeError::Seal)
    );
}
