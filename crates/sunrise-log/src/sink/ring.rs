//! In-memory ring buffer sink.
//!
//! Per `spec/10-cross-cutting/logging.md` §8, the `ring` sink is a 4 MiB
//! circular buffer that always accepts `trace+`. It backs the
//! diagnostic-bundle export.

use super::Sink;
use crate::level::Level;
use parking_lot::Mutex;
use std::collections::VecDeque;

/// Bytes-budgeted ring buffer of NDJSON records.
#[derive(Debug)]
pub struct RingSink {
    inner: Mutex<RingState>,
}

#[derive(Debug)]
struct RingState {
    capacity_bytes: usize,
    used_bytes: usize,
    records: VecDeque<Vec<u8>>,
}

impl RingSink {
    /// Construct with a byte budget; default per spec is 4 MiB.
    #[must_use]
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(RingState {
                capacity_bytes,
                used_bytes: 0,
                records: VecDeque::new(),
            }),
        }
    }

    /// Default 4 MiB ring per logging.md §10 (`SUNRISE_LOG_RING_BYTES`).
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(4 * 1024 * 1024)
    }

    /// Snapshot all currently-held records, in insertion order.
    ///
    /// Used by the diagnostic-bundle exporter (logging.md §8).
    #[must_use]
    pub fn snapshot(&self) -> Vec<Vec<u8>> {
        let s = self.inner.lock();
        s.records.iter().cloned().collect()
    }

    /// Number of records currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().records.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().records.is_empty()
    }
}

impl Sink for RingSink {
    fn min_level(&self) -> Level {
        Level::Trace
    }

    fn write(&self, record_ndjson: &[u8]) {
        let mut state = self.inner.lock();
        // Drop oldest until the new record fits. Newline byte counted.
        let needed = record_ndjson.len() + 1;
        if needed > state.capacity_bytes {
            // Single record exceeds capacity — drop it; can't accommodate.
            return;
        }
        while state.used_bytes + needed > state.capacity_bytes {
            if let Some(oldest) = state.records.pop_front() {
                state.used_bytes = state.used_bytes.saturating_sub(oldest.len() + 1);
            } else {
                break;
            }
        }
        let mut buf = Vec::with_capacity(needed);
        buf.extend_from_slice(record_ndjson);
        buf.push(b'\n');
        state.used_bytes += buf.len();
        state.records.push_back(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_evicts_oldest() {
        let r = RingSink::new(20); // 20 bytes total, including '\n's
        r.write(b"a"); // 2 bytes incl '\n' → used=2
        r.write(b"b"); // 2 bytes incl '\n' → used=4
        r.write(b"c"); // 2 bytes incl '\n' → used=6
        assert_eq!(r.len(), 3);
        // Push a record that requires evicting "a" and "b" but "c" remains.
        r.write(&[b'x'; 15]); // needs 16; 6 + 16 > 20, evict "a" → 4+16=20 ok? 4+16=20 ≤ 20, exit eviction loop.
        let snap = r.snapshot();
        // Eviction stops when there's room: "a" gone, "b" + "c" + xxxx remain.
        assert!(!snap.iter().any(|r| r == b"a\n"));
        assert!(snap.iter().any(|r| r.starts_with(b"x")));
    }

    #[test]
    fn oversized_record_dropped() {
        let r = RingSink::new(8);
        r.write(&[b'x'; 16]);
        assert!(r.is_empty());
    }

    #[test]
    fn snapshot_preserves_order() {
        let r = RingSink::with_default_capacity();
        r.write(b"first");
        r.write(b"second");
        r.write(b"third");
        let snap = r.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(&snap[0], b"first\n");
        assert_eq!(&snap[1], b"second\n");
        assert_eq!(&snap[2], b"third\n");
    }
}
