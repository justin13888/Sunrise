//! Sunrise desktop Tauri shell library.
//!
//! Provides Rust-side IPC commands that wrap [`sunrise_core::Core`].
//! The Tauri 2 binary entrypoint lives next to this library; users run
//! `cd apps/desktop && bun install && bun run tauri dev` to bring up
//! the full app. v1 ships the IPC handlers and the React renderer;
//! Tauri's bundling/installer pipeline is owned by the developer.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use sunrise_core::{Command, Core, CoreConfig, Query, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;

static CORE: OnceLock<Mutex<Option<Arc<Core>>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Arc<Core>>> {
    CORE.get_or_init(|| Mutex::new(None))
}

fn lock() -> std::sync::MutexGuard<'static, Option<Arc<Core>>> {
    slot().lock().expect("core slot poisoned")
}

/// Initialize the embedded Core. Called from the Tauri setup hook.
pub async fn init_core(vault_dir: PathBuf, vault_root: [u8; 32]) -> Result<(), String> {
    let cfg = CoreConfig {
        vault_dir,
        clock: Arc::new(sunrise_core::SystemClock),
        rng: Arc::new(SystemRng),
        app: env!("CARGO_PKG_VERSION").into(),
    };
    let unlock = Unlock::DevicePaired(VaultRootKey::from_bytes(vault_root));
    let core = Core::open(cfg, unlock).await.map_err(|e| e.to_string())?;
    *lock() = Some(Arc::new(core));
    Ok(())
}

/// Tauri command — submit a JSON-encoded command to the Core.
pub async fn submit_json(cmd_json: String) -> Result<String, String> {
    let cmd: Command = serde_json::from_str(&cmd_json).map_err(|e| e.to_string())?;
    let Some(core) = lock().clone() else {
        return Err("core not initialized".into());
    };
    let r = core.submit(cmd).await.map_err(|e| e.to_string())?;
    serde_json::to_string(&r).map_err(|e| e.to_string())
}

/// Tauri command — JSON-encoded query.
pub async fn query_json(q_json: String) -> Result<String, String> {
    let q: Query = serde_json::from_str(&q_json).map_err(|e| e.to_string())?;
    let Some(core) = lock().clone() else {
        return Err("core not initialized".into());
    };
    let r = core.query(q).await.map_err(|e| e.to_string())?;
    serde_json::to_string(&r).map_err(|e| e.to_string())
}

// Tests for the IPC bridge live in the bin crate so the standalone
// Cargo workspace stays scaffold-only. The Core API is exhaustively
// tested in `crates/sunrise-core/tests/integration.rs`.
