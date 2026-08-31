//! Where formatted records land.
//!
//! `tracing-subscriber` formats; it leaves the byte destination to a
//! [`MakeWriter`]. Two are needed that it does not ship:
//!
//! * [`RollingFile`] — a size-capped NDJSON file, because a full-screen TUI
//!   cannot log to stderr without shredding its own display, and an
//!   uncapped log file on a laptop is an out-of-disk incident waiting for a
//!   retry loop. This is the `file` sink `docs/10-cross-cutting/logging.md`
//!   §8 has always specified and that no code ever implemented.
//! * [`Capture`] — an in-memory buffer, so redaction tests can assert on the
//!   exact bytes a sink would have received.
//!
//! Neither is a logging implementation; both are ~40 lines of I/O glue of the
//! kind `MakeWriter` exists to accept.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tracing_subscriber::fmt::MakeWriter;

/// Default size cap before the log rolls, matching logging.md §8's 16 MiB
/// rotation threshold.
pub const DEFAULT_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Append-only NDJSON file writer with a single-generation size roll.
///
/// At `max_bytes` the current file is renamed to `<name>.1` (replacing any
/// previous `.1`) and a fresh file is opened. Two generations is the whole
/// retention policy: enough to survive a roll mid-incident, bounded at
/// `2 × max_bytes` on disk no matter how loud the process gets.
#[derive(Debug, Clone)]
pub struct RollingFile {
    inner: Arc<Mutex<RollingState>>,
}

#[derive(Debug)]
struct RollingState {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
}

impl RollingFile {
    /// Open (creating parent directories as needed) the log at `path`.
    ///
    /// # Errors
    /// Any filesystem error creating the directory or opening the file.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_max_bytes(path, DEFAULT_MAX_BYTES)
    }

    /// As [`RollingFile::open`], with an explicit size cap.
    ///
    /// # Errors
    /// Any filesystem error creating the directory or opening the file.
    pub fn with_max_bytes(path: impl Into<PathBuf>, max_bytes: u64) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = open_append(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(RollingState {
                path,
                file,
                written,
                max_bytes,
            })),
        })
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rolled_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

impl Write for RollingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.inner.lock();
        if state.written >= state.max_bytes {
            // Roll. A failure here must not lose the record: fall through and
            // keep appending to the current file rather than returning an
            // error that the subscriber would silently swallow anyway.
            let target = rolled_path(&state.path);
            if std::fs::rename(&state.path, &target).is_ok() {
                if let Ok(fresh) = open_append(&state.path) {
                    state.file = fresh;
                    state.written = 0;
                }
            }
        }
        let n = state.file.write(buf)?;
        state.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().file.flush()
    }
}

impl<'a> MakeWriter<'a> for RollingFile {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// In-memory writer that keeps every byte a sink would have seen.
///
/// Test-facing, but not `#[cfg(test)]`: integration tests in `tests/` are a
/// separate crate and need it from the public API.
#[derive(Debug, Clone, Default)]
pub struct Capture {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Capture {
    /// An empty capture buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written so far, as a lossy UTF-8 string.
    #[must_use]
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock()).into_owned()
    }

    /// Everything written so far, verbatim.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        self.buf.lock().clone()
    }

    /// Discard everything written so far.
    pub fn clear(&self) {
        self.buf.lock().clear();
    }

    /// The captured NDJSON lines, empty lines removed.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        self.contents()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(ToString::to_string)
            .collect()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.lock().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sunrise-log-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn capture_accumulates_and_splits_lines() {
        let cap = Capture::new();
        let mut w = cap.make_writer();
        w.write_all(b"one\n").unwrap();
        w.write_all(b"two\n").unwrap();
        assert_eq!(cap.lines(), vec!["one", "two"]);
        cap.clear();
        assert!(cap.lines().is_empty());
    }

    #[test]
    fn rolling_file_creates_missing_parent_dirs() {
        let path = tmpdir("mkdir").join("nested").join("a.ndjson");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut f = RollingFile::open(&path).unwrap();
        f.write_all(b"hello\n").unwrap();
        f.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
    }

    #[test]
    fn rolling_file_rolls_at_cap_and_keeps_one_generation() {
        let path = tmpdir("roll").join("b.ndjson");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(rolled_path(&path));

        let mut f = RollingFile::with_max_bytes(&path, 8).unwrap();
        f.write_all(b"aaaaaaaaaa\n").unwrap(); // 11 bytes: over cap after write
        f.write_all(b"bbbb\n").unwrap(); // triggers the roll
        f.flush().unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bbbb\n");
        assert_eq!(
            std::fs::read_to_string(rolled_path(&path)).unwrap(),
            "aaaaaaaaaa\n"
        );
    }

    #[test]
    fn rolling_file_bounds_disk_use_across_many_rolls() {
        let path = tmpdir("bound").join("c.ndjson");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(rolled_path(&path));

        let mut f = RollingFile::with_max_bytes(&path, 16).unwrap();
        for _ in 0..200 {
            f.write_all(b"0123456789\n").unwrap();
        }
        f.flush().unwrap();

        let live = std::fs::metadata(&path).unwrap().len();
        let old = std::fs::metadata(rolled_path(&path)).unwrap().len();
        // 200 × 11 = 2200 bytes offered; at most two generations survive.
        assert!(
            live + old <= 2 * 16 + 2 * 11,
            "unbounded growth: {live} + {old}"
        );
    }

    #[test]
    fn rolling_file_appends_to_an_existing_log() {
        let path = tmpdir("append").join("d.ndjson");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"prior\n").unwrap();
        let mut f = RollingFile::open(&path).unwrap();
        f.write_all(b"next\n").unwrap();
        f.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "prior\nnext\n");
    }
}
