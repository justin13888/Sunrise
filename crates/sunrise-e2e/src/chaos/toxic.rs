//! Fault-injecting [`Transport`] wrapper.
//!
//! [`Toxic`] sits between a [`Core`](sunrise_core::Core) and a real transport
//! (or the in-process [`loopback`](super::loopback)) and applies configurable
//! network faults to every frame passing through, in **both** directions:
//!
//! - **drop** — with `drop_prob`, the frame silently vanishes. On send the call
//!   still returns `Ok` (the peer just never sees it); on receive the frame is
//!   swallowed and the wrapper keeps waiting for the next one.
//! - **corrupt** — with `corrupt_prob`, exactly one byte of the frame is
//!   mutated (one bit flipped). Corrupting a byte in the 11-byte frame header
//!   typically makes the frame undecodable (`BadMagic` / `BadVersion`), while
//!   corrupting a payload byte survives framing but fails AEAD verification
//!   downstream — both are acceptable "tampered frame" outcomes.
//! - **delay** — with a configured range, the frame is held for a uniformly
//!   random [`Duration`] before it is forwarded (`tokio::time::sleep`), in
//!   both directions unless [`ToxicConfig::delay_send_only`] confines it to
//!   sends.
//! - **partition** — a runtime switch on [`FaultHandle`]; while partitioned all
//!   sends and receives return a transport error, modelling a cut link.
//!
//! The faults apply to the frame stream. The account API beside it (blobs,
//! snapshots, key bundle, identity chain, revocation) passes straight through
//! to the inner transport, except that a partition refuses it too: a cut link
//! carries nothing.
//!
//! ## Counters
//!
//! [`FaultHandle`] counts what the wrapper actually did, so a test can wait
//! for a fault to fire before it heals and prove it fired, rather than
//! sleeping and hoping: [`dropped`](FaultHandle::dropped),
//! [`corrupted`](FaultHandle::corrupted) and
//! [`refused_sends`](FaultHandle::refused_sends). It also counts the
//! `Subscribe` frames it forwarded
//! ([`subscribes_sent`](FaultHandle::subscribes_sent)): on an idle live
//! session each one is an anti-entropy resync round, which is what an absence
//! check waits on in place of a fixed sleep.
//!
//! ## Determinism
//!
//! All random decisions are drawn from a seeded `rand_chacha` `ChaCha20` RNG.
//! The seed comes from the `SUNRISE_FUZZ_SEED` environment variable or an
//! explicit constructor argument ([`Toxic::with_seed`]). No `thread_rng` is
//! used (it is banned by `clippy.toml`), so a run is exactly reproducible from
//! its seed.
//!
//! [`seed_from_env`] and [`DEFAULT_FUZZ_SEED`] are re-exports from
//! [`sunrise_test_seed`], which is the workspace's single reader of that
//! variable — the property tests draw from the same one.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_sync::transport::{
    BlobCommit, IdentityChainPinned, KeyBundlePin, KeyBundlePinned, RevokeOutcome, SnapshotPin,
    Transport, TransportError,
};
use sunrise_wire_protocol::{decode_frame, MsgKind};

// The seed convention itself lives in `sunrise-test-seed`, which is the one
// reader in the workspace: the property tests plumb the same resolved value
// into `ProptestConfig::rng_seed` (#119). Re-exported rather than moved out of
// sight, because `chaos.rs` and `docs/10-cross-cutting/testing.md` both name
// these two symbols on this path.
pub use sunrise_test_seed::{seed_from_env, DEFAULT_FUZZ_SEED};

/// Static configuration for a [`Toxic`] wrapper.
///
/// The same fault profile is applied independently to each direction (send and
/// receive). Probabilities are clamped to `[0.0, 1.0]`. The default is a
/// pass-through (no faults).
#[derive(Debug, Clone, Copy, Default)]
pub struct ToxicConfig {
    /// Probability in `[0.0, 1.0]` that a frame is silently dropped.
    pub drop_prob: f64,
    /// Probability in `[0.0, 1.0]` that a frame has one byte corrupted.
    pub corrupt_prob: f64,
    /// Inclusive `(min, max)` delay range applied to each forwarded frame.
    /// `None` forwards with no artificial delay.
    pub delay: Option<(Duration, Duration)>,
    /// Apply [`delay`](Self::delay) to sent frames only, and receive
    /// undelayed. `false`, the default, delays both directions.
    pub delay_send_only: bool,
}

