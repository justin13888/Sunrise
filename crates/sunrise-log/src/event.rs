//! Event names.
//!
//! Per `docs/10-cross-cutting/logging.md` §3, every Sunrise log record carries
//! an `ev` field naming the event, following the grammar
//! `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`. The first segment is a short
//! package id; subsequent segments are hierarchical.
//!
//! `tracing` does not care what goes in a string field, so the catalogue
//! discipline is enforced by a scan instead: the `event_catalog` test
//! (`tests/event_catalog/`) reads every `ev = "…"` literal the workspace ships,
//! checks it against this grammar with [`is_valid_name`], and fails if any of
//! them is missing from `docs/10-cross-cutting/log-events.md`. That is the
//! part that keeps the catalogue honest — a name nobody documented cannot
//! ship.

/// Why a name fails the `ev` grammar, for the tests that pin each rejection.
#[derive(Debug, PartialEq, Eq)]
enum NameError {
    /// Name was empty.
    Empty,
    /// First character of a segment was not `[a-z]`, at this byte offset.
    BadSegmentStart(usize),
    /// A character outside `[a-z0-9_.]`.
    BadChar {
        /// The offending character.
        ch: char,
        /// Byte offset within the input.
        at: usize,
    },
    /// Empty segment between dots (`..`) or trailing dot, near this offset.
    EmptySegment(usize),
}

/// Whether `s` satisfies the `ev` grammar.
///
/// For callers that check names they did not author — the `event_catalog`
/// test scans the workspace with it.
#[must_use]
pub fn is_valid_name(s: &str) -> bool {
    validate(s).is_ok()
}

fn validate(s: &str) -> Result<(), NameError> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(NameError::Empty);
    }
    let mut segment_start = true;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if segment_start {
            if !(b.is_ascii_lowercase()) {
                return Err(NameError::BadSegmentStart(i));
            }
            segment_start = false;
        } else if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' {
            // ok
        } else if b == b'.' {
            segment_start = true;
            // Disallow consecutive dots.
            if i + 1 < bytes.len() && bytes[i + 1] == b'.' {
                return Err(NameError::EmptySegment(i));
            }
        } else {
            // Decode the offending char in UTF-8 properly.
            let ch = s[i..].chars().next().unwrap_or('?');
            return Err(NameError::BadChar { ch, at: i });
        }
        i += 1;
    }
    if segment_start {
        // Trailing dot.
        return Err(NameError::EmptySegment(bytes.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        assert!(validate("sync.session.opened").is_ok());
        assert!(validate("crypto").is_ok());
        assert!(validate("crypto.kdf.start").is_ok());
        assert!(validate("ui.input.lat").is_ok());
        assert!(validate("a").is_ok());
        assert!(validate("a1.b2_3").is_ok());
    }

    #[test]
    fn rejects_invalid_names() {
        assert_eq!(validate(""), Err(NameError::Empty));
        assert_eq!(validate("Sync"), Err(NameError::BadSegmentStart(0)));
        assert_eq!(validate("9sync"), Err(NameError::BadSegmentStart(0)));
        assert!(matches!(validate("sync."), Err(NameError::EmptySegment(_))));
        assert!(matches!(
            validate("sync..kdf"),
            Err(NameError::EmptySegment(_))
        ));
        assert!(matches!(
            validate(".sync"),
            Err(NameError::BadSegmentStart(0))
        ));
        assert!(matches!(
            validate("sync-kdf"),
            Err(NameError::BadChar { ch: '-', .. })
        ));
        assert!(matches!(
            validate("sync kdf"),
            Err(NameError::BadChar { ch: ' ', .. })
        ));
    }

    #[test]
    fn is_valid_name_matches_validate() {
        for good in ["sync.session.opened", "srv.req.end", "a", "a1.b2_3"] {
            assert!(is_valid_name(good), "{good} should be valid");
        }
        for bad in [
            "",
            "Sync",
            "sync.",
            "sync..kdf",
            ".sync",
            "sync-kdf",
            "sync kdf",
        ] {
            assert!(!is_valid_name(bad), "{bad} should be invalid");
        }
    }
}
