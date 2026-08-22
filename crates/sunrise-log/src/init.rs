//! Bootstrap — assemble the `tracing` subscriber every Sunrise binary installs.
//!
//! Per `docs/10-cross-cutting/logging.md` §10 a binary initialises logging
//! once, before anything else runs. What it installs is a plain
//! `tracing_subscriber` stack:
//!
//! ```text
//! Registry
//!   └── EnvFilter        (SUNRISE_LOG, tracing-subscriber directive syntax)
//!       └── RedactionLayer  (the allowlist veto — this crate's actual job)
//!           └── fmt layer   (NDJSON or pretty, to stderr / file / capture)
//! ```
//!
//! Levels, spans, callsite caching, per-target filtering and formatting are
//! `tracing`'s. Only the middle layer is ours.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{Dispatch, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use crate::redact::RedactionLayer;
use crate::time::Rfc3339Millis;
use crate::writer::{Capture, RollingFile};

/// Env var: per-target filter directives, `tracing-subscriber` syntax.
pub const ENV_LOG: &str = "SUNRISE_LOG";
/// Env var: `ndjson` (default) or `pretty` (dev builds only).
pub const ENV_LOG_FORMAT: &str = "SUNRISE_LOG_FORMAT";
/// Env var: override the file destination for file-logging binaries.
pub const ENV_LOG_FILE: &str = "SUNRISE_LOG_FILE";

/// Filter used when `SUNRISE_LOG` is unset or unparsable.
pub const DEFAULT_FILTER: &str = "info";

/// Errors raised while installing the logger.
#[derive(Debug, Error)]
pub enum LogError {
    /// A subscriber was already installed for this process.
    #[error("a tracing subscriber is already installed")]
    AlreadyInitialized,
    /// The log file could not be opened.
    #[error("cannot open log file {path}: {source}")]
    LogFile {
        /// Path that failed to open.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
}

/// Record encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// One JSON object per line. The ingest format; the default everywhere.
    #[default]
    Ndjson,
    /// Human-readable single line. Dev builds only — see [`LogFormat::parse`].
    Pretty,
}

impl LogFormat {
    /// Parse a `SUNRISE_LOG_FORMAT` value.
    ///
    /// `debug_build` gates `pretty`: logging.md §8 refuses the pretty
    /// formatter in release builds, because a release binary's output is
    /// something an ingest pipeline parses, not something a human reads. An
    /// unrecognised value falls back to NDJSON rather than failing startup —
    /// a typo in an env var must not stop a server from booting.
    #[must_use]
    pub fn parse(value: Option<&str>, debug_build: bool) -> Self {
        match value.map(str::trim) {
            Some("pretty") if debug_build => Self::Pretty,
            _ => Self::Ndjson,
        }
    }

    /// Read [`ENV_LOG_FORMAT`] from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self::parse(
            std::env::var(ENV_LOG_FORMAT).ok().as_deref(),
            cfg!(debug_assertions),
        )
    }
}

/// Where formatted records go.
#[derive(Debug, Clone)]
pub enum LogTarget {
    /// Standard error. Servers and CLIs.
    Stderr,
    /// A size-capped NDJSON file. Full-screen terminal apps, whose display
    /// stderr would corrupt.
    File(PathBuf),
    /// An in-memory buffer. Tests.
    Capture(Capture),
}

/// Everything needed to build a subscriber.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// Destination.
    pub target: LogTarget,
    /// `EnvFilter` directives.
    pub filter: String,
    /// Record encoding.
    pub format: LogFormat,
}

impl LogConfig {
    /// Config for `target`, taking filter and format from the environment.
    #[must_use]
    pub fn from_env(target: LogTarget) -> Self {
        Self {
            target,
            filter: std::env::var(ENV_LOG).unwrap_or_else(|_| DEFAULT_FILTER.to_string()),
            format: LogFormat::from_env(),
        }
    }
}

/// The default log-file location for a file-logging binary.
///
/// `docs/10-cross-cutting/logging.md` §8:
/// `~/.local/state/sunrise/log/<binary>.ndjson` on Linux, honouring
/// `XDG_STATE_HOME` when set. Pure over its inputs so it is testable without
/// touching the process environment.
#[must_use]
pub fn log_path_in(binary: &str, xdg_state_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let base = match xdg_state_home.map(str::trim).filter(|s| !s.is_empty()) {
        Some(state) => PathBuf::from(state),
        None => PathBuf::from(home.unwrap_or("."))
            .join(".local")
            .join("state"),
    };
    base.join("sunrise")
        .join("log")
        .join(format!("{binary}.ndjson"))
}