impl ToxicConfig {
    /// A pass-through configuration: no drop, no corruption, no delay.
    #[must_use]
    pub const fn passthrough() -> Self {
        Self {
            drop_prob: 0.0,
            corrupt_prob: 0.0,
            delay: None,
            delay_send_only: false,
        }
    }

    /// A clean link to the relay whose round trip averages `rtt`, as the
    /// sync-latency harness injects it (#366): every *send* waits `rtt`,
    /// jittered uniformly by ±20 % from the seeded RNG, and receives are not
    /// delayed. No drop, no corruption.
    ///
    /// Charged to the send because of what a send is on the SSE transport: a
    /// `POST` whose reply is the ack, so a real sender is held for one round
    /// trip per send and has its ack the moment the send returns. And an op
    /// one device sends reaches another after half a round trip up to the
    /// relay and half a round trip down, one RTT in all, which is the same
    /// RTT spent before the relay sees it here. Delaying the receive side as
    /// well would count the reader's half twice and hold each ack a second
    /// time, and the receive hold is serial — one frame per delay — so under
    /// a steady stream the acks queue behind it until the driver's retransmit
    /// policy gives up on them.
    #[must_use]
    pub fn round_trip(rtt: Duration) -> Self {
        Self {
            delay: Some((rtt.mul_f64(0.8), rtt.mul_f64(1.2))),
            delay_send_only: true,
            ..Self::passthrough()
        }
    }
}

/// Shared, cheaply-clonable runtime switches for a [`Toxic`] wrapper.
///
/// A clone of the handle is held by the wrapper and (typically) by the test, so
/// the test can flip a [`partition`](Self::partition) or retune probabilities
/// while traffic is in flight. All operations are lock-free.
#[derive(Clone)]
pub struct FaultHandle {
    inner: Arc<FaultState>,
}

struct FaultState {
    partitioned: AtomicBool,
    /// `f64` drop probability stored as raw bits.
    drop_bits: AtomicU64,
    /// `f64` corrupt probability stored as raw bits.
    corrupt_bits: AtomicU64,
    /// Frames the drop fault swallowed, both directions.
    dropped: AtomicU64,
    /// Frames the corrupt fault altered, both directions.
    corrupted: AtomicU64,
    /// Sends the partition refused.
    refused_sends: AtomicU64,
    /// `Subscribe` frames forwarded unaltered to the inner transport.
    subscribes_sent: AtomicU64,
}

/// How often [`FaultHandle::wait_until`] re-reads the counters.
const WAIT_POLL: Duration = Duration::from_millis(10);

impl FaultHandle {
    fn new(drop_prob: f64, corrupt_prob: f64) -> Self {
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
    /// `recv_frame` on the associated [`Toxic`] returns a transport error.
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
    /// with the link un-partitioned. Share it across every [`Toxic`] a
    /// transport factory produces (via [`Toxic::with_handle`]) so one handle
    /// steers the live connection *and* every reconnect.
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

/// Fault-injecting [`Transport`] wrapper. See the [module docs](self) for the
/// fault model.
pub struct Toxic<T: Transport> {
    inner: T,
    faults: FaultHandle,
    delay: Option<(Duration, Duration)>,
    /// [`ToxicConfig::delay_send_only`].
    delay_send_only: bool,
    rng: ChaCha20Rng,
    /// An inbound frame already taken off `inner`, waiting out its delay.
    ///
    /// It lives here rather than on `recv_frame`'s stack because the driver
    /// polls `recv_frame` inside a `select!` and drops the future whenever
    /// another branch wins. A frame held across an `await` on the stack goes
    /// with it — a loss no test asked the injector for, and one that grows
    /// with the delay range. Parked in the wrapper it simply resumes.
    ///
    /// `tokio::time::Instant` rather than `std::time::Instant`: this is a
    /// transport deadline, not a reading of the wall clock the workspace lint
    /// is protecting, and it is what `sleep_until` takes.
    held: Option<(Vec<u8>, tokio::time::Instant)>,
}

impl<T: Transport> fmt::Debug for Toxic<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Toxic")
            .field("faults", &self.faults)
            .field("delay", &self.delay)
            .finish_non_exhaustive()
    }
}

