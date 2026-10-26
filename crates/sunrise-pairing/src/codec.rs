//! The CBOR helpers every pairing decoder and encoder shares.
//!
//! The assembled payload ([`crate::payload`]) and the three wire messages
//! ([`crate::protocol`]) read and write the same shapes under the same framing
//! bound, so the helpers belong to neither. Both import them from here, and
//! neither imports a helper from the other.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::PairingPayloadError;

/// Decode one pairing message's CBOR into a value tree, under the framing
/// bound every decoder in this crate shares, the payload's included.
///
/// # Errors
/// [`PairingPayloadError::TooDeep`] past [`sunrise_cbor::MAX_VALUE_DEPTH`],
/// [`PairingPayloadError::Cbor`] for anything else that does not decode.
pub(crate) fn decode_message(bytes: &[u8]) -> Result<ciborium::value::Value, PairingPayloadError> {
    sunrise_cbor::decode_value(&mut &bytes[..]).map_err(|e| match e {
        sunrise_cbor::ValueDecodeError::TooDeep => PairingPayloadError::TooDeep,
        sunrise_cbor::ValueDecodeError::Malformed(m) => PairingPayloadError::Cbor(m),
    })
}

pub(crate) fn int(n: u8) -> ciborium::value::Value {
    ciborium::value::Value::Integer(ciborium::value::Integer::from(n))
}

pub(crate) fn arr32(b: &[u8], what: &'static str) -> Result<[u8; 32], PairingPayloadError> {
    b.try_into()
        .map_err(|_| PairingPayloadError::BadField(what))
}

pub(crate) fn arr16(b: &[u8], what: &'static str) -> Result<[u8; 16], PairingPayloadError> {
    b.try_into()
        .map_err(|_| PairingPayloadError::BadField(what))
}

/// Read a `stream_id -> epoch -> key` CBOR map into `out`.
///
/// Shared by the payload and the grant, which carry the same map for the same
/// reason and must therefore reject the same malformed shapes.
pub(crate) fn read_stream_keys(
    streams: Vec<(ciborium::value::Value, ciborium::value::Value)>,
    out: &mut BTreeMap<[u8; 16], BTreeMap<u32, [u8; 32]>>,
) -> Result<(), PairingPayloadError> {
    use ciborium::value::Value;
    for (sid, per_epoch) in streams {
        let Value::Bytes(sid) = sid else {
            return Err(PairingPayloadError::BadField("stream_keys key"));
        };
        let Value::Map(per_epoch) = per_epoch else {
            return Err(PairingPayloadError::BadField("stream_keys value"));
        };
        let sid = arr16(&sid, "stream_keys key")?;
        let entry = out.entry(sid).or_default();
        for (epoch, key) in per_epoch {
            let Value::Integer(epoch) = epoch else {
                return Err(PairingPayloadError::BadField("epoch"));
            };
            let Value::Bytes(key) = key else {
                return Err(PairingPayloadError::BadField("stream key"));
            };
            let epoch = u32::try_from(i128::from(epoch))
                .map_err(|_| PairingPayloadError::BadField("epoch range"))?;
            entry.insert(epoch, arr32(&key, "stream key")?);
        }
    }
    Ok(())
}

/// The CBOR array a read-bound set travels as, or `None` for an empty set.
///
/// `None` rather than an empty array so that the field is **absent** when
/// there is nothing to carry: a sponsor with no bounded device then encodes
/// exactly the bytes it did before the field existed, which is what keeps the
/// Apple fixture and every older joiner's view of the message unchanged.
/// Ascending, because a `BTreeSet` iterates in order, so the encoding is
/// canonical without a sort step.
pub(crate) fn read_bounds_value(bounds: &BTreeSet<[u8; 16]>) -> Option<ciborium::value::Value> {
    use ciborium::value::Value;
    (!bounds.is_empty())
        .then(|| Value::Array(bounds.iter().map(|id| Value::Bytes(id.to_vec())).collect()))
}

/// Read a read-bound array into `out`.
///
/// Shared by the payload and the grant, like [`read_stream_keys`]. Every entry
/// must be a 16-byte device id: a malformed one is refused rather than
/// skipped, because a skipped id is a device the joiner goes on sealing keys
/// to while believing it adopted its sponsor's bound.
pub(crate) fn read_read_bounds(
    ids: Vec<ciborium::value::Value>,
    out: &mut BTreeSet<[u8; 16]>,
) -> Result<(), PairingPayloadError> {
    use ciborium::value::Value;
    for id in ids {
        let Value::Bytes(id) = id else {
            return Err(PairingPayloadError::BadField("read_bounds entry"));
        };
        out.insert(arr16(&id, "read_bounds entry")?);
    }
    Ok(())
}
