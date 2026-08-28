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
//! dropped.
//!
//! # Cursors
//!
//! Replay is filtered by the per-device cursors the subscriber sends. Each
//! retained frame carries the highest `seq` it holds per originating device
//! ([`FrameHead`]), read at publish time from the envelopes' *cleartext*
//! routing header — `stream_id` / `device_id` / `seq`, never their contents
//! (see `sunrise_cbor::envelope_header`). A frame every one of whose heads is
//! at or below the subscriber's cursor for that device is skipped: the
//! subscriber already applied all of it.
//!
//! Filtering is deliberately one-sided. A frame is skipped only when it is
//! *provably* redundant; anything the hub cannot read a head from is replayed.
//! Over-delivery costs a client one idempotent no-op, whereas under-delivery
//! is data loss.
//!
//! # Bounds and the eviction watermark
//!
//! The ring is capped two ways, both to protect memory; whichever binds
//! first evicts oldest-first ([`RingCaps`]):
//! - `max_frames` (default 4096 frames)
//! - `max_bytes`  (default 16 MiB of raw frame bytes)
//!
//! Eviction is where a cursor stops being enough. Every evicted frame raises
//! the channel's `evicted_through` watermark for the devices it carried, and a
//! subscriber whose cursor for such a device sits *below* that watermark is
//! missing ops the hub can no longer produce. That is reported as a typed
//! [`CursorGap`], because the alternative — replaying the ring and saying
//! nothing — is silent data loss: past the ring bounds a returning device drops
//! ops with no error and no way to detect it (issue #19). Offline catch-up
//! worked before this only by accident of the ring being larger than the
//! backlog.
//!
//! # Restart semantics
//!
//! The ring is in-memory only (v1 self-host). Server restart loses all
//! retained history *and* its eviction watermarks, so a fresh hub reports no
//! gaps: it cannot distinguish "never had it" from "evicted it". Clients
//! recover via their own outbox + cursors. Multi-node servers swap this for a
//! Postgres NOTIFY / Redis backend (Phase 17) without touching the WS handler.

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

/// The highest `seq` one retained frame carries for one originating device.
///
/// This is the entire basis of cursor filtering, and it is all the hub reads
/// out of an op: `(device_id, seq)` are cleartext routing fields, the payload
/// is not touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHead {
    /// 16-byte id of the device that signed the ops.
    pub device_id: [u8; 16],
    /// Highest `seq` from that device in this frame.
    pub max_seq: u64,
}

/// One broadcast frame routed through the hub.
#[derive(Debug, Clone)]
pub struct RelayFrame {
    /// Sender connection id (so the live receiver can filter self-emits).
    pub from: ConnId,
    /// Raw wire-frame bytes (full 11-byte header + payload). Forwarded
    /// verbatim to subscribers.
    pub bytes: Vec<u8>,
    /// Per-device high-water marks this frame carries.
    ///
    /// Empty means "unknown" — a control frame, or ops whose routing header
    /// the hub could not read. Such a frame is never filtered out and never
    /// raises an eviction watermark, so an unreadable header can only ever
    /// cause a redundant replay, never a missed op.
    pub heads: Vec<FrameHead>,
}

impl RelayFrame {
    /// A frame with no known per-device heads: always replayed, never
    /// contributes to the eviction watermark.
    #[must_use]
    pub const fn opaque(from: ConnId, bytes: Vec<u8>) -> Self {
        Self {
            from,
            bytes,
            heads: Vec::new(),
        }
    }

    /// Whether every device head in this frame is already covered by
    /// `cursors`, i.e. the subscriber has provably applied all of it.
    fn covered_by(&self, cursors: &HashMap<[u8; 16], u64>) -> bool {
        !self.heads.is_empty()
            && self
                .heads
                .iter()
                .all(|h| cursors.get(&h.device_id).copied().unwrap_or(0) >= h.max_seq)
    }
}

/// One device for which a subscriber's cursor predates what the ring still
/// holds. The missing ops are gone from this hub; the subscriber has to
/// resync from a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorGap {
    /// The originating device whose ops were evicted.
    pub device_id: [u8; 16],
    /// The subscriber's last applied `seq` for that device (0 if it sent none).
    pub cursor: u64,
    /// Highest `seq` from that device the ring has already dropped. Everything
    /// in `cursor+1 ..= evicted_through` is unrecoverable here.
    pub evicted_through: u64,
}

