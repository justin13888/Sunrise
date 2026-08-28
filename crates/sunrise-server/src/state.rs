//! Shared server state held by Axum extractors.

use crate::auth::{NullVerifier, TokenVerifier};
use crate::config::ServerConfig;
use crate::metrics::Metrics;
use crate::relay::RelayHub;
use crate::store::{Store, StoreError};
use std::sync::Arc;

/// Pluggable wall-clock; tests inject a fake.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current unix time in milliseconds.
    fn now_ms(&self) -> u64;
}

/// Production clock backed by `std::time::SystemTime`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        #[allow(clippy::disallowed_methods)]
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO);
        u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
    }
}

/// State injected into every handler via `axum::extract::State`.
#[derive(Debug, Clone)]
pub struct ServerState {
    /// Server configuration (immutable for the lifetime of the process).
    pub config: Arc<ServerConfig>,
    /// In-process relay hub for `OpBatch` fan-out.
    pub relay: RelayHub,
    /// Wall-clock source.
    pub clock: Arc<dyn Clock>,
    /// Token verifier used by authenticated routes.
    pub token_verifier: Arc<dyn TokenVerifier>,
    /// Account + device persistence.
    pub store: Arc<Store>,
    /// Metric registry; cheap to clone.
    pub metrics: Metrics,
    /// Blob store root (self-host filesystem path).
    pub blob_root: Arc<std::path::PathBuf>,
    /// Retention bounds for the durable relay op log.
    pub durable_caps: crate::relay_log::DurableCaps,
}

impl ServerState {
    /// Wrap a [`ServerConfig`] with default production clock + empty relay.
    ///
    /// # Panics
    ///
    /// If the store at [`ServerConfig::sqlite_path`] cannot be opened. Use
    /// [`ServerState::try_new`] where that is a condition to report rather than
    /// a reason to abort; the binary entrypoint does. An in-memory store — the
    /// `sqlite_path: None` case every test takes — has nothing to fail on.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self::try_new(config).expect("open account store")
    }

    /// Wrap a [`ServerConfig`], reporting store-open failure.
    pub fn try_new(config: ServerConfig) -> Result<Self, StoreError> {
        let store = Store::open(config.sqlite_path.as_deref())?;
        Ok(Self::assemble(
            config,
            Arc::new(SystemClock),
            Arc::new(store),
        ))
    }

    fn assemble(config: ServerConfig, clock: Arc<dyn Clock>, store: Arc<Store>) -> Self {
        let blob_root = config
            .blob_root
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("sunrise-self-host-blobs"));
        Self {
            config: Arc::new(config),
            relay: RelayHub::new(),
            clock,
            token_verifier: Arc::new(NullVerifier),
            store,
            metrics: Metrics::new(),
            blob_root: Arc::new(blob_root),
            durable_caps: crate::relay_log::DurableCaps::default(),
        }
    }

    /// Replace the durable log's retention bounds.
    ///
    /// Production uses the defaults. A test that has to reach *past* durable
    /// retention — where the gap report lives now that the in-memory ring no
    /// longer bounds history — cannot do so by publishing 256 MiB.
    #[must_use]
    pub const fn with_durable_caps(mut self, caps: crate::relay_log::DurableCaps) -> Self {
        self.durable_caps = caps;
        self
    }

    /// Whether the installed verifier is the single-tenant self-host one.
    ///
    /// Drives the startup refusal in `main` and the operator warning. Asking
    /// the verifier rather than tracking a separate flag keeps the two from
    /// drifting apart — the dangerous state is precisely "NullVerifier
    /// installed", not "some config field says self-host".
    #[must_use]
    pub fn is_single_tenant(&self) -> bool {
        self.token_verifier.is_single_tenant()
    }

    /// Replace the relay's retained-ring bounds.
    ///
    /// Production uses the defaults. A test that has to reach *past* the ring —
    /// the eviction watermark and the cursor-gap report only exist beyond it —
    /// cannot do so by publishing 4096 frames, so the bounds are injectable.
    #[must_use]
    pub fn with_ring_caps(mut self, caps: crate::relay::RingCaps) -> Self {
        self.relay = RelayHub::with_caps(caps);
        self
    }

    /// Replace the token verifier (production wiring, and tests).
    #[must_use]
    pub fn with_verifier(mut self, verifier: Arc<dyn TokenVerifier>) -> Self {
        self.token_verifier = verifier;
        self
    }

    /// Wrap a [`ServerConfig`] with a caller-supplied clock (used by tests).
    ///
    /// # Panics
    ///
    /// As [`ServerState::new`].
    #[must_use]
    pub fn with_clock(config: ServerConfig, clock: Arc<dyn Clock>) -> Self {
        let store = Store::open(config.sqlite_path.as_deref()).expect("open account store");
        Self::assemble(config, clock, Arc::new(store))
    }
}
