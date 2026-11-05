//! How an id with no `EntityRef` prefix is written out.
//!
//! A device id is a BLAKE3 derivation of `D_S_pub`, an identity id one of
//! `ID_S_pub`, and an op id and a blob id are random: none of them is a ULID,
//! so lowercase hex is the only form they have ever had on screen, in a log or
//! on the wire. Every client writes them the same way, which is why the one
//! spelling lives here rather than in each seam (#536 left three copies).

/// Lowercase hex of `bytes`, two digits a byte, no separator.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Lowercase hex of a 16-byte id: 32 characters.
#[must_use]
pub fn hex_id(id: &[u8; 16]) -> String {
    hex(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sixteen_byte_id_is_thirty_two_lowercase_digits() {
        let id = [
            0x00, 0x01, 0x0a, 0x0f, 0x10, 0x7f, 0x80, 0xab, 0xcd, 0xef, 0xf0, 0xff, 0x12, 0x34,
            0x56, 0x78,
        ];
        assert_eq!(hex_id(&id), "00010a0f107f80abcdeff0ff12345678");
    }

    #[test]
    fn empty_bytes_are_an_empty_string() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0xde, 0xad]), "dead");
    }
}
