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
use std::sync::atomic::Ordering;
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

mod handle;

pub use handle::FaultHandle;

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
mod tests;
