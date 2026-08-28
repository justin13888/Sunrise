//! `CoreConfig` — the dependency-injection bundle.
//!
//! Per `docs/01-architecture/shared-core.md` §determinism. Every clock /
//! RNG / storage handle the core needs is injected via this struct so
//! tests can substitute deterministic fakes.

use std::path::PathBuf;
use std::sync::Arc;
use sunrise_cbor::hlc::{Hlc, HlcError};

/// Pluggable wall-clock source. Tests inject a fake; production wraps
/// `std::time::SystemTime::now()` behind a narrowly-scoped `#[allow]` on
/// [`SystemClock::now_ms`] — the single relaxation of the determinism gate in
/// this crate's clock path.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current unix time in milliseconds.
    fn now_ms(&self) -> u64;

    /// IANA name of the *device-local* timezone, used to pin the civil
    /// (zone-less) window dimensions of a Task's scheduling constraints to real
    /// instants — per `docs/02-domain/scheduling-constraints.md` §Evaluation
    /// timezone, a Task evaluates in the device zone (a Routine evaluates in
    /// its own).
    ///
    /// Defaulted to `"UTC"` so existing implementors keep compiling and so a
    /// test that injects only a clock still gets a deterministic zone. Ambient
    /// timezone state is as much a determinism hazard as an ambient clock, so
    /// it enters the core through this one injected seam and nowhere else.
    fn timezone(&self) -> String {
        "UTC".to_string()
    }
}

/// Pluggable hybrid-logical-clock source.
///
/// Separate from [`Clock`] because it is *stateful*: it is the only thing in
/// the core that carries mutable state between calls, and the `(physical,
/// logical)` pair it returns is not a function of the wall clock alone. It is
/// injected for the same reason [`Clock`] is — `clippy.toml`'s
/// disallowed-methods list and the CI determinism gate forbid reading an
/// ambient clock inside crate source, and a test that wants to model a skewed
/// replica needs to supply the skew rather than discover it.
pub trait HlcClock: Send + Sync + std::fmt::Debug {
    /// Stamp an op this device is emitting. Strictly increasing per call.
    fn send(&self) -> Hlc;

    /// Absorb an `Hlc` received from a peer, advancing local state so that
    /// everything this device emits afterwards sorts after `received`.
    ///
    /// # Errors
    /// [`HlcError::DriftTooLarge`] when `received` is too far in the future to
    /// be a legitimate reading; the caller rejects the op rather than adopting
    /// the bad clock.
    fn observe(&self, received: Hlc) -> Result<(), HlcError>;

    /// The current local reading, without advancing it. Diagnostics only.
    fn peek(&self) -> Hlc;
}

/// The production [`HlcClock`]: HLC state over an injected [`Clock`].
///
/// State is deliberately **not persisted**. A restart resets the logical
/// counter to 0, which is safe because the physical component dominates the
/// ordering and only moves forward; the one case a reset can produce — two ops
/// from this device sharing a `(physical_ms, logical)` pair across a restart —
/// is what the `seq` term in the LWW tuple is there to break.
#[derive(Debug)]
pub struct MonotonicHlc {
    clock: Arc<dyn Clock>,
    state: parking_lot::Mutex<Hlc>,
}

impl MonotonicHlc {
    /// Build over `clock`, starting from a zero reading.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            state: parking_lot::Mutex::new(Hlc::default()),
        }
    }
}

impl HlcClock for MonotonicHlc {
    fn send(&self) -> Hlc {
        let now = self.clock.now_ms();
        let mut st = self.state.lock();
        *st = st.send(now);
        *st
    }

    fn observe(&self, received: Hlc) -> Result<(), HlcError> {
        let now = self.clock.now_ms();
        let mut st = self.state.lock();
        *st = st.receive(received, now)?;
        Ok(())
    }

    fn peek(&self) -> Hlc {
        *self.state.lock()
    }
}

/// Pluggable randomness source.
pub trait Rng: Send + Sync + std::fmt::Debug {
    /// Fill `dest` with cryptographically random bytes.
    fn fill_bytes(&self, dest: &mut [u8]);
}

/// Production clock backed by `std::time::SystemTime`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        // The determinism gate forbids SystemTime::now(). This is the one
        // controlled boundary where the wall clock enters the core: every
        // domain op reads time through `Clock::now_ms`, and this impl is what
        // that resolves to in production. Tests inject a FakeClock and never
        // reach here, so no test outcome depends on the host clock.
        #[allow(clippy::disallowed_methods)]
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
    }

    fn timezone(&self) -> String {
        // `TimeZone::system()` reads the OS zone (TZ / /etc/localtime) and
        // already falls back to UTC when it cannot resolve one. Only the
        // production clock consults it; tests inject a fake and get a fixed
        // zone, so no test outcome depends on the host's timezone.
        jiff::tz::TimeZone::system()
            .iana_name()
            .unwrap_or("UTC")
            .to_string()
    }
}