impl<T: Transport> Toxic<T> {
    /// Wrap `inner`, drawing the RNG seed from `SUNRISE_FUZZ_SEED` (or
    /// [`DEFAULT_FUZZ_SEED`] when unset). Returns the wrapper plus a
    /// [`FaultHandle`] clone for runtime control.
    #[must_use]
    pub fn new(inner: T, config: ToxicConfig) -> (Self, FaultHandle) {
        let seed = seed_from_env().unwrap_or(DEFAULT_FUZZ_SEED);
        // The same line the property tests print, so one grep over a CI log
        // finds the reproduction for either half of the suite.
        sunrise_test_seed::announce("chaos", seed);
        Self::with_seed(inner, config, seed)
    }

    /// Wrap `inner` with an explicit RNG `seed` (ignores the environment).
    /// Returns the wrapper plus a [`FaultHandle`] clone.
    #[must_use]
    pub fn with_seed(inner: T, config: ToxicConfig, seed: u64) -> (Self, FaultHandle) {
        let faults = FaultHandle::new(config.drop_prob, config.corrupt_prob);
        let toxic = Self {
            inner,
            faults: faults.clone(),
            delay: config.delay,
            delay_send_only: config.delay_send_only,
            rng: ChaCha20Rng::seed_from_u64(seed),
            held: None,
        };
        (toxic, faults)
    }

    /// Wrap `inner` reusing an existing shared [`FaultHandle`] (rather than
    /// minting a fresh one as [`Toxic::with_seed`] does), with an explicit RNG
    /// `seed`. Every wrapper built from the same handle observes the same
    /// runtime drop / corrupt / partition switches — the shape a reconnecting
    /// transport factory needs, where one handle must steer every connection it
    /// opens. The delay comes from `config` ([`ToxicConfig::delay`] and
    /// [`ToxicConfig::delay_send_only`]); its probabilities are ignored, because
    /// the handle's are the ones in force.
    #[must_use]
    pub fn with_handle(inner: T, faults: FaultHandle, config: ToxicConfig, seed: u64) -> Self {
        Self {
            inner,
            faults,
            delay: config.delay,
            delay_send_only: config.delay_send_only,
            rng: ChaCha20Rng::seed_from_u64(seed),
            held: None,
        }
    }

    /// A fresh clone of this wrapper's [`FaultHandle`].
    #[must_use]
    pub fn faults(&self) -> FaultHandle {
        self.faults.clone()
    }

