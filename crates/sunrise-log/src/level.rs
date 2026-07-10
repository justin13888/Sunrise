//! Logging levels.
//!
//! Five levels, RFC 5424-aligned semantics, mapped 1:1 to `tracing::Level` /
//! `pino`. See `docs/10-cross-cutting/logging.md` §2.

use serde::{Deserialize, Serialize};

/// Severity of a log record. Lower discriminant = lower severity.
///
/// Note: spec disallows a separate `fatal`. Conditions that would be fatal are
/// logged at [`Level::Error`] immediately followed by a controlled abort path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Per-byte parse decisions, inner-loop counters, frame-by-frame UI events.
    Trace,
    /// One-per-operation diagnostic detail.
    Debug,
    /// One-per-significant-event.
    Info,
    /// Recoverable degradation.
    Warn,
    /// Operation failed; user-visible behavior affected.
    Error,
}

impl Level {
    /// Lowercase canonical name used in NDJSON `lv` field and env filters.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Parse a level from its lowercase canonical name. Returns `None` on miss.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

impl core::fmt::Display for Level {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_stable() {
        assert!(Level::Trace < Level::Debug);
        assert!(Level::Debug < Level::Info);
        assert!(Level::Info < Level::Warn);
        assert!(Level::Warn < Level::Error);
    }

    #[test]
    fn parse_round_trip() {
        for lv in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
        ] {
            assert_eq!(Level::parse(lv.as_str()), Some(lv));
        }
        assert_eq!(Level::parse("FATAL"), None);
        assert_eq!(Level::parse("info "), None);
    }

    #[test]
    fn serializes_lowercase() {
        let s = serde_json::to_string(&Level::Info).unwrap();
        assert_eq!(s, r#""info""#);
    }
}
