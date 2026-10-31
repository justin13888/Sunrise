//! Fixed-width byte arrays and their lowercase hex: the conversions every
//! layer of the crate needs and none of them owns.
//!
//! `engine::ids::blob16` and `blob32` are deliberately not here. They pad a
//! wrong-width blob where these refuse one; their doc says why.

use core::fmt::Write as _;

/// A 16-byte id read out of a DB blob, or `None` if the blob is not 16 bytes.
///
/// The crate's strict blob-to-id decoder. It used to zero-pad, and that was a
/// silent way of manufacturing a *valid* value from a corrupt one: a truncated
/// `identity_id` padded with zeros still compares equal to itself, so the vault
/// opened and every cert bound to an identity that never existed. A short blob
/// is a corrupt row, and the caller is in a position to say so — which is what
/// every caller does: the Subscribe-frame builders in `core` skip a row that
/// cannot name a stream or a device, and [`crate::Engine`]'s
/// `emit_key_envelopes` skips a `devices` row rather than sealing a Stream key
/// to a padded id.
pub(crate) fn to16(raw: &[u8]) -> Option<[u8; 16]> {
    raw.try_into().ok()
}

/// A 32-byte key read out of a DB blob, or `None` if the blob is not 32 bytes.
/// See [`to16`] for why this is not a pad.
pub(crate) fn to32(raw: &[u8]) -> Option<[u8; 32]> {
    raw.try_into().ok()
}

/// A 64-byte signature read out of a DB blob, or `None` if the blob is not 64
/// bytes. See [`to16`] for why this is not a pad.
///
/// Every caller is in the identity chain, where a signature that is the wrong
/// width is a row the fold must refuse rather than one it can pad into
/// verifying against nothing.
pub(crate) fn to64(raw: &[u8]) -> Option<[u8; 64]> {
    raw.try_into().ok()
}

/// The first four bytes of a 16-byte id as lowercase hex — **8 characters**,
/// not a full encoding.
///
/// The canonical producer of the `sender_h` and `subject_h` log fields (see
/// `sunrise_log::field`), and the short form every `Debug` impl in
/// [`crate::keychain`] prints.
///
/// Named for what it does, deliberately: the unqualified name `hex16`
/// elsewhere in this workspace (`sunrise_domain::export`,
/// `sunrise_core_bindings::dto`) takes the same `&[u8; 16]` and emits all
/// **32** characters. A `hex16` that emitted 8 read like a complete id in a
/// debug dump, which is why that spelling no longer exists here.
pub(crate) fn hex_short(b: &[u8; 16]) -> String {
    let [a, b, c, d, ..] = *b;
    hex_bytes(&[a, b, c, d])
}

/// A whole blob as lowercase hex.
///
/// Distinct from [`hex_short`], which truncates to four bytes because what it
/// renders is an id and four bytes are enough to correlate one. This renders
/// everything that has to survive whole: a version ref's ids, a fingerprint, or
/// the wrong-width `stream_id` of a row a revocation could not rotate, where
/// truncating would throw away the only thing that distinguishes one such row
/// from another.
pub(crate) fn hex_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// The 16 bytes that 32 hex characters spell, or `None` for any other text.
pub(crate) fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_width_blob_is_refused_rather_than_padded_or_cut() {
        for len in [0, 15, 17] {
            assert_eq!(to16(&vec![1u8; len]), None, "to16 of {len} bytes");
        }
        for len in [0, 31, 33] {
            assert_eq!(to32(&vec![1u8; len]), None, "to32 of {len} bytes");
        }
        for len in [0, 63, 65] {
            assert_eq!(to64(&vec![1u8; len]), None, "to64 of {len} bytes");
        }
    }

    #[test]
    fn an_exact_width_blob_keeps_its_bytes_in_order() {
        let raw: Vec<u8> = (0..64).collect();
        assert_eq!(to16(&raw[..16]).unwrap()[..], raw[..16]);
        assert_eq!(to32(&raw[..32]).unwrap()[..], raw[..32]);
        assert_eq!(to64(&raw).unwrap()[..], raw[..]);
    }

    #[test]
    fn hex_is_lowercase_zero_padded_and_short_means_four_bytes() {
        let id = [
            0x00, 0x0a, 0xb0, 0xff, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x34,
        ];
        assert_eq!(hex_short(&id), "000ab0ff");
        assert_eq!(hex_bytes(&id), "000ab0ff120000000000000000000034");
        assert_eq!(hex_bytes(&[]), "");
    }

    #[test]
    fn unhex16_inverts_hex_bytes_and_refuses_other_text() {
        let id: [u8; 16] = core::array::from_fn(|i| u8::try_from(i * 17).unwrap());
        assert_eq!(unhex16(&hex_bytes(&id)), Some(id));
        assert_eq!(unhex16(""), None);
        assert_eq!(unhex16(&"0".repeat(30)), None, "too short");
        assert_eq!(unhex16(&"0".repeat(34)), None, "too long");
        assert_eq!(unhex16(&"g".repeat(32)), None, "not hex");
        assert_eq!(unhex16(&"é".repeat(16)), None, "32 bytes, not ascii");
    }
}