/// Production CSPRNG backed by `getrandom`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRng;

impl Rng for SystemRng {
    fn fill_bytes(&self, dest: &mut [u8]) {
        // OS CSPRNG; production binds via rand's OsRng.
        use rand_core::RngCore;
        rand::rngs::OsRng.fill_bytes(dest);
    }
}

/// Bundle injected when opening a [`crate::Core`].
#[derive(Clone)]
pub struct CoreConfig {
    /// Vault directory.
    pub vault_dir: PathBuf,
    /// Wall-clock source.
    pub clock: Arc<dyn Clock>,
    /// Causal-ordering source. Every emitted op is stamped from this, and
    /// every received op is observed into it.
    pub hlc: Arc<dyn HlcClock>,
    /// Random source.
    pub rng: Arc<dyn Rng>,
    /// `<semver>+<platform>` for log records and Hello frames.
    pub app: String,
    /// Sync configuration. `None` = offline mode (no driver task spawned;
    /// current single-device behavior).
    pub sync: Option<crate::sync_driver::SyncConfig>,
}

impl std::fmt::Debug for CoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreConfig")
            .field("vault_dir", &self.vault_dir)
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}

impl CoreConfig {
    /// Construct with the production clock + RNG.
    #[must_use]
    pub fn production(vault_dir: PathBuf, app: impl Into<String>) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        Self {
            vault_dir,
            hlc: Arc::new(MonotonicHlc::new(Arc::clone(&clock))),
            clock,
            rng: Arc::new(SystemRng),
            app: app.into(),
            sync: None,
        }
    }

    /// Construct with an injected clock + RNG, deriving the HLC from that
    /// clock. The shape every test wants: skew the [`Clock`] and the HLC skews
    /// with it, without the caller assembling two objects that must agree.
    #[must_use]
    pub fn with_clock(
        vault_dir: PathBuf,
        app: impl Into<String>,
        clock: Arc<dyn Clock>,
        rng: Arc<dyn Rng>,
    ) -> Self {
        Self {
            vault_dir,
            hlc: Arc::new(MonotonicHlc::new(Arc::clone(&clock))),
            clock,
            rng,
            app: app.into(),
            sync: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic test clock.
    #[derive(Debug)]
    struct FakeClock {
        ms: parking_lot::Mutex<u64>,
    }
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.ms.lock()
        }
    }

    #[test]
    fn config_holds_injected_clock() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "0.1.0+test",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(123),
            }),
            Arc::new(SystemRng),
        );
        assert_eq!(cfg.clock.now_ms(), 123);
    }

    #[test]
    fn the_hlc_follows_the_injected_clock() {
        let clock = Arc::new(FakeClock {
            ms: parking_lot::Mutex::new(1_000),
        });
        let cfg = CoreConfig::with_clock("/tmp".into(), "t", clock.clone(), Arc::new(SystemRng));
        assert_eq!(cfg.hlc.send(), Hlc::at(1_000));
        *clock.ms.lock() = 2_000;
        assert_eq!(cfg.hlc.send(), Hlc::at(2_000));
    }

    #[test]
    fn successive_sends_within_one_millisecond_still_order() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "t",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(1_000),
            }),
            Arc::new(SystemRng),
        );
        let a = cfg.hlc.send();
        let b = cfg.hlc.send();
        assert!(b > a, "two ops in one millisecond must still be ordered");
    }

    #[test]
    fn observing_a_faster_peer_pulls_this_device_past_it() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "t",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(1_000),
            }),
            Arc::new(SystemRng),
        );
        let peer = Hlc::at(1_200);
        cfg.hlc.observe(peer).unwrap();
        assert!(cfg.hlc.send() > peer);
    }

    #[test]
    fn a_wildly_skewed_peer_is_refused_and_does_not_move_us() {
        let cfg = CoreConfig::with_clock(
            "/tmp".into(),
            "t",
            Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(1_000),
            }),
            Arc::new(SystemRng),
        );
        let before = cfg.hlc.peek();
        let liar = Hlc::at(1_000 + sunrise_cbor::hlc::MAX_DRIFT_MS + 1);
        assert!(cfg.hlc.observe(liar).is_err());
        assert_eq!(
            cfg.hlc.peek(),
            before,
            "a refused op must not move our clock"
        );
    }
}
