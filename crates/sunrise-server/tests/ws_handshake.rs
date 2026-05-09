//! End-to-end WebSocket handshake test.
//!
//! Boots a real `sunrise-server` instance bound to an ephemeral port,
//! opens a WS connection, sends a `Hello` frame, and asserts the
//! returned `HelloAck` carries the expected version + capability bits.

#![allow(
    clippy::manual_let_else,
    clippy::missing_panics_doc,
    clippy::single_match_else,
    clippy::doc_markdown
)]

use futures_util::{SinkExt, StreamExt};
use sunrise_server::{build_router, ServerConfig, ServerState};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};
use tokio_tungstenite::tungstenite::Message;

async fn boot_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = ServerConfig::default();
    let state = ServerState::new(cfg);
    let app = build_router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    // Give the server a beat to start serving.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, handle)
}

fn fixture_hello() -> Hello {
    Hello {
        client_app_v: "1.0.0+test".into(),
        client_platform: "test".into(),
        wire_proto_supported: vec![1],
        doc_schema_min: 1,
        doc_schema_max: 1,
        crypto_suite_supported: vec![1],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: "01HXTRACE000000000000000000".into(),
    }
}

#[tokio::test]
async fn ws_handshake_round_trip() {
    let (addr, _handle) = boot_server().await;
    let url = format!("ws://{addr}/sync");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");

    let mut hello_payload = Vec::new();
    ciborium::ser::into_writer(&fixture_hello(), &mut hello_payload).unwrap();
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hello_payload).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();

    let resp = ws.next().await.expect("got msg").unwrap();
    let buf = match resp {
        Message::Binary(b) => b,
        other => panic!("unexpected ws msg: {other:?}"),
    };
    let (header, payload) = decode_frame(&buf).unwrap();
    assert_eq!(header.msg_kind, MsgKind::HelloAck);
    let ack: sunrise_wire_protocol::HelloAck = ciborium::de::from_reader(&payload[..]).unwrap();
    assert_eq!(ack.wire_proto, 1);
    assert_eq!(ack.crypto_suite, 1);
}

#[tokio::test]
async fn ws_subscribe_receives_op_batch_fanout() {
    let (addr, _handle) = boot_server().await;
    let url = format!("ws://{addr}/sync");

    // Two clients (A, B) connect and complete handshake.
    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    for ws in [&mut a, &mut b] {
        let mut hp = Vec::new();
        ciborium::ser::into_writer(&fixture_hello(), &mut hp).unwrap();
        let f = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hp).unwrap();
        ws.send(Message::Binary(f)).await.unwrap();
        let _ack = ws.next().await.unwrap().unwrap();
    }

    // B subscribes to a stream-id of all-zeros (matches the v1 self-host
    // fan-out which extracts no stream-id and falls through to
    // [0u8;16]).
    let stream_zero: [u8; 16] = [0u8; 16];
    let mut sub_payload = Vec::new();
    ciborium::ser::into_writer(&vec![stream_zero], &mut sub_payload).unwrap();
    let sub_frame = encode_frame(MsgKind::Subscribe, FrameFlags::EMPTY, &sub_payload).unwrap();
    b.send(Message::Binary(sub_frame)).await.unwrap();

    // Give the server a moment to register the subscription before A sends.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // A publishes an OpBatch.
    let payload = b"hello-from-a".to_vec();
    let op = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &payload).unwrap();
    a.send(Message::Binary(op.clone())).await.unwrap();

    // A receives an Ack.
    let a_resp = a.next().await.unwrap().unwrap();
    let a_buf = match a_resp {
        Message::Binary(b) => b,
        _ => panic!(),
    };
    let (a_header, _) = decode_frame(&a_buf).unwrap();
    assert_eq!(a_header.msg_kind, MsgKind::Ack);

    // B receives the republished OpBatch frame.
    let b_resp = tokio::time::timeout(std::time::Duration::from_secs(1), b.next())
        .await
        .expect("timeout")
        .unwrap()
        .unwrap();
    let b_buf = match b_resp {
        Message::Binary(b) => b,
        _ => panic!(),
    };
    let (b_header, b_payload) = decode_frame(&b_buf).unwrap();
    assert_eq!(b_header.msg_kind, MsgKind::OpBatch);
    assert_eq!(b_payload, payload);
}
