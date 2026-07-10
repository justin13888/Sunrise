//! Error category — mirrors `docs/10-cross-cutting/error-handling.md` §kind.

use serde::{Deserialize, Serialize};

/// Error category. Used to drive client retry/backoff and UX decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// Will likely succeed if retried.
    Transient,
    /// Will not succeed without intervention.
    Permanent,
    /// User input was invalid.
    User,
    /// A bug in our code.
    Internal,
}
