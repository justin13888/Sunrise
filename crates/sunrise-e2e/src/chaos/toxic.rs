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
//!   random [`Duration`] before it is forwarded (`tokio::time::sleep`).
//! - **partition** — a runtime switch on [`FaultHandle`]; while partitioned all
//!   sends and receives return a transport error, modelling a cut link.
//!
//! ## Determinism
//!
//! All random decisions are drawn from a seeded `rand_chacha` `ChaCha20` RNG.
//! The seed comes from the `SUNRISE_FUZZ_SEED` environment variable (see
//! [`seed_from_env`]) or an explicit constructor argument
//! ([`Toxic::with_seed`]). No `thread_rng` is used (it is banned by
//! `clippy.toml`), so a run is exactly reproducible from its seed.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use sunrise_sync::transport::{Transport, TransportError};

/// Default RNG seed used when `SUNRISE_FUZZ_SEED` is unset and no explicit seed
/// is supplied. Fixed so the harness is reproducible out of the box.
pub const DEFAULT_FUZZ_SEED: u64 = 0x5352_5f43_4841_4f53; // "SR_CHAOS"

/// Resolve the RNG seed from the `SUNRISE_FUZZ_SEED` environment variable.
///
/// Per `docs/10-cross-cutting/testing.md`, the seed convention is hex, but a
/// plain decimal integer is also accepted. A leading `0x`/`0X` forces hex.
/// Returns `None` when the variable is unset or unparseable.
#[must_use]
pub fn seed_from_env() -> Option<u64> {
    let raw = std::env::var("SUNRISE_FUZZ_SEED").ok()?;
    let t = raw.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>()
            .ok()
            .or_else(|| u64::from_str_radix(t, 16).ok())
    }
}

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
}

impl ToxicConfig {
    /// A pass-through configuration: no drop, no corruption, no delay.
    #[must_use]
    pub const fn passthrough() -> Self {
        Self {
            drop_prob: 0.0,
            corrupt_prob: 0.0,
            delay: None,
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
}

impl FaultHandle {
    fn new(drop_prob: f64, corrupt_prob: f64) -> Self {
        Self {
            inner: Arc::new(FaultState {
                partitioned: AtomicBool::new(false),
                drop_bits: AtomicU64::new(clamp_prob(drop_prob).to_bits()),
                corrupt_bits: AtomicU64::new(clamp_prob(corrupt_prob).to_bits()),
            }),
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
    rng: ChaCha20Rng,
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
            rng: ChaCha20Rng::seed_from_u64(seed),
        };
        (toxic, faults)
    }

    /// Wrap `inner` reusing an existing shared [`FaultHandle`] (rather than
    /// minting a fresh one as [`Toxic::with_seed`] does), with an explicit RNG
    /// `seed`. Every wrapper built from the same handle observes the same
    /// runtime drop / corrupt / partition switches — the shape a reconnecting
    /// transport factory needs, where one handle must steer every connection it
    /// opens. `delay` is the static per-frame delay range (mirrors
    /// [`ToxicConfig::delay`]).
    #[must_use]
    pub fn with_handle(
        inner: T,
        faults: FaultHandle,
        delay: Option<(Duration, Duration)>,
        seed: u64,
    ) -> Self {
        Self {
            inner,
            faults,
            delay,
            rng: ChaCha20Rng::seed_from_u64(seed),
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
    /// byte differs. Empty frames are left untouched.
    fn maybe_corrupt(&mut self, mut frame: Vec<u8>) -> Vec<u8> {
        if frame.is_empty() {
            return frame;
        }
        if self.rng.gen_bool(self.faults.corrupt_prob()) {
            let idx = self.rng.gen_range(0..frame.len());
            let bit = self.rng.gen_range(0u32..8);
            frame[idx] ^= 1u8 << bit;
        }
        frame
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
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        if self.faults.is_partitioned() {
            return Err(TransportError::Unavailable("partitioned".into()));
        }
        // Drop: the frame vanishes, but the send still "succeeds".
        if self.rng.gen_bool(self.faults.drop_prob()) {
            return Ok(());
        }
        let frame = self.maybe_corrupt(frame);
        let delay = self.sample_delay();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.inner.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            if self.faults.is_partitioned() {
                return Err(TransportError::Unavailable("partitioned".into()));
            }
            let Some(frame) = self.inner.recv_frame().await? else {
                return Ok(None);
            };
            // Drop: swallow this inbound frame and keep waiting.
            if self.rng.gen_bool(self.faults.drop_prob()) {
                continue;
            }
            let frame = self.maybe_corrupt(frame);
            let delay = self.sample_delay();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            return Ok(Some(frame));
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.inner.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_from_env_parses_decimal_and_hex() {
        // These run in-process; guard against parallel env races by scoping.
        std::env::set_var("SUNRISE_FUZZ_SEED", "42");
        assert_eq!(seed_from_env(), Some(42));
        std::env::set_var("SUNRISE_FUZZ_SEED", "0xff");
        assert_eq!(seed_from_env(), Some(255));
        std::env::set_var("SUNRISE_FUZZ_SEED", "deadbeef");
        assert_eq!(seed_from_env(), Some(0xdead_beef));
        std::env::remove_var("SUNRISE_FUZZ_SEED");
        assert_eq!(seed_from_env(), None);
    }

    #[test]
    fn probabilities_clamp() {
        let h = FaultHandle::new(2.0, -1.0);
        assert!((h.drop_prob() - 1.0).abs() < f64::EPSILON);
        assert!(h.corrupt_prob().abs() < f64::EPSILON);
        h.set_drop_prob(0.5);
        assert!((h.drop_prob() - 0.5).abs() < f64::EPSILON);
    }
}
