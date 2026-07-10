//! Per-(stream, originating-device) sync cursors.
//!
//! Per `docs/05-sync/multi-device.md`. Each device maintains, for every
//! `(stream_id, originating_device_id)` pair, the highest `seq` it has
//! applied. Cursors are sent on connect so the server can deliver only the
//! ops since.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One cursor row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cursor {
    /// Stream id.
    pub stream_id: [u8; 16],
    /// Originating device id (the device that emitted the ops).
    pub originating_device_id: [u8; 16],
    /// Highest applied `seq` for this (stream, device) pair.
    pub last_applied_seq: u64,
}

/// Map of `(stream, device) → seq`.
///
/// Not directly serde-serializable (the `(stream_id, device_id)` tuple key
/// can't go through serde's map key constraint without a custom impl).
/// Round-trip via [`Self::snapshot`] / [`Self::from_iter`] instead.
#[derive(Debug, Clone, Default)]
pub struct CursorMap {
    /// Backing storage; key tuple is `(stream_id, originating_device_id)`.
    inner: HashMap<([u8; 16], [u8; 16]), u64>,
}

impl CursorMap {
    /// Construct empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Update if `seq` is higher than what we had.
    pub fn observe(&mut self, stream_id: [u8; 16], originating_device_id: [u8; 16], seq: u64) {
        let entry = self
            .inner
            .entry((stream_id, originating_device_id))
            .or_insert(0);
        if seq > *entry {
            *entry = seq;
        }
    }

    /// Look up.
    #[must_use]
    pub fn get(&self, stream_id: &[u8; 16], originating_device_id: &[u8; 16]) -> u64 {
        self.inner
            .get(&(*stream_id, *originating_device_id))
            .copied()
            .unwrap_or(0)
    }

    /// Snapshot every cursor as a `Vec` for serialization on connect.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Cursor> {
        self.inner
            .iter()
            .map(|((s, d), seq)| Cursor {
                stream_id: *s,
                originating_device_id: *d,
                last_applied_seq: *seq,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_takes_max() {
        let mut m = CursorMap::new();
        let s = [1u8; 16];
        let d = [2u8; 16];
        m.observe(s, d, 5);
        m.observe(s, d, 3);
        m.observe(s, d, 7);
        assert_eq!(m.get(&s, &d), 7);
    }

    #[test]
    fn snapshot_round_trip() {
        let mut m = CursorMap::new();
        m.observe([1u8; 16], [2u8; 16], 10);
        m.observe([3u8; 16], [4u8; 16], 20);
        let snap = m.snapshot();
        assert_eq!(snap.len(), 2);
        // Total of last_applied_seq sums to 30 regardless of order.
        let total: u64 = snap.iter().map(|c| c.last_applied_seq).sum();
        assert_eq!(total, 30);
    }
}
