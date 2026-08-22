//! WebSocket sync endpoint at `/sync`.
//!
//! Per `docs/05-sync/wire-protocol.md`. The handshake is:
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
use sunrise_error::ErrorCode;
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, CaughtUpPayload, ErrorPayload, FrameFlags, Hello,
    MsgKind, OpBatchPayload, SubscribePayload, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};

use crate::relay::{ConnId, RelayFrame};

/// Mount the `/sync` WebSocket route.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new().route("/sync", get(handler))
}

async fn handler(
    State(state): State<ServerState>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Authenticate at the *upgrade*, before a single frame is exchanged. The
    // same pipeline the REST routes run, minus device binding: an upgrade
    // carries no body to sign. Resolving the account here is also what applies
    // `allow_signup` to sync — a server with sign-up off must not relay for an
    // account it never provisioned.
    let account_id = match crate::auth::request::authenticate_token(&state, &headers).await {
        Ok((_subject, account)) => account.account_id,
        Err(e) => {
            state.metrics.incr("sunrise_sync_unauthenticated_total");
            return e.into_response();
        }
    };
    // The channel namespace comes from the verified account, never from the
    // client. See `account_hash`.
    let account = account_hash(&account_id);
    ws.on_upgrade(move |socket| async move { run_session(socket, state, account).await })
}

/// Channel-namespace hash for a resolved account id.
///
/// Subscribe frames carry a stream id but **no account**: the account half of
/// the channel key is derived here from the account the verified token's
/// `(iss, sub)` resolves to, so a client cannot name an account it does not
/// own. That is what makes cross-tenant
/// subscription impossible rather than merely discouraged — the previous code
/// hashed a fixed constant, so every session on the server shared one
/// namespace and any subscriber received every other subscriber's frames.
fn account_hash(account_id: &str) -> [u8; 16] {
    let h = blake3::hash(account_id.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.as_bytes()[..16]);
    out
}

/// Session lifecycle: negotiate, then enter the sync loop.
async fn run_session(socket: WebSocket, state: ServerState, account: [u8; 16]) {
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
            let _ =
                send_error_frame(&mut sink, e.as_error_code(), &format!("frame decode: {e}")).await;
            return;
        }
    };
    if header.msg_kind != MsgKind::Hello {
        let _ = send_error_frame(
            &mut sink,
            ErrorCode::ProtocolBadMagic,
            "expected Hello as first frame",
        )
        .await;
        return;
    }
    let hello: Hello = match ciborium::de::from_reader(&payload[..]) {
        Ok(h) => h,
        Err(e) => {
            let _ = send_error_frame(
                &mut sink,
                ErrorCode::SyncOpInvalid,
                &format!("hello decode: {e}"),
            )
            .await;
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
            // The one WebSocket failure an operator cannot diagnose from the
            // client side: a version/capability mismatch closes the socket
            // before the client has anything to show a user.
            tracing::warn!(
                ev = "srv.ws.rejected",
                err_code = %e.as_error_code(),
                err_kind = "permanent",
                retryable = false,
                account_h = %crate::logging::id_h(&account),
                "handshake rejected"
            );
            let _ =
                send_error_frame(&mut sink, e.as_error_code(), &format!("negotiation: {e}")).await;
            return;
        }
    };

    let mut ack_payload = Vec::new();
    if ciborium::ser::into_writer(&ack, &mut ack_payload).is_err() {
        let _ = send_error_frame(&mut sink, ErrorCode::FatalInternal, "ack encode").await;
        return;
    }
    let Ok(ack_frame) = encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &ack_payload) else {
        return;
    };
    if sink.send(Message::Binary(ack_frame)).await.is_err() {
        return;
    }

    tracing::info!(
        ev = "srv.ws.connect",
        account_h = %crate::logging::id_h(&account),
        wire_v = u64::from(ack.wire_proto),
        crypto_v = u64::from(ack.crypto_suite),
        "relay session opened"
    );

    // ---- Sync loop ----
    sync_loop(conn_id, sink, stream, state, account).await;

    tracing::info!(
        ev = "srv.ws.disconnect",
        account_h = %crate::logging::id_h(&account),
        "relay session closed"
    );
}

