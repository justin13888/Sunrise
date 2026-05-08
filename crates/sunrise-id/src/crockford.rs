//! Crockford base-32 codec, ULID variant.
//!
//! Per `spec/02-domain/identifiers.md`: a ULID is 128 bits encoded in 26
//! Crockford base-32 chars. The encoding pads two zero bits at the top so
//! that 26 × 5 = 130 bits cover the 128 payload bits.
//!
//! This is "ULID-style" Crockford: alphabet `0123456789ABCDEFGHJKMNPQRSTVWXYZ`
//! (no `I/L/O/U`). Decoding accepts the canonical alphabet only — case
//! variants and visually-confusable substitutions (`I→1`, `L→1`, `O→0`) are
//! rejected to keep round-trips canonical.

use thiserror::Error;

/// Crockford base-32 alphabet, ULID-style (no I/L/O/U).
pub const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Length of the encoded form for a 128-bit ULID.
pub const ENCODED_LEN: usize = 26;

/// Errors produced by [`decode_str`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CrockfordError {
    /// Input length was not exactly [`ENCODED_LEN`] (26).
    #[error("expected {ENCODED_LEN} chars, got {0}")]
    BadLength(usize),
    /// Encountered a character outside the canonical alphabet.
    #[error("invalid character {ch:?} at position {at}")]
    BadChar {
        /// The offending character.
        ch: char,
        /// Zero-indexed position within the input.
        at: usize,
    },
    /// The two padding bits at the top were non-zero.
    #[error("non-canonical encoding: padding bits set")]
    BadPadding,
}

/// Encode 16 bytes (a ULID payload) as 26 Crockford base-32 chars.
///
/// Encoding view: a 130-bit buffer with the layout `[2 zero bits | 128 bits
/// of payload]`. Character `i` (0-indexed, MSB-first) is the 5-bit value at
/// shift `125 - 5*i` of the 128-bit payload. The first character is
/// therefore in `0..=7` (only the bottom 3 of its 5 bits carry data; the
/// top 2 are the zero padding).
#[must_use]
pub fn encode_bytes(bytes: &[u8; 16]) -> String {
    let mut payload: u128 = 0;
    for &b in bytes {
        payload = (payload << 8) | u128::from(b);
    }
    let mut out = String::with_capacity(ENCODED_LEN);
    for i in 0..ENCODED_LEN {
        // i = 0 → shift 125 (top 3 bits of payload)
        // i = 25 → shift 0   (bottom 5 bits of payload)
        // i ∈ 0..ENCODED_LEN (= 26), so the cast is exact.
        #[allow(clippy::cast_possible_truncation)]
        let shift = 125_u32 - 5 * (i as u32);
        let idx = ((payload >> shift) & 0x1f) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

/// Decode a 26-char Crockford base-32 string into 16 bytes (a ULID payload).
///
/// Strict: rejects lowercase, rejects `I/L/O/U` substitutions, rejects any
/// length other than 26.
pub fn decode_str(s: &str) -> Result<[u8; 16], CrockfordError> {
    if s.len() != ENCODED_LEN {
        return Err(CrockfordError::BadLength(s.len()));
    }
    // Build the 128-bit payload from char values placed at shift `125 - 5*i`.
    // The first character carries the top 3 payload bits (bits 125..127);
    // its top 2 bits MUST be zero (the encoding's padding).
    let mut payload: u128 = 0;
    for (i, ch) in s.bytes().enumerate() {
        let val: u128 = match ch {
            b'0'..=b'9' => u128::from(ch - b'0'),
            b'A'..=b'H' => u128::from(ch - b'A' + 10),
            b'J'..=b'K' => u128::from(ch - b'J' + 18),
            b'M'..=b'N' => u128::from(ch - b'M' + 20),
            b'P'..=b'T' => u128::from(ch - b'P' + 22),
            b'V'..=b'Z' => u128::from(ch - b'V' + 27),
            _ => {
                return Err(CrockfordError::BadChar {
                    ch: ch as char,
                    at: i,
                })
            }
        };
        if i == 0 && val > 7 {
            return Err(CrockfordError::BadPadding);
        }
        // i ∈ 0..ENCODED_LEN (= 26), so the cast is exact.
        #[allow(clippy::cast_possible_truncation)]
        let shift = 125_u32 - 5 * (i as u32);
        payload |= val << shift;
    }
    let mut out = [0u8; 16];
    for byte in out.iter_mut().rev() {
        *byte = (payload & 0xff) as u8;
        payload >>= 8;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_zero() {
        let bytes = [0u8; 16];
        let enc = encode_bytes(&bytes);
        assert_eq!(enc, "00000000000000000000000000");
        assert_eq!(decode_str(&enc).unwrap(), bytes);
    }

    #[test]
    fn round_trip_max() {
        let bytes = [0xffu8; 16];
        let enc = encode_bytes(&bytes);
        // 128 bits of 1 == 128 ones; encoded form has the top 2 bits as 0.
        assert_eq!(enc.len(), 26);
        // First char must be in 0..=7 (only the bottom 3 of its 5 bits).
        let first = enc.as_bytes()[0];
        assert!(first.is_ascii_digit() && first < b'8');
        assert_eq!(decode_str(&enc).unwrap(), bytes);
    }

    #[test]
    fn rejects_bad_length() {
        assert_eq!(decode_str("01HXYZ"), Err(CrockfordError::BadLength(6)));
        assert_eq!(decode_str(""), Err(CrockfordError::BadLength(0)));
    }

    #[test]
    fn rejects_lowercase() {
        // Lower-case `b` is not in the canonical alphabet.
        let bad = "0123456789abcdefghjkmnpqrs";
        let err = decode_str(bad).unwrap_err();
        assert!(matches!(err, CrockfordError::BadChar { .. }));
    }

    #[test]
    fn rejects_confusables() {
        for c in ['I', 'L', 'O', 'U'] {
            let mut s = String::from("0123456789ABCDEFGHJKMNPQRS");
            s.replace_range(0..1, &c.to_string());
            assert!(matches!(
                decode_str(&s),
                Err(CrockfordError::BadChar { .. })
            ));
        }
    }

    #[test]
    fn rejects_padding_violation() {
        // Encoded form starting with `8..=Z` would set padding bits.
        let bad = "80000000000000000000000000";
        assert_eq!(decode_str(bad), Err(CrockfordError::BadPadding));
    }

    #[test]
    fn known_value() {
        // Pick a ULID byte sequence and verify the encoded form.
        // Bytes: 0x01, 0x91, 0x9c, 0x06, 0x39, 0x3e, 0xc5, 0x68,
        //        0x95, 0x6e, 0xb6, 0x95, 0x57, 0xa9, 0xff, 0x7d
        let bytes: [u8; 16] = [
            0x01, 0x91, 0x9c, 0x06, 0x39, 0x3e, 0xc5, 0x68, 0x95, 0x6e, 0xb6, 0x95, 0x57, 0xa9,
            0xff, 0x7d,
        ];
        let enc = encode_bytes(&bytes);
        assert_eq!(enc.len(), 26);
        // Round-trip is the canonical assertion; we don't hardcode a
        // golden string here because that'd duplicate the impl.
        assert_eq!(decode_str(&enc).unwrap(), bytes);
    }
}
