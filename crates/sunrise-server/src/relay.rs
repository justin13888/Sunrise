//! In-process relay hub for `OpBatch` fan-out.
//!
//! Each subscribed device joins a tokio `broadcast::Sender` keyed by
//! `(account_id, stream_id)`. When a peer device pushes an `OpBatch` for a
//! Stream, the hub republishes the raw frame bytes on the channel; every
//! other subscribed device receives it. The original sender is filtered
//! out by their connection id so they don't receive their own ops back.
//!
//! v1 self-host scope: in-process only. Multi-node servers swap this for
//! a Postgres NOTIFY / Redis pub-sub backend (Phase 17) without touching
//! the WS handler.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Capacity per relay channel. Senders never block — they overflow into
/// `RecvError::Lagged(n)` for slow receivers, which is logged and counted.
const CHANNEL_CAPACITY: usize = 256;

/// Monotonic connection id; unique per WS session.
pub type ConnId = u64;

/// One broadcast frame routed through the hub.
#[derive(Debug, Clone)]
pub struct RelayFrame {
    /// Sender connection id (so the receiver can filter self-emits).
    pub from: ConnId,
    /// Raw wire-frame bytes (full 11-byte header + payload). Forwarded
    /// verbatim to subscribers.
    pub bytes: Vec<u8>,
}

/// `(account_id_hash, stream_id)` channel key.
pub type StreamKey = ([u8; 16], [u8; 16]);

/// Relay hub. Cheap to clone (`Arc` inside).
#[derive(Debug, Clone, Default)]
pub struct RelayHub {
    inner: Arc<Mutex<RelayInner>>,
}

#[derive(Debug, Default)]
struct RelayInner {
    channels: HashMap<StreamKey, broadcast::Sender<RelayFrame>>,
    next_conn: u64,
}

impl RelayHub {
    /// Construct an empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh connection id.
    pub fn next_conn(&self) -> ConnId {
        let mut inner = self.inner.lock();
        let id = inner.next_conn;
        inner.next_conn = inner.next_conn.wrapping_add(1);
        id
    }

    /// Subscribe to a `(account, stream)` channel, creating it if needed.
    pub fn subscribe(&self, key: StreamKey) -> broadcast::Receiver<RelayFrame> {
        let mut inner = self.inner.lock();
        let tx = inner
            .channels
            .entry(key)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0);
        tx.subscribe()
    }

    /// Publish a frame; returns the number of receivers the broadcast
    /// reached (best-effort — slow ones see `Lagged`).
    pub fn publish(&self, key: StreamKey, frame: RelayFrame) -> usize {
        let inner = self.inner.lock();
        if let Some(tx) = inner.channels.get(&key) {
            tx.send(frame).unwrap_or(0)
        } else {
            0
        }
    }

    /// Number of active channels (used by /metrics, tests).
    #[must_use]
    pub fn active_channels(&self) -> usize {
        self.inner.lock().channels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> StreamKey {
        ([1u8; 16], [2u8; 16])
    }

    #[tokio::test]
    async fn publish_to_subscribers() {
        let hub = RelayHub::new();
        let mut rx_a = hub.subscribe(key());
        let mut rx_b = hub.subscribe(key());
        let n = hub.publish(
            key(),
            RelayFrame {
                from: 0,
                bytes: vec![1, 2, 3],
            },
        );
        assert_eq!(n, 2);
        let a = rx_a.recv().await.unwrap();
        let b = rx_b.recv().await.unwrap();
        assert_eq!(a.bytes, vec![1, 2, 3]);
        assert_eq!(b.bytes, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_zero() {
        let hub = RelayHub::new();
        let n = hub.publish(
            key(),
            RelayFrame {
                from: 0,
                bytes: vec![],
            },
        );
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn fresh_conn_ids_are_monotonic() {
        let hub = RelayHub::new();
        let a = hub.next_conn();
        let b = hub.next_conn();
        let c = hub.next_conn();
        assert!(b > a && c > b);
    }
}
