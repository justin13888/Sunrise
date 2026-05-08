//! Stderr NDJSON sink.

use super::Sink;
use crate::level::Level;
use parking_lot::Mutex;
use std::io::{self, Write};

/// Lock-protected writer over `io::stderr`.
///
/// `eprintln!`/`println!` are forbidden by the workspace clippy config; this
/// sink is the only sanctioned path to stderr from within the workspace.
#[derive(Debug)]
pub struct StderrSink {
    min: Level,
    lock: Mutex<()>,
}

impl StderrSink {
    /// Construct with a minimum level filter.
    #[must_use]
    pub const fn new(min: Level) -> Self {
        Self {
            min,
            lock: Mutex::new(()),
        }
    }
}

impl Sink for StderrSink {
    fn min_level(&self) -> Level {
        self.min
    }

    fn write(&self, record_ndjson: &[u8]) {
        let _guard = self.lock.lock();
        // Use the locked stderr handle directly so the workspace
        // `print_stderr` lint stays satisfied (we're not using `eprintln!`).
        let stderr = io::stderr();
        let mut handle = stderr.lock();
        let _ = handle.write_all(record_ndjson);
        let _ = handle.write_all(b"\n");
        let _ = handle.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_level_returned() {
        let s = StderrSink::new(Level::Warn);
        assert_eq!(s.min_level(), Level::Warn);
    }
}
