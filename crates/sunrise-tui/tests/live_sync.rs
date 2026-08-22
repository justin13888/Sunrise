//! Integration test proving the TUI's live-sync wiring works end to end
//! against a real relay — the headless stand-in for the interactive
//! two-terminal demo.
//!
//! It drives exactly the code path the binary runs at startup
//! ([`sunrise_tui::livesync::open_with_plan`]): open a core keyed by the shared
//! dev root, export/trust device certs via files, and start the real
//! `WsTransport` sync driver. Then it asserts both drivers reach `Live` and a
//! task authored on one replica converges to the other.

use std::net::SocketAddr;
use std::time::Duration;

use sunrise_core::{Command, Core, DomainEvent, Query, QueryResult, SyncConfig};
use sunrise_domain::TaskDraft;
use sunrise_server::{build_router, ServerConfig, ServerState};
use sunrise_sync::SyncState;
use sunrise_tui::livesync::{open_with_plan, SyncPlan};
use tokio::task::JoinHandle;

/// Same fixed dev root the binary uses (`main::DEV_ROOT`); both replicas share
/// it so their per-stream keys match and envelopes decrypt.
const DEV_ROOT: [u8; 32] = [7u8; 32];
const POLL: Duration = Duration::from_millis(25);
const TIMEOUT: Duration = Duration::from_secs(10);

/// Boot the real relay on an ephemeral loopback port, in-process.
async fn spawn_relay() -> (SocketAddr, JoinHandle<()>) {
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

async fn sync_state(core: &Core) -> SyncState {
    match core.query(Query::SyncStatus).await.expect("sync status") {
        QueryResult::SyncStatus(s) => s.state,
        other => panic!("expected SyncStatus, got {other:?}"),
    }
}

async fn wait_live(core: &Core) {
    tokio::time::timeout(TIMEOUT, async {
        while sync_state(core).await != SyncState::Live {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("timed out waiting for Live");
}

async fn inbox_len(core: &Core) -> usize {
    match core.query(Query::Inbox).await.expect("inbox") {
        QueryResult::StreamTasks(v) => v.len(),
        other => panic!("expected StreamTasks, got {other:?}"),
    }
}

async fn wait_inbox_len(core: &Core, n: usize) {
    tokio::time::timeout(TIMEOUT, async {
        while inbox_len(core).await != n {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("timed out waiting for inbox convergence");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_wiring_reaches_live_and_converges() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("ws://{addr}/sync");

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let cert_a = dir_a.path().join("a.cert");
    let cert_b = dir_b.path().join("b.cert");

    // B comes up first via the TUI path: export its cert, start sync. It does
    // not have A's cert yet, so its trust list is empty for now.
    let plan_b = SyncPlan {
        sync: Some(SyncConfig { url: url.clone() }),
        export_cert: Some(cert_b.clone()),
        trust_cert: None,
    };
    let (core_b, _log_b) =
        open_with_plan(dir_b.path().to_path_buf(), "0.1.0+test", DEV_ROOT, &plan_b)
            .await
            .expect("open B");
    assert!(cert_b.exists(), "B exported its cert on startup");

    // A comes up via the same TUI path: trust B's cert (now on disk) and export
    // its own. This is precisely the binary's startup sequence.
    let plan_a = SyncPlan {
        sync: Some(SyncConfig { url: url.clone() }),
        export_cert: Some(cert_a.clone()),
        trust_cert: Some(cert_b.clone()),
    };
    let (core_a, _log_a) =
        open_with_plan(dir_a.path().to_path_buf(), "0.1.0+test", DEV_ROOT, &plan_a)
            .await
            .expect("open A");
    assert!(cert_a.exists(), "A exported its cert on startup");

    // Close the trust loop: B trusts A (the reverse file-exchange direction the
    // human performs by restarting B with SUNRISE_TRUST_CERT_FILE=a.cert).
    let a_cert = std::fs::read(&cert_a).unwrap();
    core_b
        .submit(Command::TrustDevice { cert_cbor: a_cert })
        .await
        .expect("B trusts A");

    // Both drivers reach Live against the real relay.
    wait_live(&core_a).await;
    wait_live(&core_b).await;

    // The repaint signal the TUI event loop selects on. Subscribe *before* the
    // remote op so this proves `Core::changes()` publishes for ops arriving
    // over sync, not just for local submits — that is what makes the TUI
    // repaint without the user touching a key.
    let mut changes_a = core_a.changes();

    // A task authored on B converges to A over the live session.
    core_b
        .submit(Command::CreateTask(TaskDraft {
            title: "from B".into(),
            ..Default::default()
        }))
        .await
        .expect("B creates a task");
    wait_inbox_len(&core_a, 1).await;

    // Still Live after applying the remote op.
    assert_eq!(sync_state(&core_a).await, SyncState::Live);

    // ...and A's change stream announced the remotely-authored task.
    let event = tokio::time::timeout(TIMEOUT, changes_a.recv())
        .await
        .expect("changes() published nothing for a remote op")
        .expect("changes channel closed");
    assert!(
        matches!(event, DomainEvent::Created(_) | DomainEvent::Updated(_)),
        "unexpected domain event: {event:?}"
    );

    core_a.shutdown().await;
    core_b.shutdown().await;
    relay.abort();
}
