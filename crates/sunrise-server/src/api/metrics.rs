//! The operator's `/metrics` exposition, in the Prometheus text format.
//!
//! # Why it is mounted conditionally
//!
//! `docs/06-server/overview.md` says the operator surfaces are loopback only.
//! This route once sat at the router root with no auth layer and no bind check,
//! so on any non-loopback deployment it served the relay's counters — session
//! counts, per-account activity shape — to anyone who asked.
//!
//! Mounting it only when the listener is loopback is the narrowest reading of
//! the documented contract that is also enforceable without connect-info
//! plumbing. An operator who wants it remotely puts a proxy in front, which is
//! what the doc already tells them to do.
//!
//! Conditional mounting rather than a runtime check is deliberate: an operation
//! that is not mounted is absent from the description as well as from the
//! router, so the document does not advertise a surface this deployment
//! refuses to serve.

use crate::api::error::ApiError;
use crate::state::ServerState;
use kynos::di::inject::Inject;
use kynos::extract::body::text::Text;

/// The counter registry, in the Prometheus text exposition format.
///
/// Unauthenticated, and mounted only on a loopback listener — see the module
/// docs for why those two facts belong together.
#[kynos::get("/metrics", operation_id = "metrics")]
pub async fn metrics(Inject(state): Inject<ServerState>) -> Result<Text, ApiError> {
    sample_gauges(&state).await;
    Ok(Text(state.metrics.render()))
}

/// Read every scrape-time gauge from the state that is authoritative for it.
///
/// `docs/06-server/metrics.md` §Rules: a gauge is sampled when it is read,
/// never kept by paired increments that drift. So each one is a measurement
/// taken here, of the thing itself.
async fn sample_gauges(state: &ServerState) {
    // Expired and idle sessions are reaped first, so the gauge counts the
    // sessions a client could still use rather than the ones nothing has
    // collected yet. A session store that does not answer leaves the gauge
    // at its last reading rather than reporting a count nobody took; the
    // failure is logged where it happened.
    if state.sessions.collect(state.clock.now_ms()).await.is_ok() {
        if let Ok(n) = state.sessions.len().await {
            #[allow(clippy::cast_precision_loss)]
            state
                .metrics
                .set_gauge("sunrise_sync_sessions_active", &[], n as f64);
        }
    }

    // An in-memory store has no file to measure, so it has no series rather
    // than a zero that reads as an empty database.
    if let Some(path) = state.config.sqlite_path.as_deref() {
        #[allow(clippy::cast_precision_loss)]
        state
            .metrics
            .set_gauge("sunrise_db_size_bytes", &[], db_size_bytes(path) as f64);
    }

    sample_counts(state);

    // The guards alive right now: one per request inside the handler stack,
    // this scrape included, and one per open event stream.
    #[allow(clippy::cast_precision_loss)]
    {
        let m = &state.metrics;
        m.set_gauge(
            "sunrise_http_in_flight_requests",
            &[],
            state.in_flight.count() as f64,
        );
        m.set_gauge(
            "sunrise_sync_streams_active",
            &[],
            state.drain.open_streams() as f64,
        );
        m.set_counter(
            "sunrise_db_busy_total",
            &[],
            crate::store::busy_total() as f64,
        );
    }

    if let Some(sample) = crate::metrics::process::sample() {
        crate::metrics::process::record(&state.metrics, &sample);
    }
}

/// The row counts: accounts, devices by state, and the durable relay log's
/// bytes, in one read of the store.
///
/// A store that does not answer leaves each gauge at its last reading, as the
/// session gauge above does; the failure is the store's to log.
fn sample_counts(state: &ServerState) {
    let Ok(stats) = state.store.stats() else {
        return;
    };
    #[allow(clippy::cast_precision_loss)]
    let n = |v: u64| v as f64;
    let m = &state.metrics;
    m.set_gauge("sunrise_accounts", &[], n(stats.accounts));
    m.set_gauge(
        "sunrise_devices",
        &[("state", "active")],
        n(stats.devices_active),
    );
    m.set_gauge(
        "sunrise_devices",
        &[("state", "revoked")],
        n(stats.devices_revoked),
    );
    m.set_gauge("sunrise_relay_log_bytes", &[], n(stats.relay_bytes));
}

