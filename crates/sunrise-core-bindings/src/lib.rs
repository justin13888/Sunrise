//! The UniFFI seam over `sunrise-core`.
//!
//! One opaque handle ([`SunriseCore`]), one command type, one query type, one
//! result type, and a change stream. The generated Swift is what the macOS app
//! talks to; the same scaffolding generates Kotlin when Android arrives
//! (ADR-0019).
//!
//! # Shape
//!
//! * [`SunriseCore::open`] is an **async constructor**, so the tokio runtime
//!   UniFFI supplies is live for everything the handle later does.
//! * [`SunriseCore::submit`] and [`SunriseCore::query`] are async and return
//!   `Result`, which reaches Swift as `async throws`.
//! * The domain vocabulary crosses as [`crate::dto`] records and [`crate::types`]
//!   enums. `sunrise-domain` gains **no uniffi dependency**: the enums are
//!   declared with `#[uniffi::remote]`, and the structured types are mirrored.
//! * [`ChangeListener`] is a foreign-implemented trait — the way a
//!   `broadcast::Receiver` becomes something Swift can hold.
//!
//! # Three things that are load-bearing, not stylistic
//!
//! 1. **There is no process-global runtime and no `block_on`.** UniFFI runs
//!    exported async methods on its own tokio runtime; `block_on` inside that
//!    context deadlocks. The handle captures [`tokio::runtime::Handle`] in the
//!    async constructor and spawns through it.
//! 2. **`async_runtime = "tokio"` does not put *sync* exported methods on the
//!    runtime.** A sync method runs on the calling foreign thread with no
//!    reactor, so a bare `tokio::spawn` in one compiles and then panics —
//!    reaching Swift as a trapped `rustPanic`, i.e. a crash. Every spawn here
//!    goes through the captured handle.
//! 3. **The exported task record is [`dto::TaskItem`], not `Task`.** A Swift
//!    `Task` shadows `_Concurrency.Task` and breaks every `Task { }` in the
//!    app.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    // The split-optional edit records are wide by construction: each clearable
    // field costs one `bool`. See `dto::TaskEdit`.
    clippy::struct_excessive_bools
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sunrise_core::{Core, CoreConfig, CoreError, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use thiserror::Error;
use tokio::runtime::Handle;
use tokio::sync::broadcast::error::RecvError;

pub mod command;
pub mod dto;
pub mod query;
pub mod types;

pub use command::CoreCommand;
pub use dto::{CommandOutcome, TaskItem};
pub use query::{CoreQuery, CoreQueryResult};

uniffi::setup_scaffolding!();

/// Everything that can go wrong across the seam.
///
/// `#[uniffi(flat_error)]` because [`sunrise_core::CoreError`] is a tree of
/// `#[error(transparent)]` wrappers around six other crates' error types, and
/// UniFFI cannot model that structurally. Flattening keeps the *variant* —
/// which is what a client branches on — and carries the detail as the message,
/// which is what a client shows.
#[derive(Debug, Error, uniffi::Error)]
#[uniffi(flat_error)]
#[non_exhaustive]
pub enum BindingError {
    /// The vault could not be opened, read or written.
    #[error("core: {0}")]
    Core(String),
    /// An id string was not a valid `EntityRef`.
    #[error("not an id: {id} ({cause})")]
    BadId {
        /// What was passed.
        id: String,
        /// Why it was rejected.
        cause: String,
    },
    /// A timestamp, date or time was not readable.
    #[error("not a time: {value} ({cause})")]
    BadTime {
        /// What was passed.
        value: String,
        /// Why it was rejected.
        cause: String,
    },
    /// A vault root key was not 32 bytes.
    #[error("vault root must be 32 bytes, got {len}")]
    BadVaultRoot {
        /// How many bytes arrived.
        len: u32,
    },
    /// A fixed-width field arrived at the wrong length, or was not hex.
    ///
    /// UniFFI cannot express `[u8; 32]`, so a key crosses as a `Vec<u8>` and a
    /// digest as a hex string. Their width is a contract the seam has to check
    /// somewhere, and here is the last place it can be checked cheaply.
    #[error("{field} must be {expected} bytes")]
    BadFixedBytes {
        /// Which field.
        field: String,
        /// How many bytes it must carry.
        expected: u32,
    },
}

impl From<CoreError> for BindingError {
    fn from(e: CoreError) -> Self {
        Self::Core(e.to_string())
    }
}

/// A live vault.
///
/// Opaque on the foreign side: it is an `Arc`-held object with methods, not a
/// record. Everything that touches storage goes through here.
#[derive(uniffi::Object)]
pub struct SunriseCore {
    inner: Arc<Core>,
    /// The tokio runtime UniFFI is driving this object's async methods on,
    /// captured in the async constructor.
    ///
    /// Load-bearing: [`SunriseCore::subscribe_changes`] and
    /// [`SunriseCore::start_sync`] are **sync** exported methods, and a sync
    /// exported method runs on the calling foreign thread with no reactor
    /// installed. A bare `tokio::spawn` there panics at runtime. Spawning
    /// through a captured handle works from any thread.
    rt: Handle,
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Open (or create) the vault at `vault_dir`, keyed by a 32-byte root.
    ///
    /// The root comes from the platform keychain or a completed pairing; this
    /// seam does not derive it.
    #[uniffi::constructor]
    pub async fn open(
        vault_dir: String,
        vault_root: Vec<u8>,
        app_version: String,
    ) -> Result<Arc<Self>, BindingError> {
        let root: [u8; 32] =
            vault_root
                .as_slice()
                .try_into()
                .map_err(|_| BindingError::BadVaultRoot {
                    len: u32::try_from(vault_root.len()).unwrap_or(u32::MAX),
                })?;
        let cfg = CoreConfig::production(PathBuf::from(vault_dir), app_version);
        let core = Core::open(cfg, Unlock::DevicePaired(VaultRootKey::from_bytes(root))).await?;
        Ok(Arc::new(Self {
            inner: Arc::new(core),
            // Inside an async exported method, so a runtime is definitely
            // installed. This is the only place that is guaranteed.
            rt: Handle::current(),
        }))
    }

    /// Submit one mutating command.
    pub async fn submit(&self, cmd: CoreCommand) -> Result<CommandOutcome, BindingError> {
        let res = self.inner.submit(cmd.into_core()?).await?;
        Ok(CommandOutcome::from(&res))
    }

    /// Run one read query.
    pub async fn query(&self, q: CoreQuery) -> Result<CoreQueryResult, BindingError> {
        let res = self.inner.query(q.into_core()).await?;
        Ok(CoreQueryResult::from_core(res))
    }

    /// Parse one capture line (`#stream @context ^when !priority ~duration`)
    /// and commit it, in the caller's timezone.
    ///
    /// `tz` is an IANA zone name; an unknown one falls back to UTC rather than
    /// refusing the capture, because losing what someone typed is the worse
    /// failure.
    pub async fn capture(&self, text: String, tz: String) -> Result<CommandOutcome, BindingError> {
        let zone = jiff::tz::TimeZone::get(&tz).unwrap_or(jiff::tz::TimeZone::UTC);
        let parsed = self.inner.capture(&text, &zone).await?;
        let res = self
            .inner
            .submit(sunrise_core::Command::CreateTask(parsed.draft))
            .await?;
        Ok(CommandOutcome::from(&res))
    }

    /// Stop the sync driver and release the vault lock. Idempotent.
    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    /// The core's clock, in epoch milliseconds. Every `now_ms` a query wants
    /// should come from here so one reading drives the whole screen.
    pub fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }

    /// This device's certificate (canonical CBOR), for pairing.
    pub fn device_cert(&self) -> Vec<u8> {
        self.inner.device_cert()
    }

    /// Start the live-sync driver against `url` (a relay `/sync` WebSocket).
    ///
    /// Deliberately sync: it only spawns. See the note on [`SunriseCore::rt`]
    /// for why the spawn cannot use `tokio::spawn`.
    pub fn start_sync(&self, url: String) -> Result<(), BindingError> {
        let _guard = self.rt.enter();
        self.inner.start_sync(ws_factory(&url))?;
        Ok(())
    }

    /// Start the periodic routine-materialization timer.
    pub fn start_routine_timer(&self, interval_ms: u64) -> Result<(), BindingError> {
        let _guard = self.rt.enter();
        self.inner
            .start_routine_timer(std::time::Duration::from_millis(interval_ms))?;
        Ok(())
    }

    /// Subscribe to domain changes. Cancel by dropping or cancelling the
    /// returned [`Subscription`].
    pub fn subscribe_changes(&self, listener: Arc<dyn ChangeListener>) -> Arc<Subscription> {
        let mut rx = self.inner.changes();
        let handle = self.rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => listener.on_change(ChangeEvent::from(&ev)),
                    Err(RecvError::Lagged(n)) => listener.on_lagged(n),
                    Err(RecvError::Closed) => {
                        listener.on_closed();
                        break;
                    }
                }
            }
        });
        Arc::new(Subscription {
            handle: Mutex::new(Some(handle)),
        })
    }
}

