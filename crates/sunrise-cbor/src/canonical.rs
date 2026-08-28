//! Canonical CBOR encode/decode wrapper around `ciborium`.
//!
//! Per `docs/03-crypto/data-encryption-format.md`, byte-exact AAD requires
//! deterministic CBOR (RFC 8949 §4.2 "Core Deterministic Encoding"). Our
//! requirements:
//!
//! - Map keys MUST be sorted bytewise by their CBOR-encoded byte sequence
//!   (deterministic length-then-lexicographic for the byte string form).
//! - Definite-length items only.
//! - Shortest-form integer encoding.
//! - No floats.
//!
//! `ciborium` alone gives neither property. A derived `Serialize` emits map
//! fields in DECLARATION order, and `#[serde(flatten)]` makes it emit an
//! INDEFINITE-length map (`0xbf … 0xff`) because the length is not known up
//! front. So [`encode_canonical`] does not hand the value straight to
//! `ciborium`: it materializes it as a `ciborium::value::Value`, sorts every
//! map's keys by their encoded bytes, and writes that. A `Value::Map` always
//! encodes with a definite length, so both problems are fixed in one place
//! rather than in every `Serialize` impl in the workspace.
//!
//! Key sorting is not cosmetic. It is what makes FORWARD COMPATIBILITY work:
//! a reader that meets a field it does not know keeps it in a
//! `#[serde(flatten)]` map and re-emits it, and the bytes come back identical
//! ONLY if the field lands in the same position it started in. Under
//! declaration order it would not — the writer puts its new field wherever it
//! declared it, and the reader would append it at the end. Under key order
//! both agree, because neither of them chose the position.
//!
//! [`decode_canonical`] asserts canonical form on decode (re-encode →
//! equality), which is therefore also the check that a preserved unknown field
//! round-trips byte-exactly.

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;

/// Errors produced by canonical CBOR helpers.
#[derive(Debug, Error)]
pub enum CanonicalError {
    /// `ciborium` serialization failure (e.g., unsupported type).
    #[error("CBOR encode error: {0}")]
    Encode(String),
    /// `ciborium` deserialization failure (malformed CBOR).
    #[error("CBOR decode error: {0}")]
    Decode(String),
    /// Decoded value re-encoded to bytes ≠ input bytes — non-canonical input.
    #[error("non-canonical CBOR: re-encoded length {reencoded} ≠ input {input}")]
    NonCanonical {
        /// Length of the input bytes.
        input: usize,
        /// Length of the re-encoded bytes.
        reencoded: usize,
    },
}

/// Marker trait for types whose `Serialize` impl produces canonical CBOR.
///
/// Concrete types in `sunrise-domain` / `sunrise-crypto` impl this to
/// document the contract; the crate-level encode helper uses it as a
/// boundary marker.
pub trait CanonicalEncoding: Serialize {}

/// Encode `value` to canonical CBOR bytes: definite lengths, map keys sorted
/// bytewise by their encoded form.
///
/// # Errors
/// [`CanonicalError::Encode`] if the value cannot be represented in CBOR.
pub fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let value = ciborium::value::Value::serialized(value)
        .map_err(|e| CanonicalError::Encode(e.to_string()))?;
    let value = canonicalize(value);
    let mut out = Vec::with_capacity(64);
    ciborium::ser::into_writer(&value, &mut out)
        .map_err(|e| CanonicalError::Encode(e.to_string()))?;
    Ok(out)
}