/// The main database file plus its write-ahead log, in bytes.
///
/// The WAL is counted because under WAL mode it is where recent writes live
/// until a checkpoint, and an operator sizing a disk needs both. A file that
/// cannot be read contributes nothing.
fn db_size_bytes(path: &std::path::Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    [path, std::path::Path::new(&wal)]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use crate::api::testing::Client;
    use crate::ServerConfig;
    use kynos::http::{Method, StatusCode};

    /// Loopback serves it, unauthenticated, as the operator expects.
    #[tokio::test]
    async fn a_loopback_listener_serves_the_counters() {
        let client = Client::new(ServerConfig {
            bind: "127.0.0.1:8443".to_owned(),
            ..ServerConfig::default()
        });
        // Move a counter so the body is not trivially empty.
        client
            .send(Method::GET, "/api/v1/devices", None)
            .await
            .assert_status(StatusCode::OK);

        let res = client.send_as(Method::GET, "/metrics", None, None).await;
        res.assert_status(StatusCode::OK);
        let body = String::from_utf8_lossy(&res.bytes);
        assert!(
            body.contains(
                "sunrise_http_requests_total{endpoint=\"/api/v1/devices\",method=\"GET\",status=\"200\"} 1"
            ),
            "the exposition must carry the counters:\n{body}"
        );
        // The gauges are sampled by the scrape itself, and the process
        // constants are there from the start.
        assert!(body.contains("sunrise_sync_sessions_active 0\n"), "{body}");
        assert!(body.contains("# TYPE sunrise_build_info gauge\n"), "{body}");
        assert!(
            body.contains("# TYPE sunrise_start_time_seconds gauge\n"),
            "{body}"
        );
    }

    /// The scrape-time readings `metrics.md` lists for #435, each after the
    /// action it measures: the bearer verified, the account and its devices
    /// counted by state, the store time of the route that read them, the
    /// scrape itself in flight, and the series that are always present.
    #[tokio::test]
    async fn a_scrape_reads_what_the_requests_before_it_did() {
        let state = crate::ServerState::new(ServerConfig {
            bind: "127.0.0.1:8443".to_owned(),
            ..ServerConfig::default()
        });
        let store = std::sync::Arc::clone(&state.store);
        let client = crate::api::testing::Client::from_state(state);
        let (kept, _) = crate::api::testing::register_device(&client, 1, "kept", None).await;
        let (gone, _) = crate::api::testing::register_device(&client, 2, "gone", None).await;
        let account = store.device_owner(&gone).unwrap().expect("an owner");
        assert_eq!(
            store.device_owner(&kept).unwrap().as_deref(),
            Some(&*account)
        );
        store.revoke_device(&account, &gone, 1).unwrap();
        client
            .send(Method::GET, "/api/v1/devices", None)
            .await
            .assert_status(StatusCode::OK);

        let res = client.send_as(Method::GET, "/metrics", None, None).await;
        res.assert_status(StatusCode::OK);
        let body = String::from_utf8_lossy(&res.bytes);
        for line in [
            "sunrise_auth_verify_total{result=\"ok\"} 3\n",
            "# TYPE sunrise_accounts gauge\nsunrise_accounts 1\n",
            "sunrise_devices{state=\"active\"} 1\n",
            "sunrise_devices{state=\"revoked\"} 1\n",
            "sunrise_relay_log_bytes 0\n",
            "sunrise_sync_streams_active 0\n",
            // This scrape, which is inside the handler stack while it samples.
            "sunrise_http_in_flight_requests 1\n",
            "# TYPE sunrise_db_busy_total counter\n",
            "sunrise_db_query_duration_seconds_count{endpoint=\"/api/v1/devices\"} 3\n",
        ] {
            assert!(body.contains(line), "missing {line:?} in:\n{body}");
        }
        // Once the scrape has ended it is no longer in flight.
        let again = client.send_as(Method::GET, "/metrics", None, None).await;
        assert!(
            String::from_utf8_lossy(&again.bytes).contains("sunrise_http_in_flight_requests 1\n"),
            "only the second scrape is in flight"
        );
    }

    /// A file-backed store reports its size; the WAL is part of it.
    #[tokio::test]
    async fn a_file_backed_store_reports_its_size() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let client = Client::new(ServerConfig {
            bind: "127.0.0.1:8443".to_owned(),
            sqlite_path: Some(dir.path().join("relay.sqlite3")),
            ..ServerConfig::default()
        });
        let res = client.send_as(Method::GET, "/metrics", None, None).await;
        res.assert_status(StatusCode::OK);
        let body = String::from_utf8_lossy(&res.bytes);
        let size: f64 = body
            .lines()
            .find_map(|l| l.strip_prefix("sunrise_db_size_bytes "))
            .unwrap_or_else(|| panic!("no db size in:\n{body}"))
            .parse()
            .expect("a number");
        assert!(size > 0.0, "{body}");
    }

    /// A non-loopback listener does not serve it at all.
    ///
    /// The counters describe session counts and per-account activity shape, and
    /// this route once sat at the router root with no auth and no bind check.
    #[tokio::test]
    async fn a_public_listener_does_not_mount_it() {
        let client = Client::new(ServerConfig {
            bind: "0.0.0.0:8443".to_owned(),
            // A public bind with the single-tenant verifier is refused by
            // `validate`, which this test does not call: the question here is
            // only which routes the router carries.
            ..ServerConfig::default()
        });

        client
            .send_as(Method::GET, "/metrics", None, None)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }
}
