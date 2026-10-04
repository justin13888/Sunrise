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
    sample_gauges(&state);
    Ok(Text(state.metrics.render()))
}

/// Read every scrape-time gauge from the state that is authoritative for it.
///
/// `docs/06-server/metrics.md` §Rules: a gauge is sampled when it is read,
/// never kept by paired increments that drift. So each one is a measurement
/// taken here, of the thing itself.
fn sample_gauges(state: &ServerState) {
    // Expired and idle sessions are reaped first, so the gauge counts the
    // sessions a client could still use rather than the ones nothing has
    // collected yet.
    state.sessions.collect(state.clock.now_ms());
    #[allow(clippy::cast_precision_loss)]
    state.metrics.set_gauge(
        "sunrise_sync_sessions_active",
        &[],
        state.sessions.len() as f64,
    );

    // An in-memory store has no file to measure, so it has no series rather
    // than a zero that reads as an empty database.
    if let Some(path) = state.config.sqlite_path.as_deref() {
        #[allow(clippy::cast_precision_loss)]
        state
            .metrics
            .set_gauge("sunrise_db_size_bytes", &[], db_size_bytes(path) as f64);
    }
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
