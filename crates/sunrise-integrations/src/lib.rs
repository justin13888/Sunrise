//! External integrations.
//!
//! Implements `docs/09-integrations/`. v1 ships:
//!
//! - [`ical`]: RFC 5545 import/export (subset).
//! - [`gcal`]: Google Calendar OAuth flow + event sync (interface only;
//!   the HTTP client is bound by callers in the desktop / mobile apps
//!   so OAuth tokens stay on-device).
//!
//! Each integration runs through the [`IntegrationProvider`] trait so
//! the core can drive runs uniformly.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::useless_format,
    clippy::uninlined_format_args,
    clippy::format_push_string,
    clippy::needless_pass_by_value,
    clippy::missing_const_for_fn
)]

pub mod gcal;
pub mod ical;

use async_trait::async_trait;
use thiserror::Error;

/// Catalog of integration kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationKind {
    /// Google Calendar.
    GoogleCalendar,
    /// iCalendar / RFC 5545.
    ICalendar,
}

/// Run-time error from an integration.
#[derive(Debug, Error)]
pub enum IntegrationError {
    /// Auth failure (token expired, scope insufficient).
    #[error("auth: {0}")]
    Auth(String),
    /// Provider returned a hard error (rate limit, server error).
    #[error("provider: {0}")]
    Provider(String),
    /// Decode of a calendar payload failed.
    #[error("decode: {0}")]
    Decode(String),
    /// IO failure on local files.
    #[error("io: {0}")]
    Io(String),
}

/// Pluggable integration runner. Each returns a summary of what it did
/// (counts of imported / exported / skipped items) so the core's
/// observability layer can log it as `int.run.ok`.
#[async_trait]
pub trait IntegrationProvider: Send + Sync + std::fmt::Debug {
    /// Kind discriminator.
    fn kind(&self) -> IntegrationKind;

    /// Run a sync cycle. Returns counts.
    async fn run(&self) -> Result<RunSummary, IntegrationError>;
}

/// Per-run summary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunSummary {
    /// Items imported (created locally).
    pub imported: u32,
    /// Items exported (created remotely).
    pub exported: u32,
    /// Items skipped (already-in-sync).
    pub skipped: u32,
    /// Items that failed to import/export.
    pub failed: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_summary_round_trips() {
        let s = RunSummary {
            imported: 1,
            exported: 2,
            skipped: 3,
            failed: 0,
        };
        let j = serde_json::to_string(&s).unwrap();
        let back: RunSummary = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }
}
