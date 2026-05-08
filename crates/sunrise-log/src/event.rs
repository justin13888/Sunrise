//! Event names.
//!
//! Per `spec/10-cross-cutting/logging.md` §3, the `ev` field follows the
//! grammar `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`. The first segment is a
//! short package id; subsequent segments are hierarchical. New event names
//! require an entry in `docs/log-events.md` (gated by the per-package
//! catalog snapshot test).

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
    /// Used by the `event!` macro to const-fold the literal at the call site;
    /// invalid names fail compilation rather than runtime.
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
    fn const_new_for_literals() {
        const N: EventName = EventName::const_new("sync.session.opened");
        assert_eq!(N.as_str(), "sync.session.opened");
    }
}
