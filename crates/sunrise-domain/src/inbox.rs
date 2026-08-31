//! Inbox pseudo-stream.
//!
//! Per `docs/02-domain/streams.md`, the Inbox is a fixed-id Stream that holds
//! unassigned tasks. It is not user-creatable, deletable, or editable.

use sunrise_id::EntityRef;

/// Fixed Inbox stream id (raw bytes). All-zero so the prefixed form is
/// `str_00000000000000000000000000`.
pub const INBOX_STREAM_BYTES: [u8; 16] = [0u8; 16];

/// Fixed Inbox stream as an [`EntityRef`].
#[must_use]
pub fn inbox_stream_ref() -> EntityRef {
    EntityRef::new(sunrise_id::EntityKind::Stream, INBOX_STREAM_BYTES)
}

/// Display form: `str_INBOX0000000000000000000000`.
///
/// Note: spec requires a sentinel ULID body; v1 uses 16 zero bytes. The
/// prefix is `str_`, so the rendered form is `str_00000000000000000000000000`.
/// Some spec drafts cited `str_INBOX...`; we use the simpler all-zero
/// canonical form here. The pseudo-id is a convention, not a wire ULID.
#[must_use]
pub fn inbox_stream_str() -> String {
    inbox_stream_ref().to_str()
}

/// `INBOX_STREAM_ID` constant in the canonical string form.
pub const INBOX_STREAM_ID: &str = "str_00000000000000000000000000";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_constants_consistent() {
        assert_eq!(inbox_stream_str(), INBOX_STREAM_ID);
    }
}
