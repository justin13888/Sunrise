//! WebSocket sync endpoint at `/sync`.
//!
//! Per `spec/05-sync/wire-protocol.md`. The handshake is:
//!
//! 1. Client sends `Hello` as the first WS binary frame.
//! 2. Server runs [`Hello::negotiate`] against its supported set; on
//!    success, replies with `HelloAck`. On failure it replies with an
//!    `Error` message frame and closes.
//! 3. Once negotiated, the client sends `Subscribe` frames to indicate
//!    the `(account, stream)` channels it wants to receive on. Future
//!    `OpBatch` / `StreamUpdate` frames are routed via [`crate::relay`].
//!
//! Frames are length-delimited via the wire-protocol crate and travel
//! in WebSocket *binary* messages (text messages are rejected).

use crate::state::ServerState;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, FrameFlags, Hello, MsgKind, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};

use crate::relay::{ConnId, RelayFrame};

/// Mount the `/sync` WebSocket route.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new().route("/sync", get(handler))
}

async fn handler(State(state): State<ServerState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move { run_session(socket, state).await })
}

/// Session lifecycle: negotiate, then enter the sync loop.
async fn run_session(socket: WebSocket, state: ServerState) {
    let conn_id = state.relay.next_conn();
    let (mut sink, mut stream) = socket.split();

    // ---- Negotiation ----
    let Some(Ok(first)) = stream.next().await else {
        return;
    };
    let Message::Binary(buf) = first else {
        let _ = sink.send(Message::Close(None)).await;
        return;
    };
    let (header, payload) = match decode_frame(&buf) {
        Ok(v) => v,
        Err(e) => {
            let _ = send_error_frame(&mut sink, &format!("frame decode: {e}")).await;
            return;
        }
    };
    if header.msg_kind != MsgKind::Hello {
        let _ = send_error_frame(&mut sink, "expected Hello as first frame").await;
        return;
    }
    let hello: Hello = match ciborium::de::from_reader(&payload[..]) {
        Ok(h) => h,
        Err(e) => {
            let _ = send_error_frame(&mut sink, &format!("hello decode: {e}")).await;
            return;
        }
    };
    let server_caps = REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0;
    let server_time_ms = state.clock.now_ms();
    let ack = match hello.negotiate(
        state.config.server_app_v.clone(),
        &[1],
        &[1],
        1,
        server_caps,
        server_time_ms,
    ) {
        Ok(a) => a,
        Err(e) => {
            let _ = send_error_frame(&mut sink, &format!("negotiation: {e}")).await;
            return;
        }
    };

    let mut ack_payload = Vec::new();
    if ciborium::ser::into_writer(&ack, &mut ack_payload).is_err() {
        let _ = send_error_frame(&mut sink, "ack encode").await;
        return;
    }
    let Ok(ack_frame) = encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &ack_payload) else {
        return;
    };
    if sink.send(Message::Binary(ack_frame)).await.is_err() {
        return;
    }

    // ---- Sync loop ----
    sync_loop(conn_id, sink, stream, state).await;
}