    /// Consume the wrapper, returning the inner transport.
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// With probability `corrupt_prob`, flip a single bit of a single
    /// (RNG-chosen) byte. Length is preserved and, when it fires, exactly one
    /// byte differs. Empty frames are left untouched. Returns whether it
    /// fired, having counted it on the handle.
    fn maybe_corrupt(&mut self, frame: &mut [u8]) -> bool {
        if frame.is_empty() {
            return false;
        }
        if !self.rng.gen_bool(self.faults.corrupt_prob()) {
            return false;
        }
        let idx = self.rng.gen_range(0..frame.len());
        let bit = self.rng.gen_range(0u32..8);
        frame[idx] ^= 1u8 << bit;
        self.faults.inner.corrupted.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Draw the drop fault for one frame, counting it on the handle when it
    /// fires.
    fn should_drop(&mut self) -> bool {
        let drop = self.rng.gen_bool(self.faults.drop_prob());
        if drop {
            self.faults.inner.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop
    }

    /// Refuse a call while partitioned. Uncounted: only
    /// [`send_frame`](Transport::send_frame) counts its refusals.
    fn refuse_if_partitioned(&self) -> Result<(), TransportError> {
        if self.faults.is_partitioned() {
            return Err(TransportError::Unavailable("partitioned".into()));
        }
        Ok(())
    }

    /// Sample a uniform delay within the configured range (inclusive). Returns
    /// `Duration::ZERO` when no range is set.
    fn sample_delay(&mut self) -> Duration {
        let Some((lo, hi)) = self.delay else {
            return Duration::ZERO;
        };
        let lo_ns = u64::try_from(lo.as_nanos()).unwrap_or(u64::MAX);
        let hi_ns = u64::try_from(hi.as_nanos()).unwrap_or(u64::MAX);
        if hi_ns <= lo_ns {
            return Duration::from_nanos(lo_ns);
        }
        Duration::from_nanos(self.rng.gen_range(lo_ns..=hi_ns))
    }
}

#[async_trait]
impl<T: Transport> Transport for Toxic<T> {
    async fn send_frame(&mut self, mut frame: Vec<u8>) -> Result<(), TransportError> {
        if self.faults.is_partitioned() {
            self.faults
                .inner
                .refused_sends
                .fetch_add(1, Ordering::Relaxed);
            return Err(TransportError::Unavailable("partitioned".into()));
        }
        // Drop: the frame vanishes, but the send still "succeeds".
        if self.should_drop() {
            return Ok(());
        }
        let subscribe =
            decode_frame(&frame).is_ok_and(|(header, _)| header.msg_kind == MsgKind::Subscribe);
        let corrupted = self.maybe_corrupt(&mut frame);
        let delay = self.sample_delay();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.inner.send_frame(frame).await?;
        if subscribe && !corrupted {
            self.faults
                .inner
                .subscribes_sent
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            if self.faults.is_partitioned() {
                return Err(TransportError::Unavailable("partitioned".into()));
            }
            // Both awaits below are resumption points, not restart points: the
            // frame and its due time are parked in `self.held` before either
            // one, so a cancelled poll costs latency and never a frame.
            if self.held.is_none() {
                let Some(mut frame) = self.inner.recv_frame().await? else {
                    return Ok(None);
                };
                // Drop: swallow this inbound frame and keep waiting.
                if self.should_drop() {
                    continue;
                }
                self.maybe_corrupt(&mut frame);
                let delay = if self.delay_send_only {
                    Duration::ZERO
                } else {
                    self.sample_delay()
                };
                let due = tokio::time::Instant::now() + delay;
                self.held = Some((frame, due));
            }
            let Some((_, due)) = self.held.as_ref() else {
                unreachable!("held was just filled");
            };
            tokio::time::sleep_until(*due).await;
            let Some((frame, _)) = self.held.take() else {
                unreachable!("held was just filled");
            };
            return Ok(Some(frame));
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.inner.close().await
    }

    async fn revoke_device(
        &mut self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.revoke_device(device_id).await
    }

    async fn blob_init(
        &mut self,
        stream_id: &[u8; 16],
        chunk_count: u32,
        size_bytes: u64,
    ) -> Result<String, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner
            .blob_init(stream_id, chunk_count, size_bytes)
            .await
    }

    async fn blob_put_chunk(
        &mut self,
        upload_id: &str,
        chunk_idx: u32,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.blob_put_chunk(upload_id, chunk_idx, bytes).await
    }

    async fn blob_finalize(
        &mut self,
        upload_id: &str,
        ciphertext_hash: &[u8; 32],
        chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner
            .blob_finalize(upload_id, ciphertext_hash, chunk_hashes)
            .await
    }

    async fn blob_fetch(&mut self, blob_id: &[u8; 16]) -> Result<Option<Vec<u8>>, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.blob_fetch(blob_id).await
    }

    async fn snapshot_latest(
        &mut self,
        stream_id: &[u8; 16],
    ) -> Result<Option<SnapshotPin>, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.snapshot_latest(stream_id).await
    }

