//! The web client's seam over `sunrise-core` (ADR-0055).
//!
//! Four calls, JSON in and JSON out, the shape ADR-0012 named for this crate:
//!
//! | JS (`wasm-bindgen`)                  | Rust                        |
//! |--------------------------------------|-----------------------------|
//! | `openVault(dir, root, app, tz)`      | [`WebCore::open`]           |
//! | `core.submitJson(commandJson)`       | [`WebCore::submit_json`]    |
//! | `core.queryJson(queryJson)`          | [`WebCore::query_json`]     |
//! | `core.closeVault()`                  | [`WebCore::close`]          |
//!
//! A command is [`sunrise_core::Command`] and a query [`sunrise_core::Query`]
//! in `serde_json`'s externally tagged form — `{"CreateTask":{…}}`, `"Inbox"` —
//! so this crate adds no second vocabulary for the web to drift against. What
//! comes back is the serialized [`sunrise_core::CommandResult`] or
//! [`sunrise_core::QueryResult`].
//!
//! # Two builds, one dispatch layer
//!
//! Everything outside [`web`] is target-independent and is what the tests
//! below drive, natively, against real `SQLCipher`. [`web`] exists only on
//! `wasm32-unknown-unknown` and adds the three things a browser needs and a
//! test does not: the OPFS `SyncAccessHandle` pool VFS, a clock read from
//! `Date.now()`, and the `wasm-bindgen` exports.
//!
//! # What the web vault is not
//!
//! On wasm, `rusqlite` binds `sqlite-wasm-rs`, which is `SQLite` without
//! `SQLCipher`: the database in OPFS is **plaintext**, and the vault root the
//! caller hands over keys nothing at rest. Only one tab may hold the vault; the
//! worker enforces that with `navigator.locks` before it calls
//! [`WebCore::open`], because the process-wide file lock native clients take
//! has no filesystem to stand on here. Both are ADR-0055's accepted gaps.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use sunrise_core::{Clock, Command, Core, CoreConfig, CoreError, Query, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use thiserror::Error;

