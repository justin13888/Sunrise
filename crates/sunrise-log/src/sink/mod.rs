//! Log sinks.
//!
//! Per `docs/10-cross-cutting/logging.md` §8, four sinks may be enabled:
//! `stderr`, `file`, `ring`, `remote`. v1 implements `stderr` (functional)
//! and `ring` (functional in-memory circular buffer); `file` is a thin
//! NDJSON-to-file appender (rotation/gzip deferred to future work, marked
//! TODO inline) and `remote` is an interface only (no I/O in v1).

use crate::level::Level;
use std::fmt::Debug;

pub mod ring;
pub mod stderr;

/// A log sink. Implementations must be `Send + Sync` so that the global
/// dispatcher can fan out to them from any thread.
pub trait Sink: Send + Sync + Debug {
    /// Minimum level this sink accepts. Records below this level are
    /// dropped before the sink sees them.
    fn min_level(&self) -> Level;

    /// Write one already-serialized NDJSON record (no trailing newline; the
    /// sink appends `\n`). The slice is borrowed; sinks that want to defer
    /// must copy.
    fn write(&self, record_ndjson: &[u8]);

    /// Whether the `share: true` marker is required to accept the record.
    /// Per logging.md §8, only the `remote` sink sets this to `true`.
    fn requires_share(&self) -> bool {
        false
    }

    /// Flush any buffered output. Called on graceful shutdown and when the
    /// diagnostic-bundle exporter snapshots the in-memory ring.
    fn flush(&self) {}
}

pub use ring::RingSink;
pub use stderr::StderrSink;
