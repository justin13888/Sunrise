//! End-to-end WebSocket sync tests.
//!
//! Boots a real `sunrise-server` instance bound to an ephemeral port and
//! drives the `/sync` endpoint: handshake, OpBatch fan-out + Ack, malformed
//! payload rejection, late-subscriber replay + CaughtUp marker.

#![allow(
    clippy::manual_let_else,
    clippy::missing_panics_doc,
    clippy::single_match_else,
    clippy::doc_markdown
)]

use futures_util::{SinkExt, StreamExt};
use sunrise_server::{build_router, ServerConfig, ServerState};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, CaughtUpPayload, ErrorPayload, FrameFlags, Hello,
    MsgKind, OpBatchPayload, SubscribeEntry, SubscribePayload, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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

async fn connect(addr: std::net::SocketAddr) -> Ws {
    let url = format!("ws://{addr}/sync");
    let (ws, _resp) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    ws
}

/// Send Hello and consume the HelloAck.
async fn handshake(ws: &mut Ws) {
    let mut hp = Vec::new();
    ciborium::ser::into_writer(&fixture_hello(), &mut hp).unwrap();
    let f = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hp).unwrap();
    ws.send(Message::Binary(f)).await.unwrap();
    let ack = ws.next().await.unwrap().unwrap();
    let buf = binary(ack);
    let (hdr, _) = decode_frame(&buf).unwrap();
    assert_eq!(hdr.msg_kind, MsgKind::HelloAck);
}

async fn subscribe(ws: &mut Ws, stream_id: [u8; 16]) {
    let sub = SubscribePayload {
        streams: vec![SubscribeEntry {
            cursors: vec![],
            stream_id,
        }],
    };
    let frame = encode_frame(
        MsgKind::Subscribe,
        FrameFlags::EMPTY,
        &sub.encode().unwrap(),
    )
    .unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();
}

fn op_batch_frame(stream_id: [u8; 16], batch_id: u64, op: &[u8]) -> Vec<u8> {
    let batch = OpBatchPayload {
        ops: vec![op.to_vec()],
        batch_id,
        stream_id,
    };
    encode_frame(
        MsgKind::OpBatch,
        FrameFlags::EMPTY,
        &batch.encode().unwrap(),
    )
    .unwrap()
}

fn binary(msg: Message) -> Vec<u8> {
    match msg {
        Message::Binary(b) => b,
        other => panic!("unexpected ws msg: {other:?}"),
    }
}

async fn next_binary(ws: &mut Ws) -> Vec<u8> {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .expect("timeout waiting for frame")
        .expect("stream ended")
        .expect("ws error");
    binary(msg)
}

#[tokio::test]
async fn ws_handshake_round_trip() {
    let (addr, _handle) = boot_server().await;
    let mut ws = connect(addr).await;

    let mut hello_payload = Vec::new();
    ciborium::ser::into_writer(&fixture_hello(), &mut hello_payload).unwrap();
    let frame = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hello_payload).unwrap();
    ws.send(Message::Binary(frame)).await.unwrap();

    let buf = next_binary(&mut ws).await;
    let (header, payload) = decode_frame(&buf).unwrap();
    assert_eq!(header.msg_kind, MsgKind::HelloAck);
    let ack: sunrise_wire_protocol::HelloAck = ciborium::de::from_reader(&payload[..]).unwrap();
    assert_eq!(ack.wire_proto, 1);
    assert_eq!(ack.crypto_suite, 1);
}

#[tokio::test]
async fn ws_subscribe_receives_op_batch_fanout() {
    let (addr, _handle) = boot_server().await;
    let mut a = connect(addr).await;
    let mut b = connect(addr).await;
    handshake(&mut a).await;
    handshake(&mut b).await;

    let stream: [u8; 16] = [3u8; 16];
    subscribe(&mut b, stream).await;

    // B first sees the CaughtUp marker (empty backlog) for the stream.
    let cu_buf = next_binary(&mut b).await;
    let (cu_hdr, cu_payload) = decode_frame(&cu_buf).unwrap();
    assert_eq!(cu_hdr.msg_kind, MsgKind::StreamUpdate);
    let cu = CaughtUpPayload::decode(&cu_payload).unwrap();
    assert_eq!(cu.stream_id, stream);

    // A publishes an OpBatch on the stream.
    let op = b"hello-from-a".to_vec();
    let frame = op_batch_frame(stream, 7, &op);
    a.send(Message::Binary(frame.clone())).await.unwrap();

    // A receives an Ack with the matching stream-id + batch-id.
    let a_buf = next_binary(&mut a).await;
    let (a_hdr, a_payload) = decode_frame(&a_buf).unwrap();
    assert_eq!(a_hdr.msg_kind, MsgKind::Ack);
    let ack = AckPayload::decode(&a_payload).unwrap();
    assert_eq!(ack.stream_id, stream);
    assert_eq!(ack.batch_id, 7);

    // B receives the republished OpBatch frame, byte-identical to A's.
    let b_buf = next_binary(&mut b).await;
    assert_eq!(b_buf, frame, "fan-out frame must be byte-identical");
    let (b_hdr, _) = decode_frame(&b_buf).unwrap();
    assert_eq!(b_hdr.msg_kind, MsgKind::OpBatch);
}

