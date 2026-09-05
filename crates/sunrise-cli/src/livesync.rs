//! Dev/demo live-sync wiring: environment variables into a running sync
//! session.
//!
//! Factored out of `main` for two reasons:
//!
//! 1. The env→plan assembly ([`plan_from_env`]) is a pure function, unit-tested
//!    without touching the process environment.
//! 2. The open-core-and-start-sync sequence ([`open_with_plan`]) can be driven
//!    by an integration test against a spawned relay, with no process boundary
//!    and no second device (see `tests/live_sync.rs`).
//!
//! # Pairing is a dev affordance here
//!
//! Two vaults that share a vault root are **not** two devices on one account.
//! Since ADR-0024 each vault mints its own account identity, Stream keys are
//! random rather than derived from the root, and a device joins an account by
//! being handed a `PairingPayload` — the identity keys plus every Stream key —
//! over an authenticated channel.
//!
//! Sharing a root was already a dev stand-in ([`crate::vault::ENV_VAULT_ROOT`],
//! since every vault otherwise gets its own); this is the second half of the
//! same stand-in, and it now has to carry the payload rather than a
//! certificate. For the demo we shuttle it through **files**:
//! `SUNRISE_EXPORT_PAIRING_FILE` writes this device's payload after startup and
//! `SUNRISE_PAIRING_FILE` is read *before* the vault opens and adopted as the
//! account.
//!
//! The payload is the account, in the clear, on disk. That is acceptable for a
//! two-terminal walkthrough on one machine and is **not** how production
//! pairing works — real pairing seals it through the Noise XX channel in
//! `sunrise-pairing`, which is what the app does.

use std::path::PathBuf;

use std::sync::Arc;
use sunrise_auth::CredentialStore;

