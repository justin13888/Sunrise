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
    use crate::{decode_lenient, encode_canonical, CanonicalError};
    use ciborium::value::Value;

    /// Decode `v` as a [`CborValue`] and assert it is refused for carrying a
    /// float, not for any other reason.
    fn assert_refused_as_float(v: &Value) {
        let bytes = encode_canonical(v).unwrap();
        match decode_lenient::<CborValue>(&bytes) {
            Err(CanonicalError::Decode(msg)) => assert!(
                msg.contains("floating point is not permitted"),
                "refused for the wrong reason: {msg}"
            ),
            other => panic!("a float must not become a preserved value: {other:?}"),
        }
    }

    #[test]
    fn a_float_is_refused_so_eq_stays_reflexive() {
        assert_refused_as_float(&Value::Float(f64::NAN));
    }

    #[test]
    fn a_nested_float_is_refused_too() {
        assert_refused_as_float(&Value::Array(vec![
            Value::Integer(1.into()),
            Value::Array(vec![Value::Float(1.5)]),
        ]));
    }

    #[test]
    fn a_float_map_key_is_refused() {
        assert_refused_as_float(&Value::Map(vec![(
            Value::Float(1.5),
            Value::Integer(1.into()),
        )]));
    }

    #[test]
    fn a_float_map_value_is_refused() {
        assert_refused_as_float(&Value::Map(vec![(
            Value::Text("k".into()),
            Value::Float(1.5),
        )]));
    }

    #[test]
    fn a_float_inside_a_tag_is_refused() {
        assert_refused_as_float(&Value::Tag(1_000, Box::new(Value::Float(1.5))));
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
