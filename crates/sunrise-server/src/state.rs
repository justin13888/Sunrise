//! Shared server state held by Axum extractors.

use crate::auth::{NullVerifier, TokenVerifier};
use crate::config::ServerConfig;
use crate::metrics::Metrics;
use crate::relay::RelayHub;
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
    /// Metric registry; cheap to clone.
    pub metrics: Metrics,
    /// Blob store root (self-host filesystem path).
    pub blob_root: Arc<std::path::PathBuf>,
}

impl ServerState {
    /// Wrap a [`ServerConfig`] with default production clock + empty relay.
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        let blob_root = config
            .blob_root
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("sunrise-self-host-blobs"));
        Self {
            config: Arc::new(config),
            relay: RelayHub::new(),
            clock: Arc::new(SystemClock),
            token_verifier: Arc::new(NullVerifier),
            metrics: Metrics::new(),
            blob_root: Arc::new(blob_root),
        }
    }

    /// Wrap a [`ServerConfig`] with a caller-supplied clock (used by tests).
    #[must_use]
    pub fn with_clock(config: ServerConfig, clock: Arc<dyn Clock>) -> Self {
        let blob_root = config
            .blob_root
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("sunrise-self-host-blobs"));
        Self {
            config: Arc::new(config),
            relay: RelayHub::new(),
            clock,
            token_verifier: Arc::new(NullVerifier),
            metrics: Metrics::new(),
            blob_root: Arc::new(blob_root),
        }
    }
}
