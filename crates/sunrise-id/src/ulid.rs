//! Raw 128-bit ULIDs (untyped).
//!
//! `EntityRef` (in `entity_ref.rs`) layers an `EntityKind` discriminator on
//! top.

use crate::crockford::{decode_str, encode_bytes, CrockfordError, ENCODED_LEN};
use core::fmt;
use thiserror::Error;

/// 128-bit ULID; raw bytes are the authoritative representation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ulid([u8; 16]);

/// Errors produced by [`Ulid::parse`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UlidParseError {
    /// String length was wrong.
    #[error("invalid length")]
    BadLength,
    /// Crockford decode failed.
    #[error(transparent)]
    Crockford(#[from] CrockfordError),
}

impl Ulid {
    /// Construct from raw bytes.
    #[inline]
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The all-zero ULID, useful as a sentinel and as the inbox pseudo-id.
    pub const ZERO: Self = Self([0u8; 16]);

    /// Raw 16 bytes.
    #[inline]
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Render as a 26-char Crockford base-32 string (no prefix).
    #[must_use]
    pub fn to_str(self) -> String {
        encode_bytes(&self.0)
    }

    /// Parse from a 26-char Crockford base-32 string (no prefix).
    pub fn parse(s: &str) -> Result<Self, UlidParseError> {
        if s.len() != ENCODED_LEN {
            return Err(UlidParseError::BadLength);
        }
        decode_str(s).map(Self).map_err(UlidParseError::from)
    }

    /// Build a ULID from a `(timestamp_ms, randomness)` pair. The high 48
    /// bits are big-endian milliseconds; the low 80 bits are the
    /// caller-supplied randomness.
    #[must_use]
    pub fn from_timestamp_and_random(timestamp_ms: u64, random: [u8; 10]) -> Self {
        let mut out = [0u8; 16];
        // 48-bit timestamp, big-endian. Mask + truncate-by-cast is intentional:
        // each shift reduces to 8 trailing bits before the cast to u8.
        let ts = timestamp_ms & 0x0000_ffff_ffff_ffff;
        out[0..6].copy_from_slice(&ts.to_be_bytes()[2..8]);
        out[6..].copy_from_slice(&random);
        Self(out)
    }

    /// Big-endian 48-bit timestamp prefix (milliseconds since epoch).
    #[must_use]
    pub const fn timestamp_ms(self) -> u64 {
        ((self.0[0] as u64) << 40)
            | ((self.0[1] as u64) << 32)
            | ((self.0[2] as u64) << 24)
            | ((self.0[3] as u64) << 16)
            | ((self.0[4] as u64) << 8)
            | (self.0[5] as u64)
    }
}

impl fmt::Debug for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Ulid").field(&self.to_str()).finish()
    }
}

impl fmt::Display for Ulid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let bytes = [
            0x01, 0x91, 0x9c, 0x06, 0x39, 0x3e, 0xc5, 0x68, 0x95, 0x6e, 0xb6, 0x95, 0x57, 0xa9,
            0xff, 0x7d,
        ];
        let u = Ulid::from_bytes(bytes);
        let s = u.to_str();
        assert_eq!(s.len(), 26);
        assert_eq!(Ulid::parse(&s).unwrap(), u);
    }

    #[test]
    fn ordering_is_lexical_in_str_form() {
        // Bytes that differ in the time prefix order lexically by time.
        let early = Ulid::from_timestamp_and_random(1, [0u8; 10]);
        let late = Ulid::from_timestamp_and_random(1_700_000_000_000, [0u8; 10]);
        assert!(early < late);
        assert!(early.to_str() < late.to_str());
    }

    #[test]
    fn timestamp_round_trip() {
        let ts = 1_778_243_696_789;
        let u = Ulid::from_timestamp_and_random(ts, [1u8; 10]);
        assert_eq!(u.timestamp_ms(), ts);
    }

    #[test]
    fn from_timestamp_truncates_to_48_bits() {
        let u = Ulid::from_timestamp_and_random(u64::MAX, [0u8; 10]);
        assert_eq!(u.timestamp_ms(), 0x0000_ffff_ffff_ffff);
    }

    #[test]
    fn parse_rejects_bad_length() {
        assert_eq!(Ulid::parse(""), Err(UlidParseError::BadLength));
        assert_eq!(
            Ulid::parse("0000000000000000000000000"),
            Err(UlidParseError::BadLength)
        );
    }
}
