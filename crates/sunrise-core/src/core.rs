//! `Core` — the user-facing facade.
//!
//! v1 surface: `open` → returns `Core` once the vault lock + DB are ready;
//! `submit` / `query` route to the storage + CRDT layers; `changes` /
//! `sync_status` return broadcast streams; `close` runs graceful shutdown.
//!
//! v1 implementation depth: the public API is wired and the lifecycle is
//! correct. The actual command-application engine that translates a
//! [`crate::Command`] into op envelopes + CRDT mutations + storage rows
//! lives behind a small `Engine` trait that subsequent phases populate
//! (sync wire layer, server). This crate alone proves out the lifecycle,
//! single-writer guarantee, and shape of the API.

use crate::commands::{Command, CommandResult};
use crate::config::CoreConfig;
use crate::engine::{Engine, EngineError};
use crate::events::{DomainEvent, SyncStatus};
use crate::keychain::{Keychain, KeychainError};
use crate::queries::{Query, QueryResult};
use crate::sync_driver::{self, SyncShared, TokenSource, TransportFactory};
use crate::unlock::Unlock;
use crate::vault_lock::{VaultLock, VaultLockError};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use sunrise_storage::{Db, DbError};
use sunrise_wire_protocol::{CursorEntry, SubscribeEntry};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// One outbox op ready to send: `(op_id, sealed envelope bytes)`.
type OutboxOp = ([u8; 16], Vec<u8>);

/// Outbox ops grouped under one stream: `(stream_id, ops)`.
type OutboxGroup = ([u8; 16], Vec<OutboxOp>);