async fn sync_loop(
    conn_id: ConnId,
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    state: ServerState,
    account: [u8; 16],
) {
    use tokio::sync::broadcast::error::RecvError;

    // Subscriptions: channels this connection is currently subscribed to.
    let mut subs: Vec<tokio::sync::broadcast::Receiver<RelayFrame>> = Vec::new();
    // `account` is the channel namespace for this session. It was derived in
    // `handler` from the *verified* bearer subject and is not influenced by
    // anything the client sends, so a Subscribe frame can only ever reach
    // channels this account owns.

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
        let _ = send_error_frame(sink, ErrorCode::ProtocolBadMagic, "frame decode").await;
        return false;
    };
    match header.msg_kind {
        MsgKind::Subscribe => {
            let sub = match SubscribePayload::decode(&payload) {
                Ok(v) => v,
                Err(_) => {
                    let _ =
                        send_error_frame(sink, ErrorCode::SyncOpInvalid, "subscribe decode").await;
                    return true;
                }
            };
            tracing::debug!(
                ev = "srv.ws.subscribe",
                n_streams = sub.streams.len() as u64,
                "subscribe frame"
            );
            for entry in sub.streams {
                let sid = entry.stream_id;
                // Atomic snapshot + subscribe: retained frames first, then a
                // CaughtUp marker for this stream, then live frames flow.
                let subscription = state.relay.subscribe((account, sid));
                for retained in subscription.retained {
                    if sink.send(Message::Binary(retained.bytes)).await.is_err() {
                        return false;
                    }
                }
                if !send_caught_up(sink, sid).await {
                    return false;
                }
                subs.push(subscription.rx);
            }
            true
        }
        MsgKind::OpBatch => {
            // Decode the typed payload to route by its stream-id. On decode
            // failure, reply with a coded Error and do NOT fan out.
            let batch = match extract_op_batch(&payload) {
                Some(b) => b,
                None => {
                    let _ = send_error_frame(
                        sink,
                        ErrorCode::SyncOpInvalid,
                        "malformed OpBatch payload",
                    )
                    .await;
                    return true;
                }
            };
            let stream_id = batch.stream_id;
            let server_first_seen_ms = state.clock.now_ms();
            // The relay never decrypts an op, so its whole view of a batch is
            // shape: which stream, how many bytes. That is also everything a
            // fan-out bug needs — a batch that never reaches a peer shows up
            // here as a `srv.relay.fanout` with no matching arrival.
            tracing::debug!(
                ev = "srv.relay.fanout",
                stream_h = %crate::logging::id_h(&stream_id),
                n_bytes = buf.len() as u64,
                "op batch republished"
            );
            // Republish the original raw frame bytes verbatim for fan-out.
            state.relay.publish(
                (account, stream_id),
                RelayFrame {
                    from: conn_id,
                    bytes: buf.to_vec(),
                },
            );
            // Ack the batch with its stream-id + batch-id and the injected
            // clock's first-seen timestamp.
            let ack = AckPayload {
                batch_id: batch.batch_id,
                stream_id,
                server_first_seen_ms,
            };
            if let Ok(bytes) = ack.encode() {
                if let Ok(frame) = encode_frame(MsgKind::Ack, FrameFlags::EMPTY, &bytes) {
                    let _ = sink.send(Message::Binary(frame)).await;
                }
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

/// Encode a one-off coded [`ErrorPayload`] frame and ship it.
async fn send_error_frame(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    code: ErrorCode,
    reason: &str,
) -> Result<(), axum::Error> {
    let payload = ErrorPayload {
        code,
        reason: reason.to_string(),
    };
    let bytes = payload.encode().unwrap_or_default();
    let frame = encode_frame(MsgKind::Error, FrameFlags::EMPTY, &bytes).unwrap_or_default();
    sink.send(Message::Binary(frame)).await
}

/// Send a CaughtUp marker for `stream_id`. Returns false on send failure.
async fn send_caught_up(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    stream_id: [u8; 16],
) -> bool {
    let payload = CaughtUpPayload { stream_id };
    let Ok(bytes) = payload.encode() else {
        return true;
    };
    // CaughtUp rides the `StreamUpdate` kind (see wire-protocol payloads).
    let Ok(frame) = encode_frame(MsgKind::StreamUpdate, FrameFlags::EMPTY, &bytes) else {
        return true;
    };
    sink.send(Message::Binary(frame)).await.is_ok()
}

/// Decode the typed [`OpBatchPayload`] from a frame payload to route it by
/// stream-id. Returns `None` on any decode failure, so the caller replies
/// with a coded Error instead of silently fanning out to all-zeros.
fn extract_op_batch(payload: &[u8]) -> Option<OpBatchPayload> {
    OpBatchPayload::decode(payload).ok()
}

/// Fallback handler so the route compiles when ws not present (currently
/// always present; placeholder for offline-mode builds).
#[must_use]
pub fn fallback_handler() -> impl IntoResponse {
    (StatusCode::UPGRADE_REQUIRED, "WebSocket required for /sync")
}
