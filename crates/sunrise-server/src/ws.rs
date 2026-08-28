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
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
use sunrise_error::ErrorCode;
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, Capability, CapabilityBits, CaughtUpPayload,
    ClosePayload, ErrorPayload, FrameFlags, Hello, MsgKind, OpBatchPayload, RefreshTokenAckPayload,
    RefreshTokenPayload, SubscribeEntry, SubscribePayload, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};

use crate::auth::Verified;
use crate::relay::{ConnId, CursorGap, FrameHead, RelayFrame};
use std::collections::HashMap;

/// Mount the `/sync` WebSocket route.
#[must_use]
pub fn router() -> Router<ServerState> {
    Router::new().route("/sync", get(handler))
}

/// The credential state of one live session.
///
/// A `/sync` session is long-lived and its bearer is not: the upgrade is the
/// only place the token is presented, so without this the session simply
/// outlived its own credential for as long as the socket stayed open. Holding
/// the deadline here is what lets the loop end the session when the token
/// does.
#[derive(Debug, Clone)]
struct SessionAuth {
    /// Hashed account id — the relay channel namespace for this session.
    account: [u8; 16],
    /// `(iss, sub)`, flattened — see [`Verified::subject`]. Fixed for the life
    /// of the session.
    ///
    /// A refresh must name this same principal. On the wire, renewing a
    /// credential and changing who you are look identical, and the second is
    /// an account takeover: the relay channel namespace was derived from the
    /// upgrade's token and is never re-derived, so a session that accepted a
    /// token for a different principal would keep relaying the *first*
    /// account's traffic under the second one's authority.
    principal: String,
    /// The `device_id` claim the session opened with, if any. A refresh may
    /// not change it either, for the same reason and by the same argument the
    /// REST path makes in `auth::request::bind_device`.
    device_id: Option<String>,
    /// Wall-clock ms at which the current token expires; `None` for a verifier
    /// that issues no deadline (self-host).
    expires_at_ms: Option<u64>,
    /// The same instant on the monotonic timer, for the loop's expiry arm.
    ///
    /// Derived once per token rather than recomputed per iteration: a deadline
    /// rebuilt from `now + remaining` on every loop pass is a deadline a busy
    /// session can postpone forever.
    deadline: Option<tokio::time::Instant>,
}

impl SessionAuth {
    fn new(account: [u8; 16], verified: &Verified, now_ms: u64) -> Self {
        Self {
            account,
            principal: verified.subject.principal_key(),
            device_id: verified.subject.device_id.clone(),
            expires_at_ms: verified.expires_at_ms,
            deadline: verified
                .expires_at_ms
                .map(|at| tokio::time::Instant::now() + expires_in(at, now_ms)),
        }
    }

    /// Whether `verified` names the same principal and device this session
    /// opened as.
    fn is_same_identity(&self, verified: &Verified) -> bool {
        verified.subject.principal_key() == self.principal
            && verified.subject.device_id == self.device_id
    }

    /// Move the deadline out to a freshly verified token's `exp`.
    fn renew(&mut self, verified: &Verified, now_ms: u64) {
        self.expires_at_ms = verified.expires_at_ms;
        self.deadline = verified
            .expires_at_ms
            .map(|at| tokio::time::Instant::now() + expires_in(at, now_ms));
    }

    /// Whether the token has expired as of `now_ms`.
    fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| now_ms >= at)
    }
}

/// How long is left on a deadline, floored at zero.
fn expires_in(at_ms: u64, now_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(at_ms.saturating_sub(now_ms))
}

