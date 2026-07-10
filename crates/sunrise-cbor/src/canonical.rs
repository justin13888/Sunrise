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
//! `ciborium` produces deterministic encodings *if and only if* the
//! caller's `Serialize` impl emits map fields in sorted-key order. The
//! `encode_canonical` and `decode_canonical` helpers wrap that contract
//! and assert canonical-form on decode (i.e., re-encode → equality).

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

/// Encode `value` to canonical CBOR bytes.
///
/// The caller's `Serialize` impl is responsible for emitting map keys in
/// sorted order; this function does not reorder.
pub fn encode_canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalError> {
    let mut out = Vec::with_capacity(64);
    ciborium::ser::into_writer(value, &mut out)
        .map_err(|e| CanonicalError::Encode(e.to_string()))?;
    Ok(out)
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

    #[test]
    fn lenient_accepts_what_canonical_rejects() {
        let raw = [0x9f, 0x01, 0xff];
        let v: Vec<u32> = decode_lenient(&raw).unwrap();
        assert_eq!(v, vec![1]);
    }
}
