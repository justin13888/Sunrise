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

use sunrise_e2e::cli::{bootstrap_and_recover, first_id, settle, Device};
use sunrise_server::{ServerConfig, ServerState};
use tokio::time::Instant;

/// Serve the relay on `listener` from the database at `db` until `stop`
/// resolves and the relay has drained and closed its listener, or until the
/// server fails. Returns how it ended.
async fn serve_on(
    listener: tokio::net::TcpListener,
    db: &Path,
    stop: impl std::future::Future<Output = ()> + Send + 'static,
) -> String {
    let state = ServerState::new(ServerConfig {
        sqlite_path: Some(db.to_path_buf()),
        shutdown_grace_secs: 5,
        ..ServerConfig::default()
    });
    format!(
        "{:?}",
        sunrise_server::serve_until(state, listener, stop).await
    )
}

/// A socket bound to `addr` that does not listen: every connection to it is
/// refused, as with no relay there, but no other process can take the port.
fn hold(addr: SocketAddr) -> std::io::Result<tokio::net::TcpSocket> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    Ok(socket)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settle_converges_across_a_relay_outage() {
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join("meta.db");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("its address");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let relay = tokio::spawn({
        let db = db.clone();
        async move {
            serve_on(listener, &db, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = bootstrap_and_recover(dir.path(), &format!("http://{addr}")).await;

    let task = first_id(&a.ok(&["capture", "Renew passport"]).await);
    let _ = stop.send(());
    let ended = relay.await.expect("join the relay");
    // The listener is closed now; the port is held, not listening, from here
    // until the relay comes back on it, so nothing else can take it.
    let held = hold(addr).unwrap_or_else(|e| {
        panic!("hold {addr} through the outage (the relay ended {ended}): {e}")
    });

    // Back after longer than one `sync --once` budget, so the first round
    // can only succeed by being run again. The relay then serves until the
    // test ends, so this task ending at all means it never came back or
    // stopped.
    let mut back = tokio::spawn({
        let db = db.clone();
        async move {
            tokio::time::sleep(Duration::from_secs(35)).await;
            match held.listen(1024) {
                Ok(listener) => serve_on(listener, &db, std::future::pending()).await,
                Err(e) => format!("listen on the held address: {e}"),
            }
        }
    });
    tokio::select! {
        () = settle(&a, &b) => {}
        ended = &mut back => panic!("the relay on {addr} did not come back: {ended:?}"),
    }

    assert_eq!(b.task(&task).await["title"], "Renew passport");
    back.abort();
}

/// Only the CLI's timed-out message earns a re-run: a `sync --once` that
/// fails any other way (here, a vault with no relay to sync to) fails the
/// step at once, in less than one 30 s budget, naming the command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_that_fails_other_than_by_timing_out_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let mut device = Device::new(&dir.path().join("a"), "http://127.0.0.1:1");
    device.relay = None;
    // Built (or found) first, so the clock below times only the step.
    let _ = sunrise_e2e::cli::binary();

    let started = Instant::now();
    let failed = tokio::spawn(async move { device.sync().await })
        .await
        .expect_err("sync --once with no relay fails");
    let message = failed.into_panic();
    let message = message
        .downcast_ref::<String>()
        .expect("a formatted panic message");

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "failed after {:?}: {message}",
        started.elapsed()
    );
    assert!(
        message.contains("`sunrise sync --once` failed"),
        "{message}"
    );
    assert!(
        !message.contains(&sunrise_cli::i18n::strings::sync::timed_out(30)),
        "{message}"
    );
}