#[tokio::test]
async fn ws_op_batch_not_echoed_to_sender() {
    let (addr, _handle) = boot_server().await;
    let mut a = connect(addr).await;
    let mut b = connect(addr).await;
    handshake(&mut a).await;
    handshake(&mut b).await;

    let stream: [u8; 16] = [4u8; 16];
    // Both subscribe so A is a live receiver too; its own frame must be
    // filtered out of the live path by ConnId.
    subscribe(&mut a, stream).await;
    let _ = next_binary(&mut a).await; // A's CaughtUp
    subscribe(&mut b, stream).await;
    let _ = next_binary(&mut b).await; // B's CaughtUp

    let frame = op_batch_frame(stream, 1, b"op");
    a.send(Message::Binary(frame)).await.unwrap();

    // A's next frame is its Ack — NOT its own OpBatch echoed back.
    let a_buf = next_binary(&mut a).await;
    let (a_hdr, _) = decode_frame(&a_buf).unwrap();
    assert_eq!(a_hdr.msg_kind, MsgKind::Ack);

    // B receives the fan-out.
    let b_buf = next_binary(&mut b).await;
    let (b_hdr, _) = decode_frame(&b_buf).unwrap();
    assert_eq!(b_hdr.msg_kind, MsgKind::OpBatch);
}

#[tokio::test]
async fn ws_malformed_op_batch_nacked_no_fanout() {
    let (addr, _handle) = boot_server().await;
    let mut a = connect(addr).await;
    let mut b = connect(addr).await;
    handshake(&mut a).await;
    handshake(&mut b).await;

    let stream: [u8; 16] = [0u8; 16];
    subscribe(&mut b, stream).await;
    let _ = next_binary(&mut b).await; // B's CaughtUp

    // A sends an OpBatch frame whose payload is not a valid OpBatchPayload.
    let bad = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, b"not-cbor").unwrap();
    a.send(Message::Binary(bad)).await.unwrap();

    // A receives a coded Error, not an Ack.
    let a_buf = next_binary(&mut a).await;
    let (a_hdr, a_payload) = decode_frame(&a_buf).unwrap();
    assert_eq!(a_hdr.msg_kind, MsgKind::Error);
    let err = ErrorPayload::decode(&a_payload).unwrap();
    assert_eq!(err.code, sunrise_error::ErrorCode::SyncOpInvalid);

    // Connection stays usable: a subsequent valid OpBatch is acked and
    // fanned out to B.
    let good = op_batch_frame(stream, 9, b"ok");
    a.send(Message::Binary(good.clone())).await.unwrap();
    let ack_buf = next_binary(&mut a).await;
    let (ack_hdr, ack_payload) = decode_frame(&ack_buf).unwrap();
    assert_eq!(ack_hdr.msg_kind, MsgKind::Ack);
    assert_eq!(AckPayload::decode(&ack_payload).unwrap().batch_id, 9);

    // B never received a frame for the malformed batch — its first (and
    // only) OpBatch is the good one.
    let b_buf = next_binary(&mut b).await;
    assert_eq!(b_buf, good);
}

#[tokio::test]
async fn ws_late_subscriber_replays_then_caught_up_then_live() {
    let (addr, _handle) = boot_server().await;
    let mut a = connect(addr).await;
    let mut b = connect(addr).await;
    handshake(&mut a).await;
    handshake(&mut b).await;

    let stream: [u8; 16] = [5u8; 16];

    // A publishes 3 batches before B subscribes; drain A's 3 Acks.
    let mut frames = Vec::new();
    for i in 0..3u64 {
        let f = op_batch_frame(stream, i, format!("op-{i}").as_bytes());
        a.send(Message::Binary(f.clone())).await.unwrap();
        frames.push(f);
        let ack_buf = next_binary(&mut a).await;
        let (ack_hdr, _) = decode_frame(&ack_buf).unwrap();
        assert_eq!(ack_hdr.msg_kind, MsgKind::Ack);
    }

    // B subscribes late → receives the 3 retained frames in order.
    subscribe(&mut b, stream).await;
    for expected in &frames {
        let got = next_binary(&mut b).await;
        assert_eq!(
            &got, expected,
            "retained replay must be byte-identical + ordered"
        );
        let (hdr, _) = decode_frame(&got).unwrap();
        assert_eq!(hdr.msg_kind, MsgKind::OpBatch);
    }

    // Then the CaughtUp marker for the stream.
    let cu_buf = next_binary(&mut b).await;
    let (cu_hdr, cu_payload) = decode_frame(&cu_buf).unwrap();
    assert_eq!(cu_hdr.msg_kind, MsgKind::StreamUpdate);
    assert_eq!(
        CaughtUpPayload::decode(&cu_payload).unwrap().stream_id,
        stream
    );

    // Then a live 4th batch flows through.
    let live = op_batch_frame(stream, 3, b"op-live");
    a.send(Message::Binary(live.clone())).await.unwrap();
    let _a_ack = next_binary(&mut a).await;
    let b_live = next_binary(&mut b).await;
    assert_eq!(b_live, live);
}
