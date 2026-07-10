//! End-to-end gating crate.
//!
//! Boots cross-crate scenarios that span [`sunrise_core`] + [`sunrise_server`].
//! Used by Phase 17 release-gating checks per `docs/10-cross-cutting/testing.md`.
//!
//! This crate's library surface is a **test harness**: helpers to boot the real
//! relay in-process, open [`Core`] instances wired to it over real `WebSocket`s,
//! exchange device trust, and assert convergence with event-driven waits (no
//! bare sleeps for correctness — only a short poll interval inside a timeout).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::missing_panics_doc)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use sunrise_core::{
    BoxTransport, Clock, Command, Core, CoreConfig, Query, QueryResult, SyncConfig, SystemRng,
    TransportFactory, Unlock,
};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{Task, TaskState};
use sunrise_server::{build_router, ServerConfig, ServerState};
use sunrise_sync::WsTransport;
use tokio::task::JoinHandle;

/// Crate-level marker used by the test harness.
pub const E2E_CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// App identity string the harness cores advertise in their sync `Hello`.
const APP_ID: &str = "0.1.0+e2e";

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

/// Boot the real `sunrise-server` router on an ephemeral loopback port,
/// in-process. Returns the bound address plus the serving task's handle (abort
/// it to shut the relay down). Follows `server_health_e2e.rs`'s boot pattern.
pub async fn spawn_relay() -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = build_router(ServerState::new(ServerConfig::default()));
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, handle)
}

/// Build a [`TransportFactory`] that dials `ws://{addr}/sync` with the real
/// [`WsTransport`] on every connect attempt (initial connect + every
/// reconnect).
#[must_use]
pub fn ws_factory(addr: SocketAddr) -> TransportFactory {
    let url = format!("ws://{addr}/sync");
    Arc::new(move || {
        let url = url.clone();
        Box::pin(async move {
            let t = WsTransport::connect(&url).await?;
            Ok(Box::new(t) as BoxTransport)
        }) as sunrise_core::ConnectFuture
    })
}

// ---------------------------------------------------------------------------
// Cores
// ---------------------------------------------------------------------------

/// Open (or reopen) a [`Core`] on `vault_dir` keyed by the shared paired-device
/// `root`, then start its sync driver against the relay at `addr`.
///
/// The paired-device model gives both replicas the **same** vault root (so
/// their per-stream keys match and envelopes decrypt) but distinct random
/// device identities (the keychain seeds a fresh id per vault).
pub async fn open_synced_core(
    vault_dir: &Path,
    root: [u8; 32],
    addr: SocketAddr,
    clock: Arc<dyn Clock>,
) -> Arc<Core> {
    let cfg = CoreConfig {
        vault_dir: vault_dir.to_path_buf(),
        clock,
        rng: Arc::new(SystemRng),
        app: APP_ID.into(),
        sync: Some(SyncConfig {
            url: format!("ws://{addr}/sync"),
        }),
    };
    let core = Core::open(cfg, Unlock::DevicePaired(VaultRootKey::from_bytes(root)))
        .await
        .expect("open core");
    let core = Arc::new(core);
    core.start_sync(ws_factory(addr)).expect("start sync");
    core
}

/// Exchange device certs both ways via [`Command::TrustDevice`] so each core
/// accepts the other's signed op envelopes. Trust is persistent local state, so
/// it survives a `close`/reopen.
pub async fn trust_each_other(a: &Core, b: &Core) {
    let cert_a = a.device_cert();
    let cert_b = b.device_cert();
    a.submit(Command::TrustDevice { cert_cbor: cert_b })
        .await
        .expect("a trusts b");
    b.submit(Command::TrustDevice { cert_cbor: cert_a })
        .await
        .expect("b trusts a");
}

// ---------------------------------------------------------------------------
// Canonical projections (table-dump equality)
// ---------------------------------------------------------------------------

/// Canonical, comparable projection of a [`Task`] for cross-replica equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalTask {
    /// Stable task id (prefixed ULID string).
    pub id: String,
    /// Title.
    pub title: String,
    /// User-visible state (`todo` / `in_progress` / `done` / `cancelled`).
    pub state: String,
    /// Owning stream's raw 16-byte id.
    pub stream_id: [u8; 16],
    /// `scheduled_at` in epoch ms, if set.
    pub scheduled_at_ms: Option<i64>,
    /// `due_at` in epoch ms, if set.
    pub due_at_ms: Option<i64>,
    /// Tombstone flag.
    pub deleted: bool,
    /// Optional priority.
    pub priority: Option<u8>,
    /// Scheduling constraints, rendered canonically (stable derived `Debug`).
    pub constraints: Vec<String>,
}

/// Canonical, comparable projection of a stream row for cross-replica equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalStream {
    /// Stream id (prefixed ULID string).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Palette color (lowercase wire form).
    pub color: String,
    /// Archived flag.
    pub archived: bool,
}

fn state_str(s: TaskState) -> String {
    match s {
        TaskState::Todo => "todo",
        TaskState::InProgress => "in_progress",
        TaskState::Done => "done",
        TaskState::Cancelled => "cancelled",
    }
    .to_string()
}

