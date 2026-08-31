//! `Plain<T>` — the redaction wrapper.
//!
//! Plaintext domain data (Task title/body, Note body, attachment file name,
//! integration external title, person name, email, Stream name, Context
//! name, search query, etc.) crosses module boundaries only inside
//! `Plain<T>`.
//!
//! # The guarantee
//!
//! `Plain<T>` implements **no `Display`**, **no `serde::Serialize`**, and
//! **no `tracing::Value`**. Its `Debug` prints the fixed string `Plain<…>`.
//! Those four facts together close every route a value can take into a
//! `tracing` record:
//!
//! | Call site | Result |
//! |---|---|
//! | `info!(title = plain)` | does not compile — no `Value` impl |
//! | `info!(title = %plain)` | does not compile — no `Display` impl |
//! | `info!(title = ?plain)` | compiles, records the literal `Plain<…>` |
//! | `info!(title = field::debug(&plain))` | records the literal `Plain<…>` |
//! | `info!("{plain:?}")` | records the literal `Plain<…>` |
//! | span fields, by any of the above | same |
//!
//! So the sink cannot see the payload. `tests/redaction.rs` asserts exactly
//! this, over random payloads, through every one of those paths.
//!
//! # The escape hatch, and what guards it
//!
//! [`Plain::expose`] returns the inner value, because real code has to render
//! a task title *somewhere*. Two gates keep that somewhere away from logs:
//!
//! * CI greps `\bplain[a-z_]*\.expose[[:space:]]*\(` across every surface
//!   that logs and rejects a match (`.github/workflows/ci.yml`,
//!   `log-redaction`);
//! * an exposed `String` logged under a field name nobody vetted is refused
//!   at runtime by [`crate::RedactionLayer`].
//!
//! See `docs/10-cross-cutting/logging.md` §6.

use core::fmt;
use zeroize::Zeroize;

/// Wrapper marking a value as containing plaintext user data.
///
/// Construct via [`Plain::new`]; read back via [`Plain::expose`] (banned in
/// log/telemetry/observability surfaces). See the module docs for the full
/// list of formatting paths this closes.
#[derive(Clone)]
pub struct Plain<T>(T);

impl<T> Plain<T> {
    /// Tag a value as plaintext user data.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Read the inner value.
    ///
    /// **DO NOT CALL** from logging, telemetry, or observability surfaces.
    /// CI greps for `\bplain[a-z_]*\.expose\(` in those subtrees and rejects.
    #[inline]
    pub fn expose(self) -> T {
        self.0
    }

    /// Read the inner value by reference.
    ///
    /// **DO NOT CALL** from logging, telemetry, or observability surfaces.
    #[inline]
    pub fn expose_ref(&self) -> &T {
        &self.0
    }

    /// Map the inner value, preserving the `Plain` wrapper.
    #[must_use]
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Plain<U> {
        Plain(f(self.0))
    }
}

/// Debug prints `Plain<…>` opaquely so accidental `{:?}` does not leak.
impl<T> fmt::Debug for Plain<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Plain<…>")
    }
}

// `Display` is intentionally NOT implemented — it would make `%plain` compile.
// `serde::Serialize` is intentionally NOT implemented — it would let the JSON
// formatter render the payload.
// `tracing::Value` is intentionally NOT implemented — it would make a bare
// `info!(field = plain)` compile.

impl<T: Zeroize> Zeroize for Plain<T> {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl<T> From<T> for Plain<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak() {
        let p = Plain::new("user title");
        let s = format!("{p:?}");
        assert_eq!(s, "Plain<…>");
        assert!(!s.contains("user"));
    }

    #[test]
    fn expose_returns_inner() {
        let p = Plain::new(String::from("secret"));
        assert_eq!(p.expose(), "secret");
    }

    #[test]
    fn debug_is_opaque_for_every_inner_type() {
        // The opacity must not depend on `T: Debug` doing something sensible.
        #[derive(Clone)]
        struct Loud;
        impl fmt::Debug for Loud {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("SECRET-PAYLOAD")
            }
        }
        assert_eq!(format!("{:?}", Plain::new(Loud)), "Plain<…>");
        assert_eq!(format!("{:?}", Plain::new(vec![1u8, 2, 3])), "Plain<…>");
        assert_eq!(format!("{:?}", Plain::new(Some("x"))), "Plain<…>");
    }

    #[test]
    fn nesting_does_not_unwrap() {
        let p = Plain::new(Plain::new("inner"));
        assert_eq!(format!("{p:?}"), "Plain<…>");
    }

    #[test]
    fn map_preserves_wrapper() {
        let p: Plain<String> = Plain::new(String::from("abc")).map(|s| s.to_uppercase());
        assert_eq!(p.expose(), "ABC");
    }
}
