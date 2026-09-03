//! `sync_session` bench: one full session establishment against a real
//! `sunrise-server` booted once on an ephemeral loopback port.
//!
//! Was `ws_handshake`, measuring a TCP connect plus a WebSocket upgrade plus a
//! framed `Hello`/`HelloAck` exchange. ADR-0023 replaced that with
//! `POST /sync/session`, so the same thing is measured through the transport
//! that replaced the socket: one `SseTransport` per iteration, one `Hello`
//! frame in, one `HelloAck` frame out.
//!
//! The comparison across the transport change is not apples to apples and the
//! baseline should be re-taken rather than read against the old number: a
//! WebSocket upgrade and an HTTP POST are different amounts of work, and the
//! bench is informational (`bench/baseline.json`) rather than a merge gate.
//!
//! A single multi-threaded Tokio runtime is built for the whole bench; each
//! iteration `block_on`s a fresh transport, so the measured cost is TCP connect
//! plus one request/response.

#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_server::{ServerConfig, ServerState};
use sunrise_sync::{SseTransport, Transport};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};
use tokio::runtime::Runtime;

fn fixture_hello() -> Hello {
    Hello {
        client_app_v: "0.1.0+bench".into(),
        client_platform: "bench".into(),
        // Read from the constants, not pinned. A literal here made the bench
        // fail on `CRYPTO_SUITE_V`'s 1 -> 2 bump with `VALIDATION_INVALID`,
        // which reads as a broken server rather than a stale fixture.
        wire_proto_supported: vec![u32::from(WIRE_PROTO_V)],
        doc_schema_min: u32::from(DOC_SCHEMA_FLOOR),
        doc_schema_max: u32::from(DOC_SCHEMA_V),
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: "01HXTRACE000000000000000000".into(),
    }
}

/// Bind an ephemeral port and start serving the real surface; returns its addr.
async fn boot_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let state = ServerState::new(ServerConfig::default());
    tokio::spawn(async move {
        let _ = sunrise_server::serve(state, listener).await;
    });
    // The listener is bound before the task is spawned, so a connection cannot
    // be refused — but the accept loop has not necessarily started, and the
    // first iteration is the one that would pay for it. Yield until it answers.
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    addr
}

/// One full establishment: `Hello` in, `HelloAck` out.
async fn establish(addr: SocketAddr) {
    let mut transport = SseTransport::connect(&format!("http://{addr}"));

    let mut hello_payload = Vec::new();
    ciborium::ser::into_writer(&fixture_hello(), &mut hello_payload).expect("encode hello");
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hello_payload).expect("frame");
    transport.send_frame(frame).await.expect("send hello");

    let buf = transport
        .recv_frame()
        .await
        .expect("recv error")
        .expect("stream ended");
    let (header, _payload) = decode_frame(&buf).expect("decode ack");
    assert_eq!(header.msg_kind, MsgKind::HelloAck);
}

fn bench_sync_session(c: &mut Criterion) {
    let rt = Runtime::new().expect("build tokio runtime");
    let addr = rt.block_on(boot_server());

    c.bench_function("sync_session", |b| {
        b.iter(|| {
            rt.block_on(establish(addr));
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(30)
        .measurement_time(Duration::from_secs(6));
    targets = bench_sync_session
}
criterion_main!(benches);
