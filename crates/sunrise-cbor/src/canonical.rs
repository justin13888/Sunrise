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
//!
//! Both decoders bound nesting: [`decode_canonical`] and [`decode_lenient`] at
//! [`MAX_DOC_DEPTH`], the bound for document content, and
//! [`decode_canonical_within`] at whatever bound a framing decoder names
//! (`crate::bounded`).

use crate::bounded::{decode_bounded, ValueDecodeError, MAX_DOC_DEPTH};
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
    /// The input nests more arrays, maps and tags deep than the decoder's
    /// bound ([`MAX_DOC_DEPTH`] unless the caller named another).
    #[error("CBOR nests deeper than its decoder allows")]
    TooDeep,
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
/// Nesting is bounded at [`MAX_DOC_DEPTH`]; a framing decoder that wants the
/// tighter bound uses [`decode_canonical_within`].
///
/// Returns [`CanonicalError::NonCanonical`] if the round-trip differs, and
/// [`CanonicalError::TooDeep`] past the bound.
pub fn decode_canonical<T>(bytes: &[u8]) -> Result<T, CanonicalError>
where
    T: DeserializeOwned + Serialize,
{
    decode_canonical_within(bytes, MAX_DOC_DEPTH)
}

/// [`decode_canonical`] with nesting bounded at `max_depth` arrays, maps and
/// tags, the outermost one included (`crate::bounded::decode_bounded`).
///
/// # Errors
/// [`CanonicalError::TooDeep`] past the bound, [`CanonicalError::Decode`] for
/// malformed input, [`CanonicalError::NonCanonical`] if the round trip
/// differs, trailing bytes included.
pub fn decode_canonical_within<T>(bytes: &[u8], max_depth: usize) -> Result<T, CanonicalError>
where
    T: DeserializeOwned + Serialize,
{
    let value: T = decode_bounded(&mut &bytes[..], max_depth).map_err(from_bounded)?;
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
///
/// Nesting is bounded at [`MAX_DOC_DEPTH`], as [`decode_canonical`]'s is.
///
/// # Errors
/// [`CanonicalError::TooDeep`] past the bound, [`CanonicalError::Decode`] for
/// anything else that does not decode.
pub fn decode_lenient<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CanonicalError> {
    decode_bounded(&mut &bytes[..], MAX_DOC_DEPTH).map_err(from_bounded)
}

fn from_bounded(e: ValueDecodeError) -> CanonicalError {
    match e {
        ValueDecodeError::TooDeep => CanonicalError::TooDeep,
        ValueDecodeError::Malformed(m) => CanonicalError::Decode(m),
    }
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

    /// `depth` one-element arrays around the integer 1: canonical CBOR.
    fn nested_arrays(depth: usize) -> Vec<u8> {
        let mut bytes = vec![0x81; depth];
        bytes.push(0x01);
        bytes
    }

    /// Both decoders admit [`MAX_DOC_DEPTH`] levels and refuse one more as
    /// too deep, not as malformed; `decode_canonical_within` holds its own
    /// bound instead.
    #[test]
    fn decoders_are_bounded_at_the_document_depth_unless_told_otherwise() {
        use ciborium::value::Value;
        let at = nested_arrays(MAX_DOC_DEPTH);
        let over = nested_arrays(MAX_DOC_DEPTH + 1);
        assert!(decode_canonical::<Value>(&at).is_ok());
        assert!(decode_lenient::<Value>(&at).is_ok());
        assert!(matches!(
            decode_canonical::<Value>(&over),
            Err(CanonicalError::TooDeep)
        ));
        assert!(matches!(
            decode_lenient::<Value>(&over),
            Err(CanonicalError::TooDeep)
        ));

        let framing = crate::MAX_VALUE_DEPTH;
        assert!(decode_canonical_within::<Value>(&nested_arrays(framing), framing).is_ok());
        assert!(matches!(
            decode_canonical_within::<Value>(&nested_arrays(framing + 1), framing),
            Err(CanonicalError::TooDeep)
        ));
    }
}