/// The browser build: OPFS, `Date.now()`, and the `wasm-bindgen` exports.
///
/// Must run in a **dedicated worker**: the `SyncAccessHandle` pool VFS needs
/// `FileSystemSyncAccessHandle`, which no other context has.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub mod web {
    use super::{HostClock, WebCore};
    use std::path::PathBuf;
    use std::sync::Arc;
    use wasm_bindgen::prelude::*;

    /// The OPFS directory the pool VFS keeps its files in. Named for the
    /// product rather than left at the library's `.opfs-sahpool`, so another
    /// `SQLite` user on the same origin cannot collide with it.
    const POOL_DIRECTORY: &str = ".sunrise-sahpool";

    fn js_now() -> u64 {
        // `Date.now()` is integral milliseconds since the epoch; a negative or
        // non-finite reading (a host clock set before 1970) clamps to 0.
        let now = js_sys::Date::now();
        if now.is_finite() && now > 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ms = now as u64;
            ms
        } else {
            0
        }
    }

    fn js_err(e: impl std::fmt::Display) -> JsError {
        JsError::new(&e.to_string())
    }

    /// An open vault, as JS holds it.
    #[wasm_bindgen]
    #[derive(Debug)]
    pub struct WasmCore {
        inner: Arc<WebCore>,
    }

    /// Register the OPFS pool VFS as `SQLite`'s default, then open (or create)
    /// the vault at `vault_dir` inside it.
    ///
    /// `root` is the 32-byte vault root, `app` the `<semver>+web` string, and
    /// `timezone` the IANA zone `Intl.DateTimeFormat().resolvedOptions()`
    /// reports. Rejects outside a dedicated worker.
    #[wasm_bindgen(js_name = openVault)]
    pub async fn open_vault(
        vault_dir: String,
        root: Vec<u8>,
        app: String,
        timezone: String,
    ) -> Result<WasmCore, JsError> {
        let cfg = sqlite_wasm_vfs::sahpool::OpfsSAHPoolCfgBuilder::new()
            .directory(POOL_DIRECTORY)
            .build();
        sqlite_wasm_vfs::sahpool::install::<sqlite_wasm_rs::WasmOsCallback>(&cfg, true)
            .await
            .map_err(js_err)?;
        let core = WebCore::open(
            PathBuf::from(vault_dir),
            &root,
            &app,
            HostClock::new(js_now, timezone),
        )
        .await
        .map_err(js_err)?;
        Ok(WasmCore {
            inner: Arc::new(core),
        })
    }

    #[wasm_bindgen]
    impl WasmCore {
        /// Apply one command (JSON); resolves to its result (JSON).
        #[wasm_bindgen(js_name = submitJson)]
        pub fn submit_json(&self, command: String) -> js_sys::Promise {
            let inner = Arc::clone(&self.inner);
            wasm_bindgen_futures::future_to_promise(async move {
                inner
                    .submit_json(&command)
                    .await
                    .map(JsValue::from)
                    .map_err(|e| js_err(e).into())
            })
        }

        /// Run one query (JSON); resolves to its result (JSON).
        #[wasm_bindgen(js_name = queryJson)]
        pub fn query_json(&self, query: String) -> js_sys::Promise {
            let inner = Arc::clone(&self.inner);
            wasm_bindgen_futures::future_to_promise(async move {
                inner
                    .query_json(&query)
                    .await
                    .map(JsValue::from)
                    .map_err(|e| js_err(e).into())
            })
        }

        /// Close the vault; every later call rejects.
        #[wasm_bindgen(js_name = closeVault)]
        pub fn close_vault(&self) -> js_sys::Promise {
            let inner = Arc::clone(&self.inner);
            wasm_bindgen_futures::future_to_promise(async move {
                inner
                    .close()
                    .await
                    .map(|()| JsValue::UNDEFINED)
                    .map_err(|e| js_err(e).into())
            })
        }
    }
}

/// Why a web-core call failed. Crosses to JS as the message of a thrown
/// `Error`, so each arm's text is what a console shows.
#[derive(Debug, Error)]
pub enum WebCoreError {
    /// The vault root was not 32 bytes.
    #[error("vault root must be 32 bytes, got {len}")]
    BadVaultRoot {
        /// The length the caller passed.
        len: usize,
    },
    /// The request was not a well-formed command or query.
    #[error("malformed request: {0}")]
    Request(serde_json::Error),
    /// A result would not serialize. Not reachable for any result the core
    /// returns today; kept typed rather than unwrapped.
    #[error("unserializable result: {0}")]
    Response(serde_json::Error),
    /// The core refused or failed the call.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The vault has been closed.
    #[error("vault is closed")]
    Closed,
    /// `close` was called while another call still held the vault.
    #[error("vault is busy; close it once in-flight calls have returned")]
    Busy,
}

/// A wall clock read from the host, with the host's zone fixed at open.
///
/// The core never reads an ambient clock (`clippy.toml`'s determinism rules);
/// this is the seam it is handed instead. `now` is a plain function so the
/// browser build can pass `Date.now()` and a test can pass a constant.
#[derive(Debug, Clone)]
pub struct HostClock {
    now: fn() -> u64,
    timezone: String,
}

impl HostClock {
    /// A clock reading `now` and reporting `timezone`, an IANA name. A name the
    /// core's bundled tzdb does not know evaluates as UTC there, the fallback
    /// every native clock gets too.
    #[must_use]
    pub fn new(now: fn() -> u64, timezone: impl Into<String>) -> Self {
        Self {
            now,
            timezone: timezone.into(),
        }
    }
}

impl Clock for HostClock {
    fn now_ms(&self) -> u64 {
        (self.now)()
    }

    fn timezone(&self) -> String {
        self.timezone.clone()
    }
}

