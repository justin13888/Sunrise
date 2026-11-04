//! [`FaultHandle`], the runtime switches and counters a [`Toxic`](super::Toxic)
//! wrapper shares with the test that steers it.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::ToxicConfig;

/// Shared, cheaply-clonable runtime switches for a [`Toxic`](super::Toxic) wrapper.
///
/// A clone of the handle is held by the wrapper and (typically) by the test, so
/// the test can flip a [`partition`](Self::partition) or retune probabilities
/// while traffic is in flight. All operations are lock-free.
#[derive(Clone)]
pub struct FaultHandle {
    pub(super) inner: Arc<FaultState>,
}

pub(super) struct FaultState {
    partitioned: AtomicBool,
    /// `f64` drop probability stored as raw bits.
    drop_bits: AtomicU64,
    /// `f64` corrupt probability stored as raw bits.
    corrupt_bits: AtomicU64,
    /// Frames the drop fault swallowed, both directions.
    pub(super) dropped: AtomicU64,
    /// Frames the corrupt fault altered, both directions.
    pub(super) corrupted: AtomicU64,
    /// Sends the partition refused.
    pub(super) refused_sends: AtomicU64,
    /// `Subscribe` frames forwarded unaltered to the inner transport.
    pub(super) subscribes_sent: AtomicU64,
}

/// How often [`FaultHandle::wait_until`] re-reads the counters.
const WAIT_POLL: Duration = Duration::from_millis(10);

impl FaultHandle {
    pub(super) fn new(drop_prob: f64, corrupt_prob: f64) -> Self {
        Self {
            inner: Arc::new(FaultState {
                partitioned: AtomicBool::new(false),
                drop_bits: AtomicU64::new(clamp_prob(drop_prob).to_bits()),
                corrupt_bits: AtomicU64::new(clamp_prob(corrupt_prob).to_bits()),
                dropped: AtomicU64::new(0),
                corrupted: AtomicU64::new(0),
                refused_sends: AtomicU64::new(0),
                subscribes_sent: AtomicU64::new(0),
            }),
        }
    }

    /// Frames the drop fault has swallowed so far, sends and receives alike,
    /// across every wrapper sharing this handle.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Frames the corrupt fault has altered so far, sends and receives alike,
    /// across every wrapper sharing this handle.
    #[must_use]
    pub fn corrupted(&self) -> u64 {
        self.inner.corrupted.load(Ordering::Relaxed)
    }

    /// Sends the partition has refused so far. A session's first act is a
    /// send, so while partitioned each reconnect attempt refuses one.
    #[must_use]
    pub fn refused_sends(&self) -> u64 {
        self.inner.refused_sends.load(Ordering::Relaxed)
    }

    /// `Subscribe` frames forwarded unaltered to the inner transport so far.
    ///
    /// A session sends one when it opens, one when it learns a new Stream,
    /// and one per anti-entropy resync. The resync sends it after that
    /// round's blob drain has run, so on an idle live session `n` more of
    /// these is `n` complete resync rounds.
    #[must_use]
    pub fn subscribes_sent(&self) -> u64 {
        self.inner.subscribes_sent.load(Ordering::Relaxed)
    }

    /// Poll until `done` holds of this handle, panicking with `what` if
    /// `timeout` passes first.
    ///
    /// # Panics
    ///
    /// When `done` is still false after `timeout`.
    pub async fn wait_until(&self, what: &str, timeout: Duration, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + timeout;
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out after {timeout:?} waiting for {what}: {self:?}"
            );
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Cut (or restore) the link. While partitioned, every `send_frame` and
    /// `recv_frame` on the associated [`Toxic`](super::Toxic) returns a
    /// transport error.
    pub fn partition(&self, on: bool) {
        self.inner.partitioned.store(on, Ordering::Relaxed);
    }

    /// Whether the link is currently partitioned.
    #[must_use]
    pub fn is_partitioned(&self) -> bool {
        self.inner.partitioned.load(Ordering::Relaxed)
    }

    /// Set the per-frame drop probability (clamped to `[0.0, 1.0]`).
    pub fn set_drop_prob(&self, p: f64) {
        self.inner
            .drop_bits
            .store(clamp_prob(p).to_bits(), Ordering::Relaxed);
    }

    /// Current per-frame drop probability.
    #[must_use]
    pub fn drop_prob(&self) -> f64 {
        f64::from_bits(self.inner.drop_bits.load(Ordering::Relaxed))
    }

    /// Set the per-frame corruption probability (clamped to `[0.0, 1.0]`).
    pub fn set_corrupt_prob(&self, p: f64) {
        self.inner
            .corrupt_bits
            .store(clamp_prob(p).to_bits(), Ordering::Relaxed);
    }

    /// Current per-frame corruption probability.
    #[must_use]
    pub fn corrupt_prob(&self) -> f64 {
        f64::from_bits(self.inner.corrupt_bits.load(Ordering::Relaxed))
    }

    /// Build a handle initialised from `config`'s drop/corruption probabilities,
    /// with the link un-partitioned. Share it across every
    /// [`Toxic`](super::Toxic) a transport factory produces (via
    /// [`Toxic::with_handle`](super::Toxic::with_handle)) so one handle steers
    /// the live connection *and* every reconnect.
    #[must_use]
    pub fn from_config(config: ToxicConfig) -> Self {
        Self::new(config.drop_prob, config.corrupt_prob)
    }
}

impl fmt::Debug for FaultHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultHandle")
            .field("partitioned", &self.is_partitioned())
            .field("drop_prob", &self.drop_prob())
            .field("corrupt_prob", &self.corrupt_prob())
            .field("dropped", &self.dropped())
            .field("corrupted", &self.corrupted())
            .field("refused_sends", &self.refused_sends())
            .field("subscribes_sent", &self.subscribes_sent())
            .finish()
    }
}

fn clamp_prob(p: f64) -> f64 {
    p.clamp(0.0, 1.0)
}