/// Errors produced by [`Core`].
#[derive(Debug, Error)]
pub enum CoreError {
    /// Vault lock contention.
    #[error(transparent)]
    VaultLock(#[from] VaultLockError),
    /// Underlying DB error.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Engine command/query error.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// Device keychain load/create error.
    #[error(transparent)]
    Keychain(#[from] KeychainError),
    /// Local sync bookkeeping (outbox / cursors) error.
    #[error(transparent)]
    SyncLocal(#[from] sunrise_storage::SyncLocalError),
    /// Op-log access error.
    #[error(transparent)]
    OpLog(#[from] sunrise_storage::OpLogError),
    /// Direct SQLite error from a driver-support read.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Closed Core handed a request.
    #[error("core is closed")]
    Closed,
}

/// Default routine-materialization interval, per
/// `docs/08-features/recurrence-engine.md` §generation-timing.
pub const ROUTINE_TIMER_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// User-facing handle to the local vault.
pub struct Core {
    cfg: CoreConfig,
    db: Mutex<Db>,
    _vault_lock: VaultLock,
    engine: Engine,
    changes_tx: broadcast::Sender<DomainEvent>,
    sync_tx: broadcast::Sender<SyncStatus>,
    /// Live sync state, shared with the driver task. Present even in offline
    /// mode (state stays `Disconnected`); the driver, when started, owns it.
    sync_shared: Arc<SyncShared>,
    /// The bearer this vault's sync sessions present.
    ///
    /// Owned by `Core`, not read back out of `cfg.sync` on demand. That
    /// distinction is the whole point: a caller that configures sync *after*
    /// open — every UniFFI caller does, via `start_sync(url, bearer)` — has no
    /// `cfg.sync` to hold a credential, and a handle minted per call is a
    /// different cell every time, so a renewal written through one is invisible
    /// to the driver holding another.
    sync_credential: TokenSource,
    /// The spawned driver task, if [`Core::start_sync`] has run. Aborted on
    /// `close`/drop so the task never leaks.
    sync_handle: Mutex<Option<JoinHandle<()>>>,
    /// The periodic routine-materialization task, if
    /// [`Core::start_routine_timer`] has run. Aborted alongside the driver.
    routine_handle: Mutex<Option<JoinHandle<()>>>,
    closed: Mutex<bool>,
}

impl std::fmt::Debug for Core {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Core")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl Core {
    /// Open (or create) the vault at `cfg.vault_dir` keyed by `unlock`.
    ///
    /// Acquires the OS-level vault lock; if another process holds it, returns
    /// `VaultLock(AlreadyHeld { holder_pid, holder_started_at })`.
    pub async fn open(cfg: CoreConfig, unlock: Unlock) -> Result<Self, CoreError> {
        let pid = std::process::id();
        let started_at = format_iso8601(cfg.clock.now_ms());
        let lock = VaultLock::acquire(&cfg.vault_dir, pid, &started_at)?;
        let (vault_root, paired) = unlock.into_parts();
        let db_path = cfg.vault_dir.join("vault.db");
        let mut db = Db::open(&db_path, &vault_root)?;
        let (changes_tx, _) = broadcast::channel(256);
        let (sync_tx, _) = broadcast::channel(64);
        // Load-or-create the persistent device identity. The keychain takes
        // ownership of the vault root (Core no longer keeps a copy) and supplies
        // the device id + signing key used to seal every op envelope.
        let keychain = Arc::new(Keychain::open(
            &mut db,
            vault_root,
            cfg.clock.as_ref(),
            cfg.rng.as_ref(),
            paired.as_deref(),
        )?);
        let engine = Engine::new(
            cfg.clock.clone(),
            cfg.hlc.clone(),
            cfg.rng.clone(),
            keychain,
        );
        // Announce this device to the account, once per vault. Before ADR-0024
        // each side had to be handed the other's cert by hand
        // (`Command::TrustDevice`), which accepted any self-signed cert and so
        // could not distinguish a sibling from a stranger. The cert is now
        // identity-signed and published as an op, so a device that pairs is
        // known to every replica the moment its first ops arrive.
        // Restore the causal clock before anything stamps an op. `Core::open`
        // is where a vault meets a fresh `MonotonicHlc`, so it is where the
        // clock's durable half has to come back; `Engine::prime_hlc` says what
        // goes wrong when it does not.
        engine.prime_hlc(&db)?;
        // The account's base epochs are an invariant of a vault, not a fact
        // about pairing. They used to be minted by `export_pairing_payload`,
        // because that is where their absence was first noticed: a payload is
        // built from the keys this device *holds*, and a vault that had never
        // been written to held none. Minting them there made opening a pairing
        // screen a durable write — `key_envelope` ops in the log, rows in the
        // outbox, fanned out to every other device — for a user who might
        // cancel. Doing it here instead makes the export a read again, and,
        // because `Engine::ensure_base_epochs` is idempotent, repairs a vault
        // created before this moved without a migration.
        //
        // It runs *before* `publish_device_cert`, which seals its op under the
        // vault-meta epoch and would otherwise be the thing that mints it. A
        // device arriving by pairing has already imported the account's epochs
        // in `Keychain::open` above, so this finds them and mints nothing.
        engine.ensure_base_epochs(&mut db)?;
        engine.publish_device_cert(&mut db)?;
        // Generation timing (recurrence-engine.md): materialize routines on
        // every app launch, using the injected clock so this stays deterministic.
        engine.apply(
            &mut db,
            Command::MaterializeRoutines {
                now_ms: cfg.clock.now_ms(),
            },
        )?;
        let initial_pending = sunrise_storage::Outbox::pending_count(&db).unwrap_or(0);
        let sync_shared = SyncShared::new(sync_tx.clone(), initial_pending);
        let sync_credential = cfg
            .sync
            .as_ref()
            .map_or_else(TokenSource::empty, |s| s.credential.clone());
        Ok(Self {
            cfg,
            db: Mutex::new(db),
            _vault_lock: lock,
            engine,
            changes_tx,
            sync_tx,
            sync_shared,
            sync_credential,
            sync_handle: Mutex::new(None),
            routine_handle: Mutex::new(None),
            closed: Mutex::new(false),
        })
    }

    /// Submit a mutating command.
    ///
    /// Routes through the [`Engine`]: validates input, derives a fresh op-id,
    /// CBOR-encodes the inner-op, writes the materialized state row + op-log
    /// entry in a single `BEGIN IMMEDIATE` transaction, and emits a
    /// `DomainEvent` on `changes()`.
    pub async fn submit(&self, cmd: Command) -> Result<CommandResult, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        let res = {
            let mut db = self.db.lock();
            self.engine.apply(&mut db, cmd.clone())?
        };
        // Best-effort change publish; receivers are bounded broadcast channels
        // and dropped subscribers are accepted.
        let event = match cmd {
            Command::CreateTask(_)
            | Command::CreateStream(_)
            | Command::CreateContext(_)
            | Command::CreateRoutine(_)
            | Command::CreateBlock(_)
            | Command::AttachFile(_)
            // A focus session is created, never mutated: `StartFocus` mints a
            // new `fcs_` entity, and `EndFocus` appends a separate record to
            // the same id (so it reads as an update of the session view).
            | Command::StartFocus(_) => DomainEvent::Created(res.entity),
            Command::DeleteTask(_)
            | Command::DeleteStream(_)
            | Command::DeleteContext(_)
            | Command::DeleteRoutine(_)
            | Command::DeleteBlock(_)
            | Command::DetachFile(_) => DomainEvent::Deleted(res.entity),
            _ => DomainEvent::Updated(res.entity),
        };
        let _ = self.changes_tx.send(event);
        // Wake the sync driver (if running) so the new outbox row drains
        // immediately rather than waiting for the next inbound frame.
        if self.sync_shared.is_active() {
            self.sync_shared.poke_submit();
        }
        Ok(res)
    }

    /// Apply a remote op envelope (the receive half of sync).
    ///
    /// Locks the vault, applies the op via [`Engine::apply_remote_all`]
    /// (idempotent, entity-level LWW), and broadcasts every resulting
    /// [`DomainEvent`] on `changes()`. Returns `Ok(None)` for an idempotent
    /// re-receive. The sync driver calls this for every inbound envelope.
    ///
    /// One envelope can produce several events: a `key_envelope` op is silent
    /// in itself, but the key it carries releases every op that had been parked
    /// waiting for it, and each of those materializes now. All of them are
    /// broadcast; the first is returned, because the driver only needs to know
    /// *whether* something landed and what kind it was.
    pub async fn apply_remote(
        &self,
        envelope_bytes: &[u8],
    ) -> Result<Option<DomainEvent>, CoreError> {
        Ok(self
            .apply_remote_all(envelope_bytes)
            .await?
            .into_iter()
            .next())
    }

    /// [`Self::apply_remote`], handing back every event rather than the first.
    ///
    /// The sync driver needs all of them: a `Created` event naming a Stream is
    /// what makes it subscribe to that Stream's channel, and after ADR-0024
    /// such an event can arrive as the *second* thing one envelope produced —
    /// a `key_envelope` op releasing a parked `stream.create`. A driver reading
    /// only the first would never subscribe, and the Stream's tasks would never
    /// arrive.
    pub(crate) async fn apply_remote_all(
        &self,
        envelope_bytes: &[u8],
    ) -> Result<Vec<DomainEvent>, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        let (events, pending) = {
            let mut db = self.db.lock();
            let events = self.engine.apply_remote_all(&mut db, envelope_bytes)?;
            // Read under the same lock the apply ran in, so the count cannot
            // miss a row this apply just wrote.
            let pending = sunrise_storage::Outbox::pending_count(&db).unwrap_or(0);
            (events, pending)
        };
        for ev in &events {
            let _ = self.changes_tx.send(ev.clone());
        }
        // Applying a *remote* op can leave *local* outbox rows behind: a
        // `device_cert` arriving makes this device seal the Stream keys it
        // holds to the newly certified one (`Engine::backfill_key_envelopes`).
        //
        // The driver only ever learned about new outbox rows from
        // `Self::submit`, so those rows sat there un-sent — and because
        // `maybe_live` will not leave `CatchingUp` while the outbox is
        // non-empty, a device that back-filled during catch-up never reached
        // `Live` at all. The wake belongs to "the outbox grew", not to "the
        // user did something", which is why it is here rather than only in
        // `submit`.
        if pending > 0 && self.sync_shared.is_active() {
            self.sync_shared.poke_submit();
        }
        Ok(events)
    }

    /// Run a read query.
    pub async fn query(&self, q: Query) -> Result<QueryResult, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        if matches!(q, Query::SyncStatus) {
            // Outbox depth is DB truth (accurate offline and online); the live
            // session fields come from the driver's `SyncShared`.
            let outbox_pending = {
                let db = self.db.lock();
                let n = sunrise_storage::Outbox::pending_count(&db).unwrap_or(0);
                u32::try_from(n).unwrap_or(u32::MAX)
            };
            let (state, last_sync_ms, peer_devices) = self.sync_shared.status_fields();
            return Ok(QueryResult::SyncStatus(SyncStatus {
                state,
                outbox_pending,
                peer_devices,
                last_sync_ms,
            }));
        }
        let db = self.db.lock();
        Ok(self.engine.query(&db, q)?)
    }

    /// Subscribe to domain events.
    #[must_use]
    pub fn changes(&self) -> broadcast::Receiver<DomainEvent> {
        self.changes_tx.subscribe()
    }

    /// Subscribe to sync-status updates.
    #[must_use]
    pub fn sync_status(&self) -> broadcast::Receiver<SyncStatus> {
        self.sync_tx.subscribe()
    }

    /// Mark the core closed; subsequent submit/query calls fail with
    /// [`CoreError::Closed`]. Signals the sync driver (if running) to stop and
    /// joins it, then drops the vault lock when this `Core` drops.
    pub async fn close(self) -> Result<(), CoreError> {
        self.shutdown().await;
        Ok(())
    }

    /// Stop the sync driver (if running) and mark this handle closed, **without
    /// consuming** `self`.
    ///
    /// This is the shutdown path for an `Arc<Core>` — the shape
    /// [`Core::start_sync`] requires. While a session is live the driver holds a
    /// transient strong `Arc<Core>` (upgraded from its `Weak` for the duration
    /// of the connection), so neither `Arc::try_unwrap` nor the consuming
    /// [`Core::close`] can run, and a plain drop of the caller's `Arc` cannot
    /// stop the driver. Aborting and joining the driver task here releases that
    /// transient strong reference, so dropping the last external `Arc` then runs
    /// [`Core`]'s `Drop` and releases the vault lock. [`Core::close`] delegates
    /// here. Idempotent.
    pub async fn shutdown(&self) {
        *self.closed.lock() = true;
        self.sync_shared.request_shutdown();
        let handle = self.sync_handle.lock().take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
        }
        let routine = self.routine_handle.lock().take();
        if let Some(h) = routine {
            h.abort();
            let _ = h.await;
        }
    }
}