/// `(account_id_hash, stream_id)` channel key.
pub type StreamKey = ([u8; 16], [u8; 16]);

/// Outcome of [`RelayHub::subscribe`]: the retained backlog to replay first,
/// then a live receiver for subsequent frames.
#[derive(Debug)]
pub struct Subscription {
    /// Snapshot of retained frames to replay, oldest first, with everything
    /// the subscriber's cursors already cover removed.
    pub retained: Vec<RelayFrame>,
    /// Devices whose ops were evicted before the subscriber caught up. Empty
    /// in the healthy case. A non-empty list means `retained` is knowingly
    /// incomplete — the caller must tell the client rather than let it believe
    /// it is caught up.
    pub gaps: Vec<CursorGap>,
    /// Live receiver for frames published after this subscription.
    pub rx: broadcast::Receiver<RelayFrame>,
}

/// Per-channel state: the live sender, the retained-frame ring, and the
/// per-device watermark of what the ring has already dropped.
#[derive(Debug)]
struct Channel {
    tx: broadcast::Sender<RelayFrame>,
    retained: VecDeque<RelayFrame>,
    retained_bytes: usize,
    /// Highest `seq` per device that has fallen out of the ring. Monotonic:
    /// eviction only ever moves it up.
    evicted_through: HashMap<[u8; 16], u64>,
}

