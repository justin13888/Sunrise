//! The op-envelope and device-cert decoders refuse CBOR nested deeper than
//! `sunrise_cbor::MAX_VALUE_DEPTH`.
//!
//! Regressions for the nightly `Fuzz (op_envelope)` and `Fuzz (device_cert)`
//! out-of-memory findings. Both inputs were a few kilobytes of arrays, maps and
//! tags nested tens of levels deep inside a field the decoder keeps or skips,
//! truncated before the nest closed. One such input is harmless; millions of
//! distinct ones grew Address Sanitizer's never-freed stack depot past the
//! 2 GiB RSS limit, because the decoder recursed once per level up to
//! `ciborium`'s default of 256. The crash files themselves are committed as
//! seeds under `fuzz/seeds/`; these tests rebuild each shape from scratch and
//! pin that it is refused as too deep — a bounded error, before any level past
//! the bound is read.
//!
//! The key-bundle and identity-chain decoders, which read records the relay
//! pins outside op retention, decode their whole body through
//! `sunrise_cbor::decode_canonical`, bounded at `sunrise_cbor::MAX_DOC_DEPTH`.
//! Their tests are at the end of this file.

use ciborium::value::Value;
use std::collections::BTreeMap;
use sunrise_cbor::hlc::Hlc;
use sunrise_cbor::{
    write_prefix, CanonicalError, CborValue, MagicKind, ValueDecodeError, ENVELOPE_FORMAT_FLOOR,
    MAGIC_LEN, MAX_DOC_DEPTH, MAX_VALUE_DEPTH,
};
use sunrise_crypto::keys::DeviceSigningKeyPair;
use sunrise_crypto::{
    decode_envelope, decode_key_bundle, seal_envelope, verify_envelope, AeadAlgId, DeviceCert,
    DeviceCertError, IdentityChain, IdentityChainError, KeyBundleError, OpEnvelope,
    OpEnvelopeError, IDENTITY_CHAIN_FORMAT_V, KEY_BUNDLE_FORMAT_V,
};
use sunrise_crypto_test_vectors as vectors;
use sunrise_crypto_test_vectors::identity_transition::device_cert;

/// An unknown field id above every id this build knows or reserves.
const FUTURE_FIELD: u64 = 40;

/// `levels` one-element arrays around the integer 1.
fn nest(levels: usize) -> Value {
    let mut v = Value::Integer(1.into());
    for _ in 0..levels {
        v = Value::Array(vec![v]);
    }
    v
}

/// The fuzzers' shape: arrays, one-entry maps and non-bignum tags interleaved
/// `levels` deep, and never closed — the input ends where the innermost item
/// should start.
fn open_interleaved_nest(levels: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..levels {
        match i % 3 {
            0 => out.push(0x81),
            1 => out.extend([0xa1, 0x01]),
            _ => out.extend([0xd8, 0x40]),
        }
    }
    out
}

/// Neither error type is `PartialEq`, so the refusal is matched by shape: the
/// decoder's `Cbor` variant carrying exactly the bound's own message.
fn assert_too_deep_envelope(err: &OpEnvelopeError) {
    let want = ValueDecodeError::TooDeep.to_string();
    assert!(
        matches!(err, OpEnvelopeError::Cbor(m) if *m == want),
        "expected a too-deep refusal, got {err:?}"
    );
}

fn assert_too_deep_cert(err: &DeviceCertError) {
    let want = ValueDecodeError::TooDeep.to_string();
    assert!(
        matches!(err, DeviceCertError::Cbor(m) if *m == want),
        "expected a too-deep refusal, got {err:?}"
    );
}

/// A signed envelope at this build's container whose one unknown field is
/// `value`.
fn envelope_with_unknown(value: Value) -> Vec<u8> {
    let kp = DeviceSigningKeyPair::from_secret_bytes(&vectors::DEVICE_SIGNING_SECRET);
    let mut unknown = BTreeMap::new();
    unknown.insert(FUTURE_FIELD, CborValue(value));
    seal_envelope(
        OpEnvelope {
            v: u32::from(sunrise_cbor::ENVELOPE_FORMAT_V),
            doc_schema_v: u32::from(sunrise_cbor::DOC_SCHEMA_V),
            schema_fp: None,
            stream_id: vectors::STREAM_ID,
            device_id: vectors::DEVICE_ID,
            seq: 7,
            hlc: Hlc {
                physical_ms: 1_700_000_000_000,
                logical: 0,
            },
            aead_alg: AeadAlgId::None,
            sig_alg: sunrise_crypto::SigAlgId::Ed25519,
            epoch: 0,
            nonce: [0u8; 24],
            payload: vectors::ENVELOPE_INNER.to_vec(),
            sig: [0u8; 64],
            prev_hash: None,
            heads: Vec::new(),
            unknown,
        },
        None,
        &kp,
    )
    .expect("seal")
}

