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
//! emits a canonical CBOR map, decoding those bytes returns every header field
//! verbatim, the signature verifies under the sealing device's public key, and
//! opening it under the sealing Stream key returns exactly the plaintext that
//! went in.
//!
//! A round-trip property is only worth its runtime if it can fail, so each
//! case also checks the five ways it *must*: a foreign verification key is
//! refused, an arbitrary header field cannot be altered without breaking the
//! signature, re-signing that alteration does not rescue it (the AAD is the
//! whole header), a wrong Stream key does not open the ciphertext, and a float
//! anywhere in the envelope is refused outright. Without those, an
//! implementation that signed nothing and encrypted nothing would satisfy the
//! round trip.
//!
//! Every generated integer is a weighted mixture of the range production
//! actually uses and the full domain of the type. A `seq` drawn from
//! `any::<u64>()` alone is below 2^32 with probability 2e-10, so it would
//! encode in CBOR's 9-byte form in every single case and leave the 1-, 2-, 3-
//! and 5-byte forms — the ones a real envelope uses, where `op_envelope.rs`
//! documents "first op = 1" — untested. The tail into the full domain is kept
//! because it is what catches a width truncation.
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

/// Which header field a case alters when it checks that the signature and the
/// AAD actually cover the header.
///
/// Tampering only ever with `seq` would leave the claim "the AAD covers the
/// whole header" resting on one field: dropping `doc_schema_v` from both
/// `Omit` arms — the field `op_envelope.rs` singles out as the easy one to
/// leave outside the signature and the AAD, because it sorts after `sig` — is
/// invisible to a `seq`-only check.
#[derive(Debug, Clone, Copy)]
enum Tamper {
    StreamId,
    DeviceId,
    Seq,
    HlcPhysical,
    HlcLogical,
    Epoch,
    Nonce,
    DocSchemaV,
    Payload,
    /// Add a field from a newer container format. The sender signed over the
    /// unknowns it carried, so an inserted one must break the signature too.
    AddUnknown,
}

impl Tamper {
    fn apply(self, env: &mut OpEnvelope) {
        match self {
            Self::StreamId => env.stream_id[0] ^= 1,
            Self::DeviceId => env.device_id[0] ^= 1,
            Self::Seq => env.seq = env.seq.wrapping_add(1),
            Self::HlcPhysical => env.hlc.physical_ms = env.hlc.physical_ms.wrapping_add(1),
            Self::HlcLogical => env.hlc.logical = env.hlc.logical.wrapping_add(1),
            Self::Epoch => env.epoch = env.epoch.wrapping_add(1),
            Self::Nonce => env.nonce[0] ^= 1,
            Self::DocSchemaV => env.doc_schema_v = env.doc_schema_v.wrapping_add(1),
            Self::Payload => {
                // A signed-only envelope with an empty inner op has no byte to
                // flip; lengthening it is the same alteration.
                if let Some(first) = env.payload.first_mut() {
                    *first ^= 1;
                } else {
                    env.payload.push(0);
                }
            }
            Self::AddUnknown => {
                // 99 is outside the range `unknown_strategy` draws from, so it
                // is always an addition rather than an edit.
                env.unknown
                    .insert(99, CborValue(ciborium::value::Value::Integer(1.into())));
            }
        }
    }
}

fn tamper_strategy() -> impl Strategy<Value = Tamper> {
    prop_oneof![
        Just(Tamper::StreamId),
        Just(Tamper::DeviceId),
        Just(Tamper::Seq),
        Just(Tamper::HlcPhysical),
        Just(Tamper::HlcLogical),
        Just(Tamper::Epoch),
        Just(Tamper::Nonce),
        Just(Tamper::DocSchemaV),
        Just(Tamper::Payload),
        Just(Tamper::AddUnknown),
    ]
}

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
    tamper: Tamper,
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