impl Channel {
    fn new() -> Self {
        Self {
            tx: broadcast::channel(CHANNEL_CAPACITY).0,
            retained: VecDeque::new(),
            retained_bytes: 0,
            evicted_through: HashMap::new(),
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
                // Record what just became unrecoverable, before the frame is
                // dropped. Doing it here rather than at subscribe time is what
                // makes the watermark independent of who is connected.
                for head in &evicted.heads {
                    let slot = self.evicted_through.entry(head.device_id).or_insert(0);
                    *slot = (*slot).max(head.max_seq);
                }
            } else {
                break;
            }
        }
    }

    /// Devices this subscriber is behind on that the ring can no longer serve.
    fn gaps_for(&self, cursors: &HashMap<[u8; 16], u64>) -> Vec<CursorGap> {
        let mut gaps: Vec<CursorGap> = self
            .evicted_through
            .iter()
            .filter_map(|(device_id, &evicted_through)| {
                let cursor = cursors.get(device_id).copied().unwrap_or(0);
                (cursor < evicted_through).then_some(CursorGap {
                    device_id: *device_id,
                    cursor,
                    evicted_through,
                })
            })
            .collect();
        // Deterministic order: the caller turns this into a wire message.
        gaps.sort_unstable_by(|a, b| a.device_id.cmp(&b.device_id));
        gaps
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
    /// `cursors` maps originating `device_id` to the highest `seq` the
    /// subscriber has already applied; an absent device means "nothing yet".
    /// The returned backlog omits everything those cursors provably cover, and
    /// `gaps` names any device whose ops were evicted before the subscriber
    /// reached them.
    ///
    /// Snapshot + receiver creation happen under one lock so no frame is
    /// missed or duplicated across the replay/live boundary.
    pub fn subscribe(&self, key: StreamKey, cursors: &HashMap<[u8; 16], u64>) -> Subscription {
        let mut inner = self.inner.lock();
        let ch = inner.channels.entry(key).or_insert_with(Channel::new);
        let retained: Vec<RelayFrame> = ch
            .retained
            .iter()
            .filter(|f| !f.covered_by(cursors))
            .cloned()
            .collect();
        let gaps = ch.gaps_for(cursors);
        let rx = ch.tx.subscribe();
        Subscription { retained, gaps, rx }
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

    const DEV_A: [u8; 16] = [0xAA; 16];
    const DEV_B: [u8; 16] = [0xBB; 16];

    /// A frame carrying one op from `device` at `seq`.
    fn frame(from: ConnId, device: [u8; 16], seq: u64, byte: u8) -> RelayFrame {
        RelayFrame {
            from,
            bytes: vec![byte],
            heads: vec![FrameHead {
                device_id: device,
                max_seq: seq,
            }],
        }
    }

    fn none() -> HashMap<[u8; 16], u64> {
        HashMap::new()
    }

    fn cursors(entries: &[([u8; 16], u64)]) -> HashMap<[u8; 16], u64> {
        entries.iter().copied().collect()
    }

    fn replayed(sub: &Subscription) -> Vec<u8> {
        sub.retained.iter().map(|f| f.bytes[0]).collect()
    }

    #[tokio::test]
    async fn publish_to_subscribers() {
        let hub = RelayHub::new();
        let mut sub_a = hub.subscribe(key(), &none());
        let mut sub_b = hub.subscribe(key(), &none());
        let n = hub.publish(key(), RelayFrame::opaque(0, vec![1, 2, 3]));
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
        let n = hub.publish(key(), RelayFrame::opaque(0, vec![9, 9]));
        assert_eq!(n, 0);
        assert_eq!(hub.retained_len(key()), 1);
    }

    #[tokio::test]
    async fn late_subscriber_with_no_cursors_replays_everything() {
        let hub = RelayHub::new();
        for i in 0..3u8 {
            hub.publish(key(), frame(42, DEV_A, u64::from(i) + 1, i));
        }
        let sub = hub.subscribe(key(), &none());
        assert_eq!(replayed(&sub), vec![0, 1, 2]);
        assert!(sub.gaps.is_empty());
    }

    #[tokio::test]
    async fn subscribe_then_publish_is_live_not_replayed() {
        let hub = RelayHub::new();
        let mut sub = hub.subscribe(key(), &none());
        assert!(sub.retained.is_empty());
        hub.publish(key(), RelayFrame::opaque(1, vec![7]));
        let live = sub.rx.recv().await.unwrap();
        assert_eq!(live.bytes, vec![7]);
    }

    // ---- cursor filtering ----

    /// The point of issue #19: a returning device says how far it got, and the
    /// hub replays only what comes after that.
    #[tokio::test]
    async fn a_cursor_skips_frames_it_already_covers() {
        let hub = RelayHub::new();
        for i in 1..=5u8 {
            hub.publish(key(), frame(42, DEV_A, u64::from(i), i));
        }
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, 3)]));
        assert_eq!(replayed(&sub), vec![4, 5]);
        assert!(sub.gaps.is_empty());
    }

    /// A cursor is per device. Being caught up on A says nothing about B.
    #[tokio::test]
    async fn a_cursor_for_one_device_does_not_skip_another() {
        let hub = RelayHub::new();
        hub.publish(key(), frame(1, DEV_A, 1, 10));
        hub.publish(key(), frame(2, DEV_B, 1, 20));
        hub.publish(key(), frame(1, DEV_A, 2, 11));
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, 9)]));
        assert_eq!(replayed(&sub), vec![20]);
    }

    /// A batch spanning two devices is redundant only when BOTH cursors cover
    /// it. Skipping on a partial match would drop the other device's op.
    #[tokio::test]
    async fn a_mixed_frame_needs_every_head_covered() {
        let hub = RelayHub::new();
        hub.publish(
            key(),
            RelayFrame {
                from: 1,
                bytes: vec![7],
                heads: vec![
                    FrameHead {
                        device_id: DEV_A,
                        max_seq: 3,
                    },
                    FrameHead {
                        device_id: DEV_B,
                        max_seq: 4,
                    },
                ],
            },
        );
        assert_eq!(
            replayed(&hub.subscribe(key(), &cursors(&[(DEV_A, 3)]))),
            vec![7]
        );
        assert!(hub
            .subscribe(key(), &cursors(&[(DEV_A, 3), (DEV_B, 4)]))
            .retained
            .is_empty());
    }

    /// An unreadable header must fail towards over-delivery. A skipped frame
    /// is data loss; a replayed one is an idempotent no-op on the client.
    #[tokio::test]
    async fn a_frame_with_no_heads_is_always_replayed() {
        let hub = RelayHub::new();
        hub.publish(key(), RelayFrame::opaque(1, vec![7]));
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, u64::MAX)]));
        assert_eq!(replayed(&sub), vec![7]);
    }

    // ---- eviction ----

    #[tokio::test]
    async fn ring_evicts_oldest_beyond_frame_cap() {
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: 3,
            max_bytes: usize::MAX,
        });
        for i in 0..5u8 {
            hub.publish(key(), frame(0, DEV_A, u64::from(i) + 1, i));
        }
        assert_eq!(hub.retained_len(key()), 3);
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, 2)]));
        // Oldest (seq 1, 2) evicted; newest (3, 4, 5) retained.
        assert_eq!(replayed(&sub), vec![2, 3, 4]);
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
                    heads: vec![FrameHead {
                        device_id: DEV_A,
                        max_seq: u64::from(i) + 1,
                    }],
                },
            );
        }
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, 3)]));
        assert_eq!(replayed(&sub), vec![3, 4]);
    }

    /// The defect issue #19 names: past the ring bounds a returning device
    /// used to lose ops silently. Now it is told.
    #[tokio::test]
    async fn a_cursor_below_the_eviction_watermark_is_a_typed_gap() {
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: 2,
            max_bytes: usize::MAX,
        });
        for i in 1..=5u8 {
            hub.publish(key(), frame(0, DEV_A, u64::from(i), i));
        }
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, 1)]));
        assert_eq!(
            sub.gaps,
            vec![CursorGap {
                device_id: DEV_A,
                cursor: 1,
                evicted_through: 3,
            }],
            "seqs 2 and 3 were evicted and cannot be served"
        );
        // What survives is still delivered — an incomplete replay beats none.
        assert_eq!(replayed(&sub), vec![4, 5]);
    }

    /// A subscriber that never saw this device at all is behind by definition,
    /// so a missing cursor reads as 0 rather than as "no opinion".
    #[tokio::test]
    async fn a_missing_cursor_counts_as_zero_for_gap_detection() {
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: 1,
            max_bytes: usize::MAX,
        });
        hub.publish(key(), frame(0, DEV_A, 1, 1));
        hub.publish(key(), frame(0, DEV_A, 2, 2));
        let sub = hub.subscribe(key(), &none());
        assert_eq!(sub.gaps.len(), 1);
        assert_eq!(sub.gaps[0].cursor, 0);
        assert_eq!(sub.gaps[0].evicted_through, 1);
    }

    /// Eviction of ops a subscriber already applied is not a gap. Otherwise a
    /// healthy long-lived client would be told to resync every time the ring
    /// turned over.
    #[tokio::test]
    async fn a_caught_up_cursor_sees_no_gap_however_much_was_evicted() {
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: 1,
            max_bytes: usize::MAX,
        });
        for i in 1..=5u8 {
            hub.publish(key(), frame(0, DEV_A, u64::from(i), i));
        }
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, 4)]));
        assert!(sub.gaps.is_empty(), "cursor covers everything evicted");
        assert_eq!(replayed(&sub), vec![5]);
    }

    /// Being behind on one device does not manufacture a gap for another.
    #[tokio::test]
    async fn gaps_are_reported_per_device() {
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: 1,
            max_bytes: usize::MAX,
        });
        hub.publish(key(), frame(0, DEV_A, 1, 1));
        hub.publish(key(), frame(0, DEV_B, 1, 2));
        hub.publish(key(), frame(0, DEV_B, 2, 3));
        let sub = hub.subscribe(key(), &cursors(&[(DEV_A, 1)]));
        assert_eq!(sub.gaps.len(), 1);
        assert_eq!(sub.gaps[0].device_id, DEV_B);
    }

    /// Opaque frames raise no watermark: the hub cannot claim a device lost
    /// ops it could not attribute to that device in the first place.
    #[tokio::test]
    async fn evicting_an_opaque_frame_raises_no_watermark() {
        let hub = RelayHub::with_caps(RingCaps {
            max_frames: 1,
            max_bytes: usize::MAX,
        });
        hub.publish(key(), RelayFrame::opaque(0, vec![1]));
        hub.publish(key(), RelayFrame::opaque(0, vec![2]));
        assert!(hub.subscribe(key(), &none()).gaps.is_empty());
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
