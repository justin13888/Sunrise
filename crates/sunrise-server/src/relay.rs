//! In-process relay hub for `OpBatch` fan-out with a retained-frame ring.
//!
//! Each `(account_id_hash, stream_id)` channel owns a tokio
//! `broadcast::Sender` for *live* fan-out plus a bounded [`VecDeque`] of
//! recently-published raw frames (the "replay ring"). When a peer device
//! pushes an `OpBatch` for a stream, the hub appends the raw frame to the
//! ring and republishes it live; every other subscribed device receives it.
//! The original sender is filtered out of the *live* path by connection id
//! (see [`crate::ws`]) so it doesn't receive its own ops back.
//!
//! On [`RelayHub::subscribe`], the hub atomically snapshots the ring **and**
//! creates the broadcast receiver under a single lock, so a concurrent
//! `publish` is either fully in the snapshot (delivered as replay) or fully
//! in the live stream (delivered via the receiver) — never both, never
//! dropped. Replay includes *all* retained frames, including any the
//! subscriber's own earlier connection produced: a reconnecting client gets
//! a fresh [`ConnId`], so self-emit filtering does not apply to replay. The
//! client contract is idempotent apply per `(stream, device, seq)`, so it
//! must dedupe frames it authored.
//!
//! # Bounds
//!
//! The ring is capped two ways, both to protect memory; whichever binds
//! first evicts oldest-first ([`RingCaps`]):
//! - `max_frames` (default 4096 frames)
//! - `max_bytes`  (default 16 MiB of raw frame bytes)
//!
//! # Restart semantics
//!
//! The ring is in-memory only (v1 self-host). Server restart loses all
//! retained history; clients recover via their own outbox + cursors. Multi-
//! node servers swap this for a Postgres NOTIFY / Redis backend (Phase 17)
//! without touching the WS handler.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Capacity per relay channel's live broadcast buffer. Senders never block —
/// they overflow into `RecvError::Lagged(n)` for slow receivers, which is
/// logged and counted.
const CHANNEL_CAPACITY: usize = 256;

/// Default cap on the number of retained frames per channel.
pub const DEFAULT_MAX_RETAINED_FRAMES: usize = 4096;

/// Default cap on the total retained bytes per channel (16 MiB).
pub const DEFAULT_MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;

/// Monotonic connection id; unique per WS session.
pub type ConnId = u64;

/// Bounds for a channel's retained-frame ring. Whichever cap binds first
/// evicts the oldest frames.
#[derive(Debug, Clone, Copy)]
pub struct RingCaps {
    /// Maximum retained frames before oldest are evicted.
    pub max_frames: usize,
    /// Maximum total retained raw bytes before oldest are evicted.
    pub max_bytes: usize,
}

impl Default for RingCaps {
    fn default() -> Self {
        Self {
            max_frames: DEFAULT_MAX_RETAINED_FRAMES,
            max_bytes: DEFAULT_MAX_RETAINED_BYTES,
        }
    }
}

/// One broadcast frame routed through the hub.
#[derive(Debug, Clone)]
pub struct RelayFrame {
    /// Sender connection id (so the live receiver can filter self-emits).
    pub from: ConnId,
    /// Raw wire-frame bytes (full 11-byte header + payload). Forwarded
    /// verbatim to subscribers.
    pub bytes: Vec<u8>,
}

/// `(account_id_hash, stream_id)` channel key.
pub type StreamKey = ([u8; 16], [u8; 16]);

/// Outcome of [`RelayHub::subscribe`]: the retained backlog to replay first,
/// then a live receiver for subsequent frames.
#[derive(Debug)]
pub struct Subscription {
    /// Snapshot of retained frames to replay, oldest first.
    pub retained: Vec<RelayFrame>,
    /// Live receiver for frames published after this subscription.
    pub rx: broadcast::Receiver<RelayFrame>,
}

/// Per-channel state: the live sender plus the retained-frame ring.
#[derive(Debug)]
struct Channel {
    tx: broadcast::Sender<RelayFrame>,
    retained: VecDeque<RelayFrame>,
    retained_bytes: usize,
}

impl Channel {
    fn new() -> Self {
        Self {
            tx: broadcast::channel(CHANNEL_CAPACITY).0,
            retained: VecDeque::new(),
            retained_bytes: 0,
        }
    }

    /// Append a frame, then evict oldest until within `caps`. Always keeps
    /// the just-appended frame even if it alone exceeds `max_bytes`.
    fn push_retained(&mut self, frame: &RelayFrame, caps: RingCaps) {
        self.retained_bytes += frame.bytes.len();
        self.retained.push_back(frame.clone());
        while self.retained.len() > caps.max_frames
            || (self.retained_bytes > caps.max_bytes && self.retained.len() > 1)
        {
            if let Some(evicted) = self.retained.pop_front() {
                self.retained_bytes -= evicted.bytes.len();
            } else {
                break;
            }
        }
    }
}

/// Relay hub. Cheap to clone (`Arc` inside).
#[derive(Debug, Clone)]
pub struct RelayHub {
    inner: Arc<Mutex<RelayInner>>,
    caps: RingCaps,
}

