//! Decoding an untrusted CBOR item into a `Value` tree, with a nesting bound.
//!
//! `ciborium::de::from_reader` permits 256 levels of nested arrays, maps and
//! tags. Nothing Sunrise writes outside an opaque byte string comes close: the
//! deepest known field of an op envelope is a chain head, three levels down
//! (the envelope map, the field-15 array, the head's own array), and a device
//! cert and its body are each one map of scalars. The rest of the 256 exist
//! only for whoever chooses the bytes.
//!
//! They are not free. Every level is a recursive call in the decoder, again in
//! `Drop`, and again in each tree walk this workspace runs over a preserved
//! unknown field (the float check, the canonical encoder) — stack an attacker
//! gets to spend on whatever thread decodes, which on iOS is 512 KiB off the
//! main thread. Under the nightly fuzz job's Address Sanitizer it was a memory
//! leak in effect: the sanitizer records the call stack of every allocation in
//! a depot it never frees, and
//! each different interleaving of array, map and tag levels is a new stack.
//! `Fuzz (op_envelope)` and `Fuzz (device_cert)` both grew past libFuzzer's
//! 2 GiB RSS limit that way with a live heap of under 30 MB.
//!
//! [`MAX_VALUE_DEPTH`] is the bound, and [`decode_value`] is the one place it
//! is applied. It is part of the wire contract — a container field added in
//! future must not nest deeper than this, or every build in the field refuses
//! the envelope carrying it (`docs/10-cross-cutting/protocol-versioning.md`
//! §2.1).

use ciborium::value::Value;
use thiserror::Error;

/// How many arrays, maps and tags may enclose one another in a decoded item,
/// the outermost one included.
///
/// Eight is the deepest known shape (three) with room for a newer container's
/// fields to nest five levels more, and it is the same number the note-body
/// grammar settled on for its own nesting (`sunrise-domain`'s
/// `MAX_NEST_DEPTH`). It is also small enough to matter: three kinds of
/// container in eight positions is a few thousand shapes, where sixteen
/// levels is forty million, and it is the count of shapes, not their depth,
/// that the sanitizer's stack depot grows with.
///
/// A big-integer tag (2 or 3) around a byte string of at most 16 bytes is read
/// as an integer and is not a level. Any other tag is one level, and the item
/// it wraps is counted separately.
pub const MAX_VALUE_DEPTH: usize = 8;

/// Why [`decode_value`] refused its input.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueDecodeError {
    /// The item nests more than [`MAX_VALUE_DEPTH`] containers deep.
    #[error("CBOR nests deeper than {MAX_VALUE_DEPTH} arrays, maps or tags")]
    TooDeep,
    /// Any other decode failure: truncation, an invalid header, bad UTF-8.
    #[error("{0}")]
    Malformed(String),
}

/// Decode one CBOR item from the front of `cursor`, refusing one that nests
/// more than [`MAX_VALUE_DEPTH`] containers deep.
///
/// `cursor` is advanced past the item, so a caller that must refuse trailing
/// bytes can check what is left; one that does not care can ignore it.
///
/// # Errors
/// [`ValueDecodeError::TooDeep`] past the bound, before any deeper level is
/// read; [`ValueDecodeError::Malformed`] for everything else.
pub fn decode_value(cursor: &mut &[u8]) -> Result<Value, ValueDecodeError> {
    ciborium::de::from_reader_with_recursion_limit(cursor, MAX_VALUE_DEPTH).map_err(|e| match e {
        ciborium::de::Error::RecursionLimitExceeded => ValueDecodeError::TooDeep,
        other => ValueDecodeError::Malformed(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `depth` one-element arrays around the integer 1.
    fn nested_arrays(depth: usize) -> Vec<u8> {
        let mut bytes = vec![0x81; depth];
        bytes.push(0x01);
        bytes
    }

    #[test]
    fn the_bound_admits_exactly_max_value_depth_levels() {
        let ok = nested_arrays(MAX_VALUE_DEPTH);
        let mut cursor = ok.as_slice();
        let mut v = decode_value(&mut cursor).expect("eight levels decode");
        assert!(cursor.is_empty());
        for _ in 0..MAX_VALUE_DEPTH {
            let Value::Array(mut items) = v else {
                panic!("expected an array level");
            };
            v = items.pop().expect("one element");
        }
        assert_eq!(v, Value::Integer(1.into()));

        let deep = nested_arrays(MAX_VALUE_DEPTH + 1);
        assert_eq!(
            decode_value(&mut deep.as_slice()),
            Err(ValueDecodeError::TooDeep)
        );
    }

    /// Maps and non-bignum tags are levels as arrays are, so an interleaving
    /// of all three is held to the same bound — the shape both fuzz findings
    /// were made of.
    #[test]
    fn maps_and_tags_count_as_levels() {
        let mut ok = Vec::new();
        for i in 0..MAX_VALUE_DEPTH {
            match i % 3 {
                0 => ok.push(0x81),           // array(1)
                1 => ok.extend([0xa1, 0x01]), // map(1), key 1
                _ => ok.extend([0xd8, 0x40]), // tag(64)
            }
        }
        ok.push(0x01);
        assert!(decode_value(&mut ok.as_slice()).is_ok());

        let mut deep = vec![0xd8, 0x40];
        deep.extend_from_slice(&ok);
        assert_eq!(
            decode_value(&mut deep.as_slice()),
            Err(ValueDecodeError::TooDeep)
        );
    }

    /// The bound is a refusal at the limit, not a scan of the whole input:
    /// a nest far deeper than the input could ever hold a body for is refused
    /// as too deep, not as truncated.
    #[test]
    fn a_deep_truncated_nest_is_refused_as_too_deep() {
        let deep = vec![0x81; 4096];
        assert_eq!(
            decode_value(&mut deep.as_slice()),
            Err(ValueDecodeError::TooDeep)
        );
    }

    #[test]
    fn the_cursor_is_left_after_the_item() {
        let bytes = [0x01, 0x02];
        let mut cursor = &bytes[..];
        assert_eq!(decode_value(&mut cursor), Ok(Value::Integer(1.into())));
        assert_eq!(cursor, &[0x02]);
    }

    #[test]
    fn other_failures_are_malformed() {
        assert!(matches!(
            decode_value(&mut [0x82, 0x01].as_slice()),
            Err(ValueDecodeError::Malformed(_))
        ));
    }
}
