//! [`crate::serve_until`] over a real socket: what a drain does to the work in
//! flight when it begins.
//!
//! The rest of the surface is tested in process through `Service::call`, which
//! needs no port. A drain cannot be: it is the accept loop and the connection
//! that stop, so this boots a listener and talks HTTP/1.1 to it by hand. The
//! shutdown trigger is a future the test resolves rather than a signal, which
//! is the seam `serve_until` takes so that the binary's `SIGTERM` handler and
//! this test drive the same code.

use crate::api::testing::{Client, BEARER};
use crate::{ServerConfig, ServerState};
use kynos::http::{Method, StatusCode};
use std::time::Duration;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_wire_protocol::{Capability, CapabilityBits};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The longest any one step below may take before the test fails. Every step
/// is local and should take milliseconds; the bound only turns a hang into a
/// failure that names the step.
const STEP: Duration = Duration::from_secs(5);

const STREAM_HEX: &str = "11111111111111111111111111111111";

fn hello() -> serde_json::Value {
    serde_json::json!({
        "client_app_v": "0.1.0",
        "client_platform": "test",
        "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
        "doc_schema_min": u32::from(DOC_SCHEMA_V),
        "doc_schema_max": u32::from(DOC_SCHEMA_V),
        "crypto_suite_supported": [u32::from(CRYPTO_SUITE_V)],
        "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0
            | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
        "trace": "01J000000000000000000000000",
    })
}

/// Establish a session and subscribe it to [`STREAM_HEX`], in process.
async fn subscribed_session(client: &Client) -> String {
    let res = client
        .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
        .await;
    res.assert_status(StatusCode::CREATED);
    let id = res.json()["session_id"].as_str().unwrap().to_owned();
    let res = client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({
                "streams": [{ "stream_id": STREAM_HEX, "cursors": [] }]
            })),
            &[("x-sunrise-session", &id)],
        )
        .await;
    res.assert_status(StatusCode::NO_CONTENT);
    id
}

/// Read from `socket` until what has arrived contains `needle`, or EOF.
async fn read_until(socket: &mut TcpStream, seen: &mut Vec<u8>, needle: &str) -> bool {
    let mut chunk = [0_u8; 4096];
    loop {
        if String::from_utf8_lossy(seen).contains(needle) {
            return true;
        }
        let n = tokio::time::timeout(STEP, socket.read(&mut chunk))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "waited for {needle:?}; had: {}",
                    String::from_utf8_lossy(seen)
                )
            })
            .unwrap();
        if n == 0 {
            return false;
        }
        seen.extend_from_slice(&chunk[..n]);
    }
}