/// Build the transport factory the sync driver dials with, once per connection
/// attempt (initial connect and every reconnect).
fn ws_factory(url: &str) -> sunrise_core::TransportFactory {
    let url = url.to_string();
    Arc::new(move || {
        let url = url.clone();
        Box::pin(async move {
            let t = sunrise_sync::WsTransport::connect(&url).await?;
            Ok(Box::new(t) as sunrise_core::BoxTransport)
        }) as sunrise_core::ConnectFuture
    })
}

/// One change to the local vault, addressed by entity.
///
/// Deliberately carries no payload beyond the id: a change notification is a
/// prompt to re-read, not a substitute for reading. Sending the entity would
/// make the stream a second, competing source of truth that can arrive out of
/// order with respect to the queries beside it.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ChangeEvent {
    /// An entity was created.
    Created {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity was updated.
    Updated {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity was tombstoned.
    Deleted {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity's tombstone was compacted away.
    Forgotten {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
}

impl From<&sunrise_core::DomainEvent> for ChangeEvent {
    fn from(e: &sunrise_core::DomainEvent) -> Self {
        match *e {
            sunrise_core::DomainEvent::Created(entity) => Self::Created { entity },
            sunrise_core::DomainEvent::Updated(entity) => Self::Updated { entity },
            sunrise_core::DomainEvent::Deleted(entity) => Self::Deleted { entity },
            sunrise_core::DomainEvent::Forgotten(entity) => Self::Forgotten { entity },
        }
    }
}

/// The foreign side of the change stream.
///
/// # `on_lagged` is not optional
///
/// The channel behind this is a `tokio::sync::broadcast` with a bounded
/// buffer, and it is **lossy under load**. A spike measured `seen = 257,
/// lagged = 4743` pushing 5000 events past a slow consumer — which is exactly
/// what a sync catch-up burst looks like. A listener that implements only
/// [`ChangeListener::on_change`] will therefore show stale data after any
/// burst, silently.
///
/// Treat `on_lagged` as **"re-run every query you are showing"**. It is not
/// telemetry.
#[uniffi::export(with_foreign)]
pub trait ChangeListener: Send + Sync + 'static {
    /// One change arrived. Called from a runtime worker thread, never the
    /// foreign main thread.
    fn on_change(&self, event: ChangeEvent);
    /// `skipped` events were dropped because this listener fell behind.
    /// Re-read; do not try to reconstruct what was missed.
    fn on_lagged(&self, skipped: u64);
    /// The vault closed. No further calls will arrive.
    fn on_closed(&self);
}

/// Cancel handle for [`SunriseCore::subscribe_changes`].
#[derive(uniffi::Object)]
pub struct Subscription {
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[uniffi::export]
impl Subscription {
    /// Stop delivering. Idempotent, and safe to call from any thread.
    ///
    /// Deliberately **sync**: Swift's `AsyncStream.Continuation.onTermination`
    /// is a non-async closure and cannot await, and an async cancel forces a
    /// detached teardown that races with deallocation. `JoinHandle::abort`
    /// needs no reactor, so a sync method is safe here.
    pub fn cancel(&self) {
        if let Ok(mut g) = self.handle.lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for SunriseCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SunriseCore").finish_non_exhaustive()
    }
}
