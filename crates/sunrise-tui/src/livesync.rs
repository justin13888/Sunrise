//! Dev/demo live-sync wiring for the TUI binary.
//!
//! This module turns a handful of environment variables into a running sync
//! session so a human can drive the two-terminal sync demo. It is **factored
//! out of `main`** for two reasons:
//!
//! 1. The env→plan assembly ([`plan_from_env`]) is a pure function, unit-tested
//!    without touching the process environment.
//! 2. The open-core-and-start-sync sequence ([`open_with_plan`]) can be driven
//!    by an integration test against a spawned relay — the headless stand-in
//!    for the interactive TTY demo (see `tests/live_sync.rs`).
//!
//! # Device trust is a dev affordance
//!
//! Two vaults that share a root still need a [`Command::TrustDevice`] cert
//! exchange before each accepts the other's op envelopes. Real pairing does
//! this over the wire; for the demo we shuttle certs through **files**:
//! `SUNRISE_EXPORT_CERT_FILE` writes this device's cert on startup and
//! `SUNRISE_TRUST_CERT_FILE` reads + trusts a peer's. This is intentionally
//! dead simple and is **not** how production pairing works.

use std::path::PathBuf;
use std::sync::Arc;

use sunrise_core::{
    BoxTransport, Command, ConnectFuture, Core, CoreConfig, CoreError, SyncConfig,
    TransportFactory, Unlock,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_sync::WsTransport;

/// Env var: relay `/sync` WebSocket URL. Unset ⇒ sync stays off.
/// Example for the bundled self-host server: `ws://127.0.0.1:8443/sync`.
pub const ENV_SYNC_URL: &str = "SUNRISE_SYNC_URL";
/// Env var: path to write this device's cert (canonical CBOR) on startup.
pub const ENV_EXPORT_CERT: &str = "SUNRISE_EXPORT_CERT_FILE";
/// Env var: path to a peer's cert (canonical CBOR) to trust on startup.
pub const ENV_TRUST_CERT: &str = "SUNRISE_TRUST_CERT_FILE";

/// Raw environment inputs. Kept separate from parsing so [`plan_from_env`]
/// stays a pure function over explicit values.
#[derive(Debug, Default, Clone)]
pub struct SyncEnv {
    /// Raw `SUNRISE_SYNC_URL`.
    pub url: Option<String>,
    /// Raw `SUNRISE_EXPORT_CERT_FILE`.
    pub export_cert: Option<String>,
    /// Raw `SUNRISE_TRUST_CERT_FILE`.
    pub trust_cert: Option<String>,
}

impl SyncEnv {
    /// Read the three live-sync env vars from the process environment.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            url: std::env::var(ENV_SYNC_URL).ok(),
            export_cert: std::env::var(ENV_EXPORT_CERT).ok(),
            trust_cert: std::env::var(ENV_TRUST_CERT).ok(),
        }
    }
}

/// Parsed, validated live-sync plan the runtime executes on startup.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    /// `Some` ⇒ start the driver against this relay; `None` ⇒ offline.
    pub sync: Option<SyncConfig>,
    /// `Some` ⇒ write this device's cert here on startup.
    pub export_cert: Option<PathBuf>,
    /// `Some` ⇒ read + trust this peer cert on startup.
    pub trust_cert: Option<PathBuf>,
}

impl SyncPlan {
    /// Whether sync is off (no relay URL was configured).
    #[must_use]
    pub const fn is_off(&self) -> bool {
        self.sync.is_none()
    }
}

/// Pure env→plan assembly (no I/O). Blank / whitespace-only values collapse to
/// `None`, so an exported-but-empty env var behaves as unset.
#[must_use]
pub fn plan_from_env(env: &SyncEnv) -> SyncPlan {
    fn clean(s: Option<&str>) -> Option<String> {
        s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }
    SyncPlan {
        sync: clean(env.url.as_deref()).map(|url| SyncConfig { url }),
        export_cert: clean(env.export_cert.as_deref()).map(PathBuf::from),
        trust_cert: clean(env.trust_cert.as_deref()).map(PathBuf::from),
    }
}