/// The field ids in an encoded envelope, in the order they appear on the wire.
///
/// Canonical CBOR (RFC 8949 §4.2.1) over non-negative integer keys is plain
/// numeric ordering, and `encode_cbor` gets there by sorting after it appends
/// preserved unknowns. Round-tripping alone cannot see whether that sort ran —
/// this build's decoder does not care what order it reads fields in, so a
/// sender and a receiver sharing the defect agree perfectly. What breaks is
/// the *next* hop: the original sender signed the canonical bytes, and a
/// re-emission that moved a field no longer hashes to the same value.
fn field_ids_on_the_wire(cbor: &[u8]) -> Vec<i128> {
    let value: ciborium::value::Value =
        ciborium::de::from_reader(cbor).expect("a sealed envelope is valid CBOR");
    match value {
        ciborium::value::Value::Map(entries) => entries
            .iter()
            .map(|(k, _)| match k {
                ciborium::value::Value::Integer(i) => i128::from(*i),
                other => panic!("non-integer envelope field id: {other:?}"),
            })
            .collect(),
        other => panic!("an envelope must encode as a map, got {other:?}"),
    }
}

/// Inner-op bytes. Mostly short — real ops are — with a minority long enough
/// to cross a CBOR length-prefix width and exercise a multi-block AEAD.
fn payload_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        3 => proptest::collection::vec(any::<u8>(), 0..256),
        1 => proptest::collection::vec(any::<u8>(), 256..2048),
    ]
}

/// `seq` starts at 1 and counts ops, so production lives in CBOR's narrow
/// integer forms; the full-domain tail is what pins the 9-byte form.
fn seq_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        2 => 0u64..1024,
        1 => any::<u64>(),
    ]
}

/// `physical_ms` is a Unix millisecond count, so unlike `seq` its realistic
/// range is genuinely wide — but `Hlc::at(0)` is legal and appears in the
/// crate's own fixtures, so the small forms are drawn too.
fn physical_ms_strategy() -> impl Strategy<Value = u64> {
    prop_oneof![
        1 => 0u64..4096,
        2 => 1_600_000_000_000u64..2_000_000_000_000u64,
        1 => any::<u64>(),
    ]
}

/// Stream-key epochs and the HLC's logical counter both start at 0 and are
/// bumped one at a time.
fn small_u32_strategy() -> impl Strategy<Value = u32> {
    prop_oneof![
        2 => 0u32..64,
        1 => any::<u32>(),
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
/// ids and re-emits them in their canonical position; since the sender signed
/// over them, a round trip that dropped or moved one would break the
/// signature downstream.
///
/// Id `0` is drawn deliberately and is the whole reason the canonical-order
/// assertion has teeth: it is the only id that sorts *below* every known field
/// and is not itself a known field, so it is the only draw under which
/// `encode_cbor`'s sort changes the emitted byte order at all. Ids 1..=12 are
/// excluded because they are the known fields — an entry there would be a
/// duplicate map key, which is a malformed envelope rather than a
/// forward-compatible one, and so outside this property's domain.
///
/// Floats are not generated here because they are not forward compatibility,
/// they are malformed; the case checks that rejection separately rather than
/// asserting it in a comment.
fn unknown_strategy() -> impl Strategy<Value = BTreeMap<u64, CborValue>> {
    let value = prop_oneof![
        any::<i64>().prop_map(|i| ciborium::value::Value::Integer(i.into())),
        proptest::collection::vec(any::<u8>(), 0..16).prop_map(ciborium::value::Value::Bytes),
    ];
    let id = prop_oneof![
        2 => Just(0u64),
        3 => 13u64..64,
    ];
    proptest::collection::btree_map(id, value, 0..3)
        .prop_map(|m| m.into_iter().map(|(k, v)| (k, CborValue(v))).collect())
}

fn case_strategy() -> impl Strategy<Value = Case> {
    (
        any::<[u8; 16]>(),
        any::<[u8; 16]>(),
        seq_strategy(),
        (physical_ms_strategy(), small_u32_strategy()),
        any::<bool>(),
        small_u32_strategy(),
        any::<[u8; AEAD_NONCE_LEN]>(),
        doc_schema_strategy(),
        payload_strategy(),
        (
            any::<[u8; 32]>(),
            any::<u64>(),
            unknown_strategy(),
            tamper_strategy(),
        ),
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
                (stream_key_bytes, signing_seed, unknown, tamper),
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
                    tamper,
                }
            },
        )
}

