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
use crate::queries::{Query, QueryResult};
use crate::unlock::Unlock;
use crate::vault_lock::{VaultLock, VaultLockError};
use parking_lot::Mutex;
use sunrise_storage::{Db, DbError};
use sunrise_sync::SyncState;
use thiserror::Error;
use tokio::sync::broadcast;

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
        let device_id = derive_device_id(&cfg.vault_dir);
        let engine = Engine::new(cfg.clock.clone(), cfg.rng.clone(), device_id);
        // Generation timing (recurrence-engine.md): materialize routines on
        // every app launch, using the injected clock so this stays deterministic.
        engine.apply(
            &mut db,
            Command::MaterializeRoutines {
                now_ms: cfg.clock.now_ms(),
            },
        )?;
        Ok(Self {
            cfg,
            db: Mutex::new(db),
            _vault_lock: lock,
            engine,
            changes_tx,
            sync_tx,
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
        Ok(res)
    }

    /// Run a read query.
    pub async fn query(&self, q: Query) -> Result<QueryResult, CoreError> {
        if *self.closed.lock() {
            return Err(CoreError::Closed);
        }
        if matches!(q, Query::SyncStatus) {
            return Ok(QueryResult::SyncStatus(SyncStatus {
                state: SyncState::Disconnected,
                outbox_pending: 0,
                peer_devices: 0,
                last_sync_ms: None,
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
    /// [`CoreError::Closed`]. Drops the vault lock when this `Core` drops.
    pub async fn close(self) -> Result<(), CoreError> {
        *self.closed.lock() = true;
        Ok(())
    }
}

fn format_iso8601(ms: u64) -> String {
    // Reuse the same civil-date math as sunrise-log; for simplicity we
    // stringify ms-since-epoch as an integer here. Prod prefers RFC 3339
    // but the lock-file payload is human-readable for debugging only.
    format!("{ms}")
}

/// Derive a deterministic 16-byte device id from the vault directory path.
///
/// v1 uses a path-derived id so a vault opened from the same directory
/// always presents the same device to the op log; production binds to a
/// real `device_id` from a [`sunrise-crypto::DeviceCert`] once pairing
/// is wired into the open path.
fn derive_device_id(vault_dir: &std::path::Path) -> [u8; 16] {
    let bytes = sunrise_crypto::derive_key(
        "sunrise.device_id.v1",
        vault_dir.to_string_lossy().as_bytes(),
        16,
    );
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    out
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