use sunrise_core::{
    BoxTransport, ConnectFuture, Core, CoreConfig, CoreError, SyncConfig, TokenSource,
    TransportFactory, Unlock,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_sync::SseTransport;

/// Env var: relay `/sync` WebSocket URL. Unset ⇒ sync stays off.
/// Example for the bundled self-host server: `http://127.0.0.1:8443`.
pub const ENV_SYNC_URL: &str = "SUNRISE_SYNC_URL";
/// Env var: path to write this device's pairing payload on startup.
pub const ENV_EXPORT_PAIRING: &str = "SUNRISE_EXPORT_PAIRING_FILE";
/// Env var: path to a pairing payload to adopt when opening the vault.
pub const ENV_ADOPT_PAIRING: &str = "SUNRISE_PAIRING_FILE";
/// Env var: bearer token presented on the `/sync` upgrade.
///
/// Unset is only viable against a self-host relay running `NullVerifier`;
/// every other deployment answers an unauthenticated upgrade with `401`.
pub const ENV_SYNC_TOKEN: &str = "SUNRISE_SYNC_TOKEN";

/// Raw environment inputs. Kept separate from parsing so [`plan_from_env`]
/// stays a pure function over explicit values.
#[derive(Debug, Default, Clone)]
pub struct SyncEnv {
    /// Raw `SUNRISE_SYNC_URL`.
    pub url: Option<String>,
    /// Raw `SUNRISE_EXPORT_PAIRING_FILE`.
    pub export_pairing: Option<String>,
    /// Raw `SUNRISE_PAIRING_FILE`.
    pub adopt_pairing: Option<String>,
    /// Raw `SUNRISE_SYNC_TOKEN`.
    pub token: Option<String>,
    /// Access token from a previous `sunrise login`, if any. Lower precedence
    /// than [`SyncEnv::token`].
    pub stored_token: Option<String>,
}

impl SyncEnv {
    /// Read the three live-sync env vars from the process environment.
    #[must_use]
    pub fn from_process_env() -> Self {
        Self {
            url: std::env::var(ENV_SYNC_URL).ok(),
            export_pairing: std::env::var(ENV_EXPORT_PAIRING).ok(),
            adopt_pairing: std::env::var(ENV_ADOPT_PAIRING).ok(),
            token: std::env::var(ENV_SYNC_TOKEN).ok(),
            stored_token: None,
        }
    }

    /// Attach the access token from a stored login, if one is present and not
    /// already expired. An expired one is deliberately not offered: presenting
    /// it would earn a `401` and a reconnect loop, where offering nothing at
    /// least reaches a self-host relay.
    #[must_use]
    pub fn with_stored(mut self, store: &dyn CredentialStore, now_ms: u64) -> Self {
        self.stored_token = store
            .load()
            .ok()
            .flatten()
            .filter(|c| !c.is_expired_at(now_ms))
            .map(|c| c.access_token);
        self
    }
}

/// Parsed, validated live-sync plan the runtime executes on startup.
///
/// Not `PartialEq`: [`SyncConfig`] carries a bearer handle and has no
/// meaningful equality (see its docs).
#[derive(Debug, Default, Clone)]
pub struct SyncPlan {
    /// `Some` ⇒ start the driver against this relay; `None` ⇒ offline.
    pub sync: Option<SyncConfig>,
    /// `Some` ⇒ write this device's pairing payload here on startup.
    pub export_pairing: Option<PathBuf>,
    /// `Some` ⇒ adopt this pairing payload when opening the vault.
    pub adopt_pairing: Option<PathBuf>,
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
    // A stored login beats the env var. `SUNRISE_SYNC_TOKEN` stays as the
    // override for CI and for a relay whose token comes from somewhere else,
    // but a user who ran `sunrise login` should not have to also export it.
    let token = clean(env.token.as_deref()).or_else(|| clean(env.stored_token.as_deref()));
    let credential = TokenSource::new(token);
    SyncPlan {
        sync: clean(env.url.as_deref()).map(|url| SyncConfig::new(url).with_credential(credential)),
        export_pairing: clean(env.export_pairing.as_deref()).map(PathBuf::from),
        adopt_pairing: clean(env.adopt_pairing.as_deref()).map(PathBuf::from),
    }
}

/// Build a [`TransportFactory`] that reaches `url` with the real [`SseTransport`]
/// on every connect attempt (initial connect + every reconnect), presenting
/// whatever bearer `credential` holds **at that moment**.
///
/// Reading the token per attempt rather than capturing it is the whole point:
/// a reconnect after a renewal has to present the new token, and a factory
/// that closed over a `String` would present the one sync started with
/// forever.
///
/// This is the same factory shape `sunrise-e2e::ws_factory` uses; the driver is
/// transport-agnostic and calls it once per connection attempt.
#[must_use]
pub fn ws_factory(url: &str, credential: TokenSource) -> TransportFactory {
    let url = url.to_string();
    Arc::new(move || {
        let url = url.clone();
        let bearer = credential.get();
        Box::pin(async move {
            let t = SseTransport::connect_with_bearer(&url, bearer.as_deref());
            Ok(Box::new(t) as BoxTransport)
        }) as ConnectFuture
    })
}

/// Execute `plan` against an already-opened `core`: export this device's
/// pairing payload, then start the sync driver. Returns human-readable log
/// lines for the startup banner.
///
/// Adopting a payload is *not* here, because it cannot be: an account identity
/// is chosen when the vault is created, not afterwards. [`open_with_plan`]
/// reads it before `Core::open`.
///
/// Not `async`, but must still be called from within a tokio runtime:
/// [`Core::start_sync`] spawns the driver task. It awaits nothing itself —
/// adopting a pairing payload is the one step that has to happen inside
/// `Core::open`, so it left this function. Every step is best-effort for the
/// demo: a missing/unwritable pairing file is logged, not fatal.
///
/// # Two outputs, on purpose
///
/// The returned `Vec<String>` is for the *demo*: it names the files it
/// touched, which is exactly what a human running the two-terminal walkthrough
/// needs to see. Those strings are not log records and must not become them —
/// a filesystem path carries the operator's home directory, and
/// `docs/10-cross-cutting/logging.md` §6 does not put that on the allowlist.
///
/// The `tracing` events emitted alongside carry the *structure* — did it work,
/// which relay host, what failed — with no paths in them. Integration tests
/// read the strings; the log reads the events.
pub fn apply_plan(core: &Arc<Core>, plan: &SyncPlan) -> Vec<String> {
    let mut log = Vec::new();

    if let Some(path) = &plan.export_pairing {
        match core
            .export_pairing_payload()
            .map_err(|e| e.to_string())
            .and_then(|p| sunrise_pairing::encode_pairing_payload(&p).map_err(|e| e.to_string()))
            .and_then(|bytes| write_private(path, &bytes).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                tracing::info!(
                    ev = "ui.pair.payload_exported",
                    result = "ok",
                    "pairing payload written"
                );
                log.push(format!("exported pairing payload -> {}", path.display()));
            }
            Err(cause) => {
                tracing::warn!(
                    ev = "ui.pair.payload_exported",
                    result = "failed",
                    err_code = "IO_WRITE_FAILED",
                    err_kind = "permanent",
                    retryable = false,
                    cause,
                    "pairing payload not written"
                );
                log.push(format!(
                    "pairing export failed ({}): {cause}",
                    path.display()
                ));
            }
        }
    }

    match &plan.sync {
        Some(sc) => match core.start_sync(ws_factory(&sc.url, sc.credential.clone())) {
            Ok(()) => {
                // The relay *host* is the sanctioned connection-diagnostic
                // identifier (logging.md §6.2); the full URL could carry a
                // query string, so only the host goes in.
                tracing::info!(
                    ev = "sync.session.opening",
                    relay = %relay_host(&sc.url),
                    result = "ok",
                    "sync driver started"
                );
                log.push(format!("sync driver started -> {}", sc.url));
            }
            Err(e) => {
                tracing::warn!(
                    ev = "sync.session.error",
                    relay = %relay_host(&sc.url),
                    result = "failed",
                    err_code = "SYNC_START_FAILED",
                    err_kind = "transient",
                    retryable = true,
                    cause = %e,
                    "sync driver did not start"
                );
                log.push(format!("start_sync failed: {e}"));
            }
        },
        None => {
            tracing::info!(ev = "sync.session.off", result = "skipped", "sync disabled");
            log.push(format!("sync off (set {ENV_SYNC_URL} to enable)"));
        }
    }

    log
}

