//! Validation helpers shared across entities.
//!
//! Validation errors map to canonical [`sunrise_error::ErrorCode`] values so
//! the core can surface them to UIs without translation.

use sunrise_error::ErrorCode;
use thiserror::Error;

/// Maximum length of a Task title (in chars after trim) per spec/02-domain/tasks.md.
pub const MAX_TASK_TITLE_LEN: usize = 512;

/// Maximum length of a Stream name per spec/02-domain/streams.md.
pub const MAX_STREAM_NAME_LEN: usize = 128;

/// Maximum length of a Context name (lower bound; UI may further constrain).
pub const MAX_CONTEXT_NAME_LEN: usize = 64;

/// Per spec/02-domain/tasks.md §validation: encoded Task envelope ≤ 1.25 MiB.
pub const MAX_TASK_ENVELOPE_BYTES: usize = 1_310_720;

/// Validation errors. Each carries the canonical wire code so the core can
/// surface it without translation.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ValidationError {
    /// `title` was empty after trim.
    #[error("title is empty after trim")]
    InvalidTitle,
    /// `due_at` < `scheduled_at`.
    #[error("due_at must be ≥ scheduled_at")]
    DueBeforeScheduled,
    /// Encoded Task envelope is over the size budget.
    #[error("Task envelope exceeds {MAX_TASK_ENVELOPE_BYTES} bytes")]
    PayloadTooLarge,
    /// `blocked_by` contained the task's own id (self-blocking) or formed a
    /// cycle locally.
    #[error("blocked_by introduces a cycle")]
    BlockedByCycle,
    /// Stream `parent_id` referenced a non-stream entity, an archived/deleted
    /// stream, or itself.
    #[error("invalid stream parent_id")]
    InvalidStreamParent,
    /// Field-shape violation with structured diagnostic. The string is
    /// dotted-path into the command payload.
    #[error("validation field error at {field}: {constraint}")]
    Field {
        /// Dotted-path into the command payload (e.g., `"task.title"`).
        field: &'static str,
        /// Constraint name (e.g., `"max_length"`).
        constraint: &'static str,
    },
}

impl ValidationError {
    /// Canonical error code for the wire.
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        match self {
            Self::InvalidTitle => ErrorCode::ValidationInvalidTitle,
            Self::DueBeforeScheduled => ErrorCode::ValidationDueBeforeScheduled,
            Self::PayloadTooLarge => ErrorCode::ValidationPayloadTooLarge,
            Self::BlockedByCycle => ErrorCode::ValidationBlockedByCycle,
            Self::InvalidStreamParent | Self::Field { .. } => ErrorCode::ValidationField,
        }
    }
}

/// Trim and validate a title against a max-char-after-trim rule.
///
/// Returns the trimmed string on success or [`ValidationError::InvalidTitle`]
/// on empty after trim, or `Field { constraint: "max_length" }` on too long.
pub fn validate_title(
    raw: &str,
    field: &'static str,
    max_chars: usize,
) -> Result<String, ValidationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::InvalidTitle);
    }
    if trimmed.chars().count() > max_chars {
        return Err(ValidationError::Field {
            field,
            constraint: "max_length",
        });
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_title_trims() {
        assert_eq!(validate_title("  hi  ", "task.title", 512).unwrap(), "hi");
    }

    #[test]
    fn validate_title_rejects_empty() {
        assert_eq!(
            validate_title("   ", "task.title", 512),
            Err(ValidationError::InvalidTitle)
        );
    }

    #[test]
    fn validate_title_rejects_too_long() {
        let long = "x".repeat(513);
        let err = validate_title(&long, "task.title", 512).unwrap_err();
        assert!(matches!(
            err,
            ValidationError::Field {
                constraint: "max_length",
                ..
            }
        ));
    }

    #[test]
    fn validate_title_counts_chars_not_bytes() {
        // 4-byte emoji counts as 1 char.
        let s = "🌅".repeat(512);
        assert!(validate_title(&s, "task.title", 512).is_ok());
        let too = "🌅".repeat(513);
        assert!(validate_title(&too, "task.title", 512).is_err());
    }

    #[test]
    fn error_codes_map_to_canonical() {
        assert_eq!(
            ValidationError::InvalidTitle.as_error_code(),
            ErrorCode::ValidationInvalidTitle
        );
        assert_eq!(
            ValidationError::PayloadTooLarge.as_error_code(),
            ErrorCode::ValidationPayloadTooLarge
        );
        assert_eq!(
            ValidationError::BlockedByCycle.as_error_code(),
            ErrorCode::ValidationBlockedByCycle
        );
    }
}