impl Default for RelayHub {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
struct RelayInner {
    channels: HashMap<StreamKey, Channel>,
    next_conn: u64,
}

impl RelayHub {
    /// Construct an empty hub with default ring caps.
    #[must_use]
    pub fn new() -> Self {
        Self::with_caps(RingCaps::default())
    }

    /// Construct an empty hub with caller-specified ring caps (tests inject
    /// small caps to exercise eviction).
    #[must_use]
    pub fn with_caps(caps: RingCaps) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RelayInner::default())),
            caps,
        }
    }

    /// Allocate a fresh connection id.
    pub fn next_conn(&self) -> ConnId {
        let mut inner = self.inner.lock();
        let id = inner.next_conn;
        inner.next_conn = inner.next_conn.wrapping_add(1);
        id
    }

    /// Subscribe to a `(account, stream)` channel, creating it if needed.
    ///
    /// Returns a snapshot of the retained backlog (to replay first) and a
    /// live receiver. Snapshot + receiver creation happen under one lock so
    /// no frame is missed or duplicated across the replay/live boundary.
    pub fn subscribe(&self, key: StreamKey) -> Subscription {
        let mut inner = self.inner.lock();
        let ch = inner.channels.entry(key).or_insert_with(Channel::new);
        let retained: Vec<RelayFrame> = ch.retained.iter().cloned().collect();
        let rx = ch.tx.subscribe();
        Subscription { retained, rx }
    }

    /// Publish a frame: append it to the channel's retained ring (creating
    /// the channel if needed, so late subscribers still see it) and
    /// broadcast it live. Returns the number of live receivers reached
    /// (best-effort — slow ones see `Lagged`).
    pub fn publish(&self, key: StreamKey, frame: RelayFrame) -> usize {
        let mut inner = self.inner.lock();
        let caps = self.caps;
        let ch = inner.channels.entry(key).or_insert_with(Channel::new);
        ch.push_retained(&frame, caps);
        ch.tx.send(frame).unwrap_or(0)
    }

    /// Number of active channels (used by /metrics, tests).
    #[must_use]
    pub fn active_channels(&self) -> usize {
        self.inner.lock().channels.len()
    }

    /// Number of retained frames for a channel (tests / metrics).
    #[must_use]
    pub fn retained_len(&self, key: StreamKey) -> usize {
        self.inner
            .lock()
            .channels
            .get(&key)
            .map_or(0, |c| c.retained.len())
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
        let mut sub_a = hub.subscribe(key());
        let mut sub_b = hub.subscribe(key());
        let n = hub.publish(
            key(),
            RelayFrame {
                from: 0,
                bytes: vec![1, 2, 3],
            },
        );
        assert_eq!(n, 2);
        let a = sub_a.rx.recv().await.unwrap();
        let b = sub_b.rx.recv().await.unwrap();
        assert_eq!(a.bytes, vec![1, 2, 3]);
        assert_eq!(b.bytes, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_retained() {
        let hub = RelayHub::new();
        // No live subscribers: send reaches zero, but the frame is retained.
        let n = hub.publish(
            key(),
            RelayFrame {
                from: 0,
                bytes: vec![9, 9],
            },
        );
        assert_eq!(n, 0);
        assert_eq!(hub.retained_len(key()), 1);
    }

    #[tokio::test]
    async fn late_subscriber_replays_retained() {
        let hub = RelayHub::new();
        for i in 0..3u8 {
            hub.publish(
                key(),
                RelayFrame {
                    from: 42,
                    bytes: vec![i],
                },
            );
        }
        let sub = hub.subscribe(key());
        let got: Vec<u8> = sub.retained.iter().map(|f| f.bytes[0]).collect();
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn subscribe_then_publish_is_live_not_replayed() {
        let hub = RelayHub::new();
        let mut sub = hub.subscribe(key());
        assert!(sub.retained.is_empty());
        hub.publish(
            key(),
            RelayFrame {
                from: 1,
                bytes: vec![7],
            },
        );
        let live = sub.rx.recv().await.unwrap();
        assert_eq!(live.bytes, vec![7]);
    }

    #[tokio::test]
    async fn ring_evicts_oldest_beyond_frame_cap() {
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: 3,
            max_bytes: usize::MAX,
        });
        for i in 0..5u8 {
            hub.publish(
                key(),
                RelayFrame {
                    from: 0,
                    bytes: vec![i],
                },
            );
        }
        assert_eq!(hub.retained_len(key()), 3);
        let sub = hub.subscribe(key());
        let got: Vec<u8> = sub.retained.iter().map(|f| f.bytes[0]).collect();
        // Oldest (0,1) evicted; newest (2,3,4) retained.
        assert_eq!(got, vec![2, 3, 4]);
    }

    #[tokio::test]
    async fn ring_evicts_oldest_beyond_byte_cap() {
        // Each frame is 4 bytes; cap at 10 bytes keeps at most 2 frames.
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: usize::MAX,
            max_bytes: 10,
        });
        for i in 0..5u8 {
            hub.publish(
                key(),
                RelayFrame {
                    from: 0,
                    bytes: vec![i; 4],
                },
            );
        }
        let sub = hub.subscribe(key());
        let got: Vec<u8> = sub.retained.iter().map(|f| f.bytes[0]).collect();
        assert_eq!(got, vec![3, 4]);
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