/// The session's expiry arm. Never resolves when there is no deadline, which
/// is what keeps a self-host session alive.
async fn expired(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
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
    let (verified, account_id) =
        match crate::auth::request::authenticate_token(&state, &headers).await {
            Ok((v, account)) => (v, account.account_id),
            Err(e) => {
                state.metrics.incr("sunrise_sync_unauthenticated_total");
                return e.into_response();
            }
        };
    // The channel namespace comes from the verified account, never from the
    // client. See `account_hash`.
    let account = account_hash(&account_id);
    let auth = SessionAuth::new(account, &verified, state.clock.now_ms());
    ws.on_upgrade(move |socket| async move { run_session(socket, state, auth).await })
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
async fn run_session(socket: WebSocket, state: ServerState, auth: SessionAuth) {
    let conn_id = state.relay.next_conn();
    let account = auth.account;
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
    // `SrvTokenRefresh` is optional, so it rides on top of the required set.
    // The client only sends `0x12` if it sees this bit come back agreed, which
    // is what stops a refresh from being silently swallowed by a server that
    // predates the frame.
    let server_caps = REQUIRED_CLIENT_BITS.0
        | REQUIRED_SERVER_BITS.0
        | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0;
    let server_time_ms = state.clock.now_ms();
    let ack = match hello.negotiate(
        state.config.server_app_v.clone(),
        &[u32::from(WIRE_PROTO_V)],
        &[u32::from(CRYPTO_SUITE_V)],
        u32::from(DOC_SCHEMA_FLOOR),
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
    sync_loop(conn_id, sink, stream, state, auth).await;

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
    mut auth: SessionAuth,
) {
    use tokio::sync::broadcast::error::RecvError;

    // Subscriptions: channels this connection is currently subscribed to,
    // keyed by stream so a re-Subscribe REPLACES rather than duplicates. A
    // client re-subscribes mid-session to close a cursor gap; pushing a second
    // receiver for the same stream would deliver every subsequent live frame
    // twice for the rest of the session.
    let mut subs: Vec<([u8; 16], tokio::sync::broadcast::Receiver<RelayFrame>)> = Vec::new();
    // `auth.account` is the channel namespace for this session. It was derived
    // in `handler` from the *verified* bearer subject and is not influenced by
    // anything the client sends, so a Subscribe frame can only ever reach
    // channels this account owns. Nothing — the refresh path included — ever
    // re-derives it.

    loop {
        // Build a select between (a) next inbound WS message and (b)
        // any subscribed broadcast channel.
        let next_msg = stream.next();
        let next_relay = recv_first(&mut subs);
        let deadline = expired(auth.deadline);

        tokio::select! {
            biased;
            inbound = next_msg => {
                match inbound {
                    Some(Ok(Message::Binary(buf))) => {
                        // `docs/06-server/auth.md`: the server checks `exp` on
                        // every inbound message. Cheap, and it does not depend
                        // on the timer arm having fired — the two disagree if
                        // the injected clock and the monotonic timer disagree,
                        // and the credential's own clock is the one that
                        // decides.
                        if auth.is_expired(state.clock.now_ms()) {
                            end_expired(&mut sink, &state, &auth).await;
                            break;
                        }
                        if !handle_inbound(conn_id, &buf, &mut sink, &mut subs, &mut auth, &state).await {
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
            // An idle session has no inbound frames to check `exp` against, so
            // without this arm a client that connects and then goes quiet keeps
            // an authenticated socket open indefinitely on a dead credential.
            () = deadline => {
                end_expired(&mut sink, &state, &auth).await;
                break;
            }
        }
    }

    // Orderly teardown, and it is load-bearing rather than tidiness. Closing a
    // socket that still holds unread *inbound* bytes makes the kernel send RST
    // instead of FIN, and an RST discards whatever the peer has not yet read —
    // including the frames just written to tell it why the session ended. That
    // is exactly the shape of a server-initiated close: the client is
    // mid-sentence when we stop listening. Reading to the peer's own close
    // first is what makes a typed `Close` actually arrive; it showed up as a
    // roughly 1-in-100 loss of the `AUTH_TOKEN_EXPIRED` frame.
    //
    // Bounded, because a peer that never closes must not pin the task. A
    // client that closed first drains immediately.
    let _ = sink.close().await;
    let _ = tokio::time::timeout(TEARDOWN_DRAIN, async {
        while stream.next().await.is_some() {}
    })
    .await;
}

/// How long a closing session keeps reading before giving up on the peer.
const TEARDOWN_DRAIN: std::time::Duration = std::time::Duration::from_millis(250);

/// End a session whose bearer has expired.
///
/// `Error` then `Close`, in that order, per `docs/06-server/auth.md`. Both
/// carry [`ErrorCode::AuthTokenExpired`] so the client can tell "your
/// credential aged out, renew and reconnect" from "your access was withdrawn,
/// ask the user" — indistinguishable if the close were untyped, and the two
/// call for opposite client behaviour.
async fn end_expired(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    state: &ServerState,
    auth: &SessionAuth,
) {
    state.metrics.incr("sunrise_sync_token_expired_total");
    tracing::info!(
        ev = "srv.ws.token_expired",
        account_h = %crate::logging::id_h(&auth.account),
        "session ended: bearer expired without a refresh"
    );
    let _ = send_error_frame(
        sink,
        ErrorCode::AuthTokenExpired,
        "bearer token expired; refresh and reconnect",
    )
    .await;
    if let Ok(payload) = ClosePayload::auth_token_expired().encode() {
        if let Ok(bytes) = encode_frame(MsgKind::Close, FrameFlags::EMPTY, &payload) {
            let _ = sink.send(Message::Binary(bytes)).await;
        }
    }
}

/// Receive from any of the broadcast subscriptions; returns the index that
/// produced the message and the result. If the list is empty, returns
/// `None` (after a long pending) so the select can fall through to the WS
/// stream.
async fn recv_first(
    subs: &mut [([u8; 16], tokio::sync::broadcast::Receiver<RelayFrame>)],
) -> Option<(
    usize,
    Result<RelayFrame, tokio::sync::broadcast::error::RecvError>,
)> {
    if subs.is_empty() {
        std::future::pending::<()>().await;
        return None;
    }
    // Poll all in parallel — pick the first one ready.
    let futs = subs.iter_mut().enumerate().map(|(i, (_, r))| {
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
    subs: &mut Vec<([u8; 16], tokio::sync::broadcast::Receiver<RelayFrame>)>,
    auth: &mut SessionAuth,
    state: &ServerState,
) -> bool {
    let account = auth.account;
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
                if !join_stream(&entry, sink, subs, account, state).await {
                    return false;
                }
            }
            true
        }
        MsgKind::OpBatch => handle_op_batch(conn_id, buf, &payload, sink, account, state).await,
        MsgKind::Ping => {
            let pong = encode_frame(MsgKind::Pong, FrameFlags::EMPTY, &[]).unwrap_or_default();
            if !pong.is_empty() {
                let _ = sink.send(Message::Binary(pong)).await;
            }
            true
        }
        MsgKind::RefreshToken => handle_refresh(&payload, sink, auth, state).await,
        MsgKind::Close => false,
        _ => true, // ignore anything else in v1 self-host
    }
}

/// Handle an out-of-band `0x12 RefreshToken` frame.
///
/// The server mints nothing. The device's own OIDC client obtains a new access
/// token from the issuer; this re-verifies it through the same
/// [`crate::auth::TokenVerifier`] the upgrade used and, on success, moves the
/// session's deadline out. That is the whole point of the frame: renewal costs
/// no reconnect, so a client renewing at 75% of TTL never sees an
/// `AUTH_TOKEN_EXPIRED` close at all.
///
/// Three ways it can fail, and they are deliberately not treated alike:
///
/// - **The token does not verify.** Refuse the renewal, say so, and leave the
///   session running on the credential it already has. That credential is
///   still valid — its own deadline still governs and will close the session
///   on time — so tearing down here would turn a recoverable client bug into
///   a dropped session.
/// - **The token verifies but names someone else.** End the session. The
///   channel namespace was fixed at the upgrade and is never re-derived, so
///   continuing would relay one account's traffic under another's authority.
///   This is not a mistake to tolerate.
/// - **The session's current token has already expired.** Never reaches here:
///   the per-frame expiry check in [`sync_loop`] runs first and ends the
///   session. A dead session is not resurrectable by presenting a live token;
///   the client reconnects, which re-runs the full upgrade pipeline including
///   `allow_signup` and the account lookup.
async fn handle_refresh(
    payload: &[u8],
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    auth: &mut SessionAuth,
    state: &ServerState,
) -> bool {
    let Ok(frame) = RefreshTokenPayload::decode(payload) else {
        let _ = send_error_frame(sink, ErrorCode::SyncOpInvalid, "refresh decode").await;
        return true;
    };
    let verified = match state.token_verifier.verify(&frame.token).await {
        Ok(v) => v,
        Err(e) => {
            // Same discipline as the upgrade path: a stable code and nothing
            // else. `frame.token` is a credential and never reaches a log.
            let api: crate::error::ApiError = e.into();
            state
                .metrics
                .incr("sunrise_sync_token_refresh_rejected_total");
            tracing::warn!(
                ev = "srv.ws.refresh_rejected",
                err_code = api.code,
                account_h = %crate::logging::id_h(&auth.account),
                "refresh token rejected; session keeps its current credential"
            );
            let _ =
                send_error_frame(sink, ErrorCode::AuthTokenInvalid, "refresh token rejected").await;
            return true;
        }
    };
    if !auth.is_same_identity(&verified) {
        state
            .metrics
            .incr("sunrise_sync_token_refresh_rejected_total");
        tracing::warn!(
            ev = "srv.ws.refresh_identity_mismatch",
            account_h = %crate::logging::id_h(&auth.account),
            "refresh token names a different principal; ending session"
        );
        let _ = send_error_frame(
            sink,
            ErrorCode::AuthTokenInvalid,
            "refresh token does not match this session's principal",
        )
        .await;
        return false;
    }
    auth.renew(&verified, state.clock.now_ms());
    state.metrics.incr("sunrise_sync_token_refreshed_total");
    tracing::debug!(
        ev = "srv.ws.refreshed",
        account_h = %crate::logging::id_h(&auth.account),
        "session credential renewed without a reconnect"
    );
    // Acknowledge explicitly. Every other outcome of a refresh already sends a
    // frame; leaving success as the silent one made it indistinguishable from
    // a server that never understood the request.
    let ack = RefreshTokenAckPayload {
        expires_at_ms: verified.expires_at_ms.unwrap_or(0),
    };
    match ack.encode() {
        Ok(bytes) => match encode_frame(MsgKind::RefreshTokenAck, FrameFlags::EMPTY, &bytes) {
            Ok(frame) => sink.send(Message::Binary(frame)).await.is_ok(),
            Err(_) => true,
        },
        Err(_) => true,
    }
}

/// Join one stream: filtered replay, gap report, CaughtUp, live receiver.
///
/// Returns false on a send failure (the caller ends the session).
async fn join_stream(
    entry: &SubscribeEntry,
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    subs: &mut Vec<([u8; 16], tokio::sync::broadcast::Receiver<RelayFrame>)>,
    account: [u8; 16],
    state: &ServerState,
) -> bool {
    let sid = entry.stream_id;
    // The cursors the client has always sent and the relay used to throw away
    // (issue #19). Last one wins on a duplicate device.
    let cursors: HashMap<[u8; 16], u64> = entry
        .cursors
        .iter()
        .map(|c| (c.device_id, c.last_applied_seq))
        .collect();
    // Live receiver FIRST, then the durable read. A frame published between
    // the two arrives on both paths; the client's op-log gate makes that a
    // no-op. The reverse order would drop it entirely.
    let rx = state.relay.subscribe_live((account, sid));
    let (retained, gaps) = match state.store.relay_replay((account, sid), &cursors) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                ev = "srv.relay.replay_failed",
                err_code = %ErrorCode::RelayStorageUnavailable,
                err_kind = "transient",
                retryable = true,
                result = "failed",
                stream_h = %crate::logging::id_h(&sid),
                cause = %e,
                "could not read the durable op log"
            );
            // Never send CaughtUp here: the client would record a completeness
            // it has no basis for. Ending the session makes it retry.
            let _ = send_error_frame(
                sink,
                ErrorCode::RelayStorageUnavailable,
                "relay could not read its op log",
            )
            .await;
            return false;
        }
    };
    // A gap goes out BEFORE the partial replay and before CaughtUp, so a client
    // cannot read "caught up" as "complete". Delivery continues either way: an
    // incomplete replay plus an explicit error beats one in silence.
    if !gaps.is_empty() {
        state.metrics.incr("sunrise_relay_cursor_gap_total");
        tracing::warn!(
            ev = "srv.relay.cursor_gap",
            err_code = %ErrorCode::SyncCursorGap,
            err_kind = "permanent",
            retryable = false,
            stream_h = %crate::logging::id_h(&sid),
            n_devices = gaps.len() as u64,
            "subscriber cursor predates durable retention"
        );
        if send_error_frame(sink, ErrorCode::SyncCursorGap, &gap_reason(&sid, &gaps))
            .await
            .is_err()
        {
            return false;
        }
    }
    for bytes in retained {
        if sink.send(Message::Binary(bytes)).await.is_err() {
            return false;
        }
    }
    if !send_caught_up(sink, sid).await {
        return false;
    }
    match subs.iter_mut().find(|(s, _)| *s == sid) {
        Some(slot) => slot.1 = rx,
        None => subs.push((sid, rx)),
    }
    true
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

/// Per-device high-water marks for one batch, from the envelopes' cleartext
/// routing header.
///
/// Reading `device_id` / `seq` is in scope for a relay (they are cleartext by
/// design, and the whole cursor mechanism presumes it); reading the payload is
/// not, and `sunrise_cbor::decode_envelope_header` cannot — it links no crypto
/// and returns no payload.
///
/// An op whose header will not parse contributes nothing. That is deliberate:
/// a frame with no heads is never filtered out of a replay and never raises an
/// eviction watermark, so the worst an unreadable op can do is cost a client a
/// redundant, idempotent re-apply.
fn frame_heads(batch: &OpBatchPayload) -> Vec<FrameHead> {
    let mut by_device: HashMap<[u8; 16], u64> = HashMap::new();
    for op in &batch.ops {
        if let Ok(h) = sunrise_cbor::decode_envelope_header(op) {
            let slot = by_device.entry(h.device_id).or_insert(0);
            *slot = (*slot).max(h.seq);
        }
    }
    let mut heads: Vec<FrameHead> = by_device
        .into_iter()
        .map(|(device_id, max_seq)| FrameHead { device_id, max_seq })
        .collect();
    heads.sort_unstable_by(|a, b| a.device_id.cmp(&b.device_id));
    heads
}

/// Human-readable diagnostic for a cursor gap.
///
/// Device ids are hashed exactly as every other id in a server log is: the
/// reason string reaches a client, but it also reaches the relay's own logs,
/// and a raw device id there would be a durable identifier the operator is not
/// supposed to hold (`docs/10-cross-cutting/logging.md`).
fn gap_reason(stream_id: &[u8; 16], gaps: &[CursorGap]) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "retained ring no longer covers stream {}; resync from a peer:",
        crate::logging::id_h(stream_id)
    );
    for g in gaps {
        let _ = write!(
            out,
            " device {} cursor {} < evicted_through {};",
            crate::logging::id_h(&g.device_id),
            g.cursor,
            g.evicted_through
        );
    }
    out
}

/// Handle one inbound `OpBatch`: persist it durably, fan it out, then ack.
///
/// Split out of `handle_inbound` purely for length; the ordering inside it is
/// load-bearing and documented at each step.
async fn handle_op_batch(
    conn_id: ConnId,
    buf: &[u8],
    payload: &[u8],
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    account: [u8; 16],
    state: &ServerState,
) -> bool {
    // Decode the typed payload to route by its stream-id. On decode
    // failure, reply with a coded Error and do NOT fan out.
    let batch = match extract_op_batch(payload) {
        Some(b) => b,
        None => {
            let _ =
                send_error_frame(sink, ErrorCode::SyncOpInvalid, "malformed OpBatch payload").await;
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
    let heads = frame_heads(&batch);
    // Durable first, and only then the ack. Acking an op the server
    // has not committed would promise a durability that does not
    // exist — and the client drops an acked op from its outbox, so the
    // op would be gone from both sides at once.
    if let Err(e) = state.store.relay_append(
        (account, stream_id),
        buf,
        &heads,
        server_first_seen_ms,
        state.durable_caps,
    ) {
        tracing::error!(
            ev = "srv.relay.append_failed",
            err_code = %ErrorCode::RelayStorageUnavailable,
            err_kind = "transient",
            retryable = true,
            result = "failed",
            stream_h = %crate::logging::id_h(&stream_id),
            cause = %e,
            "could not persist an op batch; refusing to ack it"
        );
        state.metrics.incr("sunrise_relay_append_failed_total");
        // No ack: the client keeps the op in its outbox and retries.
        let _ = send_error_frame(
            sink,
            ErrorCode::RelayStorageUnavailable,
            "relay could not durably store the batch",
        )
        .await;
        return true;
    }
    // Republish the original raw frame bytes verbatim for fan-out,
    // tagged with the per-device high-water marks read out of the
    // envelopes' cleartext routing headers. That tag is what lets a
    // later subscriber's cursor skip this frame.
    state.relay.publish(
        (account, stream_id),
        RelayFrame {
            from: conn_id,
            bytes: buf.to_vec(),
            heads,
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
