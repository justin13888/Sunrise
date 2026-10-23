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
//! [`MAX_VALUE_DEPTH`] is the bound for framing, and [`decode_value`] applies
//! it. It is part of the wire contract — a container field added in future
//! must not nest deeper than this, or every build in the field refuses the
//! envelope carrying it (`docs/10-cross-cutting/protocol-versioning.md`
//! §2.1). The other framing decoders are held to it as well: the wire
//! payloads, the relay's `HelloAck`, the pairing messages, the recovery
//! payload and the snapshot record.
//!
//! [`MAX_DOC_DEPTH`] is the second, looser bound, for document content: an
//! inner op, an entity, a field value. Those carry nested entity values and a
//! newer build's unknown fields at every level, so they get more room, and an
//! inner op past it is parked rather than refused (ADR-0045 §4). It is also
//! what [`crate::decode_canonical`] and [`crate::decode_lenient`] apply unless
//! a caller names another bound.
//!
//! [`decode_bounded`] is the one function both go through, for a `Value` tree
//! or a typed item, under whatever bound its caller names.

use ciborium::value::Value;
use serde::de::DeserializeOwned;
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

/// How many arrays, maps and tags may enclose one another in an item of
/// document content: an inner op, an entity, a field value.
///
/// The deepest inner op the format defines today is eight levels, an OR-set
/// remove in a `Patch`: the op's variant map, the payload, `fields`, the field
/// op, `remove`, one `[element, tags]` pair, the tag list and one tag. The
/// deepest one this build writes is seven, a preference `Patch` whose value is
/// a `{day, at}` or `{start, end}` map. A full-state op is at most five: its
/// variant map around a Task or Routine, whose constraint list holds
/// constraint maps that hold a weekday list. A note body does not count: it is
/// a byte string, decoded on its own under the note grammar's bound.
///
/// Thirty-two leaves a newer build's fields twenty-four levels beyond the
/// deepest known shape, at whatever level they are added, and it is four
/// times [`MAX_VALUE_DEPTH`]. It costs little. The items it bounds are reached
/// only through an envelope that verified and opened, so no stranger chooses
/// them, and thirty-two recursive calls is nothing on any thread's stack. What
/// it takes away is the other 224 levels ciborium would allow.
///
/// An inner op deeper than this is parked, not refused (ADR-0045 §4): its
/// envelope verified and opened, so it is a member's op, and a build with a
/// larger bound may read it. A parked op is retried only when a build with
/// another `DOC_SCHEMA_V` opens the vault, so raising this bound ships with a
/// `DOC_SCHEMA_V` bump, or the ops it would now admit stay parked. An entity
/// blob or field value the engine stores
/// from an op it applied is shallower than the op by the levels the op wraps
/// it in, so nothing stored that way is refused when it is read back.
pub const MAX_DOC_DEPTH: usize = 32;

/// Why [`decode_bounded`] refused its input.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValueDecodeError {
    /// The item nests more containers deep than its decoder's bound
    /// ([`MAX_VALUE_DEPTH`] for [`decode_value`]).
    #[error("CBOR nests deeper than its decoder allows")]
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
    decode_bounded(cursor, MAX_VALUE_DEPTH)
}

/// Decode one CBOR item from the front of `cursor` as a `T`, refusing one
/// that nests more than `max_depth` arrays, maps and tags deep, the outermost
/// one included.
///
/// `T` may be [`Value`] or any typed item. A typed decode counts what ciborium
/// counts: every array, map and non-bignum tag, and one level for each
/// externally tagged enum, whether its variant arrives as a one-entry map or
/// as bare text. So a typed shape can be refused a level sooner than the same
/// bytes read as a `Value`. A tag directly before a typed map, struct or enum
/// is skipped rather than counted.
///
/// `cursor` is advanced past the item, as [`decode_value`] advances it.
///
/// # Errors
/// [`ValueDecodeError::TooDeep`] past the bound, before any deeper level is
/// read; [`ValueDecodeError::Malformed`] for everything else, including a
/// well-formed item that is not a `T`.
pub fn decode_bounded<T: DeserializeOwned>(
    cursor: &mut &[u8],
    max_depth: usize,
) -> Result<T, ValueDecodeError> {
    ciborium::de::from_reader_with_recursion_limit(cursor, max_depth).map_err(|e| match e {
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
    fn a_named_bound_admits_exactly_that_many_levels() {
        for depth in [1, MAX_VALUE_DEPTH, MAX_DOC_DEPTH] {
            let ok = nested_arrays(depth);
            assert!(decode_bounded::<Value>(&mut ok.as_slice(), depth).is_ok());
            let deep = nested_arrays(depth + 1);
            assert_eq!(
                decode_bounded::<Value>(&mut deep.as_slice(), depth),
                Err(ValueDecodeError::TooDeep)
            );
        }
    }

    /// A typed decode is held to the bound too, through a field the type
    /// ignores as much as through one it keeps.
    #[test]
    fn a_typed_decode_is_bounded_through_an_ignored_field() {
        #[derive(Debug, serde::Deserialize)]
        struct Shallow {
            #[allow(dead_code)]
            a: u8,
        }
        // `{"a": 1, "b": [[..[1]..]]}`, `levels` arrays under "b".
        let with_ignored = |levels: usize| {
            let mut bytes = vec![0xa2, 0x61, b'a', 0x01, 0x61, b'b'];
            bytes.extend(nested_arrays(levels));
            bytes
        };
        let fits = with_ignored(MAX_VALUE_DEPTH - 1);
        assert!(decode_bounded::<Shallow>(&mut fits.as_slice(), MAX_VALUE_DEPTH).is_ok());
        let deep = with_ignored(MAX_VALUE_DEPTH);
        assert_eq!(
            decode_bounded::<Shallow>(&mut deep.as_slice(), MAX_VALUE_DEPTH).unwrap_err(),
            ValueDecodeError::TooDeep
        );
        assert!(matches!(
            decode_bounded::<Shallow>(&mut [0x01].as_slice(), MAX_VALUE_DEPTH),
            Err(ValueDecodeError::Malformed(_))
        ));
    }

    #[test]
    fn other_failures_are_malformed() {
        assert!(matches!(
            decode_value(&mut [0x82, 0x01].as_slice()),
            Err(ValueDecodeError::Malformed(_))
        ));
    }
}
