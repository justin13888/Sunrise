//! `CoreConfig` — the dependency-injection bundle.
//!
//! Per `docs/01-architecture/shared-core.md` §determinism. Every clock /
//! RNG / storage handle the core needs is injected via this struct so
//! tests can substitute deterministic fakes.

use std::path::PathBuf;
use std::sync::Arc;

/// Pluggable wall-clock source. Tests inject a fake; production wraps
/// `std::time::SystemTime::now()` behind a narrowly-scoped `#[allow]` on
/// [`SystemClock::now_ms`] — the single relaxation of the determinism gate in
/// this crate's clock path.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current unix time in milliseconds.
    fn now_ms(&self) -> u64;
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
        Self {
            vault_dir,
            clock: Arc::new(SystemClock),
            rng: Arc::new(SystemRng),
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
        let cfg = CoreConfig {
            vault_dir: "/tmp".into(),
            clock: Arc::new(FakeClock {
                ms: parking_lot::Mutex::new(123),
            }),
            rng: Arc::new(SystemRng),
            app: "0.1.0+test".into(),
            sync: None,
        };
        assert_eq!(cfg.clock.now_ms(), 123);
    }
}