fn project_task(t: &Task) -> CanonicalTask {
    CanonicalTask {
        id: t.id.to_str(),
        title: t.title.clone(),
        state: state_str(t.state),
        stream_id: *t.stream_id.bytes(),
        scheduled_at_ms: t.scheduled_at.map(jiff::Timestamp::as_millisecond),
        due_at_ms: t.due_at.map(jiff::Timestamp::as_millisecond),
        deleted: t.deleted,
        priority: t.priority,
        constraints: t
            .scheduling_constraints
            .iter()
            .map(|c| format!("{c:?}"))
            .collect(),
    }
}

/// Gather every non-deleted task across the Inbox and all live streams.
async fn all_tasks(core: &Core) -> Vec<Task> {
    let streams = match core.query(Query::StreamList).await.expect("stream list") {
        QueryResult::Streams(v) => v,
        other => panic!("expected Streams, got {other:?}"),
    };
    let mut out = Vec::new();
    for row in streams {
        match core
            .query(Query::StreamTasks(row.id))
            .await
            .expect("stream tasks")
        {
            QueryResult::StreamTasks(ts) => out.extend(ts),
            other => panic!("expected StreamTasks, got {other:?}"),
        }
    }
    out
}

/// Canonical task table for `core`, sorted by id — the equality unit for
/// convergence assertions.
pub async fn canonical_tasks(core: &Core) -> Vec<CanonicalTask> {
    let mut rows: Vec<CanonicalTask> = all_tasks(core).await.iter().map(project_task).collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// Canonical stream table for `core`, sorted by id (the synthetic Inbox row is
/// included).
pub async fn canonical_streams(core: &Core) -> Vec<CanonicalStream> {
    let streams = match core.query(Query::StreamList).await.expect("stream list") {
        QueryResult::Streams(v) => v,
        other => panic!("expected Streams, got {other:?}"),
    };
    let mut rows: Vec<CanonicalStream> = streams
        .into_iter()
        .map(|r| CanonicalStream {
            id: r.id.to_str(),
            name: r.name,
            color: r.color.as_str().to_string(),
            archived: r.archived,
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

// ---------------------------------------------------------------------------
// Event-driven waits (short poll interval inside a timeout — never a bare sleep
// used for correctness)
// ---------------------------------------------------------------------------

/// Poll interval used inside every timeout loop.
const POLL: Duration = Duration::from_millis(25);

/// The current live [`sunrise_sync::SyncState`] of `core`.
async fn sync_state(core: &Core) -> sunrise_sync::SyncState {
    match core.query(Query::SyncStatus).await.expect("sync status") {
        QueryResult::SyncStatus(s) => s.state,
        other => panic!("expected SyncStatus, got {other:?}"),
    }
}

/// Wait until `core`'s driver reports [`sunrise_sync::SyncState::Live`].
pub async fn wait_live(core: &Core, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            if sync_state(core).await == sunrise_sync::SyncState::Live {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("timed out waiting for Live");
}

/// Wait until `core`'s persistent outbox has fully drained (`outbox_pending == 0`).
pub async fn wait_pending_zero(core: &Core, timeout: Duration) {
    tokio::time::timeout(timeout, async {
        loop {
            let pending = match core.query(Query::SyncStatus).await.expect("sync status") {
                QueryResult::SyncStatus(s) => s.outbox_pending,
                other => panic!("expected SyncStatus, got {other:?}"),
            };
            if pending == 0 {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("timed out waiting for outbox to drain");
}

/// Wait until both replicas expose identical canonical task tables of exactly
/// `expected_len` rows, then return that converged table.
///
/// Requiring the length guards against a premature match on the trivially-equal
/// empty state before any op has propagated.
pub async fn wait_tasks_converge(
    a: &Core,
    b: &Core,
    expected_len: usize,
    timeout: Duration,
) -> Vec<CanonicalTask> {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(timeout, async {
        loop {
            last_a = canonical_tasks(a).await;
            last_b = canonical_tasks(b).await;
            if last_a.len() == expected_len && last_a == last_b {
                return last_a.clone();
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(converged) = res else {
        panic!(
            "tasks did not converge to {expected_len} rows in time\n  A ({}): {last_a:#?}\n  B ({}): {last_b:#?}",
            last_a.len(),
            last_b.len()
        );
    };
    converged
}

/// Wait until both replicas expose identical canonical stream tables of exactly
/// `expected_len` rows.
pub async fn wait_streams_converge(a: &Core, b: &Core, expected_len: usize, timeout: Duration) {
    let mut last_a = Vec::new();
    let mut last_b = Vec::new();
    let res = tokio::time::timeout(timeout, async {
        loop {
            last_a = canonical_streams(a).await;
            last_b = canonical_streams(b).await;
            if last_a.len() == expected_len && last_a == last_b {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await;
    let Ok(()) = res else {
        panic!(
            "streams did not converge to {expected_len} rows in time\n  A: {last_a:#?}\n  B: {last_b:#?}"
        );
    };
}
