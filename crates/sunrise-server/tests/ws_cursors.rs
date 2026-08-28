//! Cursor-filtered replay and the eviction gap report, over a real socket.
//!
//! Issue #19: the relay took the per-device cursors every client already sends
//! in `SubscribeEntry.cursors` and threw them away, replaying the whole retained
//! ring. Two consequences, both tested here:
//!
//! 1. A subscriber that says how far it got is served only what comes after.
//! 2. Past the ring bounds the relay used to under-deliver in **silence**. It
//!    now sends a typed `SYNC_CURSOR_GAP` before the partial replay.
//!
//! The op envelopes here are header-shaped rather than real sealed envelopes:
//! that is exactly the surface the relay reads (`stream_id` / `device_id` /
//! `seq`, all cleartext), and building real ones would put `sunrise-crypto` —
//! the crate holding the keys a relay must not have — into the server's test
//! graph.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use futures_util::{SinkExt, StreamExt};
use sunrise_cbor::magic::{write_prefix, MagicKind, MAGIC_LEN};
use sunrise_cbor::version::ENVELOPE_FORMAT_V;
use sunrise_error::ErrorCode;
use sunrise_server::relay::RingCaps;
use sunrise_server::{build_router, ServerConfig, ServerState};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, CursorEntry, ErrorPayload, FrameFlags, Hello, MsgKind,
    OpBatchPayload, SubscribeEntry, SubscribePayload, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const STREAM: [u8; 16] = [0x11; 16];
const DEVICE: [u8; 16] = [0x22; 16];

async fn boot(caps: RingCaps) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = ServerState::new(ServerConfig::default()).with_ring_caps(caps);
    let app = build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    addr
}

async fn connect(addr: std::net::SocketAddr) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(&format!("ws://{addr}/sync"))
        .await
        .expect("connect");
    let hello = Hello {
        client_app_v: "1.0.0+test".into(),
        client_platform: "test".into(),
        wire_proto_supported: vec![1],
        doc_schema_min: 1,
        doc_schema_max: 1,
        crypto_suite_supported: vec![1],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: String::new(),
    };
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&hello, &mut buf).unwrap();
    ws.send(Message::Binary(
        encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &buf).unwrap(),
    ))
    .await
    .unwrap();
    let (hdr, _) = decode_frame(&next_binary(&mut ws).await).unwrap();
    assert_eq!(hdr.msg_kind, MsgKind::HelloAck);
    ws
}

/// An envelope carrying only the cleartext routing header the relay reads,
/// plus a payload-shaped filler it must ignore.
fn envelope(device: [u8; 16], seq: u64) -> Vec<u8> {
    use ciborium::value::Value;
    let entries: Vec<(Value, Value)> = vec![
        (Value::Integer(1.into()), Value::Integer(3.into())),
        (Value::Integer(2.into()), Value::Bytes(STREAM.to_vec())),
        (Value::Integer(3.into()), Value::Bytes(device.to_vec())),
        (Value::Integer(4.into()), Value::Integer(seq.into())),
        (
            Value::Integer(10.into()),
            Value::Bytes(vec![u8::try_from(seq & 0xff).unwrap(); 32]),
        ),
    ];
    let mut out = vec![0u8; MAGIC_LEN];
    write_prefix(&mut out, MagicKind::OpEnvelope, ENVELOPE_FORMAT_V);
    ciborium::ser::into_writer(&Value::Map(entries), &mut out).unwrap();
    out
}

/// Publish one op at `seq` and consume its Ack.
async fn publish(ws: &mut Ws, seq: u64) {
    let batch = OpBatchPayload {
        ops: vec![envelope(DEVICE, seq)],
        batch_id: seq,
        stream_id: STREAM,
    };
    ws.send(Message::Binary(
        encode_frame(
            MsgKind::OpBatch,
            FrameFlags::EMPTY,
            &batch.encode().unwrap(),
        )
        .unwrap(),
    ))
    .await
    .unwrap();
    let (hdr, _) = decode_frame(&next_binary(ws).await).unwrap();
    assert_eq!(hdr.msg_kind, MsgKind::Ack);
}

async fn subscribe(ws: &mut Ws, cursor: Option<u64>) {
    let cursors = cursor.map_or_else(Vec::new, |last_applied_seq| {
        vec![CursorEntry {
            device_id: DEVICE,
            last_applied_seq,
        }]
    });
    let sub = SubscribePayload {
        streams: vec![SubscribeEntry {
            cursors,
            stream_id: STREAM,
        }],
    };
    ws.send(Message::Binary(
        encode_frame(
            MsgKind::Subscribe,
            FrameFlags::EMPTY,
            &sub.encode().unwrap(),
        )
        .unwrap(),
    ))
    .await
    .unwrap();
}

async fn next_binary(ws: &mut Ws) -> Vec<u8> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("timeout")
        .expect("stream ended")
        .expect("ws error")
    {
        Message::Binary(b) => b,
        other => panic!("unexpected ws message: {other:?}"),
    }
}

/// What one subscriber receives up to and including its CaughtUp marker:
/// the seqs replayed, and any coded error that preceded them.
struct Replay {
    seqs: Vec<u64>,
    errors: Vec<ErrorPayload>,
}

