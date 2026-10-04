//! Stream-key unwrap: the HPKE `key_envelope` open a device runs on a key sent
//! to it, and the vault-root unwrap it runs on a key it stored (ADR-0024).
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "key-envelope / stream-key unwrap (`sunrise-crypto`)".
//!
//! Every input goes through both paths. The seeds are the two frozen blobs in
//! `sunrise-crypto-test-vectors` — `key_envelope::SEALED` and
//! `at_rest::wrapped_stream_key::WRAPPED` — byte for byte, and each opens under
//! its own path's fixed key and context and fails the other's length or tag
//! check.
//!
//! # What is asserted
//!
//! 1. **No forgery.** Under the frozen recipient key and vault root, the only
//!    blob that opens is the one sealed there, and what comes out is the
//!    frozen Stream key. A mutation that still opened would be a key this
//!    device would start encrypting a stream under on nobody's word.
//! 2. **The context binds.** A blob that opens for its `(stream_id, epoch)`
//!    does not open for the next epoch or another stream. That binding is the
//!    HPKE `info` string on one path and the AEAD AAD on the other, and it is
//!    what stops a key sealed for one epoch being replayed into another.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_crypto::{
    hpke_open, key_envelope_info, unwrap_stream_key, DeviceDhKeyPair, VaultRootKey,
};
use sunrise_crypto_test_vectors::at_rest::wrapped_stream_key;
use sunrise_crypto_test_vectors::key_envelope;

/// A stream id no vector uses, for the cross-context checks.
const OTHER_STREAM: [u8; 16] = [0xee; 16];

fuzz_target!(|data: &[u8]| {
    // The HPKE path: a key sent to this device.
    let recipient = DeviceDhKeyPair::from_secret_bytes(key_envelope::RECIPIENT_SECRET);
    let info = key_envelope_info(&key_envelope::STREAM_ID, key_envelope::EPOCH);
    if let Ok(plaintext) = hpke_open(&recipient, &info, data, &[]) {
        assert_eq!(
            data,
            key_envelope::SEALED.as_slice(),
            "a key envelope nobody sealed opened under the frozen recipient key"
        );
        assert_eq!(
            plaintext.as_slice(),
            key_envelope::STREAM_KEY.as_slice(),
            "the frozen key envelope opened to a different Stream key"
        );
        for (stream, epoch) in [
            (key_envelope::STREAM_ID, key_envelope::EPOCH.wrapping_add(1)),
            (OTHER_STREAM, key_envelope::EPOCH),
        ] {
            assert!(
                hpke_open(&recipient, &key_envelope_info(&stream, epoch), data, &[]).is_err(),
                "a key envelope opened outside the (stream, epoch) it was sealed for"
            );
        }
    }

    // The at-rest path: a key this device wrapped under its vault root.
    let vault_root = VaultRootKey::from_bytes(wrapped_stream_key::VAULT_ROOT);
    if let Ok(key) = unwrap_stream_key(
        &vault_root,
        data,
        &wrapped_stream_key::STREAM_ID,
        wrapped_stream_key::EPOCH,
    ) {
        assert_eq!(
            data,
            wrapped_stream_key::WRAPPED.as_slice(),
            "a wrapped Stream key nobody wrapped opened under the frozen vault root"
        );
        assert_eq!(
            key.as_bytes(),
            &wrapped_stream_key::STREAM_KEY,
            "the frozen wrap opened to a different Stream key"
        );
        for (stream, epoch) in [
            (
                wrapped_stream_key::STREAM_ID,
                wrapped_stream_key::EPOCH.wrapping_add(1),
            ),
            (OTHER_STREAM, wrapped_stream_key::EPOCH),
        ] {
            assert!(
                unwrap_stream_key(&vault_root, data, &stream, epoch).is_err(),
                "a wrapped Stream key opened outside the (stream, epoch) it was wrapped for"
            );
        }
    }
});
