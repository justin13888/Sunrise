//! `CborValue` — a CBOR value preserved verbatim, with an honest `Eq`.
//!
//! Forward compatibility (`docs/10-cross-cutting/protocol-versioning.md` §7)
//! requires that a field this build does not understand be KEPT, not dropped:
//! "unknown CBOR map keys round-trip unchanged". Both the op envelope
//! (`sunrise-crypto`) and every domain entity (`sunrise-domain`) need somewhere
//! to put such a field, and both derive `Eq`, so the holder lives here — below
//! them both, beside the encoder whose rules make the round trip byte-exact.
//!
//! `ciborium::value::Value` cannot be that holder directly: it is
//! `#[non_exhaustive]` and, because it can hold an `f64`, it implements
//! `PartialEq` but not `Eq`.

use serde::{Deserialize, Serialize};

/// A preserved CBOR value: anything a future schema may put in a field this
/// build does not model.
///
/// `Eq` is sound here because [`CborValue`]'s `Deserialize` REFUSES floats,
/// and canonical Sunrise CBOR forbids them anyway (see
/// [`crate::canonical`]). Without that refusal a preserved NaN would make the
/// containing entity unequal to itself, and every `Eq` in the domain would be
/// quietly lying.
#[derive(Debug, Clone, PartialEq)]
pub struct CborValue(pub ciborium::value::Value);

impl Eq for CborValue {}

impl CborValue {
    /// Borrow the underlying value.
    #[must_use]
    pub const fn get(&self) -> &ciborium::value::Value {
        &self.0
    }
}

impl Serialize for CborValue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for CborValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = ciborium::value::Value::deserialize(d)?;
        if contains_float(&v) {
            return Err(serde::de::Error::custom(
                "floating point is not permitted in canonical Sunrise CBOR",
            ));
        }
        Ok(Self(v))
    }
}

/// Does `v` contain a float anywhere in its tree?
fn contains_float(v: &ciborium::value::Value) -> bool {
    use ciborium::value::Value;
    match v {
        Value::Float(_) => true,
        Value::Array(items) => items.iter().any(contains_float),
        Value::Map(entries) => entries
            .iter()
            .any(|(k, val)| contains_float(k) || contains_float(val)),
        Value::Tag(_, inner) => contains_float(inner),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_lenient, encode_canonical};
    use ciborium::value::Value;

    #[test]
    fn a_float_is_refused_so_eq_stays_reflexive() {
        let bytes = encode_canonical(&Value::Float(f64::NAN)).unwrap();
        let res: Result<CborValue, _> = decode_lenient(&bytes);
        assert!(res.is_err(), "a float must not become a preserved value");
    }

    #[test]
    fn a_nested_float_is_refused_too() {
        let bytes = encode_canonical(&Value::Array(vec![
            Value::Integer(1.into()),
            Value::Array(vec![Value::Float(1.5)]),
        ]))
        .unwrap();
        let res: Result<CborValue, _> = decode_lenient(&bytes);
        assert!(res.is_err());
    }

    #[test]
    fn ordinary_values_round_trip() {
        for v in [
            Value::Integer(42.into()),
            Value::Text("hello".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Array(vec![Value::Integer(1.into())]),
            Value::Bool(true),
            Value::Null,
        ] {
            let bytes = encode_canonical(&CborValue(v.clone())).unwrap();
            let back: CborValue = decode_lenient(&bytes).unwrap();
            assert_eq!(back, CborValue(v));
        }
    }
}
