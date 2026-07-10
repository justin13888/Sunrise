//! FFI-friendly facade over `sunrise-core`.
//!
//! Consumed by iOS (via UniFFI → `sunrise-core.xcframework`) and
//! Android (UniFFI → `.aar`). v1 ships a minimal, stable C-ABI surface:
//!
//! - `sunrise_open`, `sunrise_close` for vault lifecycle.
//! - `sunrise_submit_json`, `sunrise_query_json` accept/return JSON strings
//!   so we don't need to translate every `Command`/`Query` shape across
//!   the FFI boundary individually.
//!
//! UniFFI annotations are deferred to a follow-up; the v1 surface here
//! gives platform engineers concrete shapes to bind against.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::needless_pass_by_value,
    clippy::map_unwrap_or
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use sunrise_core::{Command, Core, CoreConfig, Query, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use thiserror::Error;
use tokio::runtime::Runtime;

/// FFI-friendly error.
#[derive(Debug, Error)]
pub enum BindingError {
    /// `sunrise-core` returned an error.
    #[error("core: {0}")]
    Core(String),
    /// JSON encode/decode failed.
    #[error("json: {0}")]
    Json(String),
    /// FFI handle was invalid.
    #[error("invalid handle")]
    InvalidHandle,
    /// Vault was not opened.
    #[error("vault not open")]
    NotOpen,
}

/// Borrow-then-call wrapper for the singleton Core handle. The platform
/// layer expects a single Core per process; we manage that singleton
/// here so the FFI surface stays simple.
type CoreSlot = Arc<Mutex<Option<Arc<Core>>>>;

fn slot() -> &'static CoreSlot {
    static S: OnceLock<CoreSlot> = OnceLock::new();
    S.get_or_init(|| Arc::new(Mutex::new(None)))
}

fn rt() -> &'static Runtime {
    static R: OnceLock<Runtime> = OnceLock::new();
    R.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

/// Open (or create) the vault at `vault_dir` keyed by a 32-byte vault root.
///
/// Returns the empty string on success, or an error message on failure.
/// (We use string-on-error rather than a typed enum so platform bindings
/// don't have to model the variant tree on day one.)
pub fn sunrise_open(vault_dir: String, vault_root_hex: String, app_v: String) -> String {
    let vault_root = match decode_hex32(&vault_root_hex) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let cfg = CoreConfig {
        vault_dir: PathBuf::from(vault_dir),
        clock: Arc::new(sunrise_core::SystemClock),
        rng: Arc::new(SystemRng),
        app: app_v,
        sync: None,
    };
    let unlock = Unlock::DevicePaired(VaultRootKey::from_bytes(vault_root));
    let res = rt().block_on(Core::open(cfg, unlock));
    match res {
        Ok(c) => {
            let mut guard = slot().lock().expect("slot poison");
            *guard = Some(Arc::new(c));
            String::new()
        }
        Err(e) => format!("core: {e}"),
    }
}

/// Close the vault. Returns an empty string on success.
pub fn sunrise_close() -> String {
    let mut guard = slot().lock().expect("slot poison");
    let Some(core) = guard.take() else {
        return String::new();
    };
    drop(guard);
    let core = match Arc::try_unwrap(core) {
        Ok(c) => c,
        Err(_arc) => return "core still has outstanding references".into(),
    };
    rt().block_on(async move { core.close().await.map_err(|e| e.to_string()) })
        .err()
        .unwrap_or_default()
}

/// Submit a JSON-encoded command. Returns the JSON-encoded `CommandResult`
/// on success, or an error message prefixed with `"error: "` on failure.
pub fn sunrise_submit_json(cmd_json: String) -> String {
    let cmd: Command = match serde_json::from_str(&cmd_json) {
        Ok(c) => c,
        Err(e) => return format!("error: json: {e}"),
    };
    let core = match get_core() {
        Ok(c) => c,
        Err(e) => return format!("error: {e}"),
    };
    match rt().block_on(core.submit(cmd)) {
        Ok(r) => serde_json::to_string(&r).unwrap_or_else(|e| format!("error: json: {e}")),
        Err(e) => format!("error: core: {e}"),
    }
}

