//! `serde` adapters that keep an epoch-millisecond wire field while the Rust
//! side holds a [`jiff::Timestamp`].
//!
//! Three entities — `FocusStart`, `FocusEnd`, `ReviewSnapshot` — grew a set of
//! raw `u64` epoch-millisecond fields while every other entity in the domain
//! used `Timestamp`. `ReviewSnapshot` managed to hold both in one struct:
//! `created_at: Timestamp` beside `window_start_ms: u64`. That is not a
//! stylistic complaint. A bare `u64` carries no unit, no epoch, and no
//! guarantee it is not a DURATION: `FocusEnd` has `ended_at_ms` (an instant)
//! and `actual_focused_ms` (a duration) as the same type, and nothing but the
//! field name stops one being passed where the other is expected.
//!
//! The Rust type is therefore unified onto `Timestamp`, and the WIRE
//! representation is left exactly as it was — an integer count of
//! milliseconds, under the same `_ms` key. Changing the encoding as well would
//! have broken every `DOC_SCHEMA_V = 1` and `= 2` payload for a change with no
//! protocol content, and would have forced `DOC_SCHEMA_FLOOR` up for the sake
//! of tidiness.
//!
//! Durations stay `u64` milliseconds. They are not instants and should not
//! pretend to be.

use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serializer};

/// Serialize a [`Timestamp`] as epoch milliseconds.
///
/// # Errors
/// Never fails for a representable `Timestamp`.
pub fn serialize<S: Serializer>(t: &Timestamp, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_i64(t.as_millisecond())
}

/// Deserialize a [`Timestamp`] from epoch milliseconds.
///
/// A value outside the representable range clamps to the epoch rather than
/// failing: refusing the whole op over one out-of-range integer would lose an
/// entire session or review, and an obviously-wrong instant is easier to see
/// and repair than a missing record.
///
/// # Errors
/// Only if the field is not an integer at all.
pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Timestamp, D::Error> {
    let ms = i64::deserialize(d)?;
    Ok(Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH))
}

/// `u64` epoch milliseconds → [`Timestamp`], clamping the impossible.
#[must_use]
pub fn from_u64(ms: u64) -> Timestamp {
    i64::try_from(ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .unwrap_or(Timestamp::UNIX_EPOCH)
}

/// [`Timestamp`] → `u64` epoch milliseconds, clamping pre-epoch instants to 0.
#[must_use]
pub fn to_u64(t: Timestamp) -> u64 {
    u64::try_from(t.as_millisecond()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Holder {
        #[serde(with = "crate::epoch_ms")]
        at: Timestamp,
    }

    /// The whole point: the wire stays an integer, so a payload written before
    /// the Rust type changed still reads.
    #[test]
    fn the_wire_form_is_still_a_bare_integer() {
        let h = Holder {
            at: from_u64(1_700_000_000_123),
        };
        let bytes = sunrise_cbor::encode_canonical(&h).unwrap();
        // a1 62 6174 1b 0000018bcfe56813 — map(1), "at", u64.
        assert_eq!(&bytes[..4], &[0xa1, 0x62, b'a', b't']);
        assert_eq!(bytes[4], 0x1b, "encoded as an integer, not a string");

        let back: Holder = sunrise_cbor::decode_canonical(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn round_trips_through_u64() {
        for ms in [0u64, 1, 1_700_000_000_123] {
            assert_eq!(to_u64(from_u64(ms)), ms);
        }
    }

    #[test]
    fn an_impossible_value_clamps_rather_than_failing() {
        assert_eq!(from_u64(u64::MAX), Timestamp::UNIX_EPOCH);
    }
}