/// The `host[:port]` of a relay URL, for the `relay` log field.
///
/// Everything after the authority is dropped: a `http://…?access_token=…`
/// must never reach a log, and the host is all a connection diagnostic needs.
/// Falls back to `"unknown"` rather than echoing an unparsable string back.
#[must_use]
pub fn relay_host(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // Strip any `user:pass@` credential prefix.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.is_empty() {
        "unknown".to_string()
    } else {
        host.to_string()
    }
}

/// Open a core at `vault_dir` keyed by `root`, then execute
/// `plan` (export/trust certs, start sync). Returns the `Arc<Core>` plus the
/// startup log.
///
/// This is exactly the sequence the binary runs at startup, factored out so the
/// integration test can drive it against a spawned relay without a TTY.
///
/// `root` stays an explicit argument rather than being resolved in here: where
/// a root comes from is [`crate::vault`]'s question, and a two-replica test has
/// to be able to say so. Note that a shared root is no longer *sufficient* to
/// make two vaults one account — `plan.adopt_pairing` is what does that.
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
    let mut preamble = Vec::new();
    // Read before opening: the identity a vault belongs to is decided when it
    // is created, so a payload that arrives later cannot be adopted at all.
    // A missing or malformed file is logged and the vault opens as its own
    // account, which is what an operator who forgot the export step wants to
    // see rather than a failed startup.
    let paired = plan.adopt_pairing.as_ref().and_then(|path| {
        match std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|b| sunrise_pairing::decode_pairing_payload(&b).map_err(|e| e.to_string()))
        {
            Ok(payload) => {
                tracing::info!(
                    ev = "ui.pair.payload_adopted",
                    result = "ok",
                    "pairing payload adopted"
                );
                preamble.push(format!("adopted pairing payload <- {}", path.display()));
                Some(Box::new(payload))
            }
            Err(cause) => {
                tracing::warn!(
                    ev = "ui.pair.payload_adopted",
                    result = "skipped",
                    err_code = "IO_READ_FAILED",
                    err_kind = "permanent",
                    retryable = false,
                    cause,
                    "pairing payload not adopted"
                );
                preamble.push(format!(
                    "pairing payload not read ({}): {cause}",
                    path.display()
                ));
                None
            }
        }
    });
    let core = Arc::new(
        Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(root),
                paired,
            },
        )
        .await?,
    );
    let mut log = preamble;
    log.extend(apply_plan(&core, plan));
    Ok((core, log))
}

