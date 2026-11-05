//! `DeviceCert`'s unit tests, one file per theme, sharing the fixtures below.
//!
//! * [`binding`]: issue, verify and round-trip, the signature covering the
//!   bytes that arrived, and [`DeviceCert::verify_binding`].
//! * [`widths`]: the five fields the CDDL fixes at an exact byte width, and
//!   the body's key ids.
//! * [`nickname`]: the nickname's byte bound, at both ends and in both
//!   directions.

use super::*;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;

mod binding;
mod nickname;
mod widths;

fn fixture(d_s_pub: [u8; 32]) -> DeviceCertInner {
    DeviceCertInner {
        v: 1,
        device_id: [7u8; 16],
        d_s_pub,
        d_d_pub: [9u8; 32],
        identity_id: [3u8; 16],
        created_at_ms: 1_700_000_000_000,
        nickname: "Justin's Mac".to_string(),
        platform: "macos15".to_string(),
    }
}

// The bodies below are built as `Value::Map`s rather than by editing encoded
// bytes, so each test states the field it changed instead of a byte offset
// that has to be decoded to be understood.

/// The body fields of [`fixture`] as an editable list, **decoded from
/// `body_to_cbor`'s own output** rather than retyped here.
///
/// Retyping the encoder's key-id layout would let the two drift: renumber a
/// field in production and these tests would keep sending the old id, which
/// the in-range arm refuses as `BadField("body field shape")` — the very
/// error the width tests assert, so all of them would pass while measuring
/// nothing. Deriving the map means a renumbering moves the tests with it.
fn body_fields() -> Vec<(i64, Value)> {
    let encoded = body_to_cbor(&fixture([1u8; 32])).expect("the fixture encodes");
    let decoded: Value =
        ciborium::de::from_reader(encoded.as_slice()).expect("its own output decodes");
    let Value::Map(map) = decoded else {
        panic!("body_to_cbor emits a map")
    };
    map.into_iter()
        .map(|(k, v)| match k {
            Value::Integer(i) => (
                i64::try_from(i128::from(i)).expect("body field ids are small integers"),
                v,
            ),
            other => panic!("body_to_cbor emits integer keys, got {other:?}"),
        })
        .collect()
}

fn encode_fields(fields: Vec<(i64, Value)>) -> Vec<u8> {
    let map = fields
        .into_iter()
        .map(|(id, v)| (Value::Integer(Integer::from(id)), v))
        .collect();
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("body map encodes");
    out
}

/// [`body_fields`] with the one field `id` carrying `value` instead.
fn body_cbor_with(id: i64, value: Value) -> Vec<u8> {
    let mut fields = body_fields();
    let slot = fields
        .iter_mut()
        .find(|(k, _)| *k == id)
        .expect("the field id under test is one of the eight");
    slot.1 = value;
    encode_fields(fields)
}

/// The outer `{1: body_bytes, 2: sig}` map [`DeviceCert::to_cbor`] emits,
/// with `body` as the body byte string and the signature at `width` bytes.
fn outer_cbor(body: Vec<u8>, sig_width: usize) -> Vec<u8> {
    let map = vec![
        (Value::Integer(Integer::from(1)), Value::Bytes(body)),
        (
            Value::Integer(Integer::from(2)),
            Value::Bytes(vec![0xab; sig_width]),
        ),
    ];
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(map), &mut out).expect("outer map encodes");
    out
}