/// Run a JSON-encoded query.
pub fn sunrise_query_json(q_json: String) -> String {
    let q: Query = match serde_json::from_str(&q_json) {
        Ok(c) => c,
        Err(e) => return format!("error: json: {e}"),
    };
    let core = match get_core() {
        Ok(c) => c,
        Err(e) => return format!("error: {e}"),
    };
    match rt().block_on(core.query(q)) {
        Ok(r) => serde_json::to_string(&r).unwrap_or_else(|e| format!("error: json: {e}")),
        Err(e) => format!("error: core: {e}"),
    }
}

fn get_core() -> Result<Arc<Core>, BindingError> {
    let guard = slot().lock().map_err(|_| BindingError::InvalidHandle)?;
    guard.clone().ok_or(BindingError::NotOpen)
}

fn decode_hex32(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!("vault_root_hex must be 64 chars (got {})", s.len()));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("vault_root_hex parse: {e}"))?;
        out[i] = byte;
    }
    Ok(out)
}

/// Avoid unused-warnings on the `CommandResult` / `QueryResult` re-export
/// path; FFI consumers reach these via `sunrise_submit_json` /
/// `sunrise_query_json` JSON shapes, but having the symbols visible here
/// is convenient for downstream bindgen-style tooling.
pub use sunrise_core::{CommandResult as FfiCommandResult, QueryResult as FfiQueryResult};

#[cfg(test)]
mod tests {
    use super::*;

    // The FFI surface manages a single process-global `Core` (see `slot()`),
    // so tests that open or close the vault must not run concurrently.
    static VAULT_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn decode_hex_round_trip() {
        let h = "0".repeat(64);
        let b = decode_hex32(&h).unwrap();
        assert_eq!(b, [0u8; 32]);
    }

    #[test]
    fn decode_hex_rejects_short() {
        assert!(decode_hex32("abc").is_err());
    }

    #[test]
    fn open_then_close_round_trip() {
        let _guard = VAULT_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let key = "ab".repeat(32);
        let err = sunrise_open(
            dir.path().to_string_lossy().into_owned(),
            key,
            "0.1.0+ffi-test".into(),
        );
        assert!(err.is_empty(), "open returned: {err}");
        let close_err = sunrise_close();
        assert!(close_err.is_empty(), "close returned: {close_err}");
    }

    #[test]
    fn submit_query_round_trip() {
        let _guard = VAULT_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let key = "cd".repeat(32);
        let _ = sunrise_open(
            dir.path().to_string_lossy().into_owned(),
            key,
            "0.1.0+ffi-test".into(),
        );
        let cmd = serde_json::json!({"CreateTask": {
            "title": "ffi task",
            "contexts": [],
        }});
        let r = sunrise_submit_json(cmd.to_string());
        assert!(!r.starts_with("error:"), "submit returned: {r}");
        let q = serde_json::json!("Inbox");
        let r = sunrise_query_json(q.to_string());
        assert!(r.contains("ffi task"), "query result: {r}");
        let _ = sunrise_close();
    }

    #[test]
    fn stream_list_and_search_json_round_trip() {
        let _guard = VAULT_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let key = "ef".repeat(32);
        let _ = sunrise_open(
            dir.path().to_string_lossy().into_owned(),
            key,
            "0.1.0+ffi-test".into(),
        );

        let cmd = serde_json::json!({"CreateTask": {
            "title": "searchable widget",
            "contexts": [],
        }});
        let r = sunrise_submit_json(cmd.to_string());
        assert!(!r.starts_with("error:"), "submit returned: {r}");

        // StreamList: externally-tagged unit variant -> bare string.
        let r = sunrise_query_json(serde_json::json!("StreamList").to_string());
        assert!(!r.starts_with("error:"), "stream list: {r}");
        let parsed: serde_json::Value = serde_json::from_str(&r).unwrap();
        let streams = parsed
            .get("Streams")
            .and_then(|v| v.as_array())
            .expect("Streams array");
        assert_eq!(streams[0]["name"], "Inbox");
        assert_eq!(streams[0]["open_task_count"], 1);

        // Search: externally-tagged struct variant.
        let q = serde_json::json!({"Search": {"text": "widget", "limit": 10}});
        let r = sunrise_query_json(q.to_string());
        assert!(!r.starts_with("error:"), "search: {r}");
        assert!(r.contains("searchable widget"), "search result: {r}");

        let _ = sunrise_close();
    }
}
