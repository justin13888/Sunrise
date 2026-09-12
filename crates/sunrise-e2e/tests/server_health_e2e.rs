//! Boot the server, hit /health, /meta, /metrics, /api/v1/accounts.
//!
//! This is the end-to-end "release-gate" check that the server bin
//! actually serves traffic against the routes wired in `lib.rs`.

#![allow(
    clippy::missing_panics_doc,
    clippy::manual_let_else,
    clippy::items_after_statements,
    clippy::single_match_else,
    clippy::needless_pass_by_value,
    clippy::doc_markdown
)]

use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
use sunrise_server::{ServerConfig, ServerState};

async fn boot() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = ServerState::new(ServerConfig::default());
    tokio::spawn(async move {
        let _ = sunrise_server::serve(state, listener).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    addr
}

async fn get(addr: &std::net::SocketAddr, path: &str) -> (u16, String) {
    let url = format!("http://{addr}{path}");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: e2e\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let s = String::from_utf8_lossy(&buf).to_string();
    let status_line = s.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    let _ = url;
    (status, s)
}

#[tokio::test]
async fn health_meta_metrics_round_trip() {
    let addr = boot().await;

    let (s, body) = get(&addr, "/api/v1/health").await;
    assert_eq!(s, 200, "health body: {body}");

    let (s, body) = get(&addr, "/api/v1/meta").await;
    assert_eq!(s, 200, "meta body: {body}");

    // The substring check this replaces — `contains("wire_proto") ||
    // contains("crypto_suite")` on the raw response string — was satisfied by
    // the field names alone, either one of them sufficing, and never read a
    // value. What a client actually negotiates against is the numbers, so the
    // body is decoded through the generated client and compared with the
    // constants the server re-exports them from.
    let meta = sunrise_relay_client::api::Client::new(&format!("http://{addr}"))
        .expect("a client over the booted relay")
        .meta()
        .await
        .expect("meta must decode into the described shape")
        .into_inner();
    assert_eq!(
        meta.wire_proto_supported,
        vec![i64::from(WIRE_PROTO_V)],
        "the relay must advertise the wire protocol this build speaks"
    );
    assert_eq!(
        meta.crypto_suite_supported,
        vec![i64::from(CRYPTO_SUITE_V)],
        "and the crypto suite"
    );
    assert_eq!(
        meta.doc_schema_floor,
        i64::from(DOC_SCHEMA_FLOOR),
        "and the lowest document schema it still accepts"
    );

    let (s, body) = get(&addr, "/metrics").await;
    assert_eq!(s, 200, "metrics body: {body}");
}

#[tokio::test]
async fn missing_route_404s() {
    let addr = boot().await;
    let (s, _) = get(&addr, "/api/v1/no-such-route").await;
    assert_eq!(s, 404);
}
