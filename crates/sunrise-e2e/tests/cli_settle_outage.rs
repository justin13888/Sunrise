//! The CLI harness's `settle` rides out a relay outage (issue #698).
//!
//! `settle` used to be four fixed `sync --once` rounds, each with the CLI's
//! 30 s budget, so one reconnect cascade failed the scenario. Here the relay
//! is down when a settle starts and comes back on the same address, from the
//! same database, part-way through the first device's first round: that
//! `sync --once` runs out its budget, the harness runs it again, and both
//! devices still converge.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle};
use sunrise_server::{ServerConfig, ServerState};

/// Serve the relay on `addr` from the database at `db`, until aborted.
async fn serve(addr: SocketAddr, db: &Path) {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind the relay's address");
    serve_on(listener, db).await;
}

/// Serve the relay on `listener` from the database at `db`, until aborted.
async fn serve_on(listener: tokio::net::TcpListener, db: &Path) {
    let state = ServerState::new(ServerConfig {
        sqlite_path: Some(db.to_path_buf()),
        ..ServerConfig::default()
    });
    let _ = sunrise_server::serve(state, listener).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settle_converges_across_a_relay_outage() {
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join("meta.db");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("its address");
    let relay = tokio::spawn({
        let db = db.clone();
        async move { serve_on(listener, &db).await }
    });
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = bootstrap_and_recover(dir.path(), &format!("http://{addr}")).await;

    let task = first_id(&a.ok(&["capture", "Renew passport"]).await);
    relay.abort();
    let _ = relay.await;

    // Back after longer than one `sync --once` budget, so the first round
    // can only succeed by being run again.
    let back = tokio::spawn({
        let db = db.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(35)).await;
            serve(addr, &db).await;
        }
    });
    settle(&a, &b).await;

    assert_eq!(b.task(&task).await["title"], "Renew passport");
    back.abort();
}