/// One open vault.
///
/// `close` has to consume the [`Core`] while `submit_json` and `query_json`
/// only borrow it, and a `wasm-bindgen` future must own what it touches. So
/// the core sits behind an `Arc` that each call clones out of the slot before
/// awaiting, and `close` takes the slot empty and needs the last reference.
#[derive(Debug)]
pub struct WebCore {
    core: Mutex<Option<Arc<Core>>>,
}

impl WebCore {
    /// Open (or create) the vault under `vault_dir`, keyed by the 32-byte
    /// `root`.
    ///
    /// The root always opens as [`Unlock::DevicePaired`] with no pairing
    /// material: the web client has no pairing or recovery flow yet, so every
    /// web vault is its own account.
    pub async fn open(
        vault_dir: PathBuf,
        root: &[u8],
        app: &str,
        clock: HostClock,
    ) -> Result<Self, WebCoreError> {
        let bytes: [u8; 32] = root
            .try_into()
            .map_err(|_| WebCoreError::BadVaultRoot { len: root.len() })?;
        let cfg = CoreConfig::with_clock(vault_dir, app, Arc::new(clock), Arc::new(SystemRng));
        let core = Core::open(
            cfg,
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(bytes),
                paired: None,
            },
        )
        .await?;
        Ok(Self {
            core: Mutex::new(Some(Arc::new(core))),
        })
    }

    fn live(&self) -> Result<Arc<Core>, WebCoreError> {
        self.core
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(WebCoreError::Closed)
    }

    /// Apply one [`Command`], given as JSON; returns the
    /// [`sunrise_core::CommandResult`] as JSON.
    pub async fn submit_json(&self, command: &str) -> Result<String, WebCoreError> {
        let cmd: Command = serde_json::from_str(command).map_err(WebCoreError::Request)?;
        let res = self.live()?.submit(cmd).await?;
        serde_json::to_string(&res).map_err(WebCoreError::Response)
    }

    /// Run one [`Query`], given as JSON; returns the
    /// [`sunrise_core::QueryResult`] as JSON.
    pub async fn query_json(&self, query: &str) -> Result<String, WebCoreError> {
        let q: Query = serde_json::from_str(query).map_err(WebCoreError::Request)?;
        let res = self.live()?.query(q).await?;
        serde_json::to_string(&res).map_err(WebCoreError::Response)
    }

    /// Close the vault. Every later call returns [`WebCoreError::Closed`];
    /// closing twice is not an error.
    ///
    /// Refuses with [`WebCoreError::Busy`] while another call still holds the
    /// core, and leaves it open, rather than dropping a vault mid-write.
    pub async fn close(&self) -> Result<(), WebCoreError> {
        let taken = self
            .core
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(shared) = taken else {
            return Ok(());
        };
        match Arc::try_unwrap(shared) {
            Ok(core) => Ok(core.close().await?),
            Err(shared) => {
                *self.core.lock().unwrap_or_else(PoisonError::into_inner) = Some(shared);
                Err(WebCoreError::Busy)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-01-01T00:00:00Z. Fixed so no outcome depends on the host clock.
    fn fixed_now() -> u64 {
        1_767_225_600_000
    }

    fn clock() -> HostClock {
        HostClock::new(fixed_now, "America/Toronto")
    }

    async fn open(dir: &std::path::Path) -> WebCore {
        WebCore::open(dir.to_path_buf(), &[7; 32], "0.1.0+test", clock())
            .await
            .expect("open")
    }

    #[test]
    fn the_host_clock_reports_what_the_host_gave_it() {
        assert_eq!(clock().timezone(), "America/Toronto");
        assert_eq!(clock().now_ms(), fixed_now());
    }

    #[tokio::test]
    async fn a_root_that_is_not_32_bytes_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = WebCore::open(dir.path().to_path_buf(), &[7; 31], "0.1.0+test", clock())
            .await
            .unwrap_err();
        assert!(
            matches!(err, WebCoreError::BadVaultRoot { len: 31 }),
            "{err}"
        );
    }

    /// The round trip the web client makes: create a task in JSON, read it
    /// back from the Inbox in JSON, complete it, and see the state change.
    #[tokio::test]
    async fn json_commands_and_queries_round_trip_through_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let core = open(dir.path()).await;

        let created: serde_json::Value = serde_json::from_str(
            &core
                .submit_json(r#"{"CreateTask":{"title":"Renew passport","contexts":[]}}"#)
                .await
                .unwrap(),
        )
        .unwrap();
        let id = created["entity"].as_str().expect("entity id is a string");
        assert!(id.starts_with("tsk_"), "{id}");

        let inbox: serde_json::Value =
            serde_json::from_str(&core.query_json(r#""Inbox""#).await.unwrap()).unwrap();
        let tasks = inbox["StreamTasks"]
            .as_array()
            .expect("Inbox is StreamTasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["id"], id);
        assert_eq!(tasks[0]["title"], "Renew passport");
        assert_eq!(tasks[0]["state"], "todo");

        // Today answers `Tasks`, not `StreamTasks`; `apps/web/src/wasm.ts`
        // reads both.
        let today: serde_json::Value = serde_json::from_str(
            &core
                .query_json(&format!(
                    r#"{{"Today":{{"now_ms":{},"contexts":[]}}}}"#,
                    fixed_now()
                ))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(today["Tasks"].is_array(), "{today}");

        core.submit_json(&format!(r#"{{"CompleteTask":"{id}"}}"#))
            .await
            .unwrap();
        let task: serde_json::Value = serde_json::from_str(
            &core
                .query_json(&format!(r#"{{"EntityById":"{id}"}}"#))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(task["Task"]["state"], "done");
    }

    #[tokio::test]
    async fn malformed_json_is_a_request_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let core = open(dir.path()).await;
        for bad in ["", "{", r#"{"NoSuchCommand":{}}"#, r#""Inbox""#] {
            let err = core.submit_json(bad).await.unwrap_err();
            assert!(matches!(err, WebCoreError::Request(_)), "{bad}: {err}");
        }
        let err = core.query_json(r#"{"CreateTask":{}}"#).await.unwrap_err();
        assert!(matches!(err, WebCoreError::Request(_)), "{err}");
    }

    #[tokio::test]
    async fn a_closed_vault_refuses_every_call_and_closes_twice() {
        let dir = tempfile::tempdir().unwrap();
        let core = open(dir.path()).await;
        core.close().await.unwrap();
        assert!(matches!(
            core.query_json(r#""Inbox""#).await.unwrap_err(),
            WebCoreError::Closed
        ));
        assert!(matches!(
            core.submit_json(r#"{"CreateTask":{"title":"x","contexts":[]}}"#)
                .await
                .unwrap_err(),
            WebCoreError::Closed
        ));
        core.close().await.unwrap();
    }

    /// Closing releases the vault: the same directory opens again, and what
    /// was written before is still there.
    #[tokio::test]
    async fn close_releases_the_vault_for_the_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let core = open(dir.path()).await;
        core.submit_json(r#"{"CreateTask":{"title":"Kept","contexts":[]}}"#)
            .await
            .unwrap();
        core.close().await.unwrap();

        let reopened = open(dir.path()).await;
        let inbox: serde_json::Value =
            serde_json::from_str(&reopened.query_json(r#""Inbox""#).await.unwrap()).unwrap();
        assert_eq!(inbox["StreamTasks"][0]["title"], "Kept");
    }

    #[tokio::test]
    async fn close_refuses_while_a_call_holds_the_core() {
        let dir = tempfile::tempdir().unwrap();
        let core = open(dir.path()).await;
        let held = core.live().unwrap();
        assert!(matches!(
            core.close().await.unwrap_err(),
            WebCoreError::Busy
        ));
        // Still open, and closes once the holder lets go.
        core.query_json(r#""Inbox""#).await.unwrap();
        drop(held);
        core.close().await.unwrap();
    }
}
