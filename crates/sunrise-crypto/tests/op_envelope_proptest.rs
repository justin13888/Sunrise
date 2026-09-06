//! `OpEnvelope` round-trip, as a property.
//!
//! `docs/10-cross-cutting/testing.md` §2 lists "Op envelope round-trip
//! (encode/decode/encrypt/decrypt/sign/verify)" among the properties this
//! project holds. The unit tests in `op_envelope.rs` pin three or four
//! hand-chosen envelopes; this file states the claim the document actually
//! makes — that it holds for *any* well-formed envelope — and lets `proptest`
//! look for the one that does not.
//!
//! The property, in full: for an arbitrary well-formed envelope, sealing it
//! and decoding the resulting bytes returns every header field verbatim, the
//! signature verifies under the sealing device's public key, and opening it
//! under the sealing Stream key returns exactly the plaintext that went in.
//!
//! A round-trip property is only worth its runtime if it can fail, so each
//! case also checks the four ways it *must*: a foreign verification key is
//! refused, a mutated header breaks the signature, re-signing that mutation
//! does not rescue it (the AAD covers the whole header), and a wrong Stream
//! key does not open the ciphertext. Without those, an implementation that
//! signed nothing and encrypted nothing would satisfy the round trip.
//!
//! Free fields are generated; the two secrets are derived from generated
//! bytes, which is cheap for a Stream key (32 raw bytes) and one seeded
//! Ed25519 keygen for the device.

use proptest::prelude::*;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use std::collections::BTreeMap;
use sunrise_cbor::hlc::Hlc;
use sunrise_cbor::version::{DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, ENVELOPE_FORMAT_V};
use sunrise_cbor::{CborValue, MAGIC_LEN};
use sunrise_crypto::keys::{DeviceSigningKeyPair, StreamKey};
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{
    decode_envelope, seal_envelope, sign_envelope, verify_envelope, AeadAlgId, OpEnvelope,
    OpEnvelopeError, SigAlgId, AEAD_NONCE_LEN, AEAD_TAG_LEN,
};

/// One generated envelope, in the terms the codec takes.
#[derive(Debug, Clone)]
struct Case {
    stream_id: [u8; 16],
    device_id: [u8; 16],
    seq: u64,
    hlc: Hlc,
    /// `true` selects `aead_alg = 1` (XChaCha20-Poly1305), `false` the
    /// signed-only control envelope.
    aead: bool,
    epoch: u32,
    nonce: [u8; AEAD_NONCE_LEN],
    doc_schema_v: u32,
    inner: Vec<u8>,
    stream_key_bytes: [u8; 32],
    signing_seed: u64,
    unknown: BTreeMap<u64, CborValue>,
}

impl Case {
    fn aead_alg(&self) -> AeadAlgId {
        if self.aead {
            AeadAlgId::XChaCha20Poly1305
        } else {
            AeadAlgId::None
        }
    }

    /// The plaintext envelope handed to `seal_envelope`.
    fn envelope(&self) -> OpEnvelope {
        OpEnvelope {
            v: u32::from(ENVELOPE_FORMAT_V),
            stream_id: self.stream_id,
            device_id: self.device_id,
            seq: self.seq,
            hlc: self.hlc,
            aead_alg: self.aead_alg(),
            sig_alg: SigAlgId::Ed25519,
            epoch: self.epoch,
            nonce: self.nonce,
            payload: self.inner.clone(),
            sig: [0u8; 64],
            doc_schema_v: self.doc_schema_v,
            unknown: self.unknown.clone(),
        }
    }
}

fn signing_key(seed: u64) -> DeviceSigningKeyPair {
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    DeviceSigningKeyPair::generate(&mut rng)
}

/// Inner-op bytes. Mostly short — real ops are — with a minority long enough
/// to cross a CBOR length-prefix width and exercise a multi-block AEAD.
fn payload_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => proptest::collection::vec(any::<u8>(), 0..256),
        1 => proptest::collection::vec(any::<u8>(), 256..2048),
    ]
}

/// Anything at or above the floor is readable by contract, so the whole range
/// above it is legitimate — including values far beyond this build's own
/// `DOC_SCHEMA_V`, which is exactly the forward-compatible case ADR-0015
/// promises.
fn doc_schema_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        1 => Just(u32::from(DOC_SCHEMA_FLOOR)),
        1 => Just(u32::from(DOC_SCHEMA_V)),
        2 => u32::from(DOC_SCHEMA_FLOOR)..=u32::MAX,
    ]
}