/// [`log_path_in`] against the process environment, with [`ENV_LOG_FILE`]
/// taking precedence.
#[must_use]
pub fn default_log_path(binary: &str) -> PathBuf {
    if let Ok(explicit) = std::env::var(ENV_LOG_FILE) {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    log_path_in(
        binary,
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

fn fmt_layer<S, W>(format: LogFormat, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    match format {
        LogFormat::Ndjson => tracing_subscriber::fmt::layer()
            .json()
            // `ev` and the message sit at the top level rather than nested
            // under `fields`, so a record is one flat object as
            // schemas/log-record.v1.json describes.
            .flatten_event(true)
            .with_current_span(true)
            // The full ancestor list would repeat every parent span's fields
            // on every line; the innermost span is what identifies the
            // operation.
            .with_span_list(false)
            .with_timer(Rfc3339Millis)
            .with_writer(writer)
            .boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer()
            .with_timer(Rfc3339Millis)
            .with_writer(writer)
            .boxed(),
    }
}

/// Assemble a subscriber without installing it.
///
/// Tests use this with `tracing::subscriber::with_default` so each one gets
/// its own dispatcher instead of racing over the process-global slot.
///
/// # Errors
/// [`LogError::LogFile`] if a [`LogTarget::File`] path cannot be opened.
pub fn build_subscriber(cfg: LogConfig) -> Result<Dispatch, LogError> {
    build_subscriber_with(cfg, RedactionLayer::new())
}

/// As [`build_subscriber`], with a caller-supplied redaction layer.
///
/// # Errors
/// [`LogError::LogFile`] if a [`LogTarget::File`] path cannot be opened.
pub fn build_subscriber_with(
    cfg: LogConfig,
    redaction: RedactionLayer,
) -> Result<Dispatch, LogError> {
    let LogConfig {
        target,
        filter,
        format,
    } = cfg;

    // An unparsable directive string falls back to the default rather than
    // aborting startup: `SUNRISE_LOG=inf` should cost you verbosity, not the
    // process.
    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let base = Registry::default().with(filter).with(redaction);

    let sink = match target {
        LogTarget::Stderr => fmt_layer(format, std::io::stderr),
        LogTarget::Capture(cap) => fmt_layer(format, cap),
        LogTarget::File(path) => {
            let file =
                RollingFile::open(&path).map_err(|source| LogError::LogFile { path, source })?;
            fmt_layer(format, file)
        }
    };

    Ok(Dispatch::new(base.with(sink)))
}

/// Build and install the process-global subscriber.
///
/// # Errors
/// [`LogError::LogFile`] if the log file cannot be opened;
/// [`LogError::AlreadyInitialized`] on a second call.
pub fn init(cfg: LogConfig) -> Result<(), LogError> {
    let dispatch = build_subscriber(cfg)?;
    tracing::dispatcher::set_global_default(dispatch).map_err(|_| LogError::AlreadyInitialized)
}

/// Install a stderr logger configured from the environment.
///
/// # Errors
/// [`LogError::AlreadyInitialized`] on a second call.
pub fn init_stderr() -> Result<(), LogError> {
    init(LogConfig::from_env(LogTarget::Stderr))
}

/// Install a file logger for `binary`, returning the path in use.
///
/// This is what a full-screen terminal application calls: writing records to
/// stdout or stderr would interleave them with the alternate-screen buffer
/// and corrupt the display. There is no stderr fallback for the same reason —
/// if the file cannot be opened the caller gets an error and decides, and the
/// TUI's answer is to run without logs rather than to wreck the screen.
///
/// # Errors
/// [`LogError::LogFile`] if the path cannot be opened;
/// [`LogError::AlreadyInitialized`] on a second call.
pub fn init_file(binary: &str) -> Result<PathBuf, LogError> {
    let path = default_log_path(binary);
    init_file_at(&path)?;
    Ok(path)
}

/// As [`init_file`], at an explicit path.
///
/// # Errors
/// [`LogError::LogFile`] if the path cannot be opened;
/// [`LogError::AlreadyInitialized`] on a second call.
pub fn init_file_at(path: &Path) -> Result<(), LogError> {
    init(LogConfig::from_env(LogTarget::File(path.to_path_buf())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_defaults_to_ndjson() {
        assert_eq!(LogFormat::parse(None, true), LogFormat::Ndjson);
        assert_eq!(LogFormat::parse(Some("ndjson"), true), LogFormat::Ndjson);
        assert_eq!(LogFormat::default(), LogFormat::Ndjson);
    }

    #[test]
    fn pretty_is_dev_only() {
        assert_eq!(LogFormat::parse(Some("pretty"), true), LogFormat::Pretty);
        // Release build: the request is refused, not honoured.
        assert_eq!(LogFormat::parse(Some("pretty"), false), LogFormat::Ndjson);
    }

    #[test]
    fn unknown_format_falls_back_rather_than_failing() {
        assert_eq!(LogFormat::parse(Some("yaml"), true), LogFormat::Ndjson);
        assert_eq!(LogFormat::parse(Some(""), true), LogFormat::Ndjson);
    }

    #[test]
    fn log_path_honours_xdg_state_home() {
        assert_eq!(
            log_path_in("sunrise-tui", Some("/var/state"), Some("/home/u")),
            PathBuf::from("/var/state/sunrise/log/sunrise-tui.ndjson")
        );
    }

    #[test]
    fn log_path_falls_back_to_local_state_under_home() {
        assert_eq!(
            log_path_in("sunrise-tui", None, Some("/home/u")),
            PathBuf::from("/home/u/.local/state/sunrise/log/sunrise-tui.ndjson")
        );
        // A blank XDG_STATE_HOME behaves as unset.
        assert_eq!(
            log_path_in("sunrise-tui", Some("  "), Some("/home/u")),
            PathBuf::from("/home/u/.local/state/sunrise/log/sunrise-tui.ndjson")
        );
    }

    #[test]
    fn bad_filter_directives_do_not_break_the_build() {
        let cfg = LogConfig {
            target: LogTarget::Capture(Capture::new()),
            filter: "this is not a filter!!".to_string(),
            format: LogFormat::Ndjson,
        };
        assert!(build_subscriber(cfg).is_ok());
    }

    #[test]
    fn file_target_reports_an_unopenable_path() {
        let cfg = LogConfig {
            // A path whose parent is an existing *file* cannot be created.
            target: LogTarget::File(PathBuf::from("/dev/null/nope/x.ndjson")),
            filter: DEFAULT_FILTER.to_string(),
            format: LogFormat::Ndjson,
        };
        assert!(matches!(
            build_subscriber(cfg),
            Err(LogError::LogFile { .. })
        ));
    }
}