async fn drain_until_caught_up(ws: &mut Ws) -> Replay {
    let mut seqs = Vec::new();
    let mut errors = Vec::new();
    loop {
        let buf = next_binary(ws).await;
        let (hdr, payload) = decode_frame(&buf).unwrap();
        match hdr.msg_kind {
            MsgKind::StreamUpdate => return Replay { seqs, errors },
            MsgKind::OpBatch => {
                let batch = OpBatchPayload::decode(&payload).unwrap();
                for op in &batch.ops {
                    seqs.push(sunrise_cbor::decode_envelope_header(op).unwrap().seq);
                }
            }
            MsgKind::Error => errors.push(ErrorPayload::decode(&payload).unwrap()),
            other => panic!("unexpected kind {other:?}"),
        }
    }
}

/// The headline fix: a returning device is served only what it has not seen.
#[tokio::test]
async fn a_cursor_narrows_the_replay_to_what_the_subscriber_missed() {
    let addr = boot(RingCaps::default()).await;
    let mut writer = connect(addr).await;
    for seq in 1..=5 {
        publish(&mut writer, seq).await;
    }

    let mut reader = connect(addr).await;
    subscribe(&mut reader, Some(3)).await;
    let replay = drain_until_caught_up(&mut reader).await;
    assert_eq!(replay.seqs, vec![4, 5], "only the unseen ops are replayed");
    assert!(replay.errors.is_empty());
}

/// A subscriber with nothing still gets everything: filtering must never be a
/// silent reduction of what a fresh device sees.
#[tokio::test]
async fn no_cursor_still_replays_the_whole_ring() {
    let addr = boot(RingCaps::default()).await;
    let mut writer = connect(addr).await;
    for seq in 1..=3 {
        publish(&mut writer, seq).await;
    }

    let mut reader = connect(addr).await;
    subscribe(&mut reader, None).await;
    assert_eq!(drain_until_caught_up(&mut reader).await.seqs, vec![1, 2, 3]);
}

/// Issue #19's actual defect. Past the ring bounds the ops are gone; what
/// changed is that the subscriber is told, with a code it can act on, instead
/// of being handed a short replay and a CaughtUp marker.
#[tokio::test]
async fn a_cursor_past_the_ring_bound_gets_a_typed_gap_not_silence() {
    let addr = boot(RingCaps {
        max_frames: 2,
        max_bytes: usize::MAX,
    })
    .await;
    let mut writer = connect(addr).await;
    for seq in 1..=5 {
        publish(&mut writer, seq).await;
    }

    let mut reader = connect(addr).await;
    subscribe(&mut reader, Some(1)).await;
    let replay = drain_until_caught_up(&mut reader).await;

    assert_eq!(
        replay.errors.len(),
        1,
        "exactly one gap report: {:?}",
        replay.errors
    );
    assert_eq!(replay.errors[0].code, ErrorCode::SyncCursorGap);
    assert_eq!(
        replay.seqs,
        vec![4, 5],
        "what survives is still delivered; seqs 2-3 are unrecoverable"
    );
}

/// A client that is fully caught up must not be told to resync merely because
/// the ring turned over behind it — that would fire on every healthy long-lived
/// session.
#[tokio::test]
async fn eviction_of_already_applied_ops_is_not_a_gap() {
    let addr = boot(RingCaps {
        max_frames: 2,
        max_bytes: usize::MAX,
    })
    .await;
    let mut writer = connect(addr).await;
    for seq in 1..=5 {
        publish(&mut writer, seq).await;
    }

    let mut reader = connect(addr).await;
    subscribe(&mut reader, Some(5)).await;
    let replay = drain_until_caught_up(&mut reader).await;
    assert!(replay.errors.is_empty(), "{:?}", replay.errors);
    assert!(replay.seqs.is_empty());
}

/// A mid-session re-subscribe is how a client closes a gap it detected. It must
/// replace the existing receiver, not add a second one: two receivers on one
/// channel would duplicate every live frame for the rest of the session.
#[tokio::test]
async fn resubscribing_replaces_the_receiver_rather_than_duplicating_it() {
    let addr = boot(RingCaps::default()).await;
    let mut reader = connect(addr).await;
    subscribe(&mut reader, None).await;
    assert!(drain_until_caught_up(&mut reader).await.seqs.is_empty());

    subscribe(&mut reader, None).await;
    assert!(drain_until_caught_up(&mut reader).await.seqs.is_empty());

    let mut writer = connect(addr).await;
    publish(&mut writer, 1).await;
    publish(&mut writer, 2).await;

    // Both live frames arrive exactly once, in order. A duplicated receiver
    // would deliver seq 1 twice before seq 2.
    let mut seen = Vec::new();
    for _ in 0..2 {
        let buf = next_binary(&mut reader).await;
        let (hdr, payload) = decode_frame(&buf).unwrap();
        assert_eq!(hdr.msg_kind, MsgKind::OpBatch);
        let batch = OpBatchPayload::decode(&payload).unwrap();
        seen.push(
            sunrise_cbor::decode_envelope_header(&batch.ops[0])
                .unwrap()
                .seq,
        );
    }
    assert_eq!(seen, vec![1, 2]);
}