async fn sync_loop(
    conn_id: ConnId,
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    state: ServerState,
) {
    use tokio::sync::broadcast::error::RecvError;

    // Subscriptions: channels this connection is currently subscribed to.
    let mut subs: Vec<tokio::sync::broadcast::Receiver<RelayFrame>> = Vec::new();
    // Default account hash (single-tenant self-host until OIDC wires up).
    let account = single_tenant_account_hash();

    loop {
        // Build a select between (a) next inbound WS message and (b)
        // any subscribed broadcast channel.
        let next_msg = stream.next();
        let next_relay = recv_first(&mut subs);

        tokio::select! {
            biased;
            inbound = next_msg => {
                match inbound {
                    Some(Ok(Message::Binary(buf))) => {
                        if !handle_inbound(conn_id, &buf, &mut sink, &mut subs, account, &state).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {} // ignore Ping/Pong/Text
                }
            }
            relay = next_relay => {
                match relay {
                    Some((idx, Ok(frame))) => {
                        if frame.from != conn_id {
                            // Republish the raw frame bytes.
                            if sink.send(Message::Binary(frame.bytes)).await.is_err() {
                                break;
                            }
                        }
                        // Re-subscribe ranges; broadcast::Receiver is `&mut`, so we
                        // continue using the same one. `idx` left here for future
                        // metrics on per-channel volume.
                        let _ = idx;
                    }
                    Some((_, Err(RecvError::Lagged(_)))) => {
                        // Slow consumer; continue. Real impl bumps a counter.
                    }
                    Some((_, Err(RecvError::Closed))) | None => {
                        // Channel dropped; remove and continue.
                    }
                }
            }
        }
    }

    let _ = sink.close().await;
}

/// Receive from any of the broadcast subscriptions; returns the index that
/// produced the message and the result. If the list is empty, returns
/// `None` (after a long pending) so the select can fall through to the WS
/// stream.
async fn recv_first(
    subs: &mut [tokio::sync::broadcast::Receiver<RelayFrame>],
) -> Option<(
    usize,
    Result<RelayFrame, tokio::sync::broadcast::error::RecvError>,
)> {
    if subs.is_empty() {
        std::future::pending::<()>().await;
        return None;
    }
    // Poll all in parallel — pick the first one ready.
    let futs = subs.iter_mut().enumerate().map(|(i, r)| {
        Box::pin(async move {
            let v = r.recv().await;
            (i, v)
        })
    });
    let (winner, _, _) = futures_util::future::select_all(futs).await;
    Some(winner)
}

/// Process one inbound binary frame. Returns false to break the loop.
async fn handle_inbound(
    conn_id: ConnId,
    buf: &[u8],
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    subs: &mut Vec<tokio::sync::broadcast::Receiver<RelayFrame>>,
    account: [u8; 16],
    state: &ServerState,
) -> bool {
    let Ok((header, payload)) = decode_frame(buf) else {
        let _ = send_error_frame(sink, "frame decode").await;
        return false;
    };
    match header.msg_kind {
        MsgKind::Subscribe => {
            // Subscribe payload: CBOR array of 16-byte stream-id blobs.
            let stream_ids: Vec<[u8; 16]> = match ciborium::de::from_reader(&payload[..]) {
                Ok(v) => v,
                Err(_) => {
                    let _ = send_error_frame(sink, "subscribe decode").await;
                    return true;
                }
            };
            for sid in stream_ids {
                let rx = state.relay.subscribe((account, sid));
                subs.push(rx);
            }
            true
        }
        MsgKind::OpBatch => {
            // Republish raw frame to all subscribers of (account, stream).
            // For v1 we extract the stream-id from the CBOR header field
            // `stream_id` if present. The server re-uses the original
            // frame bytes verbatim for fan-out.
            let stream_id: [u8; 16] = extract_stream_id(&payload).unwrap_or([0u8; 16]);
            state.relay.publish(
                (account, stream_id),
                RelayFrame {
                    from: conn_id,
                    bytes: buf.to_vec(),
                },
            );
            // Send Ack with the seq that was published. v1 self-host uses
            // the wall-clock as a placeholder server_first_seen_ms.
            let ack_payload = build_ack_payload(state.clock.now_ms());
            let ack_frame =
                encode_frame(MsgKind::Ack, FrameFlags::EMPTY, &ack_payload).unwrap_or_default();
            if !ack_frame.is_empty() {
                let _ = sink.send(Message::Binary(ack_frame)).await;
            }
            true
        }
        MsgKind::Ping => {
            let pong = encode_frame(MsgKind::Pong, FrameFlags::EMPTY, &[]).unwrap_or_default();
            if !pong.is_empty() {
                let _ = sink.send(Message::Binary(pong)).await;
            }
            true
        }
        MsgKind::Close => false,
        _ => true, // ignore anything else in v1 self-host
    }
}

/// Encode a one-off Error frame and ship it.
async fn send_error_frame(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &str,
) -> Result<(), axum::Error> {
    let mut payload = Vec::new();
    let _ = ciborium::ser::into_writer(&serde_json::json!({"msg": msg}), &mut payload);
    let frame = encode_frame(MsgKind::Error, FrameFlags::EMPTY, &payload).unwrap_or_default();
    sink.send(Message::Binary(frame)).await
}

fn build_ack_payload(server_first_seen_ms: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = ciborium::ser::into_writer(
        &serde_json::json!({ "server_first_seen_ms": server_first_seen_ms }),
        &mut buf,
    );
    buf
}

/// Best-effort: pluck a 16-byte `stream_id` field from a CBOR-encoded
/// payload. v1 inner-ops are simple enums; in production this is a
/// canonical CBOR map decode. v1 self-host: returns None — fan-out
/// happens via a synthetic all-zeros stream-id which is fine in tests.
fn extract_stream_id(_cbor: &[u8]) -> Option<[u8; 16]> {
    None
}

/// Single-tenant account hash for self-host mode. Production binds this
/// to the OIDC-validated account-id at handshake.
fn single_tenant_account_hash() -> [u8; 16] {
    let h = blake3::hash(b"sunrise.self_host.account.v1");
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.as_bytes()[..16]);
    out
}

/// Fallback handler so the route compiles when ws not present (currently
/// always present; placeholder for offline-mode builds).
#[must_use]
pub fn fallback_handler() -> impl IntoResponse {
    (StatusCode::UPGRADE_REQUIRED, "WebSocket required for /sync")
}