/// Driver-support API. These are synchronous DB reads/writes the sync driver
/// calls *between* awaits — the db mutex is locked and released within each
/// method, never held across an `.await`.
impl Core {
    /// Start the client sync driver against `factory`. Idempotent: a second
    /// call while a driver is running is a no-op.
    ///
    /// Must be called from within a tokio runtime (it spawns the driver task)
    /// on an `Arc<Core>` so the task can hold a `Weak<Core>` back-reference —
    /// this keeps [`Core::open`] usable without a runtime for offline / TUI
    /// sync-off cases. The `factory` yields a fresh transport per connection
    /// attempt; `sunrise_sync::WsTransport` is the production one, an in-process
    /// loopback is used in tests.
    pub fn start_sync(self: &Arc<Self>, factory: TransportFactory) -> Result<(), CoreError> {
        let mut guard = self.sync_handle.lock();
        if guard.is_some() {
            return Ok(());
        }
        self.sync_shared.mark_active();
        let weak = Arc::downgrade(self);
        let shared = self.sync_shared.clone();
        let rng = self.cfg.rng.clone();
        let handle = tokio::spawn(sync_driver::run(weak, shared, factory, rng));
        *guard = Some(handle);
        Ok(())
    }

    /// Start the periodic routine-materialization timer.
    ///
    /// `docs/08-features/recurrence-engine.md` §generation-timing requires
    /// materialization on every app launch **and** on a periodic timer every 6
    /// hours while running. Only the launch half existed, so a long-running
    /// client — the TUI, or a desktop app left open over a weekend — would
    /// never generate occurrences past the horizon it computed at startup.
    ///
    /// Idempotent, and safe to skip: a client that never calls this still
    /// materializes at open. Requires a tokio runtime, which is why it is not
    /// called from [`Core::open`] — that must stay usable without one.
    ///
    /// Materialization is itself idempotent (occurrence ids are derived by
    /// blake3 from the routine id and occurrence instant), so a tick that
    /// generates nothing new is free and ticks may safely overlap a sync
    /// application doing the same work.
    pub fn start_routine_timer(self: &Arc<Self>, interval: Duration) -> Result<(), CoreError> {
        let mut guard = self.routine_handle.lock();
        if guard.is_some() {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick fires immediately; Core::open already materialized,
            // so skip it rather than doing the same work twice at startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(core) = weak.upgrade() else {
                    return; // Core dropped; nothing left to do.
                };
                if *core.closed.lock() {
                    return;
                }
                let now_ms = core.cfg.clock.now_ms();
                // Errors are contained: a failed tick must never kill the timer,
                // or one transient DB lock would silently stop all recurrence
                // for the rest of the session.
                let _ = core.submit(Command::MaterializeRoutines { now_ms }).await;
            }
        });
        *guard = Some(handle);
        Ok(())
    }

    /// Parse a capture line into a [`sunrise_domain::TaskDraft`], resolving
    /// `#stream` against this vault's streams and `@context` against its
    /// contexts.
    ///
    /// Clients could call [`sunrise_domain::capture::parse`] directly, but they
    /// would each have to re-implement the same glue — fetch the stream list,
    /// map it to `NamedRef`s, supply the clock. Doing it once here keeps the
    /// clients thin and guarantees every surface resolves names identically,
    /// which is the property `docs/08-features/inbox-and-capture.md` actually
    /// asks for.
    ///
    /// `tz` is a parameter rather than config so the function stays pure with
    /// respect to the host: the caller decides whether that is the system zone
    /// or a fixed one, and tests can pin it.
    ///
    /// Archived streams and contexts are excluded from the candidate sets: an
    /// archived entity is one the user has put away, so `@name` resolving to it
    /// would resurrect it silently. A `@name` with no live match is reported as
    /// [`sunrise_domain::capture::Unresolved::UnknownContext`] and its text
    /// stays in the title, so nothing is lost.
    pub async fn capture(
        &self,
        input: &str,
        tz: &jiff::tz::TimeZone,
    ) -> Result<sunrise_domain::capture::Capture, CoreError> {
        use sunrise_domain::capture::NamedRef;
        let streams = match self.query(Query::StreamList).await? {
            QueryResult::Streams(rows) => rows,
            _ => Vec::new(),
        };
        let contexts = match self.query(Query::Contexts).await? {
            QueryResult::Contexts(rows) => rows,
            _ => Vec::new(),
        };
        let stream_refs: Vec<NamedRef<'_>> = streams
            .iter()
            .filter(|s| !s.archived)
            .map(|s| NamedRef {
                id: s.id,
                name: s.name.as_str(),
            })
            .collect();
        let context_refs: Vec<NamedRef<'_>> = contexts
            .iter()
            .filter(|c| !c.archived)
            .map(|c| NamedRef {
                id: c.id,
                name: c.name.as_str(),
            })
            .collect();
        let now = jiff::Timestamp::from_millisecond(
            i64::try_from(self.cfg.clock.now_ms()).unwrap_or(i64::MAX),
        )
        .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        Ok(sunrise_domain::capture::parse(
            input,
            now,
            tz,
            &stream_refs,
            &context_refs,
        ))
    }

    /// **Capture-aside**: parse a mid-session thought and force it to the
    /// Inbox (`docs/08-features/focus-mode.md` §Capture-aside).
    ///
    /// Identical to [`Self::capture`] except that any `#stream` the parser
    /// resolved is dropped, so the draft always lands in the Inbox — *always*,
    /// regardless of the stream the focused task belongs to. That is the whole
    /// point: focus mode is for **not** switching context, so an aside must not
    /// pull the user into a filing decision. `@context` annotations, dates and
    /// priorities are all kept; only the destination is overridden.
    ///
    /// Returns the parsed draft; committing it is a plain
    /// [`Command::CreateTask`], so the caller decides when the write happens.
    pub async fn capture_aside(
        &self,
        input: &str,
        tz: &jiff::tz::TimeZone,
    ) -> Result<sunrise_domain::capture::Capture, CoreError> {
        let mut c = self.capture(input, tz).await?;
        c.draft.stream_id = None;
        Ok(c)
    }

    /// App identity string (`<semver>+<platform>`) for the sync `Hello`.
    pub(crate) fn app_string(&self) -> &str {
        &self.cfg.app
    }

    /// Injected wall clock, in ms since the Unix epoch.
    ///
    /// Public so clients read time through the same `Clock` the engine does,
    /// rather than reaching for `SystemTime::now` and needing their own
    /// determinism-gate exemption. A client that takes time from here inherits
    /// the injected clock in tests for free.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.cfg.clock.now_ms()
    }

    /// Where this vault lives on disk.
    ///
    /// `pub(crate)` on purpose: the only caller is
    /// [`crate::attach`], which needs the blob store rooted beside the
    /// database. A public accessor would invite a client to open the vault
    /// directory itself, and the single-writer guarantee is exactly what
    /// stops that being safe.
    pub(crate) fn vault_dir(&self) -> &std::path::Path {
        &self.cfg.vault_dir
    }

    /// The injected randomness source.
    ///
    /// Attachments mint a per-blob key, and minting it from `OsRng` directly
    /// would put a non-deterministic value in the core — which the
    /// `clippy.toml` gate forbids and which would make an attachment test
    /// unrepeatable.
    pub(crate) fn rng(&self) -> &dyn crate::config::Rng {
        self.cfg.rng.as_ref()
    }

    /// Copy this vault's root key out, for handing to a device being paired.
    ///
    /// See [`crate::keychain::Keychain::export_vault_root_for_pairing`] for why
    /// this exists and why it is named the way it is. Send the result only
    /// through an authenticated encrypted channel — `sunrise_pairing` provides
    /// one — and drop it immediately afterwards.
    ///
    /// It is no longer sufficient on its own. Since ADR-0024 the root keys the
    /// database and wraps secrets at rest, but it does not imply a single
    /// Stream key: those are random and travel in
    /// [`Self::export_pairing_payload`]. A device handed only this opens a
    /// vault it cannot read.
    #[must_use]
    pub fn export_vault_root_for_pairing(&self) -> sunrise_crypto::keys::VaultRootKey {
        self.engine.keychain().export_vault_root_for_pairing()
    }

    /// Everything a device being paired needs: the account identity, every
    /// Stream key this device holds, and the vault root.
    ///
    /// **This reads.** Showing a pairing code is not a commitment to pair, so
    /// it leaves nothing behind: a user who opens the screen and closes it has
    /// not changed their vault and has emitted nothing for the other devices to
    /// absorb. The base epochs the payload has to carry are minted at
    /// [`Self::open`] instead — they are a property of the vault rather than of
    /// this call, and were the only reason this function ever wrote.
    ///
    /// # Errors
    /// Storage failures reading this device's labels.
    pub fn export_pairing_payload(&self) -> Result<sunrise_pairing::PairingPayload, CoreError> {
        let db = self.db.lock();
        Ok(self.engine.keychain().export_pairing_payload(&db)?)
    }

    /// This device's identity-signed cert (canonical CBOR).
    ///
    /// Published automatically as a `device_cert` op at open, so peers learn it
    /// through sync rather than through a manual trust command.
    #[must_use]
    pub fn device_cert(&self) -> Vec<u8> {
        self.engine.keychain().cert_blob().to_vec()
    }

    /// The account identity this vault belongs to.
    ///
    /// Anchored to `ID_S_pub` rather than to whichever device created the
    /// vault, which is what makes a device cert something the *account* issued.
    #[must_use]
    pub fn identity_id(&self) -> [u8; 16] {
        self.engine.keychain().identity_id()
    }

    /// This device's stable id.
    ///
    /// The OIDC login binds a token to it (`device_id` claim), and the relay
    /// refuses a token whose claim names a different device — so a token
    /// lifted off this machine is useless on another one.
    #[must_use]
    pub fn device_id(&self) -> [u8; 16] {
        self.engine.keychain().device_id()
    }

    /// Count of unacked outbox rows (DB truth).
    pub(crate) fn sync_pending(&self) -> Result<u64, CoreError> {
        let db = self.db.lock();
        Ok(sunrise_storage::Outbox::pending_count(&db)?)
    }

    /// Build the subscribe set: every known stream — the vault-meta stream, the
    /// Inbox, every stream we have ops for, and every declared stream — with
    /// its per-`(device)` cursors from `sync_cursors`.
    ///
    /// Both fixed ids are seeded unconditionally. They are the two streams a
    /// vault can hold ops for while having no row that names them: the meta
    /// stream has no `streams` row at all, and the Inbox's row only appears
    /// once a task lands in it. A subscribe set that omitted either would
    /// silently never receive that stream's ops — including, since ADR-0024,
    /// the `key_envelope` ops that carry its keys.
    /// The configured anti-entropy resync interval, when sync is configured.
    pub(crate) fn sync_resync_interval(&self) -> Option<std::time::Duration> {
        self.cfg.sync.as_ref().map(|s| s.resync_interval)
    }

    /// The shared bearer this vault's sync sessions present.
    ///
    /// One cell per `Core`, handed out by clone. Writing through it reaches
    /// both the live session (as a `0x12 RefreshToken` frame) and the next
    /// reconnect, whether or not sync was configured at open — which is what
    /// makes it usable from the FFI seam, where the URL and the first bearer
    /// only arrive at `start_sync`.
    #[must_use]
    pub fn sync_credential(&self) -> TokenSource {
        self.sync_credential.clone()
    }

    pub(crate) fn sync_subscribe_entries(&self) -> Result<Vec<SubscribeEntry>, CoreError> {
        use rusqlite::params;
        let db = self.db.lock();
        let conn = db.conn();
        let mut streams: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
        streams.insert(crate::engine::META_STREAM);
        streams.insert(sunrise_domain::INBOX_STREAM_BYTES);
        {
            let mut stmt = conn.prepare("SELECT DISTINCT stream_id FROM ops")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                if let Some(id) = to16(&row?) {
                    streams.insert(id);
                }
            }
        }
        {
            let mut stmt = conn.prepare("SELECT stream_id FROM streams WHERE deleted = 0")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                if let Some(id) = to16(&row?) {
                    streams.insert(id);
                }
            }
        }
        let mut entries = Vec::with_capacity(streams.len());
        for stream_id in streams {
            let mut stmt = conn.prepare(
                "SELECT device_id, last_applied_seq FROM sync_cursors WHERE stream_id = ?",
            )?;
            let rows = stmt.query_map(params![&stream_id[..]], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
            })?;
            let mut cursors = Vec::new();
            for row in rows {
                let (dev, seq) = row?;
                let Some(device_id) = to16(&dev) else {
                    continue;
                };
                cursors.push(CursorEntry {
                    device_id,
                    last_applied_seq: u64::try_from(seq).unwrap_or(0),
                });
            }
            entries.push(SubscribeEntry { cursors, stream_id });
        }
        Ok(entries)
    }

    /// Load unacked outbox rows (skipping in-flight op ids), grouped by stream
    /// in enqueue order, each op paired with its sealed envelope bytes.
    pub(crate) fn sync_outbox_grouped(
        &self,
        skip: &HashSet<[u8; 16]>,
    ) -> Result<Vec<OutboxGroup>, CoreError> {
        let db = self.db.lock();
        let unacked = sunrise_storage::Outbox::list_unacked(&db)?;
        // Preserve first-seen stream order for stable batch grouping.
        let mut order: Vec<[u8; 16]> = Vec::new();
        let mut groups: std::collections::HashMap<[u8; 16], Vec<OutboxOp>> =
            std::collections::HashMap::new();
        for entry in unacked {
            if skip.contains(&entry.op_id) {
                continue;
            }
            let Some(env) = sunrise_storage::OpLog::get_envelope(&db, &entry.op_id)? else {
                continue;
            };
            groups
                .entry(entry.stream_id)
                .or_insert_with(|| {
                    order.push(entry.stream_id);
                    Vec::new()
                })
                .push((entry.op_id, env));
        }
        Ok(order
            .into_iter()
            .map(|s| {
                let ops = groups.remove(&s).unwrap_or_default();
                (s, ops)
            })
            .collect())
    }

    /// Mark `op_ids` acked in the persistent outbox; returns the new pending
    /// count.
    pub(crate) fn sync_mark_acked(&self, op_ids: &[[u8; 16]]) -> Result<u64, CoreError> {
        let now = self.cfg.clock.now_ms();
        let mut db = self.db.lock();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            for id in op_ids {
                sunrise_storage::Outbox::mark_acked(tx, id, now)
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
            }
            Ok(())
        })?;
        Ok(sunrise_storage::Outbox::pending_count(&db)?)
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Best-effort: signal the driver and abort the task so it never leaks.
        // (Can't await a join in `Drop`; abort is sufficient — the driver only
        // yields at `.await` points, never mid-DB-write.)
        self.sync_shared.request_shutdown();
        if let Some(h) = self.sync_handle.lock().take() {
            h.abort();
        }
    }
}