    async fn snapshot_pin(
        &mut self,
        stream_id: &[u8; 16],
        blob_id: &[u8; 16],
    ) -> Result<SnapshotPin, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.snapshot_pin(stream_id, blob_id).await
    }

    async fn key_bundle_latest(&mut self) -> Result<Option<KeyBundlePin>, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.key_bundle_latest().await
    }

    async fn key_bundle_pin(
        &mut self,
        blob_id: &[u8; 16],
        manifest_digest: &[u8; 32],
        replaces: Option<&[u8; 16]>,
    ) -> Result<KeyBundlePinned, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner
            .key_bundle_pin(blob_id, manifest_digest, replaces)
            .await
    }

    async fn identity_chain_latest(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.identity_chain_latest().await
    }

    async fn identity_chain_pin(
        &mut self,
        chain: &[u8],
        replaces: Option<&[u8; 32]>,
    ) -> Result<IdentityChainPinned, TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.identity_chain_pin(chain, replaces).await
    }

    async fn recovery_blob_replace(&mut self, blob: &[u8]) -> Result<(), TransportError> {
        self.refuse_if_partitioned()?;
        self.inner.recovery_blob_replace(blob).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probabilities_clamp() {
        let h = FaultHandle::new(2.0, -1.0);
        assert!((h.drop_prob() - 1.0).abs() < f64::EPSILON);
        assert!(h.corrupt_prob().abs() < f64::EPSILON);
        h.set_drop_prob(0.5);
        assert!((h.drop_prob() - 0.5).abs() < f64::EPSILON);
    }

    fn a_frame(kind: MsgKind) -> Vec<u8> {
        sunrise_wire_protocol::encode_frame(
            kind,
            sunrise_wire_protocol::FrameFlags::EMPTY,
            &[1, 2, 3],
        )
        .expect("encode")
    }

    #[tokio::test]
    async fn every_drop_is_counted_in_both_directions() {
        let (a, mut b) = crate::chaos::loopback_pair();
        let config = ToxicConfig {
            drop_prob: 1.0,
            ..ToxicConfig::passthrough()
        };
        let (mut toxic, faults) = Toxic::with_seed(a, config, 1);
        for _ in 0..3 {
            toxic
                .send_frame(vec![7])
                .await
                .expect("a dropped send succeeds");
        }
        assert_eq!(faults.dropped(), 3);

        b.send_frame(vec![8]).await.expect("peer send");
        b.close().await.expect("peer close");
        assert_eq!(toxic.recv_frame().await.expect("recv"), None);
        assert_eq!(faults.dropped(), 4, "the inbound frame was swallowed");
        assert_eq!(faults.corrupted(), 0);
    }

    #[tokio::test]
    async fn every_corruption_is_counted_and_a_corrupted_subscribe_is_not() {
        let (a, mut b) = crate::chaos::loopback_pair();
        let config = ToxicConfig {
            corrupt_prob: 1.0,
            ..ToxicConfig::passthrough()
        };
        let (mut toxic, faults) = Toxic::with_seed(a, config, 2);
        toxic
            .send_frame(a_frame(MsgKind::Subscribe))
            .await
            .expect("send");
        assert_eq!(faults.corrupted(), 1);
        assert_eq!(
            faults.subscribes_sent(),
            0,
            "a Subscribe the relay cannot read is no resync round"
        );

        b.send_frame(vec![9, 9]).await.expect("peer send");
        toxic.recv_frame().await.expect("recv").expect("a frame");
        assert_eq!(faults.corrupted(), 2);
        assert_eq!(faults.dropped(), 0);
    }

    #[tokio::test]
    async fn only_forwarded_subscribes_are_counted() {
        let (a, _b) = crate::chaos::loopback_pair();
        let (mut toxic, faults) = Toxic::with_seed(a, ToxicConfig::passthrough(), 3);
        toxic
            .send_frame(a_frame(MsgKind::Subscribe))
            .await
            .expect("send");
        toxic
            .send_frame(a_frame(MsgKind::OpBatch))
            .await
            .expect("send");
        toxic.send_frame(vec![0; 4]).await.expect("send");
        assert_eq!(faults.subscribes_sent(), 1);

        faults.partition(true);
        assert!(toxic.send_frame(a_frame(MsgKind::Subscribe)).await.is_err());
        assert_eq!(faults.subscribes_sent(), 1, "a refused one never left");
        assert_eq!(faults.refused_sends(), 1);
    }

    #[tokio::test]
    async fn the_account_api_reaches_the_inner_transport_unless_partitioned() {
        let (a, _b) = crate::chaos::loopback_pair();
        let (mut toxic, faults) = Toxic::with_seed(a, ToxicConfig::passthrough(), 4);
        // The loopback has no account API, so reaching it answers Unsupported.
        assert!(matches!(
            toxic.blob_fetch(&[0; 16]).await,
            Err(TransportError::Unsupported)
        ));
        faults.partition(true);
        assert!(matches!(
            toxic.blob_fetch(&[0; 16]).await,
            Err(TransportError::Unavailable(_))
        ));
        assert_eq!(faults.refused_sends(), 0, "only sends are counted");
    }

    #[tokio::test]
    async fn wait_until_returns_once_the_condition_holds() {
        let h = FaultHandle::new(0.0, 0.0);
        h.inner.dropped.fetch_add(1, Ordering::Relaxed);
        h.wait_until("a drop", Duration::from_millis(50), |h| h.dropped() > 0)
            .await;
    }

    #[tokio::test]
    #[should_panic(expected = "waiting for a drop")]
    async fn wait_until_panics_at_its_timeout() {
        let h = FaultHandle::new(0.0, 0.0);
        h.wait_until("a drop", Duration::from_millis(30), |h| h.dropped() > 0)
            .await;
    }
}
