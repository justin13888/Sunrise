//! Stable canonical error codes shared across the workspace.
//!
//! Per `docs/10-cross-cutting/error-handling.md`, the registry source of
//! truth is `codes.toml`. The Rust enum here is intended to be a generated
//! mirror; until the build-script codegen lands, it is hand-maintained and
//! kept in sync by the test in `tests/manifest_in_sync.rs`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codes;
pub mod kind;
pub mod recoverability;

pub use codes::ErrorCode;
pub use kind::ErrorKind;
pub use recoverability::Recoverability;

use serde::{Deserialize, Serialize};

/// Canonical error envelope returned by the core to the UI.
///
/// Per `docs/10-cross-cutting/error-handling.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreError {
    /// Stable canonical code.
    pub code: ErrorCode,
    /// Whether the operation can be retried, requires user action, or is
    /// fatal.
    pub recoverable: Recoverability,
    /// Dev-only diagnostic; never shown to users verbatim. MUST NOT contain
    /// plaintext user data (per logging.md §6 — same redaction rules apply
    /// because errors flow through logs).
    pub diagnostic: String,
}

impl CoreError {
    /// Construct a transient retryable error.
    #[must_use]
    pub fn transient(code: ErrorCode, diagnostic: impl Into<String>) -> Self {
        Self {
            code,
            recoverable: Recoverability::Transient,
            diagnostic: diagnostic.into(),
        }
    }

    /// Construct an error that requires user intervention to resolve.
    #[must_use]
    pub fn user_action(code: ErrorCode, diagnostic: impl Into<String>) -> Self {
        Self {
            code,
            recoverable: Recoverability::UserActionRequired,
            diagnostic: diagnostic.into(),
        }
    }

    /// Construct a fatal error.
    #[must_use]
    pub fn fatal(code: ErrorCode, diagnostic: impl Into<String>) -> Self {
        Self {
            code,
            recoverable: Recoverability::Fatal,
            diagnostic: diagnostic.into(),
        }
    }
}

impl core::fmt::Display for CoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({})", self.code.as_str(), self.recoverable)
    }
}

impl std::error::Error for CoreError {}