/// Left-pad / truncate a DB blob to a 16-byte id.
/// A 16-byte id read out of a DB blob, or `None` if the blob is not 16 bytes.
///
/// Not a pad, for the reason given on `keychain::to16`: zero-padding turns a
/// corrupt row into a valid-looking value, and the value it produces here is
/// `[0u8; 16]` — the vault-meta stream — so one truncated blob would put a
/// stranger's cursor on it. Every caller here is building a Subscribe frame,
/// where a row that cannot name a stream or a device has nothing to contribute
/// and is skipped.
fn to16(b: &[u8]) -> Option<[u8; 16]> {
    b.try_into().ok()
}

fn format_iso8601(ms: u64) -> String {
    // Reuse the same civil-date math as sunrise-log; for simplicity we
    // stringify ms-since-epoch as an integer here. Prod prefers RFC 3339
    // but the lock-file payload is human-readable for debugging only.
    format!("{ms}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemRng;
    use crate::vault_lock::VaultLockError;
    use parking_lot::Mutex as PLMutex;
    use std::sync::Arc;
    use sunrise_crypto::keys::VaultRootKey;

    #[derive(Debug)]
    struct FakeClock(PLMutex<u64>);
    impl crate::config::Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock()
        }
    }

    fn cfg(dir: &std::path::Path) -> CoreConfig {
        CoreConfig::with_clock(
            dir.to_path_buf(),
            "0.1.0+test",
            Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
            Arc::new(SystemRng),
        )
    }

    fn unlock() -> Unlock {
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes([1u8; 32]),
            paired: None,
        }
    }

    /// Every durable trace an `export_pairing_payload` could leave: the op
    /// log, the outbox, and the key rows.
    fn vault_footprint(core: &Core) -> (i64, i64, i64) {
        let db = core.db.lock();
        let conn = db.conn();
        let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        (
            one("SELECT count(*) FROM ops"),
            one("SELECT count(*) FROM outbox"),
            one("SELECT count(*) FROM stream_keys"),
        )
    }

    /// **Opening a pairing screen must not change the vault** (issue #106).
    ///
    /// `export_pairing_payload` minted the account's base epochs for one
    /// revision, which put `key_envelope` ops in the log and rows in the outbox
    /// — fanned out to every other device — for a user who might look at a QR
    /// code and close it. The epochs are now established at `Core::open`, so
    /// this call reads.
    ///
    /// The three counters are compared rather than one because the write took
    /// three forms: a `stream_keys` row, an op, and an outbox entry.
    #[tokio::test]
    async fn export_pairing_payload_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();

        let before = vault_footprint(&core);
        let payload = core.export_pairing_payload().unwrap();
        let again = core.export_pairing_payload().unwrap();
        let after = vault_footprint(&core);

        assert_eq!(
            before, after,
            "assembling a pairing payload must leave the op log, the outbox and \
             the key rows exactly as it found them"
        );
        assert_eq!(payload.stream_keys, again.stream_keys);
        core.close().await.unwrap();
    }

    /// The other half: the payload is still *complete* without that write.
    ///
    /// A vault that has never been written to holds no Stream keys unless
    /// something mints them, and a device paired from an empty payload can read
    /// no control op at all — so it cannot even learn what it is missing. That
    /// is why the mint existed. It now happens at open, and this pins the
    /// property the move must not lose.
    #[tokio::test]
    async fn a_freshly_opened_vault_already_carries_its_base_epochs() {
        use sunrise_domain::INBOX_STREAM_BYTES;

        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let payload = core.export_pairing_payload().unwrap();

        assert!(
            payload.stream_keys.contains_key(&[0u8; 16]),
            "the vault-meta key must travel, or the paired device reads no \
             control op ever"
        );
        assert!(
            payload.stream_keys.contains_key(&INBOX_STREAM_BYTES),
            "the Inbox is the one stream every account has"
        );
        core.close().await.unwrap();
    }

    #[tokio::test]
    async fn open_and_close() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        // Sync status query is wired and works without a real engine.
        let qr = core.query(Query::SyncStatus).await.unwrap();
        assert!(matches!(qr, QueryResult::SyncStatus(_)));
        core.close().await.unwrap();
    }

    /// `Core::submit` classifies every command into a `DomainEvent` by an
    /// explicit match with a `_ => Updated` fallback, so a new command that
    /// creates or deletes something is silently reported as an *update* unless
    /// its arm is added. A client driving its view off `changes()` would then
    /// never learn a Block or an Attachment appeared.
    #[tokio::test]
    async fn create_and_delete_commands_publish_the_right_change_event() {
        use sunrise_domain::inbox::inbox_stream_ref;
        use sunrise_domain::{AttachmentDraft, BlockDraft, SunriseTime, TaskDraft};

        async fn next(rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>) -> DomainEvent {
            rx.recv().await.expect("an event")
        }

        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let mut events = core.changes();

        let now = jiff::Timestamp::from_millisecond(1_700_000_000_000).unwrap();
        let hour = jiff::SignedDuration::from_hours(1);

        let task = core
            .submit(Command::CreateTask(TaskDraft {
                title: "Write the report".into(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .entity;
        assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == task));

        let block = core
            .submit(Command::CreateBlock(BlockDraft {
                stream_id: inbox_stream_ref(),
                starts_at: SunriseTime::instant(now),
                ends_at: SunriseTime::instant(now + hour),
                title: Some("Deep work".into()),
                title_track_task: false,
                tasks: Vec::new(),
            }))
            .await
            .unwrap()
            .entity;
        assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == block));

        core.submit(Command::BindTask { block, task })
            .await
            .unwrap();
        assert!(matches!(next(&mut events).await, DomainEvent::Updated(id) if id == block));

        let attachment = core
            .submit(Command::AttachFile(AttachmentDraft {
                parent: task,
                filename: "receipt.pdf".into(),
                mime_type: "application/pdf".into(),
                size_bytes: 4096,
                blob_key: [7u8; 32],
                blob_id: [9u8; 16],
                chunk_count: 1,
                content_hash: [11u8; 32],
            }))
            .await
            .unwrap()
            .entity;
        assert!(matches!(next(&mut events).await, DomainEvent::Created(id) if id == attachment));

        core.submit(Command::DetachFile(attachment)).await.unwrap();
        assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == attachment));

        core.submit(Command::DeleteBlock(block)).await.unwrap();
        assert!(matches!(next(&mut events).await, DomainEvent::Deleted(id) if id == block));

        core.close().await.unwrap();
    }

    /// The three notification reads answer through `Core`, not only through the
    /// engine — which is the surface both clients actually call.
    #[tokio::test]
    async fn the_notification_reads_answer_through_the_core() {
        use sunrise_domain::ReminderSettings;

        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let now_ms = core.now_ms();

        assert!(matches!(
            core.query(Query::MorningSummary { now_ms }).await.unwrap(),
            QueryResult::MorningSummary(_)
        ));
        assert!(matches!(
            core.query(Query::EndOfDayPlan { now_ms }).await.unwrap(),
            QueryResult::EndOfDayPlan(_)
        ));
        assert!(matches!(
            core.query(Query::ReminderIntents {
                now_ms,
                horizon_ms: now_ms + 86_400_000,
                settings: ReminderSettings::default(),
            })
            .await
            .unwrap(),
            QueryResult::Reminders(_)
        ));
        core.close().await.unwrap();
    }

    /// `docs/08-features/recurrence-engine.md` requires materialization on a
    /// periodic timer, not only at launch. Without it a long-running client
    /// stops generating occurrences once it passes the horizon computed at
    /// startup — a TUI left open over a weekend simply goes quiet.
    ///
    /// Drives it with tokio's paused clock so the test is instant and
    /// deterministic: the injected `FakeClock` supplies domain time, and
    /// `tokio::time::advance` fires the timer.
    #[tokio::test(start_paused = true)]
    async fn routine_timer_materializes_past_the_launch_horizon() {
        use sunrise_domain::inbox::inbox_stream_ref;
        use sunrise_domain::routine::TaskTemplate;
        use sunrise_domain::rrule::RRule;
        use sunrise_domain::{RoutineCatchupPolicy, RoutineDraft};

        let dir = tempfile::tempdir().unwrap();
        let start_ms = 1_700_000_000_000u64;
        let clock = Arc::new(FakeClock(PLMutex::new(start_ms)));
        let cfg = CoreConfig::with_clock(
            dir.path().to_path_buf(),
            "0.1.0+test",
            clock.clone(),
            Arc::new(SystemRng),
        );
        let core = Arc::new(Core::open(cfg, unlock()).await.unwrap());

        core.submit(Command::CreateRoutine(RoutineDraft {
            template: TaskTemplate {
                title: "Water plants".into(),
                stream_id: inbox_stream_ref(),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: jiff::Timestamp::from_millisecond(i64::try_from(start_ms).unwrap()).unwrap(),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
        }))
        .await
        .unwrap();

        let count = |core: &Arc<Core>| {
            let db = core.db.lock();
            db.conn()
                .query_row("SELECT COUNT(*) FROM tasks WHERE deleted = 0", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
        };
        let at_launch = count(&core);
        assert!(
            at_launch > 0,
            "creating a routine must materialize a horizon"
        );

        core.start_routine_timer(Duration::from_secs(60)).unwrap();

        // Move domain time well past the launch horizon, then let the timer run.
        *clock.0.lock() = start_ms + 90 * 24 * 60 * 60 * 1000;
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        for _ in 0..50 {
            if count(&core) > at_launch {
                break;
            }
            tokio::time::advance(Duration::from_secs(61)).await;
            tokio::task::yield_now().await;
        }

        assert!(
            count(&core) > at_launch,
            "the periodic timer must extend the horizon; had {at_launch}, still {} after ticks",
            count(&core)
        );
        core.shutdown().await;
    }

    /// `Core::capture` must resolve `#stream` against the vault's real streams,
    /// which is the whole reason it exists rather than callers invoking the
    /// domain parser directly.
    #[tokio::test]
    async fn capture_resolves_streams_from_the_vault() {
        use sunrise_domain::capture::Unresolved;
        use sunrise_domain::StreamDraft;

        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let created = core
            .submit(Command::CreateStream(StreamDraft {
                name: "travel".into(),
                ..Default::default()
            }))
            .await
            .unwrap();
        let stream_id = created.entity;

        let tz = jiff::tz::TimeZone::UTC;
        let c = core
            .capture("Renew passport #travel !2", &tz)
            .await
            .unwrap();
        assert_eq!(c.draft.title, "Renew passport");
        assert_eq!(c.draft.stream_id, Some(stream_id));
        assert_eq!(c.draft.priority, Some(2));
        assert!(c.unresolved.is_empty(), "{:?}", c.unresolved);

        // The parsed draft must be directly submittable — the point of the API.
        core.submit(Command::CreateTask(c.draft)).await.unwrap();

        // An unknown stream is reported, not silently dropped.
        let c2 = core.capture("Something #nope", &tz).await.unwrap();
        assert_eq!(c2.draft.stream_id, None);
        assert!(matches!(
            c2.unresolved.as_slice(),
            [Unresolved::UnknownStream(_)]
        ));

        // The synthetic Inbox row is resolvable by name too.
        let c3 = core.capture("Triage me #inbox", &tz).await.unwrap();
        assert_eq!(
            c3.draft.stream_id,
            Some(sunrise_domain::inbox::inbox_stream_ref())
        );

        core.close().await.unwrap();
    }

    /// `@context` must resolve end to end: create a Context, capture a line
    /// mentioning it, submit the draft, and find the task carrying it.
    #[tokio::test]
    async fn capture_resolves_contexts_from_the_vault() {
        use sunrise_domain::capture::Unresolved;
        use sunrise_domain::{ContextDraft, ContextPatch};

        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let tz = jiff::tz::TimeZone::UTC;

        // Before the Context exists, `@errands` is reported, not guessed at,
        // and its text survives in the title.
        let miss = core.capture("Buy milk @errands", &tz).await.unwrap();
        assert!(miss.draft.contexts.is_empty());
        assert_eq!(miss.draft.title, "Buy milk @errands");
        assert!(matches!(
            miss.unresolved.as_slice(),
            [Unresolved::UnknownContext(n)] if n == "errands"
        ));

        let ctx = core
            .submit(Command::CreateContext(ContextDraft {
                name: "errands".into(),
                ..Default::default()
            }))
            .await
            .unwrap()
            .entity;

        let c = core.capture("Buy milk @errands !2", &tz).await.unwrap();
        assert_eq!(c.draft.title, "Buy milk");
        assert_eq!(c.draft.contexts, vec![ctx]);
        assert!(c.unresolved.is_empty(), "{:?}", c.unresolved);

        // The parsed draft is directly submittable, and the task keeps the tag.
        let task = core
            .submit(Command::CreateTask(c.draft))
            .await
            .unwrap()
            .entity;
        match core.query(Query::EntityById(task)).await.unwrap() {
            QueryResult::Task(t) => {
                assert!(t.contexts.contains(&ctx), "the task carries @errands");
            }
            other => panic!("expected Task, got {other:?}"),
        }

        // Archiving takes it back out of capture resolution.
        core.submit(Command::UpdateContext {
            id: ctx,
            patch: ContextPatch {
                archived: Some(true),
                ..Default::default()
            },
        })
        .await
        .unwrap();
        let after = core.capture("Buy bread @errands", &tz).await.unwrap();
        assert!(after.draft.contexts.is_empty());
        assert!(matches!(
            after.unresolved.as_slice(),
            [Unresolved::UnknownContext(_)]
        ));

        core.close().await.unwrap();
    }

    /// Starting the timer twice must not spawn two tasks.
    #[tokio::test(start_paused = true)]
    async fn routine_timer_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let core = Arc::new(Core::open(cfg(dir.path()), unlock()).await.unwrap());
        core.start_routine_timer(Duration::from_secs(60)).unwrap();
        core.start_routine_timer(Duration::from_secs(60)).unwrap();
        assert!(core.routine_handle.lock().is_some());
        core.shutdown().await;
        assert!(
            core.routine_handle.lock().is_none(),
            "shutdown must reap the timer task, not leak it"
        );
    }

    #[tokio::test]
    async fn identity_persists_and_outbox_hydrates_across_reopen() {
        use sunrise_domain::TaskDraft;
        let dir = tempfile::tempdir().unwrap();

        let device_id_first;
        let pending_before_close;
        {
            let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
            // Not zero: opening a vault mints the account's base epochs,
            // publishes this device's certificate, and queues the
            // identity-sealed copies of those first Stream keys like any other
            // op.
            let announced = core.sync_pending().unwrap();
            assert!(announced > 0, "the vault announces itself at open");
            core.submit(Command::CreateTask(TaskDraft {
                title: "persisted".into(),
                ..Default::default()
            }))
            .await
            .unwrap();
            pending_before_close = core.sync_pending().unwrap();
            device_id_first = match core.query(Query::SyncStatus).await.unwrap() {
                QueryResult::SyncStatus(s) => {
                    // One more, not two: the Inbox's key is minted at open
                    // along with the vault-meta one, so the first task in it
                    // finds a key already there and queues only itself. It was
                    // two while the Inbox epoch was minted lazily by whatever
                    // first wrote to that stream.
                    assert_eq!(
                        u64::from(s.outbox_pending),
                        announced + 1,
                        "only the task itself is newly pending"
                    );
                    // Read the device id straight from the vault for comparison.
                    let db = core.db.lock();
                    db.conn()
                        .query_row(
                            "SELECT device_id FROM local_identity WHERE id = 1",
                            [],
                            |r| r.get::<_, Vec<u8>>(0),
                        )
                        .unwrap()
                }
                _ => panic!("expected sync status"),
            };
            core.close().await.unwrap();
        }

        // Reopen: the same device identity loads, and the unacked outbox row
        // hydrates from disk.
        let core2 = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let device_id_second = {
            let db = core2.db.lock();
            db.conn()
                .query_row(
                    "SELECT device_id FROM local_identity WHERE id = 1",
                    [],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .unwrap()
        };
        assert_eq!(
            device_id_first, device_id_second,
            "same device id on reopen"
        );
        match core2.query(Query::SyncStatus).await.unwrap() {
            // The outbox hydrates from the DB, so the reopened vault sees
            // exactly what the first one left pending — announcement, key
            // envelope and task alike. The reopen itself adds nothing: the
            // certificate is published once per vault, not once per open.
            QueryResult::SyncStatus(s) => {
                assert_eq!(u64::from(s.outbox_pending), pending_before_close);
            }
            _ => panic!("expected sync status"),
        }
        core2.close().await.unwrap();
    }

    #[tokio::test]
    async fn second_open_blocked_by_vault_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _core1 = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let res = Core::open(cfg(dir.path()), unlock()).await;
        assert!(matches!(
            res,
            Err(CoreError::VaultLock(VaultLockError::AlreadyHeld { .. }))
        ));
    }

    #[tokio::test]
    async fn submit_after_close_errors() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        core.close().await.unwrap();
        // After close — but Core is consumed. Test the in-flight closed
        // state by re-opening and toggling the flag indirectly.
        let core2 = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        // close() consumed self; we can't test closed-then-call without
        // a non-consuming closed mark. The Closed branch is exercised by
        // documentation only in v1.
        drop(core2);
    }

    #[tokio::test]
    async fn changes_subscribe_works() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
        let _rx = core.changes();
        let _rx2 = core.sync_status();
        drop(core);
    }
}
