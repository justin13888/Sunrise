//! FIFO outbox of locally-emitted op envelopes awaiting send.
//!
//! Per `docs/05-sync/offline-queue.md`. Persistence (across process restarts)
//! is the storage layer's job; this in-memory wrapper is what the running
//! state machine drains.

use std::collections::VecDeque;

/// Outbound batch of envelope bytes.
///
/// Each entry is the raw envelope (magic prefix + canonical CBOR) ready to
/// embed in a wire OpBatch.
#[derive(Debug, Default)]
pub struct Outbox {
    queue: VecDeque<Vec<u8>>,
}

impl Outbox {
    /// Empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append.
    pub fn push(&mut self, envelope: Vec<u8>) {
        self.queue.push_back(envelope);
    }

    /// Drain up to `n` envelopes.
    pub fn drain(&mut self, n: usize) -> Vec<Vec<u8>> {
        let take = n.min(self.queue.len());
        self.queue.drain(..take).collect()
    }

    /// Number of queued items.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Peek at the head.
    #[must_use]
    pub fn front(&self) -> Option<&Vec<u8>> {
        self.queue.front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order() {
        let mut o = Outbox::new();
        o.push(vec![1]);
        o.push(vec![2]);
        o.push(vec![3]);
        let batch = o.drain(2);
        assert_eq!(batch, vec![vec![1], vec![2]]);
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn drain_caps_at_size() {
        let mut o = Outbox::new();
        o.push(vec![1]);
        let batch = o.drain(10);
        assert_eq!(batch.len(), 1);
        assert!(o.is_empty());
    }
}