/// The issue's acceptance test: an open SSE stream and an in-flight
/// `POST /sync/ops` when shutdown begins. The POST completes, the stream gets
/// a retryable terminal event and ends, and `serve_until` returns `Ok` well
/// inside the drain deadline.
#[tokio::test]
async fn a_drain_finishes_the_inflight_post_and_closes_the_stream() {
    let dir = tempfile::tempdir().unwrap();
    // On disk, so the closing checkpoint has a write-ahead log to fold back.
    let db = dir.path().join("sunrise.db");
    let wal = dir.path().join("sunrise.db-wal");
    let state = ServerState::new(ServerConfig {
        sqlite_path: Some(db.clone()),
        blob_root: Some(dir.path().join("blobs")),
        shutdown_grace_secs: 30,
        ..ServerConfig::default()
    });
    let client = Client::from_state(state.clone());
    let reader = subscribed_session(&client).await;
    let writer = subscribed_session(&client).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::serve_until(state, listener, async {
        let _ = stopped.await;
    }));

    // The stream, read until the replay is done and it is waiting on live
    // traffic — the state a long-lived client spends its life in.
    let mut sse = TcpStream::connect(addr).await.unwrap();
    sse.write_all(
        format!(
            "GET /api/v1/sync/events HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {BEARER}\r\n\
             X-Sunrise-Session: {reader}\r\nAccept: text/event-stream\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut streamed = Vec::new();
    assert!(read_until(&mut sse, &mut streamed, "caught_up").await);

    // The POST, held in flight: `Expect: 100-continue` makes the server say
    // when the handler has started reading the body, and the body is held
    // back until after the drain has begun.
    let body = serde_json::json!({ "stream_id": STREAM_HEX, "batch_id": 1, "ops": [] }).to_string();
    let mut post = TcpStream::connect(addr).await.unwrap();
    post.write_all(
        format!(
            "POST /api/v1/sync/ops HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {BEARER}\r\n\
             X-Sunrise-Session: {writer}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nExpect: 100-continue\r\n\r\n",
            body.len()
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut answered = Vec::new();
    assert!(read_until(&mut post, &mut answered, "100 Continue").await);

    // The schema the store wrote on open is still in the WAL, so the empty
    // log asserted below is the checkpoint's doing.
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "the WAL must hold frames before the drain for the checkpoint to be tested"
    );

    let _ = stop.send(());

    // The stream ends with the retryable close, then the body ends.
    assert!(
        read_until(&mut sse, &mut streamed, "\"kind\":\"closed\"").await,
        "no terminal event before the stream ended: {}",
        String::from_utf8_lossy(&streamed)
    );
    assert!(read_until(&mut sse, &mut streamed, "relay is shutting down").await);
    let text = String::from_utf8_lossy(&streamed).into_owned();
    assert!(text.contains("SYNC_NETWORK_UNAVAILABLE"), "{text}");
    assert!(
        sunrise_error::ErrorCode::SyncNetworkUnavailable.retryable(),
        "the drain's close code must be one a client reconnects on"
    );
    assert!(
        !read_until(&mut sse, &mut streamed, "never sent").await,
        "the stream must end after its terminal event"
    );

    // The POST that was in flight when the drain began still completes.
    post.write_all(body.as_bytes()).await.unwrap();
    answered.clear();
    assert!(read_until(&mut post, &mut answered, "\r\n").await);
    let status = String::from_utf8_lossy(&answered).into_owned();
    assert!(
        status.starts_with("HTTP/1.1 200"),
        "the in-flight POST did not complete: {status}"
    );

    // And the drain completes on its own, long before its 30 s deadline.
    let served = tokio::time::timeout(STEP, server)
        .await
        .expect("serve_until must return once the drain completes")
        .unwrap();
    assert!(served.is_ok(), "{served:?}");

    // The closing checkpoint folded every frame into the database file:
    // `TRUNCATE` leaves the WAL at zero bytes, so the data directory an
    // operator backs up is the one file.
    assert_eq!(
        std::fs::metadata(&wal).map_or(0, |m| m.len()),
        0,
        "the WAL still holds frames after the drain"
    );
    assert!(std::fs::metadata(&db).unwrap().len() > 0);

    // Nothing accepts any more.
    assert!(TcpStream::connect(addr).await.is_err());
}

/// `[server] shutdown_grace_secs` bounds the drain: a request whose body never
/// arrives is cut at the deadline, and `serve_until` says so rather than
/// waiting on it forever.
#[tokio::test]
async fn a_drain_that_outlasts_its_deadline_is_cut() {
    let state = ServerState::new(ServerConfig {
        shutdown_grace_secs: 1,
        ..ServerConfig::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(crate::serve_until(state, listener, async {
        let _ = stopped.await;
    }));

    let mut post = TcpStream::connect(addr).await.unwrap();
    post.write_all(
        format!(
            "POST /api/v1/devices HTTP/1.1\r\nHost: {addr}\r\nAuthorization: {BEARER}\r\n\
             Content-Type: application/json\r\nContent-Length: 64\r\n\
             Expect: 100-continue\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut answered = Vec::new();
    assert!(read_until(&mut post, &mut answered, "100 Continue").await);

    let _ = stop.send(());
    let served = tokio::time::timeout(STEP, server)
        .await
        .expect("the deadline, not the stalled request, ends the drain")
        .unwrap();
    assert!(
        matches!(
            served,
            Err(kynos::Error::Server(
                kynos::server::error::ServerError::ShutdownTimeout { .. }
            ))
        ),
        "{served:?}"
    );
    drop(post);
}
