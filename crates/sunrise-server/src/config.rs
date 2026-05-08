//! Server config (TOML-backed).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Server configuration. Read from `sunrise.toml` (production) or built
/// programmatically (tests).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// `host:port` to bind.
    pub bind: String,
    /// Server's published version string.
    pub server_app_v: String,
    /// Optional OIDC issuer URL.
    pub oidc_issuer: Option<String>,
    /// Self-host SQLite path (None = ephemeral in-memory, suitable for
    /// tests).
    pub sqlite_path: Option<PathBuf>,
    /// Self-host blob root (None = `<sqlite_dir>/blobs`).
    pub blob_root: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8443".into(),
            server_app_v: env!("CARGO_PKG_VERSION").into(),
            oidc_issuer: None,
            sqlite_path: None,
            blob_root: None,
        }
    }
}