/// 256 cases by default, matching `sunrise-log`'s property test. Each case
/// runs four Ed25519 signatures and four verifies plus up to five
/// XChaCha20-Poly1305 passes over as much as 2 KiB — the expensive part — and
/// measures at roughly 0.2 s for the file on the developer profile, because
/// `[profile.dev.package."*"]` builds dalek and chacha20poly1305 at
/// optimisation level one. That is cheap enough to stay in the crate's
/// ordinary `cargo test` rather than becoming a nightly-only job, and wide
/// enough that both AEAD modes, both payload-length classes, every
/// unknown-field count and all ten tamper targets are each hit tens of times.
///
/// `PROPTEST_CASES` still wins. Hard-coding the count into the struct literal
/// would override the environment variable `ProptestConfig::default()` reads,
/// removing the dial that lets someone widen this run without editing the
/// file; 256 is the fallback, not the ceiling. The deep sweep of this surface
/// is the `op_envelope` cargo-fuzz target, not this test.
fn config() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(256);
    ProptestConfig {
        cases,
        // `Direct`, not the `SourceParallel` default: nothing above a `tests/`
        // file holds a `lib.rs` or `main.rs`, so the default warns and drops the
        // counterexample beside this source instead. See
        // docs/10-cross-cutting/testing.md section 2.
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/op_envelope_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

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

        // The CBOR map is canonical: field ids ascend, preserved unknowns
        // included, whatever order the encoder happened to build them in.
        let ids = field_ids_on_the_wire(&bytes[MAGIC_LEN..]);
        prop_assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "envelope field ids are not in ascending canonical order: {:?}",
            ids
        );

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

        // 2. Altering the header field this case drew breaks the signature.
        let mut tampered = env.clone();
        case.tamper.apply(&mut tampered);
        prop_assert!(
            matches!(verify_envelope(&tampered, &signer_pub), Err(OpEnvelopeError::SigVerify)),
            "{:?} left the signature intact, so it is outside the signature input",
            case.tamper
        );

        if case.aead {
            // 3. Re-signing the alteration does not rescue it: the AAD is the
            //    whole header, so the tag no longer authenticates even though
            //    the signature is now valid again.
            sign_envelope(&mut tampered, &signing).expect("re-sign");
            verify_envelope(&tampered, &signer_pub).expect("the re-signed envelope verifies");
            prop_assert!(
                matches!(
                    open_envelope(&tampered, &signer_pub, key),
                    Err(OpEnvelopeError::AeadAuth)
                ),
                "{:?} survived a re-sign, so it is outside the AAD",
                case.tamper
            );

            // 4. A different Stream key does not open the ciphertext.
            let mut wrong_bytes = case.stream_key_bytes;
            wrong_bytes[0] ^= 1;
            let wrong = StreamKey::from_bytes(wrong_bytes);
            prop_assert!(matches!(
                open_envelope(&env, &signer_pub, Some(&wrong)),
                Err(OpEnvelopeError::AeadAuth)
            ));
        }

        // 5. Canonical Sunrise CBOR has no floats anywhere, so a float in an
        //    unknown field is malformed rather than merely unfamiliar. The
        //    envelope is otherwise this case's own, and it still seals — the
        //    refusal has to come from the decoder.
        let mut floated = case.envelope();
        floated.unknown.insert(
            200,
            CborValue(ciborium::value::Value::Float(1.5)),
        );
        let float_bytes = seal_envelope(floated, key, &signing)
            .expect("a float seals; it is the decoder that must refuse it");
        prop_assert!(matches!(
            decode_envelope(&float_bytes),
            Err(OpEnvelopeError::BadField("float in envelope"))
        ));
    }
}
