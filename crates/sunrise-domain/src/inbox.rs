//! Inbox pseudo-stream.
//!
//! Per `docs/02-domain/streams.md`, the Inbox is a fixed-id Stream that holds
//! unassigned tasks. It is not user-creatable, deletable, or editable.

use sunrise_id::EntityRef;

/// Fixed Inbox stream id (raw bytes): three zero bytes then ASCII
/// `sunrise.inbox`.
///
/// It used to be sixteen zero bytes, which is also the id sunrise-core uses
/// for its vault-meta stream — so Inbox task ops and Stream/routine
/// lifecycle ops shared one stream and one Stream key. That was survivable
/// while every key was derived from the vault root and every device held it. It
/// is not survivable under ADR-0024: rotating the meta stream on a revocation
/// would rotate the Inbox as a side effect, and an Inbox-scoped grant would
/// carry the vault's metadata with it.
///
/// The bytes are readable in a hex dump on purpose — a sentinel that looks like
/// data is a sentinel someone eventually mistakes for data. The three leading
/// zero bytes keep the value below the smallest timestamp a ULID can carry
/// (bytes 0..6 are a millisecond clock), so no generated id can ever collide
/// with it, and they keep the Crockford encoding's first character inside the
/// two bits the padding allows.
pub const INBOX_STREAM_BYTES: [u8; 16] = [
    0x00, 0x00, 0x00, b's', b'u', b'n', b'r', b'i', b's', b'e', b'.', b'i', b'n', b'b', b'o', b'x',
];

/// Fixed Inbox stream as an [`EntityRef`].
#[must_use]
pub fn inbox_stream_ref() -> EntityRef {
    EntityRef::new(sunrise_id::EntityKind::Stream, INBOX_STREAM_BYTES)
}

/// Display form of [`INBOX_STREAM_BYTES`].
#[must_use]
pub fn inbox_stream_str() -> String {
    inbox_stream_ref().to_str()
}

/// [`INBOX_STREAM_BYTES`] in the canonical string form.
pub const INBOX_STREAM_ID: &str = "str_0000076XBEE9MQ6S9ED5Q64VVR";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_constants_consistent() {
        assert_eq!(inbox_stream_str(), INBOX_STREAM_ID);
    }

    /// The whole point of the split: the Inbox id is not the vault-meta id.
    #[test]
    fn inbox_is_not_the_meta_stream() {
        assert_ne!(INBOX_STREAM_BYTES, [0u8; 16]);
    }

    /// A generated ULID's first six bytes are a millisecond timestamp, so any
    /// id minted after 1970 sorts above this one and none can equal it.
    #[test]
    fn no_generated_ulid_can_collide() {
        assert_eq!(&INBOX_STREAM_BYTES[..3], &[0u8; 3]);
        assert!(INBOX_STREAM_BYTES[3] != 0, "the sentinel must be readable");
    }

    #[test]
    fn the_bytes_are_ascii_sunrise_inbox() {
        assert_eq!(&INBOX_STREAM_BYTES[3..], b"sunrise.inbox");
    }
}