/// Build a [`TransportFactory`] that dials `url` with the real [`WsTransport`]
/// on every connect attempt (initial connect + every reconnect).
///
/// This is the same factory shape `sunrise-e2e::ws_factory` uses; the driver is
/// transport-agnostic and calls it once per connection attempt.
#[must_use]
pub fn ws_factory(url: &str) -> TransportFactory {
    let url = url.to_string();
    Arc::new(move || {
        let url = url.clone();
        Box::pin(async move {
            let t = WsTransport::connect(&url).await?;
            Ok(Box::new(t) as BoxTransport)
        }) as ConnectFuture
    })
}

/// Execute `plan` against an already-opened `core`: export our device cert,
/// trust a peer cert, then start the sync driver. Returns human-readable log
/// lines for the startup banner.
///
/// Must be called from within a tokio runtime ([`Core::start_sync`] spawns the
/// driver task). Every step is best-effort for the demo: a missing/unwritable
/// cert file is logged, not fatal.
pub async fn apply_plan(core: &Arc<Core>, plan: &SyncPlan) -> Vec<String> {
    let mut log = Vec::new();

    if let Some(path) = &plan.export_cert {
        match std::fs::write(path, core.device_cert()) {
            Ok(()) => log.push(format!("exported device cert -> {}", path.display())),
            Err(e) => log.push(format!("cert export failed ({}): {e}", path.display())),
        }
    }

    if let Some(path) = &plan.trust_cert {
        match std::fs::read(path) {
            Ok(bytes) => match core.submit(Command::TrustDevice { cert_cbor: bytes }).await {
                Ok(_) => log.push(format!("trusted peer cert <- {}", path.display())),
                Err(e) => log.push(format!("trust submit failed: {e}")),
            },
            Err(e) => log.push(format!("trust cert not read ({}): {e}", path.display())),
        }
    }

    match &plan.sync {
        Some(sc) => match core.start_sync(ws_factory(&sc.url)) {
            Ok(()) => log.push(format!("sync driver started -> {}", sc.url)),
            Err(e) => log.push(format!("start_sync failed: {e}")),
        },
        None => log.push(format!("sync off (set {ENV_SYNC_URL} to enable)")),
    }

    log
}

/// Open a core at `vault_dir` keyed by the shared dev `root`, then execute
/// `plan` (export/trust certs, start sync). Returns the `Arc<Core>` plus the
/// startup log.
///
/// This is exactly the sequence the binary runs at startup, factored out so the
/// integration test can drive it against a spawned relay without a TTY.
pub async fn open_with_plan(
    vault_dir: PathBuf,
    app: &str,
    root: [u8; 32],
    plan: &SyncPlan,
) -> Result<(Arc<Core>, Vec<String>), CoreError> {
    let mut cfg = CoreConfig::production(vault_dir, app.to_string());
    // Seed the config's sync URL for parity with the driver's dialing target;
    // the driver itself takes the already-built factory from `apply_plan`.
    cfg.sync.clone_from(&plan.sync);
    let core =
        Arc::new(Core::open(cfg, Unlock::DevicePaired(VaultRootKey::from_bytes(root))).await?);
    let log = apply_plan(&core, plan).await;
    Ok((core, log))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_off_with_no_env() {
        let plan = plan_from_env(&SyncEnv::default());
        assert!(plan.is_off());
        assert_eq!(plan, SyncPlan::default());
    }

    #[test]
    fn plan_reads_url_and_cert_paths_and_trims() {
        let env = SyncEnv {
            url: Some("  ws://127.0.0.1:8443/sync ".into()),
            export_cert: Some("/tmp/self.cbor".into()),
            trust_cert: Some("   ".into()), // whitespace-only -> None
        };
        let plan = plan_from_env(&env);
        assert_eq!(
            plan.sync,
            Some(SyncConfig {
                url: "ws://127.0.0.1:8443/sync".into()
            })
        );
        assert_eq!(plan.export_cert, Some(PathBuf::from("/tmp/self.cbor")));
        assert_eq!(plan.trust_cert, None);
        assert!(!plan.is_off());
    }

    #[test]
    fn blank_url_stays_off() {
        let env = SyncEnv {
            url: Some(String::new()),
            ..SyncEnv::default()
        };
        assert!(plan_from_env(&env).is_off());
    }
}
