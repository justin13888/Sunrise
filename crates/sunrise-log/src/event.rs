//! Event names.
//!
//! Per `docs/10-cross-cutting/logging.md` §3, every Sunrise log record carries
//! an `ev` field naming the event, following the grammar
//! `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`. The first segment is a short
//! package id; subsequent segments are hierarchical.
//!
//! `tracing` does not care what goes in a string field, so the catalogue
//! discipline is enforced from two sides instead:
//!
//! * [`EventName::const_new`] validates the grammar at compile time, so
//!   `const _: EventName = EventName::const_new("srv.req.end");` next to a
//!   call site turns a malformed name into a build failure;
//! * `tests/event_catalog.rs` scans the workspace for `ev = "…"` literals and
//!   fails if any of them is missing from
//!   `docs/10-cross-cutting/log-events.md`. That is the part that keeps the
//!   catalogue honest — a name nobody documented cannot ship.

use thiserror::Error;

/// Errors produced when constructing an [`EventName`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EventNameError {
    /// Name was empty.
    #[error("event name must be non-empty")]
    Empty,
    /// First character of a segment was not `[a-z]`.
    #[error("event name segment must start with [a-z]: at byte offset {0}")]
    BadSegmentStart(usize),
    /// Encountered a character outside `[a-z0-9_.]`.
    #[error("event name contains invalid character {ch:?} at byte offset {at}")]
    BadChar {
        /// The offending character.
        ch: char,
        /// Byte offset within the input.
        at: usize,
    },
    /// Empty segment between dots (`..`) or trailing dot.
    #[error("event name has empty segment near byte offset {0}")]
    EmptySegment(usize),
}

/// Validated event name.
///
/// Constructing via [`EventName::new`] enforces the grammar at runtime; the
/// macros use the `const`-time variant [`EventName::const_new`] which panics
/// on invalid input (caught at compile time by const-fold for literals).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EventName(&'static str);

impl EventName {
    /// Construct from a `&'static str`, validating the grammar at runtime.
    ///
    /// Const-folds the literal at the call site, so an invalid name fails
    /// compilation rather than runtime.
    #[allow(clippy::missing_panics_doc)] // panics caught at const-eval
    pub const fn const_new(s: &'static str) -> Self {
        let bytes = s.as_bytes();
        assert!(!bytes.is_empty(), "event name must be non-empty");
        let mut i: usize = 0;
        let mut segment_start = true;
        while i < bytes.len() {
            let b = bytes[i];
            if segment_start {
                assert!(
                    b.is_ascii_lowercase(),
                    "event name segment must start with [a-z]"
                );
                segment_start = false;
            } else if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' {
                // valid
            } else if b == b'.' {
                segment_start = true;
            } else {
                panic!("event name has invalid character");
            }
            i += 1;
        }
        assert!(!segment_start, "event name has empty trailing segment");
        Self(s)
    }

    /// Construct from a `&'static str`, returning a structured error on miss.
    pub fn new(s: &'static str) -> Result<Self, EventNameError> {
        validate(s)?;
        Ok(Self(s))
    }

    /// The validated event name as a string slice.
    #[inline]
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        self.0
    }
}

impl core::fmt::Display for EventName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

/// Whether `s` satisfies the `ev` grammar.
///
/// The `&str` (rather than `&'static str`) entry point, for callers that
/// check names they did not author — `tests/event_catalog.rs` scans the
/// workspace with it.
#[must_use]
pub fn is_valid_name(s: &str) -> bool {
    validate(s).is_ok()
}

fn validate(s: &str) -> Result<(), EventNameError> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return Err(EventNameError::Empty);
    }
    let mut segment_start = true;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if segment_start {
            if !(b.is_ascii_lowercase()) {
                return Err(EventNameError::BadSegmentStart(i));
            }
            segment_start = false;
        } else if b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' {
            // ok
        } else if b == b'.' {
            segment_start = true;
            // Disallow consecutive dots.
            if i + 1 < bytes.len() && bytes[i + 1] == b'.' {
                return Err(EventNameError::EmptySegment(i));
            }
        } else {
            // Decode the offending char in UTF-8 properly.
            let ch = s[i..].chars().next().unwrap_or('?');
            return Err(EventNameError::BadChar { ch, at: i });
        }
        i += 1;
    }
    if segment_start {
        // Trailing dot.
        return Err(EventNameError::EmptySegment(bytes.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        assert!(EventName::new("sync.session.opened").is_ok());
        assert!(EventName::new("crypto").is_ok());
        assert!(EventName::new("crypto.kdf.start").is_ok());
        assert!(EventName::new("ui.input.lat").is_ok());
        assert!(EventName::new("a").is_ok());
        assert!(EventName::new("a1.b2_3").is_ok());
    }

    #[test]
    fn rejects_invalid_names() {
        assert_eq!(EventName::new(""), Err(EventNameError::Empty));
        assert_eq!(
            EventName::new("Sync"),
            Err(EventNameError::BadSegmentStart(0))
        );
        assert_eq!(
            EventName::new("9sync"),
            Err(EventNameError::BadSegmentStart(0))
        );
        assert!(matches!(
            EventName::new("sync."),
            Err(EventNameError::EmptySegment(_))
        ));
        assert!(matches!(
            EventName::new("sync..kdf"),
            Err(EventNameError::EmptySegment(_))
        ));
        assert!(matches!(
            EventName::new(".sync"),
            Err(EventNameError::BadSegmentStart(0))
        ));
        assert!(matches!(
            EventName::new("sync-kdf"),
            Err(EventNameError::BadChar { ch: '-', .. })
        ));
        assert!(matches!(
            EventName::new("sync kdf"),
            Err(EventNameError::BadChar { ch: ' ', .. })
        ));
    }

    #[test]
    fn is_valid_name_matches_the_constructor() {
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

    #[test]
    fn const_new_for_literals() {
        const N: EventName = EventName::const_new("sync.session.opened");
        assert_eq!(N.as_str(), "sync.session.opened");
    }
}