/// Write `bytes` to `path` owner-only, with the mode set **at creation**.
///
/// The pairing payload is the whole account in the clear — `ID_S_priv`,
/// `ID_D_priv`, the vault root and every Stream key this device holds — so it
/// is strictly more sensitive than the vault root `vault.rs` already refuses to
/// write at the process umask. `std::fs::write` would have created it
/// world-readable on a permissive umask, and even a chmod afterwards leaves a
/// window in which it is not. Same rule and same reason as
/// `crate::vault::write_private` and `sunrise_auth::FileStore`.
#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    // No mode bits to set; a Windows client should not be using the file
    // stand-in at all.
    std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_off_with_no_env() {
        let plan = plan_from_env(&SyncEnv::default());
        assert!(plan.is_off());
        assert!(plan.export_pairing.is_none());
        assert!(plan.adopt_pairing.is_none());
    }

    #[test]
    fn a_token_reaches_the_sync_config() {
        let plan = plan_from_env(&SyncEnv {
            url: Some("http://127.0.0.1:8443".into()),
            token: Some("  eyJhbGciOiJSUzI1NiJ9  ".into()),
            stored_token: None,
            ..SyncEnv::default()
        });
        assert_eq!(
            plan.sync.as_ref().unwrap().credential.get().as_deref(),
            Some("eyJhbGciOiJSUzI1NiJ9"),
            "the bearer is trimmed and carried, so the upgrade can present it"
        );
    }

    /// Sync against a self-host relay is still configurable with no token at
    /// all — `NullVerifier` accepts an absent bearer, and requiring one here
    /// would make the single-binary walkthrough impossible.
    #[test]
    fn no_token_is_a_valid_plan() {
        let plan = plan_from_env(&SyncEnv {
            url: Some("http://127.0.0.1:8443".into()),
            ..SyncEnv::default()
        });
        assert!(!plan.sync.as_ref().unwrap().credential.is_set());
        assert!(!plan.is_off());
    }

    /// A blank export must not become a `Some("")` bearer: an empty
    /// `Authorization: Bearer ` header is worse than none, since it reaches the
    /// verifier as a token to reject rather than as an absent one.
    #[test]
    fn a_blank_token_is_treated_as_unset() {
        let plan = plan_from_env(&SyncEnv {
            url: Some("http://127.0.0.1:8443".into()),
            token: Some("   ".into()),
            stored_token: None,
            ..SyncEnv::default()
        });
        assert!(!plan.sync.as_ref().unwrap().credential.is_set());
    }

    #[test]
    fn plan_reads_url_and_pairing_paths_and_trims() {
        let env = SyncEnv {
            url: Some("  http://127.0.0.1:8443 ".into()),
            export_pairing: Some("/tmp/self.cbor".into()),
            adopt_pairing: Some("   ".into()), // whitespace-only -> None
            token: None,
            stored_token: None,
        };
        let plan = plan_from_env(&env);
        assert_eq!(
            plan.sync.as_ref().map(|s| s.url.as_str()),
            Some("http://127.0.0.1:8443")
        );
        assert_eq!(plan.export_pairing, Some(PathBuf::from("/tmp/self.cbor")));
        assert_eq!(plan.adopt_pairing, None);
        assert!(!plan.is_off());
    }

    #[test]
    fn relay_host_keeps_only_the_authority() {
        assert_eq!(relay_host("http://127.0.0.1:8443"), "127.0.0.1:8443");
        assert_eq!(
            relay_host("wss://relay.example.com/sync"),
            "relay.example.com"
        );
        assert_eq!(
            relay_host("relay.example.com:9000"),
            "relay.example.com:9000"
        );
    }

    #[test]
    fn relay_host_drops_credentials_and_query() {
        assert_eq!(
            relay_host("wss://user:hunter2@relay.example/sync?access_token=SECRET"),
            "relay.example"
        );
        assert_eq!(relay_host("http://h#frag"), "h");
    }

    #[test]
    fn relay_host_falls_back_rather_than_echoing_garbage() {
        assert_eq!(relay_host(""), "unknown");
        assert_eq!(relay_host("http:///"), "unknown");
    }

    #[test]
    fn blank_url_stays_off() {
        let env = SyncEnv {
            url: Some(String::new()),
            ..SyncEnv::default()
        };
        assert!(plan_from_env(&env).is_off());
    }

    /// A stored login is enough to sync — the whole point of `sunrise login`.
    #[test]
    fn a_stored_login_supplies_the_bearer() {
        let dir = tempfile::tempdir().unwrap();
        let store = sunrise_auth::FileStore::in_dir(dir.path());
        store
            .save(&sunrise_auth::Credentials::new(
                "stored-access".into(),
                None,
                Some(3600),
                0,
            ))
            .unwrap();
        let env = SyncEnv {
            url: Some("wss://relay.example/sync".into()),
            export_pairing: None,
            adopt_pairing: None,
            token: None,
            stored_token: None,
        }
        .with_stored(&store, 0);
        let plan = plan_from_env(&env);
        assert_eq!(
            plan.sync.unwrap().credential.get().as_deref(),
            Some("stored-access")
        );
    }

    /// The env var still wins, for CI and for a token minted elsewhere.
    #[test]
    fn the_env_token_overrides_a_stored_login() {
        let dir = tempfile::tempdir().unwrap();
        let store = sunrise_auth::FileStore::in_dir(dir.path());
        store
            .save(&sunrise_auth::Credentials::new(
                "stored-access".into(),
                None,
                Some(3600),
                0,
            ))
            .unwrap();
        let env = SyncEnv {
            url: Some("wss://relay.example/sync".into()),
            export_pairing: None,
            adopt_pairing: None,
            token: Some("from-env".into()),
            stored_token: None,
        }
        .with_stored(&store, 0);
        assert_eq!(
            plan_from_env(&env)
                .sync
                .unwrap()
                .credential
                .get()
                .as_deref(),
            Some("from-env")
        );
    }

    /// An expired stored token is not offered. Presenting it earns a `401` and
    /// a reconnect loop; offering nothing at least reaches a self-host relay.
    #[test]
    fn an_expired_stored_login_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let store = sunrise_auth::FileStore::in_dir(dir.path());
        store
            .save(&sunrise_auth::Credentials::new(
                "stale".into(),
                None,
                Some(1),
                0,
            ))
            .unwrap();
        let env = SyncEnv {
            url: Some("wss://relay.example/sync".into()),
            export_pairing: None,
            adopt_pairing: None,
            token: None,
            stored_token: None,
        }
        .with_stored(&store, 10_000);
        assert_eq!(
            plan_from_env(&env).sync.unwrap().credential.get(),
            None,
            "an expired token must not be presented"
        );
    }
}
