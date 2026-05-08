//! Shared server state held by Axum extractors.

use crate::config::ServerConfig;
use std::sync::Arc;

/// State injected into every handler via `axum::extract::State`.
#[derive(Debug, Clone)]
pub struct ServerState {
    /// Server configuration (immutable for the lifetime of the process).
    pub config: Arc<ServerConfig>,
}

impl ServerState {
    /// Wrap a [`ServerConfig`].
    #[must_use]
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}
