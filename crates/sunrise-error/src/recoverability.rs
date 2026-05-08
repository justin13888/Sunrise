//! UI-facing recoverability classification.

use core::fmt;
use serde::{Deserialize, Serialize};

/// How the UI should react to an error.
///
/// Per `spec/10-cross-cutting/error-handling.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recoverability {
    /// Small unobtrusive toast, auto-dismiss, no action.
    Transient,
    /// Persistent banner + button (e.g., "Sign in again").
    UserActionRequired,
    /// Modal-ish dialog with "Send diagnostics" affordance.
    Fatal,
}

impl fmt::Display for Recoverability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transient => "transient",
            Self::UserActionRequired => "user_action_required",
            Self::Fatal => "fatal",
        })
    }
}
