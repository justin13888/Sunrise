//! `ws_handshake` bench: full client connect + `Hello`/`HelloAck` round-trip
//! against a real `sunrise-server` router booted once on an ephemeral loopback
//! port. Mirrors the driving pattern in `sunrise-server/tests/ws_handshake.rs`
//! (raw `tokio-tungstenite` + wire-protocol frames), measuring one complete
//! connect-and-handshake per iteration.
//!
//! A single multi-threaded Tokio runtime is built for the whole bench; each
//! iteration `block_on`s a fresh WebSocket connection, so the measured cost is
//! TCP connect + WS upgrade + one framed request/response.

#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use futures_util::{SinkExt, StreamExt};
use sunrise_server::{build_router, ServerConfig, ServerState};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};
use tokio::runtime::Runtime;
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn fixture_hello() -> Hello {
    Hello {
        client_app_v: "0.1.0+bench".into(),
        client_platform: "bench".into(),
        wire_proto_supported: vec![1],
        doc_schema_min: 1,
        doc_schema_max: 1,
        crypto_suite_supported: vec![1],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: "01HXTRACE000000000000000000".into(),
    }
}

/// Bind an ephemeral port and start serving the real router; returns its addr.
async fn boot_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = build_router(ServerState::new(ServerConfig::default()));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// One full connect + Hello/HelloAck exchange.
async fn connect_and_handshake(addr: SocketAddr) {
    let url = format!("ws://{addr}/sync");
    let (mut ws, _resp): (Ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("ws connect");

    let mut hello_payload = Vec::new();
    ciborium::ser::into_writer(&fixture_hello(), &mut hello_payload).expect("encode hello");
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hello_payload).expect("frame");
    ws.send(Message::Binary(frame)).await.expect("send hello");

    let ack = ws
        .next()
        .await
        .expect("stream ended")
        .expect("ws recv error");
    let buf = match ack {
        Message::Binary(b) => b,
        other => panic!("unexpected ws message: {other:?}"),
    };
    let (header, _payload) = decode_frame(&buf).expect("decode ack");
    assert_eq!(header.msg_kind, MsgKind::HelloAck);
}

fn bench_ws_handshake(c: &mut Criterion) {
    let rt = Runtime::new().expect("build tokio runtime");
    let addr = rt.block_on(boot_server());

    c.bench_function("ws_handshake", |b| {
        b.iter(|| {
            rt.block_on(connect_and_handshake(addr));
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));
    targets = bench_ws_handshake
}
criterion_main!(benches);