/// Fields from a newer container format. The decoder keeps them at their own
/// ids and re-emits them in place; since the sender signed over them, a
/// round trip that dropped or moved one would break the signature. Floats are
/// not generated because canonical Sunrise CBOR has none and the decoder
/// refuses them outright.
fn unknown_strategy() -> impl Strategy<Value = BTreeMap<u64, CborValue>> {
    let value = prop_oneof![
        any::<i64>().prop_map(|i| ciborium::value::Value::Integer(i.into())),
        proptest::collection::vec(any::<u8>(), 0..16).prop_map(ciborium::value::Value::Bytes),
    ];
    proptest::collection::btree_map(13u64..64, value, 0..3)
        .prop_map(|m| m.into_iter().map(|(k, v)| (k, CborValue(v))).collect())
}

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        any::<[u8; 16]>(),
        any::<[u8; 16]>(),
        any::<u64>(),
        (any::<u64>(), any::<u32>()),
        any::<bool>(),
        any::<u32>(),
        any::<[u8; AEAD_NONCE_LEN]>(),
        doc_schema_strategy(),
        payload_strategy(),
        (any::<[u8; 32]>(), any::<u64>(), unknown_strategy()),
    )
        .prop_map(
            |(
                stream_id,
                device_id,
                seq,
                (physical_ms, logical),
                aead,
                epoch,
                nonce,
                doc_schema_v,
                inner,
                (stream_key_bytes, signing_seed, unknown),
            )| {
                Case {
                    stream_id,
                    device_id,
                    seq,
                    hlc: Hlc {
                        physical_ms,
                        logical,
                    },
                    aead,
                    // A signed-only envelope carries no Stream key, so it has
                    // no epoch: both the sealer and the decoder reject a
                    // non-zero one, and generating it would only ever produce
                    // `InconsistentAead`.
                    epoch: if aead { epoch } else { 0 },
                    nonce,
                    doc_schema_v,
                    inner,
                    stream_key_bytes,
                    signing_seed,
                    unknown,
                }
            },
        )
}

proptest! {
    // 256 cases, matching `sunrise-log`'s property test. Each case runs three
    // Ed25519 signatures and three verifies plus up to four
    // XChaCha20-Poly1305 passes over as much as 2 KiB — the expensive part —
    // and measures at ~0.15 s for the file on the developer profile, because
    // `[profile.dev.package."*"]` builds dalek and chacha20poly1305 at
    // opt-level 1. That is cheap enough to stay in the crate's ordinary
    // `cargo test` rather than becoming a nightly-only job, and wide enough
    // that both AEAD modes, both payload-length classes and every
    // unknown-field count are each hit tens of times. The deep sweep of this
    // surface is the `op_envelope` cargo-fuzz target, not this test.
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn arbitrary_envelope_round_trips(case in case_strategy()) {
        let signing = signing_key(case.signing_seed);
        let signer_pub = signing.public_bytes();
        let stream_key = StreamKey::from_bytes(case.stream_key_bytes);
        let key = if case.aead { Some(&stream_key) } else { None };

        let plain = case.envelope();
        let bytes = seal_envelope(plain.clone(), key, &signing)
            .expect("a well-formed envelope seals");

        // The magic prefix carries the container format version.
        prop_assert_eq!(&bytes[..MAGIC_LEN], b"SR\x02\x00\x03");

        let env = decode_envelope(&bytes).expect("sealed bytes decode");

        // Every header field survives the round trip verbatim.
        prop_assert_eq!(env.v, u32::from(ENVELOPE_FORMAT_V));
        prop_assert_eq!(env.stream_id, case.stream_id);
        prop_assert_eq!(env.device_id, case.device_id);
        prop_assert_eq!(env.seq, case.seq);
        prop_assert_eq!(env.hlc, case.hlc);
        prop_assert_eq!(env.aead_alg, case.aead_alg());
        prop_assert_eq!(env.sig_alg, SigAlgId::Ed25519);
        prop_assert_eq!(env.epoch, case.epoch);
        prop_assert_eq!(env.nonce, case.nonce);
        prop_assert_eq!(env.doc_schema_v, case.doc_schema_v);
        prop_assert_eq!(&env.unknown, &plain.unknown);

        // The signature verifies, and the payload comes back exactly.
        verify_envelope(&env, &signer_pub).expect("the sealing device's signature verifies");
        let opened = open_envelope(&env, &signer_pub, key).expect("the envelope opens");
        prop_assert_eq!(&opened, &case.inner);

        if case.aead {
            prop_assert_eq!(env.payload.len(), case.inner.len() + AEAD_TAG_LEN);
            // The wire payload is ciphertext. Checked only from 8 bytes up, so
            // that a chance keystream collision cannot fail the suite.
            if case.inner.len() >= 8 {
                prop_assert_ne!(&env.payload[..case.inner.len()], &case.inner[..]);
            }
        } else {
            prop_assert_eq!(&env.payload, &case.inner);
        }

        // --- the round trip is load-bearing, not vacuous ---

        // 1. Another device's key does not verify this envelope.
        let other = signing_key(case.signing_seed ^ 0xffff_ffff_ffff_ffff);
        prop_assert!(matches!(
            verify_envelope(&env, &other.public_bytes()),
            Err(OpEnvelopeError::SigVerify)
        ));

        // 2. Altering a signed header field breaks the signature.
        let mut tampered = env.clone();
        tampered.seq = tampered.seq.wrapping_add(1);
        prop_assert!(matches!(
            verify_envelope(&tampered, &signer_pub),
            Err(OpEnvelopeError::SigVerify)
        ));

        if case.aead {
            // 3. Re-signing the alteration does not rescue it: the AAD is the
            //    whole header, so the tag no longer authenticates even though
            //    the signature is now valid again.
            sign_envelope(&mut tampered, &signing).expect("re-sign");
            verify_envelope(&tampered, &signer_pub).expect("the re-signed envelope verifies");
            prop_assert!(matches!(
                open_envelope(&tampered, &signer_pub, key),
                Err(OpEnvelopeError::AeadAuth)
            ));

            // 4. A different Stream key does not open the ciphertext.
            let mut wrong_bytes = case.stream_key_bytes;
            wrong_bytes[0] ^= 1;
            let wrong = StreamKey::from_bytes(wrong_bytes);
            prop_assert!(matches!(
                open_envelope(&env, &signer_pub, Some(&wrong)),
                Err(OpEnvelopeError::AeadAuth)
            ));
        }
    }
}