/// Recursively put a decoded value into canonical form: every map's keys
/// sorted by their CBOR-encoded bytes (RFC 8949 §4.2.1).
///
/// Bytewise comparison of the ENCODED key is the rule, not comparison of the
/// decoded key: it is what makes the order independent of the key's Rust type,
/// and it is what a peer implementing the spec from the document will do.
fn canonicalize(value: ciborium::value::Value) -> ciborium::value::Value {
    use ciborium::value::Value;
    match value {
        Value::Map(entries) => {
            let mut keyed: Vec<(Vec<u8>, Value, Value)> = entries
                .into_iter()
                .map(|(k, v)| {
                    let mut kb = Vec::with_capacity(8);
                    // A key that cannot be encoded cannot have been decoded;
                    // an empty sort key keeps the function total either way.
                    let _ = ciborium::ser::into_writer(&k, &mut kb);
                    (kb, k, canonicalize(v))
                })
                .collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Map(keyed.into_iter().map(|(_, k, v)| (k, v)).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        Value::Tag(tag, inner) => Value::Tag(tag, Box::new(canonicalize(*inner))),
        other => other,
    }
}

/// Decode `bytes` and assert canonical form: re-encoding the decoded value
/// MUST produce the same bytes back. Detects deviations like:
/// - Non-sorted map keys.
/// - Indefinite-length items.
/// - Non-shortest integer encodings.
///
/// Returns [`CanonicalError::NonCanonical`] if the round-trip differs.
pub fn decode_canonical<T>(bytes: &[u8]) -> Result<T, CanonicalError>
where
    T: DeserializeOwned + Serialize,
{
    let value: T =
        ciborium::de::from_reader(bytes).map_err(|e| CanonicalError::Decode(e.to_string()))?;
    let re_encoded = encode_canonical(&value)?;
    if re_encoded.as_slice() != bytes {
        return Err(CanonicalError::NonCanonical {
            input: bytes.len(),
            reencoded: re_encoded.len(),
        });
    }
    Ok(value)
}

/// Decode without canonical-form assertion. Used at trust boundaries where
/// non-canonical input must be tolerated and re-canonicalized on relay
/// (rare; see protocol-versioning §7 forward-compat).
pub fn decode_lenient<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CanonicalError> {
    ciborium::de::from_reader(bytes).map_err(|e| CanonicalError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Tiny {
        a: u32,
        b: String,
    }

    #[test]
    fn round_trip_simple() {
        let v = Tiny {
            a: 7,
            b: "hi".to_string(),
        };
        let bytes = encode_canonical(&v).unwrap();
        let back: Tiny = decode_canonical(&bytes).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn detects_non_canonical_indefinite_length() {
        // Hand-craft an indefinite-length array CBOR: 0x9f items 0xff
        // Then assert decode_canonical rejects it.
        // Indefinite array of one u8: 0x9f 0x01 0xff
        let raw = [0x9f, 0x01, 0xff];
        let res: Result<Vec<u32>, _> = decode_canonical(&raw);
        assert!(matches!(res, Err(CanonicalError::NonCanonical { .. })));
    }

    /// The rule the module doc has always stated and the encoder never
    /// enforced: keys come out in encoded-byte order, not declaration order.
    #[test]
    fn map_keys_are_sorted_by_their_encoded_bytes() {
        #[derive(Serialize)]
        struct OutOfOrder {
            zeta: u32,
            alpha: u32,
            m: u32,
        }
        let bytes = encode_canonical(&OutOfOrder {
            zeta: 1,
            alpha: 2,
            m: 3,
        })
        .unwrap();
        // a3                     map(3)
        //   61 6d       03           "m"     : 3
        //   64 7a657461 01           "zeta"  : 1
        //   65 616c706861 02         "alpha" : 2
        //
        // Note "zeta" before "alpha": the sort is on the ENCODED key, and a
        // 4-byte string's header (0x64) is below a 5-byte string's (0x65). A
        // sort on the decoded strings would put "alpha" first and disagree
        // with every conforming peer.
        assert_eq!(
            bytes,
            [
                0xa3, 0x61, b'm', 0x03, 0x64, b'z', b'e', b't', b'a', 0x01, 0x65, b'a', b'l', b'p',
                b'h', b'a', 0x02,
            ]
        );
    }

    /// `#[serde(flatten)]` makes a derived `Serialize` emit an
    /// indefinite-length map, which canonical CBOR forbids. Routing through
    /// `Value` is what stops that reaching the wire.
    #[test]
    fn a_flattened_map_still_encodes_with_a_definite_length() {
        use std::collections::BTreeMap;
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct WithUnknown {
            a: u32,
            #[serde(flatten)]
            unknown: BTreeMap<String, ciborium::value::Value>,
        }
        let mut unknown = BTreeMap::new();
        unknown.insert(
            "b".to_string(),
            ciborium::value::Value::Integer(9i32.into()),
        );
        let v = WithUnknown { a: 1, unknown };
        let bytes = encode_canonical(&v).unwrap();
        assert_eq!(bytes[0] & 0xe0, 0xa0, "must be a definite-length map");
        assert_ne!(bytes[0], 0xbf, "must not be indefinite");

        // ...and a preserved unknown field survives a decode/re-encode
        // byte-exactly, which is the property forward compatibility rests on.
        let back: WithUnknown = decode_canonical(&bytes).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn lenient_accepts_what_canonical_rejects() {
        let raw = [0x9f, 0x01, 0xff];
        let v: Vec<u32> = decode_lenient(&raw).unwrap();
        assert_eq!(v, vec![1]);
    }
}
