//! `Plain<T>` — the redaction wrapper.
//!
//! Plaintext domain data (Task title/body, Note body, attachment file name,
//! integration external title, person name, email, Stream name, Context
//! name, search query, etc.) crosses module boundaries only inside
//! `Plain<T>`. The logging API refuses to format `Plain<T>` into any field
//! by virtue of the `expose()` lint gate (see CI check in
//! `.github/workflows/ci.yml`).
//!
//! `Plain<T>` deliberately does NOT implement [`Display`], [`Debug`] (in
//! release builds), [`serde::Serialize`], or [`std::fmt::LowerHex`]. The
//! only way to read its content is `expose()`, which is banned in
//! `telemetry/`, `logging/`, and `observability/` modules by a CI grep.
//!
//! See `spec/10-cross-cutting/logging.md` §6.

use core::fmt;
use zeroize::Zeroize;

/// Wrapper marking a value as containing plaintext user data.
///
/// Construct via [`Plain::new`]; read back via [`Plain::expose`] (banned in
/// log/telemetry/observability surfaces).
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

// `Display` is intentionally NOT implemented.
// `serde::Serialize` is intentionally NOT implemented.

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
    fn map_preserves_wrapper() {
        let p: Plain<String> = Plain::new(String::from("abc")).map(|s| s.to_uppercase());
        assert_eq!(p.expose(), "ABC");
    }
}
