//! Integration test proving the TUI's live-sync wiring works end to end
//! against a real relay — the headless stand-in for the interactive
//! two-terminal demo.
//!
//! It drives exactly the code path the binary runs at startup
//! ([`sunrise_cli::livesync::open_with_plan`]): open a core keyed by the shared
//! dev root, export/trust device certs via files, and start the real
//! `WsTransport` sync driver. Then it asserts both drivers reach `Live` and a
//! task authored on one replica converges to the other.

use std::net::SocketAddr;
use std::time::Duration;

use sunrise_cli::livesync::{open_with_plan, SyncPlan};
use sunrise_core::{Command, Core, DomainEvent, Query, QueryResult, SyncConfig};
use sunrise_domain::TaskDraft;
use sunrise_server::{ServerConfig, ServerState};
use sunrise_sync::SyncState;
use tokio::task::JoinHandle;

/// One root, shared by both replicas so their per-stream keys match and their
/// envelopes decrypt — the "one account, two devices" case.
///
/// The binary no longer holds a constant like this: `sunrise_cli::vault` mints
/// a fresh root per vault directory, and `SUNRISE_VAULT_ROOT` is how two of
/// them are told to share one until pairing lands. This test passes the root
/// to `open_with_plan` directly, which is the same thing one layer down.
const SHARED_ROOT: [u8; 32] = [7u8; 32];
const POLL: Duration = Duration::from_millis(25);
const TIMEOUT: Duration = Duration::from_secs(10);

/// Boot the real relay on an ephemeral loopback port, in-process.
async fn spawn_relay() -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let state = ServerState::new(ServerConfig::default());
    let handle = tokio::spawn(async move {
        let _ = sunrise_server::serve(state, listener).await;
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
    let url = format!("http://{addr}");

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let pairing_a = dir_a.path().join("a.pairing");

    // A comes up first via the TUI path and exports its pairing payload — the
    // account identity plus every Stream key. Before ADR-0024 this exchanged
    // certificates, because a shared vault root already implied a shared key
    // schedule; it does not any more, and two vaults on one root would be two
    // separate accounts that can never read each other.
    let plan_a = SyncPlan {
        sync: Some(SyncConfig::new(url.clone())),
        export_pairing: Some(pairing_a.clone()),
        adopt_pairing: None,
    };
    let (core_a, _log_a) = open_with_plan(
        dir_a.path().to_path_buf(),
        "0.1.0+test",
        SHARED_ROOT,
        &plan_a,
    )
    .await
    .expect("open A");
    assert!(
        pairing_a.exists(),
        "A exported its pairing payload on startup"
    );

    // B comes up as a second device on A's account by adopting that payload,
    // which is the file-shuttled stand-in for the Noise channel. Neither side
    // needs a trust command: B's certificate is signed by the account identity
    // it just adopted, and it publishes that certificate as an op.
    let plan_b = SyncPlan {
        sync: Some(SyncConfig::new(url.clone())),
        export_pairing: None,
        adopt_pairing: Some(pairing_a.clone()),
    };
    let (core_b, _log_b) = open_with_plan(
        dir_b.path().to_path_buf(),
        "0.1.0+test",
        SHARED_ROOT,
        &plan_b,
    )
    .await
    .expect("open B");

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

/// The **binary's** own sync path, not the library's.
///
/// Regression: `sunrise sync --once` polled for `Live` with an empty outbox,
/// but `run` opened the vault with `SyncPlan::default()` and never started a
/// driver, so the poll could only ever time out after 30 s. Every test above
/// this one passed throughout, because they all build the plan themselves.
#[tokio::test(flavor = "multi_thread")]
async fn the_binary_sync_once_actually_reaches_the_relay() {
    let (addr, relay) = spawn_relay().await;
    let url = format!("http://{addr}");
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = dir.path().to_path_buf();
    // The binary mints this vault's root on first open and files it here.
    // Without an explicit keystore it would file it in the developer's real
    // one, which is not a thing a test may do.
    let keystore = vault.join("keystore");

    // A local write, so there is something in the outbox to drain.
    let capture = tokio::task::spawn_blocking({
        let vault = vault.clone();
        let keystore = keystore.clone();
        move || {
            std::process::Command::new(env!("CARGO_BIN_EXE_sunrise"))
                .args(["capture", "Sent over the wire"])
                .env("SUNRISE_VAULT", &vault)
                .env("SUNRISE_KEYSTORE", &keystore)
                .env_remove("SUNRISE_VAULT_ROOT")
                .env_remove("SUNRISE_SYNC_URL")
                .output()
                .expect("run sunrise")
        }
    })
    .await
    .expect("join");
    assert!(capture.status.success(), "capture failed: {capture:?}");

    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_sunrise"))
            .args(["sync", "--once"])
            .env("SUNRISE_VAULT", &vault)
            .env("SUNRISE_KEYSTORE", &keystore)
            .env_remove("SUNRISE_VAULT_ROOT")
            .env("SUNRISE_SYNC_URL", &url)
            .output()
            .expect("run sunrise")
    })
    .await
    .expect("join");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "sync --once failed: {stdout} / {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("sync: live, outbox empty"),
        "the driver never reached the relay, stdout was {stdout:?}"
    );

    relay.abort();
}