/// The envelope map is one level, so an unknown field may nest
/// `MAX_VALUE_DEPTH - 1` more: that envelope still decodes and verifies.
#[test]
fn an_unknown_envelope_field_at_the_bound_decodes_and_verifies() {
    let bytes = envelope_with_unknown(nest(MAX_VALUE_DEPTH - 1));
    let env = decode_envelope(&bytes).expect("at the bound decodes");
    verify_envelope(&env, &vectors::DEVICE_SIGNING_PUBLIC).expect("and verifies");
    assert_eq!(
        env.unknown.get(&FUTURE_FIELD),
        Some(&CborValue(nest(MAX_VALUE_DEPTH - 1)))
    );
}

/// One level more is refused, though the encoder would happily write it.
#[test]
fn an_unknown_envelope_field_past_the_bound_is_refused() {
    let bytes = envelope_with_unknown(nest(MAX_VALUE_DEPTH));
    assert_too_deep_envelope(&decode_envelope(&bytes).unwrap_err());
}

/// The `Fuzz (op_envelope)` finding's shape: a valid prefix, an
/// indefinite-length map, and a field whose value opens a deep interleaved
/// nest the input never closes.
#[test]
fn an_open_interleaved_nest_in_an_envelope_is_refused_as_too_deep() {
    let mut bytes = vec![0u8; MAGIC_LEN];
    write_prefix(&mut bytes, MagicKind::OpEnvelope, ENVELOPE_FORMAT_FLOOR);
    bytes.extend([0xbf, 0x18, 0x28]); // indefinite map, key 40
    bytes.extend(open_interleaved_nest(256));
    assert_too_deep_envelope(&decode_envelope(&bytes).unwrap_err());
}

/// The `Fuzz (device_cert)` finding's shape: an outer map whose first key is
/// itself a stack of bignum-negative tags (`0xc3`) over one-element arrays,
/// never closed. The key is decoded into a tree before the decoder can see it
/// is not an integer, so the bound has to hold there too.
#[test]
fn an_open_tag_and_array_nest_in_a_cert_key_is_refused_as_too_deep() {
    let mut bytes = vec![0xaf]; // map(15)
    for _ in 0..128 {
        bytes.extend([0xc3, 0x81]);
    }
    assert_too_deep_cert(&DeviceCert::from_cbor(&bytes).unwrap_err());
}

/// An unknown outer field the cert decoder would skip is still held to the
/// bound, and it is refused as too deep rather than as a cert missing its
/// body.
#[test]
fn a_skipped_cert_field_past_the_bound_is_refused_as_too_deep() {
    let mut fits = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![(Value::Integer(9.into()), nest(MAX_VALUE_DEPTH - 1))]),
        &mut fits,
    )
    .unwrap();
    assert!(matches!(
        DeviceCert::from_cbor(&fits),
        Err(DeviceCertError::BadField("body"))
    ));

    let mut deep = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![(Value::Integer(9.into()), nest(MAX_VALUE_DEPTH))]),
        &mut deep,
    )
    .unwrap();
    assert_too_deep_cert(&DeviceCert::from_cbor(&deep).unwrap_err());
}

/// The body is a second decode over bytes the sender chose, and it is bounded
/// the same way: the frozen cert with one extra, deeply nested body field is
/// refused before its signature is ever looked at.
#[test]
fn a_cert_body_field_past_the_bound_is_refused_as_too_deep() {
    let with_extra_body_field = |levels: usize| {
        // The frozen body is `map(8)`; make it `map(9)` and append key 9.
        let mut body = device_cert::BODY_BYTES.to_vec();
        assert_eq!(body[0], 0xa8);
        body[0] = 0xa9;
        body.push(0x09);
        ciborium::ser::into_writer(&nest(levels), &mut body).unwrap();
        let mut out = Vec::new();
        ciborium::ser::into_writer(
            &Value::Map(vec![
                (Value::Integer(1.into()), Value::Bytes(body)),
                (
                    Value::Integer(2.into()),
                    Value::Bytes(device_cert::SIG.to_vec()),
                ),
            ]),
            &mut out,
        )
        .unwrap();
        out
    };

    let fits = DeviceCert::from_cbor(&with_extra_body_field(MAX_VALUE_DEPTH - 1))
        .expect("a skipped body field at the bound decodes");
    assert_eq!(fits.body.nickname, device_cert::NICKNAME);

    assert_too_deep_cert(
        &DeviceCert::from_cbor(&with_extra_body_field(MAX_VALUE_DEPTH)).unwrap_err(),
    );
}

