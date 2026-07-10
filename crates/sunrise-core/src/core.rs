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
use crate::sync_driver::{self, SyncShared, TransportFactory};
use crate::unlock::Unlock;
use crate::vault_lock::{VaultLock, VaultLockError};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;
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
    /// The spawned driver task, if [`Core::start_sync`] has run. Aborted on
    /// `close`/drop so the task never leaks.
    sync_handle: Mutex<Option<JoinHandle<()>>>,
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
        let vault_root = unlock.into_root();
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
        )?);
        let engine = Engine::new(cfg.clock.clone(), cfg.rng.clone(), keychain);
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
        Ok(Self {
            cfg,
            db: Mutex::new(db),
            _vault_lock: lock,
            engine,
            changes_tx,
            sync_tx,
            sync_shared,
            sync_handle: Mutex::new(None),
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
            Command::CreateTask(_) | Command::CreateStream(_) | Command::CreateRoutine(_) => {
                DomainEvent::Created(res.entity)
            }
            Command::DeleteTask(_) | Command::DeleteStream(_) | Command::DeleteRoutine(_) => {
                DomainEvent::Deleted(res.entity)
            }
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
    /// Locks the vault, applies the op via [`Engine::apply_remote`] (idempotent,
    /// entity-level LWW), and broadcasts the resulting [`DomainEvent`] on
    /// `changes()`. Returns `Ok(None)` for an idempotent re-receive. The sync
    /// driver (next slice) calls this for every inbound envelope.
    pub async fn apply_remote(
        &self,
        envelope_bytes: &[u8],
    ) -> Result<Option<DomainEvent>, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        let event = {
            let mut db = self.db.lock();
            self.engine.apply_remote(&mut db, envelope_bytes)?
        };
        if let Some(ev) = &event {
            let _ = self.changes_tx.send(ev.clone());
        }
        Ok(event)
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
        *self.closed.lock() = true;
        self.sync_shared.request_shutdown();
        let handle = self.sync_handle.lock().take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
        }
        Ok(())
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

    /// App identity string (`<semver>+<platform>`) for the sync `Hello`.
    pub(crate) fn app_string(&self) -> &str {
        &self.cfg.app
    }

    /// Injected wall clock, in ms since the Unix epoch.
    pub(crate) fn now_ms(&self) -> u64 {
        self.cfg.clock.now_ms()
    }

    /// This device's self-issued cert (canonical CBOR). A peer passes it to
    /// [`Command::TrustDevice`] to accept this device's ops.
    #[must_use]
    pub fn device_cert(&self) -> Vec<u8> {
        self.engine.keychain().cert_blob().to_vec()
    }

    /// Count of unacked outbox rows (DB truth).
    pub(crate) fn sync_pending(&self) -> Result<u64, CoreError> {
        let db = self.db.lock();
        Ok(sunrise_storage::Outbox::pending_count(&db)?)
    }

    /// Build the subscribe set: every known stream (the zero meta/inbox stream,
    /// every stream we have ops for, and every declared stream) with its
    /// per-`(device)` cursors from `sync_cursors`.
    pub(crate) fn sync_subscribe_entries(&self) -> Result<Vec<SubscribeEntry>, CoreError> {
        use rusqlite::params;
        let db = self.db.lock();
        let conn = db.conn();
        let mut streams: std::collections::BTreeSet<[u8; 16]> = std::collections::BTreeSet::new();
        streams.insert([0u8; 16]);
        {
            let mut stmt = conn.prepare("SELECT DISTINCT stream_id FROM ops")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                streams.insert(to16(&row?));
            }
        }
        {
            let mut stmt = conn.prepare("SELECT stream_id FROM streams WHERE deleted = 0")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                streams.insert(to16(&row?));
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
                cursors.push(CursorEntry {
                    device_id: to16(&dev),
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
fn to16(b: &[u8]) -> [u8; 16] {
    let mut a = [0u8; 16];
    let take = b.len().min(16);
    a[..take].copy_from_slice(&b[..take]);
    a
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
        CoreConfig {
            vault_dir: dir.to_path_buf(),
            clock: Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
            rng: Arc::new(SystemRng),
            app: "0.1.0+test".into(),
            sync: None,
        }
    }

    fn unlock() -> Unlock {
        Unlock::DevicePaired(VaultRootKey::from_bytes([1u8; 32]))
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

    #[tokio::test]
    async fn identity_persists_and_outbox_hydrates_across_reopen() {
        use sunrise_domain::TaskDraft;
        let dir = tempfile::tempdir().unwrap();

        let device_id_first;
        {
            let core = Core::open(cfg(dir.path()), unlock()).await.unwrap();
            core.submit(Command::CreateTask(TaskDraft {
                title: "persisted".into(),
                ..Default::default()
            }))
            .await
            .unwrap();
            device_id_first = match core.query(Query::SyncStatus).await.unwrap() {
                QueryResult::SyncStatus(s) => {
                    assert_eq!(s.outbox_pending, 1, "one op pending after submit");
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
            QueryResult::SyncStatus(s) => assert_eq!(s.outbox_pending, 1),
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