/// `magic ‖ body` for a record of `kind` at `version`.
fn record(kind: MagicKind, version: u16, body: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; MAGIC_LEN];
    write_prefix(&mut out, kind, version);
    out.extend_from_slice(body);
    out
}

/// The canonical encoding of a one-entry map from `key` to `levels` nested
/// arrays: `levels + 1` levels deep, the map included.
fn one_field_nested(key: u64, levels: usize) -> Vec<u8> {
    sunrise_cbor::encode_canonical(&Value::Map(vec![(
        Value::Integer(key.into()),
        nest(levels),
    )]))
    .unwrap()
}

/// Neither record decoder tells a too-deep body apart from any other body
/// that is not canonical CBOR: both map every `decode_canonical` error to the
/// one `Malformed` message. So the test pins two things: `sunrise-cbor` itself
/// calls `body` too deep, and the record decoder refused it at that step
/// rather than at a field.
fn assert_body_too_deep(body: &[u8]) {
    assert!(
        matches!(
            sunrise_cbor::decode_canonical::<Value>(body),
            Err(CanonicalError::TooDeep)
        ),
        "the body is not past the bound"
    );
}

/// A key bundle whose `identity_id` is `MAX_DOC_DEPTH - 1` nested arrays gets
/// past the CBOR decode and is refused for that field's shape; one level more
/// is refused at the CBOR decode.
#[test]
fn a_key_bundle_field_past_the_bound_is_refused_before_its_shape_is_read() {
    let fits = one_field_nested(1, MAX_DOC_DEPTH - 1);
    assert_eq!(
        decode_key_bundle(&record(MagicKind::KeyBundle, KEY_BUNDLE_FORMAT_V, &fits)).unwrap_err(),
        KeyBundleError::Malformed("identity_id")
    );

    let deep = one_field_nested(1, MAX_DOC_DEPTH);
    assert_body_too_deep(&deep);
    assert_eq!(
        decode_key_bundle(&record(MagicKind::KeyBundle, KEY_BUNDLE_FORMAT_V, &deep)).unwrap_err(),
        KeyBundleError::Malformed("not canonical CBOR")
    );
}

/// The fuzzers' shape after a key bundle's prefix: a one-entry map whose value
/// opens a deep interleaved nest the input never closes.
#[test]
fn an_open_interleaved_nest_in_a_key_bundle_is_refused_as_too_deep() {
    let mut body = vec![0xa1, 0x04]; // map(1), key 4 (`manifest`)
    body.extend(open_interleaved_nest(256));
    assert_body_too_deep(&body);
    assert_eq!(
        decode_key_bundle(&record(MagicKind::KeyBundle, KEY_BUNDLE_FORMAT_V, &body)).unwrap_err(),
        KeyBundleError::Malformed("not canonical CBOR")
    );
}

/// As for the key bundle, at both of an identity chain's fields: the genesis
/// key, and the link list, whose one link is itself the nest.
#[test]
fn an_identity_chain_field_past_the_bound_is_refused_before_its_shape_is_read() {
    let chain = |body: &[u8]| {
        IdentityChain::decode(&record(
            MagicKind::IdentityChain,
            IDENTITY_CHAIN_FORMAT_V,
            body,
        ))
        .unwrap_err()
    };
    for (key, field) in [(1, "genesis_id_s_pub"), (2, "link")] {
        assert_eq!(
            chain(&one_field_nested(key, MAX_DOC_DEPTH - 1)),
            IdentityChainError::Malformed(field),
            "field {key} at the bound"
        );
        let deep = one_field_nested(key, MAX_DOC_DEPTH);
        assert_body_too_deep(&deep);
        assert_eq!(
            chain(&deep),
            IdentityChainError::Malformed("not canonical CBOR"),
            "field {key} past the bound"
        );
    }
}

/// The fuzzers' shape after an identity chain's prefix, in the link list.
#[test]
fn an_open_interleaved_nest_in_an_identity_chain_is_refused_as_too_deep() {
    let mut body = vec![0xa1, 0x02]; // map(1), key 2 (`links`)
    body.extend(open_interleaved_nest(256));
    assert_body_too_deep(&body);
    assert_eq!(
        IdentityChain::decode(&record(
            MagicKind::IdentityChain,
            IDENTITY_CHAIN_FORMAT_V,
            &body
        ))
        .unwrap_err(),
        IdentityChainError::Malformed("not canonical CBOR")
    );
}
