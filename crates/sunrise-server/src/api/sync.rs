//! The sync surface: an SSE stream downstream, typed `POST`s upstream.
//!
//! Per [ADR-0023](../../../../docs/11-adr/0023-sse-sync-transport.md), which
//! replaces the `/sync` WebSocket. The mapping is one-for-one:
//!
//! | WebSocket frame | Operation |
//! |---|---|
//! | `Hello` / `HelloAck` | [`session`] |
//! | `Subscribe` | [`subscribe`] |
//! | server → client fan-out | [`events`] |
//! | `OpBatch` up, `Ack` down | [`ops`] |
//! | `Ping` / `Pong` | SSE keep-alive comments |
//! | `RefreshToken` | [`refresh`] |
//! | cursor replay, `SYNC_CURSOR_GAP` | `Last-Event-ID` |
//!
//! # What the ADR left open, and what was chosen
//!
//! **Where the session id travels.** ADR-0023 says `POST /sync/session`
//! "returns a session id the SSE stream carries" and does not say how. It
//! travels in `X-Sunrise-Session`, because a session id is a bearer-equivalent
//! and the query string is where this server already leaked a credential once.
//! See [`crate::sync_session`] for the full reasoning and its cost.
//!
//! **What replaces `Subscribe`.** The ADR's table has no row for it, yet it
//! carries per-stream cursors, can be re-sent to replace the stream set
//! mid-session, and drives the cursor-gap and `CaughtUp` protocol. It becomes
//! its own operation, [`subscribe`], keyed by session — which preserves the
//! replace-not-duplicate behaviour rather than dropping it into a new session.
//!
//! # What is unchanged
//!
//! The relay is untouched. `POST /sync/ops` rebuilds the same wire frame the
//! socket used to receive and hands it to `relay_append` and `RelayHub::publish`
//! verbatim, so durable retention, cursor filtering, eviction watermarks and the
//! gap report are the machinery that already exists and is tested. What the
//! stream emits is that same frame, base64-encoded because SSE is UTF-8 — the
//! bounded cost ADR-0023 accepts, and it applies to op envelopes rather than to
//! bulk data, which travels the blob 2PC instead.
//!
//! `Hello::negotiate` keeps its semantics, its error mapping and its frozen
//! version fixtures.

use crate::api::error::{codes, ApiError};
use crate::api::signed::{Signed, SignedParts};
use crate::relay::{CursorGap, FrameHead, RelayFrame};
use crate::state::ServerState;
use crate::sync_session::Session;
use base64::Engine as _;
use kynos::di::inject::Inject;
use kynos::extract::body::json::Json;
use kynos::extract::params::header::Headers;
use kynos::extract::sse::LastEventId;
use kynos::response::status::Created;
use kynos::response::stream::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, WIRE_PROTO_V};
use sunrise_error::ErrorCode;
use sunrise_wire_protocol::{
    encode_frame, Capability, CapabilityBits, CursorEntry, FrameFlags, Hello, MsgKind,
    OpBatchPayload, SubscribeEntry, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};

/// How often the stream emits a keep-alive comment.
///
/// The `Ping`/`Pong` exchange this replaces existed to keep an idle socket from
/// being reaped by an intermediary. A comment does the same job with no frame
/// type and nothing for the client to answer.
const KEEP_ALIVE_SECS: u64 = 15;

/// How often a live stream re-checks that its device is still active.
///
/// Mirrors `config.device_recheck_ms`: revoking a device has to end the session
/// it already holds, not merely refuse the next one.
const RECHECK_FLOOR_MS: u64 = 1_000;

/// How many events may queue for a slow reader before the stream is dropped.
///
/// The socket had the same bound implicitly through `broadcast`'s ring; making
/// it explicit is what keeps one stalled client from growing a queue instead of
/// being disconnected.
const STREAM_BUFFER: usize = 256;

// ---------------------------------------------------------------------------
// Session establishment
// ---------------------------------------------------------------------------

/// `POST /api/v1/sync/session` request body — the `Hello` fields.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct SessionRequest {
    /// Client app version.
    pub client_app_v: String,
    /// Platform string.
    pub client_platform: String,
    /// Ascending list of wire-protocol versions the client speaks.
    pub wire_proto_supported: Vec<u32>,
    /// Lowest document schema the client can produce.
    pub doc_schema_min: u32,
    /// Highest document schema the client can read.
    pub doc_schema_max: u32,
    /// Crypto suites the client speaks.
    pub crypto_suite_supported: Vec<u32>,
    /// Bitfield of optional features.
    pub capabilities: u64,
    /// ULID of the session trace.
    pub trace: String,
}

/// `POST /api/v1/sync/session` response body — `HelloAck` plus the id.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct SessionResponse {
    /// The session id, presented as `X-Sunrise-Session` on every later call.
    pub session_id: String,
    /// Server app version.
    pub server_app_v: String,
    /// Negotiated wire protocol version.
    pub wire_proto: u32,
    /// Negotiated crypto suite.
    pub crypto_suite: u32,
    /// The server's accepted document-schema floor.
    pub doc_schema_floor: u32,
    /// The capability bitfield both sides agreed on.
    pub capabilities: u64,
    /// Server time, advisory, for clock-skew detection.
    pub server_time_ms: u64,
}

/// The session id, presented on every operation after establishment.
#[derive(Debug, Clone, kynos::HeaderParams)]
pub struct SessionHeader {
    /// The id `POST /sync/session` returned.
    #[header(rename = "X-Sunrise-Session")]
    pub session: Option<String>,
}

/// Establish a sync session: negotiate versions and capabilities.
///
/// Replaces the `Hello`/`HelloAck` exchange. `Hello::negotiate` is called
/// unchanged, so a client that cannot agree on a wire version, a crypto suite
/// or a required capability is refused here for the same reason and with the
/// same mapping it was refused at the socket.
#[kynos::post("/api/v1/sync/session")]
pub async fn session(
    Inject(state): Inject<ServerState>,
    Signed {
        caller,
        value: body,
    }: Signed<SessionRequest>,
) -> Result<Created<Json<SessionResponse>>, ApiError> {
    let now_ms = state.clock.now_ms();
    // Hung off the busiest path rather than a timer task whose only job is to
    // take a lock occasionally.
    state.sessions.collect(now_ms);

    let hello = Hello {
        client_app_v: body.client_app_v,
        client_platform: body.client_platform,
        wire_proto_supported: body.wire_proto_supported,
        doc_schema_min: body.doc_schema_min,
        doc_schema_max: body.doc_schema_max,
        crypto_suite_supported: body.crypto_suite_supported,
        capabilities: body.capabilities,
        trace: body.trace,
    };
    // `SrvTokenRefresh` is optional, so it rides on top of the required set.
    // The client only asks for a refresh if it sees this bit come back agreed,
    // which is what stops one being silently swallowed by an older server.
    let server_caps = REQUIRED_CLIENT_BITS.0
        | REQUIRED_SERVER_BITS.0
        | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0;
    let ack = hello
        .negotiate(
            state.config.server_app_v.clone(),
            &[u32::from(WIRE_PROTO_V)],
            &[u32::from(CRYPTO_SUITE_V)],
            u32::from(DOC_SCHEMA_FLOOR),
            server_caps,
            now_ms,
        )
        .map_err(|e| {
            state.metrics.incr("sunrise_sync_negotiate_refused_total");
            tracing::warn!(
                ev = "srv.sync.negotiate_refused",
                err_kind = "user",
                reason = %e,
                "session refused"
            );
            ApiError::validation_coded(codes::VALIDATION_INVALID, e.to_string())
        })?;

    let account = account_hash(&caller.principal.account.account_id);
    let stored = Session {
        account,
        account_id: caller.principal.account.account_id.clone(),
        subject: caller.principal.subject.clone(),
        device_id: caller.device.as_ref().map(|d| d.device_id.clone()),
        deadline_ms: caller.principal.expires_at_ms,
        conn: state.relay.next_conn(),
        negotiated: ack.clone(),
        streams: Vec::new(),
        seen_ms: now_ms,
    };
    let session_id = state
        .sessions
        .insert(stored)
        .map_err(|_| ApiError::internal())?;

    state.metrics.incr("sunrise_sync_session_total");
    tracing::info!(
        ev = "srv.sync.session_open",
        account_h = %crate::logging::account_h(&caller.principal.account.account_id),
        "sync session established"
    );
    Ok(Created::at(
        "/api/v1/sync/events",
        Json(SessionResponse {
            session_id,
            server_app_v: ack.server_app_v,
            wire_proto: ack.wire_proto,
            crypto_suite: ack.crypto_suite,
            doc_schema_floor: ack.doc_schema_floor,
            capabilities: ack.capabilities,
            server_time_ms: ack.server_time_ms,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

/// One device's position in a stream.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct DeviceCursor {
    /// The originating device, 32 lowercase hex characters.
    pub device_id: String,
    /// The highest `seq` from that device this subscriber has applied.
    pub last_applied_seq: u64,
}

/// One stream to receive on, with the subscriber's cursors.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct StreamSubscription {
    /// The stream, 32 lowercase hex characters.
    pub stream_id: String,
    /// Per-device positions. Anything already covered is not replayed.
    #[serde(default)]
    pub cursors: Vec<DeviceCursor>,
}

/// `POST /api/v1/sync/subscribe` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeRequest {
    /// The streams this session wants. **Replaces** the current set rather than
    /// adding to it, which is what a re-sent `Subscribe` frame did.
    pub streams: Vec<StreamSubscription>,
}

/// Replace the set of streams this session receives on.
///
/// Takes effect on the next `GET /sync/events`. A stream already open keeps its
/// current set: re-subscribing mid-stream is a reconnect, which is what the
/// socket's re-`Subscribe` amounted to once the fan-out had been rebuilt.
#[kynos::post("/api/v1/sync/subscribe")]
pub async fn subscribe(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    Signed {
        caller,
        value: body,
    }: Signed<SubscribeRequest>,
) -> Result<kynos::response::status::NoContent, ApiError> {
    let now_ms = state.clock.now_ms();
    let (id, session) = resolve(&state, &header, &caller, now_ms)?;

    let mut streams = Vec::with_capacity(body.streams.len());
    for s in &body.streams {
        streams.push(SubscribeEntry {
            stream_id: parse_id(&s.stream_id, "stream_id")?,
            cursors: s
                .cursors
                .iter()
                .map(|c| {
                    Ok(CursorEntry {
                        device_id: parse_id(&c.device_id, "device_id")?,
                        last_applied_seq: c.last_applied_seq,
                    })
                })
                .collect::<Result<Vec<_>, ApiError>>()?,
        });
    }
    let _ = session;
    state.sessions.update(&id, now_ms, |s| s.streams = streams);
    Ok(kynos::response::status::NoContent)
}

// ---------------------------------------------------------------------------
// Ops upstream
// ---------------------------------------------------------------------------

/// `POST /api/v1/sync/ops` request body — one `OpBatch`.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct OpsRequest {
    /// The stream the batch targets, 32 lowercase hex characters.
    pub stream_id: String,
    /// Client-generated idempotency key for the batch.
    pub batch_id: u64,
    /// The already-encoded `OpEnvelope`s, base64 standard, one per element.
    /// The server never decodes their ciphertext; it reads only the cleartext
    /// routing head to tag the frame for cursor filtering.
    pub ops: Vec<String>,
}

/// `POST /api/v1/sync/ops` response body — the `Ack`.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct OpsResponse {
    /// Echoes the acked batch's `batch_id`.
    pub batch_id: u64,
    /// The stream the batch targeted.
    pub stream_id: String,
    /// Server wall-clock at which the batch was first seen, for clock-skew
    /// clamping.
    pub server_first_seen_ms: u64,
}

/// Publish one batch of ops.
///
/// Durable first, and only then the ack. Acking an op the server has not
/// committed would promise a durability that does not exist — and the client
/// drops an acked op from its outbox, so the op would be gone from both sides
/// at once. A storage failure is reported as a 503 with nothing acked, which
/// leaves the batch in the outbox to retry.
#[kynos::post("/api/v1/sync/ops")]
pub async fn ops(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    Signed {
        caller,
        value: body,
    }: Signed<OpsRequest>,
) -> Result<Json<OpsResponse>, ApiError> {
    let now_ms = state.clock.now_ms();
    let (_, session) = resolve(&state, &header, &caller, now_ms)?;
    let stream_id = parse_id(&body.stream_id, "stream_id")?;

    let ops = body
        .ops
        .iter()
        .map(|o| {
            base64::engine::general_purpose::STANDARD
                .decode(o)
                .map_err(|_| ApiError::validation("ops entries must be base64"))
        })
        .collect::<Result<Vec<Vec<u8>>, ApiError>>()?;

    // Rebuild the frame the socket used to receive, so everything downstream —
    // durable append, head extraction, fan-out, replay — is the code that
    // already exists rather than a second encoding of the same thing.
    let batch = OpBatchPayload {
        ops,
        batch_id: body.batch_id,
        stream_id,
    };
    let payload = batch
        .encode()
        .map_err(|_| ApiError::validation("batch could not be encoded"))?;
    let frame = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &payload)
        .map_err(|_| ApiError::validation("batch could not be framed"))?;

    let heads = frame_heads(&batch);
    tracing::debug!(
        ev = "srv.relay.fanout",
        stream_h = %crate::logging::id_h(&stream_id),
        n_bytes = frame.len() as u64,
        "op batch republished"
    );

    if let Err(e) = state.store.relay_append(
        (session.account, stream_id),
        &frame,
        &heads,
        now_ms,
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
        return Err(ApiError::unavailable(
            "relay could not durably store the batch",
        ));
    }

    state.relay.publish(
        (session.account, stream_id),
        RelayFrame {
            from: session.conn,
            bytes: frame,
            heads,
        },
    );

    Ok(Json(OpsResponse {
        batch_id: body.batch_id,
        stream_id: body.stream_id,
        server_first_seen_ms: now_ms,
    }))
}

// ---------------------------------------------------------------------------
// Credential renewal
// ---------------------------------------------------------------------------

/// `POST /api/v1/sync/session/refresh` request body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RefreshRequest {
    /// The replacement bearer.
    pub token: String,
}

/// `POST /api/v1/sync/session/refresh` response body.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(deny_unknown_fields)]
pub struct RefreshResponse {
    /// The new deadline, or 0 where the verifier issues none.
    pub expires_at_ms: u64,
}

/// Extend a session without re-establishing it.
///
/// The replacement must name the **same principal and the same device**. A
/// refresh that changed either would let one credential hand a live op stream
/// to another, which is the whole reason the check exists rather than simply
/// trusting a valid token.
#[kynos::post("/api/v1/sync/session/refresh")]
pub async fn refresh(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    Signed {
        caller,
        value: body,
    }: Signed<RefreshRequest>,
) -> Result<Json<RefreshResponse>, ApiError> {
    let now_ms = state.clock.now_ms();
    let (id, session) = resolve(&state, &header, &caller, now_ms)?;

    let verified = state
        .token_verifier
        .verify(&body.token)
        .await
        .map_err(|_| ApiError::unauthenticated())?;
    if verified.subject.principal_key() != session.subject.principal_key() {
        state.sessions.remove(&id);
        return Err(ApiError::unauthenticated());
    }
    if verified.subject.device_id != session.subject.device_id {
        state.sessions.remove(&id);
        return Err(ApiError::unauthenticated());
    }

    let deadline = verified.expires_at_ms;
    state
        .sessions
        .update(&id, now_ms, |s| s.deadline_ms = deadline);
    state.metrics.incr("sunrise_sync_refresh_total");
    Ok(Json(RefreshResponse {
        expires_at_ms: deadline.unwrap_or(0),
    }))
}

// ---------------------------------------------------------------------------
// The event stream
// ---------------------------------------------------------------------------

/// One event on the sync stream.
///
/// `itemSchema`-typed, which is what OpenAPI 3.2 adds and 3.1 has no way to
/// say: each event of the `text/event-stream` body is described rather than the
/// body being an opaque string.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncEvent {
    /// A batch of ops, as the verbatim wire frame, base64 standard.
    Ops {
        /// The stream it belongs to.
        stream_id: String,
        /// The frame, exactly as the relay stored it.
        frame: String,
    },
    /// The retained backlog for a stream has been fully replayed.
    ///
    /// Never sent where a gap was reported for that stream without the gap
    /// going first: a client must not read "caught up" as "complete".
    CaughtUp {
        /// The stream now live.
        stream_id: String,
    },
    /// The subscriber's cursor predates what retention still holds.
    Gap {
        /// The stream with the hole.
        stream_id: String,
        /// The canonical error code, always `SYNC_CURSOR_GAP`.
        code: String,
        /// Which devices, and how far the loss runs.
        reason: String,
    },
    /// The session is over. The stream ends after this.
    Closed {
        /// The canonical error code a client can branch on.
        code: String,
        /// Diagnostic detail.
        reason: String,
    },
}

/// The live op stream for this session.
///
/// Backpressure and disconnect are the stream's own: events are pulled one at a
/// time and never ahead, and a client that goes away drops the body, which
/// drops the stream, which drops the relay receivers it holds.
#[kynos::get("/api/v1/sync/events")]
pub async fn events(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    LastEventId(resume): LastEventId,
    SignedParts(caller): SignedParts,
) -> Result<Sse<EventStream>, ApiError> {
    let now_ms = state.clock.now_ms();
    let (id, session) = resolve(&state, &header, &caller, now_ms)?;
    let after = resume.as_deref().and_then(|s| s.parse::<u64>().ok());

    state.metrics.incr("sunrise_sync_stream_total");
    Ok(
        Sse::new(spawn_stream(state, id, session, after)).keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(KEEP_ALIVE_SECS))
                .comment("sunrise"),
        ),
    )
}

/// The stream [`events`] returns.
pub type EventStream =
    futures_util::stream::BoxStream<'static, Result<Event<SyncEvent>, std::convert::Infallible>>;

/// Drive the session's fan-out into a stream.
///
/// A task feeding a bounded channel rather than a hand-written `poll_next`,
/// because what has to happen here is a `select!` over several sources — the
/// per-stream live receivers, the token deadline, the revocation re-check — and
/// that is the shape the socket loop already had. Dropping the returned stream
/// drops the receiver, whose sender then fails, which ends the task.
fn spawn_stream(
    state: ServerState,
    id: String,
    session: Session,
    after: Option<u64>,
) -> EventStream {
    use futures_util::StreamExt as _;
    let (tx, rx) = tokio::sync::mpsc::channel::<Event<SyncEvent>>(STREAM_BUFFER);

    tokio::spawn(async move {
        let mut receivers = Vec::new();

        // Live receiver FIRST, then the durable read. A frame published between
        // the two arrives on both paths and the client's op-log gate makes that
        // a no-op; the reverse order would drop it entirely.
        for entry in &session.streams {
            let sid = entry.stream_id;
            receivers.push((sid, state.relay.subscribe_live((session.account, sid))));

            let cursors: HashMap<[u8; 16], u64> = entry
                .cursors
                .iter()
                .map(|c| (c.device_id, c.last_applied_seq))
                .collect();
            let replay = state.store.relay_replay_after(
                (session.account, sid),
                after.unwrap_or(0),
                &cursors,
            );
            let (frames, gaps) = match replay {
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
                    // Never CaughtUp here: the client would record a
                    // completeness it has no basis for.
                    let _ = tx
                        .send(closed(
                            ErrorCode::RelayStorageUnavailable,
                            "relay could not read its op log",
                        ))
                        .await;
                    return;
                }
            };

            // A gap goes out BEFORE the partial replay and before CaughtUp, so
            // a client cannot read "caught up" as "complete".
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
                let event = Event::new(SyncEvent::Gap {
                    stream_id: hex::encode(sid),
                    code: ErrorCode::SyncCursorGap.to_string(),
                    reason: gap_reason(&gaps),
                });
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            for (seq, bytes) in frames {
                let event = Event::new(SyncEvent::Ops {
                    stream_id: hex::encode(sid),
                    frame: base64::engine::general_purpose::STANDARD.encode(&bytes),
                })
                .id(seq.to_string());
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            let caught_up = Event::new(SyncEvent::CaughtUp {
                stream_id: hex::encode(sid),
            });
            if tx.send(caught_up).await.is_err() {
                return;
            }
        }

        live_loop(state, id, session, receivers, tx).await;
    });

    futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|event| (Ok(event), rx))
    })
    .boxed()
}

/// The live half: fan-out, the token deadline, and the revocation re-check.
async fn live_loop(
    state: ServerState,
    id: String,
    session: Session,
    mut receivers: Vec<([u8; 16], tokio::sync::broadcast::Receiver<RelayFrame>)>,
    tx: tokio::sync::mpsc::Sender<Event<SyncEvent>>,
) {
    let recheck =
        std::time::Duration::from_millis(state.config.device_recheck_ms.max(RECHECK_FLOOR_MS));
    let mut ticker = tokio::time::interval(recheck);
    ticker.tick().await;

    loop {
        let next = recv_first(&mut receivers);
        tokio::select! {
            biased;

            Some((sid, frame)) = next => {
                // The session's own batches are not echoed back to it: it
                // already has them, and applying its own op twice is exactly
                // what the op-log gate then has to undo.
                if frame.from == session.conn {
                    continue;
                }
                let event = Event::new(SyncEvent::Ops {
                    stream_id: hex::encode(sid),
                    frame: base64::engine::general_purpose::STANDARD.encode(&frame.bytes),
                });
                if tx.send(event).await.is_err() {
                    break;
                }
            }

            _ = ticker.tick() => {
                let now_ms = state.clock.now_ms();
                // A session outliving its own credential is the defect this
                // check exists for: the bearer was presented once, and the
                // stream is long-lived.
                if state.sessions.get(&id, now_ms).is_none() {
                    let _ = tx
                        .send(closed(ErrorCode::AuthTokenExpired, "access token expired"))
                        .await;
                    break;
                }
                // Revoking a device has to end the stream it already holds,
                // not merely refuse the next one.
                if let Some(device_id) = session.device_id.as_deref() {
                    let active = state
                        .store
                        .active_device(&session.account_id, device_id)
                        .ok()
                        .flatten();
                    if active.is_none() {
                        state.sessions.remove(&id);
                        let _ = tx
                            .send(closed(ErrorCode::AuthDeviceRevoked, "device revoked"))
                            .await;
                        break;
                    }
                }
            }
        }
    }
}

/// Await the first frame from any subscribed stream.
async fn recv_first(
    receivers: &mut [([u8; 16], tokio::sync::broadcast::Receiver<RelayFrame>)],
) -> Option<([u8; 16], RelayFrame)> {
    use futures_util::future::{select_all, FutureExt as _};
    if receivers.is_empty() {
        // Nothing to receive on: park forever rather than spinning, and let the
        // other `select!` arms drive the session.
        std::future::pending::<()>().await;
        return None;
    }
    let futures: Vec<_> = receivers
        .iter_mut()
        .map(|(sid, rx)| {
            let sid = *sid;
            async move { rx.recv().await.ok().map(|f| (sid, f)) }.boxed()
        })
        .collect();
    let (out, _, _) = select_all(futures).await;
    out
}

/// A terminal event.
fn closed(code: ErrorCode, reason: &str) -> Event<SyncEvent> {
    Event::new(SyncEvent::Closed {
        code: code.to_string(),
        reason: reason.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Shared
// ---------------------------------------------------------------------------

/// Resolve the session the header names, checking it belongs to the caller.
///
/// The ownership check is the load-bearing half. A session id is a
/// bearer-equivalent, so without it any authenticated account presenting
/// another's id would be handed that account's op stream.
fn resolve(
    state: &ServerState,
    header: &SessionHeader,
    caller: &crate::api::signed::Caller,
    now_ms: u64,
) -> Result<(String, Session), ApiError> {
    let id = header
        .session
        .as_deref()
        .ok_or_else(|| ApiError::validation("X-Sunrise-Session is required"))?;
    let session = state
        .sessions
        .get(id, now_ms)
        .ok_or_else(ApiError::unauthenticated)?;
    if session.subject.principal_key() != caller.principal.subject.principal_key() {
        return Err(ApiError::unauthenticated());
    }
    Ok((id.to_owned(), session))
}

/// Parse a 16-byte id from 32 lowercase hex characters.
fn parse_id(s: &str, field: &'static str) -> Result<[u8; 16], ApiError> {
    let mut out = [0u8; 16];
    if s.len() != 32 {
        return Err(ApiError::validation(format!(
            "{field} must carry 32 hex characters"
        )));
    }
    hex::decode_to_slice(s, &mut out)
        .map_err(|_| ApiError::validation(format!("{field} must be lowercase hex")))?;
    Ok(out)
}

/// The per-device high-water marks a batch carries, read from the envelopes'
/// cleartext routing headers. The ciphertext is never touched.
fn frame_heads(batch: &OpBatchPayload) -> Vec<FrameHead> {
    let mut highest: HashMap<[u8; 16], u64> = HashMap::new();
    for op in &batch.ops {
        if let Ok(head) = sunrise_cbor::decode_envelope_header(op) {
            let slot = highest.entry(head.device_id).or_insert(0);
            *slot = (*slot).max(head.seq);
        }
    }
    let mut out: Vec<FrameHead> = highest
        .into_iter()
        .map(|(device_id, max_seq)| FrameHead { device_id, max_seq })
        .collect();
    out.sort_by_key(|h| h.device_id);
    out
}

/// The relay channel namespace for an account.
///
/// The account id never travels to another account, and the hash is what keys
/// the channel, so a stream is addressable only by someone who already knows
/// whose it is.
fn account_hash(account_id: &str) -> [u8; 16] {
    let h = blake3::hash(account_id.as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.as_bytes()[..16]);
    out
}

/// A human-readable summary of which devices lost what.
fn gap_reason(gaps: &[CursorGap]) -> String {
    let mut parts: Vec<String> = gaps
        .iter()
        .map(|g| {
            format!(
                "{}:{}..{}",
                crate::logging::id_h(&g.device_id),
                g.cursor,
                g.evicted_through
            )
        })
        .collect();
    parts.sort();
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use crate::api::testing::Client;
    use crate::ServerConfig;
    use kynos::http::{Method, StatusCode};
    use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};

    /// A stream every test publishes into.
    const STREAM: &str = "00112233445566778899aabbccddeeff";

    fn hello() -> serde_json::Value {
        serde_json::json!({
            "client_app_v": "0.1.0",
            "client_platform": "test",
            "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
            "doc_schema_min": u32::from(DOC_SCHEMA_V),
            "doc_schema_max": u32::from(DOC_SCHEMA_V),
            "crypto_suite_supported": [u32::from(CRYPTO_SUITE_V)],
            "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0,
            "trace": "01J000000000000000000000000",
        })
    }

    /// Establish a session and return its id.
    async fn establish(client: &Client) -> String {
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
            .await;
        res.assert_status(StatusCode::CREATED);
        res.json()["session_id"]
            .as_str()
            .expect("a session id")
            .to_owned()
    }

    /// The handshake `Hello`/`HelloAck` used to carry.
    #[tokio::test]
    async fn a_session_negotiates_the_same_versions_the_handshake_did() {
        let client = Client::new(ServerConfig::default());
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
            .await;
        res.assert_status(StatusCode::CREATED);
        let body = res.json();

        assert_eq!(body["wire_proto"], u32::from(WIRE_PROTO_V));
        assert_eq!(body["crypto_suite"], u32::from(CRYPTO_SUITE_V));
        assert!(
            body["session_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("ses_")),
            "a session id must come back: {body}"
        );
    }

    /// A client the server cannot agree with is refused here for the same
    /// reason it was refused at the socket.
    #[tokio::test]
    async fn an_unnegotiable_client_is_refused() {
        let client = Client::new(ServerConfig::default());
        let mut body = hello();
        body["wire_proto_supported"] = serde_json::json!([9999]);

        client
            .send(Method::POST, "/api/v1/sync/session", Some(&body))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// Every operation after establishment needs the session id.
    #[tokio::test]
    async fn an_operation_without_a_session_header_is_refused() {
        let client = Client::new(ServerConfig::default());
        let _ = establish(&client).await;

        client
            .send(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(&serde_json::json!({ "streams": [] })),
            )
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// An id nobody issued names no session.
    #[tokio::test]
    async fn an_unknown_session_is_refused() {
        let client = Client::new(ServerConfig::default());
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(crate::api::testing::BEARER),
                Some(&serde_json::json!({ "streams": [] })),
                &[("x-sunrise-session", "ses_00000000000000000000000000000000")],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Subscribing then publishing: the batch is acked with the server's
    /// first-seen time, which is what the clock-skew clamp reads.
    #[tokio::test]
    async fn a_batch_is_acked_after_it_is_durable() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(crate::api::testing::BEARER),
                Some(&serde_json::json!({
                    "streams": [{ "stream_id": STREAM, "cursors": [] }]
                })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(crate::api::testing::BEARER),
                Some(&serde_json::json!({
                    "stream_id": STREAM,
                    "batch_id": 7,
                    "ops": [],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await;

        res.assert_status(StatusCode::OK);
        let body = res.json();
        assert_eq!(body["batch_id"], 7);
        assert_eq!(body["stream_id"], STREAM);
        assert!(body["server_first_seen_ms"].is_u64(), "{body}");
    }

    /// A stream id that is not 32 hex characters never reaches the relay.
    #[tokio::test]
    async fn a_malformed_stream_id_is_refused() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(crate::api::testing::BEARER),
                Some(&serde_json::json!({
                    "stream_id": "not-a-stream",
                    "batch_id": 1,
                    "ops": [],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// The stream replays what was published, then says it is caught up.
    ///
    /// The whole downstream half in one pass: a batch published before the
    /// stream opens is replayed from the durable log, base64 of the same frame
    /// the relay stored, and `CaughtUp` follows it.
    #[tokio::test]
    async fn the_event_stream_replays_then_reports_caught_up() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(crate::api::testing::BEARER),
                Some(&serde_json::json!({
                    "streams": [{ "stream_id": STREAM, "cursors": [] }]
                })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::NO_CONTENT);

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(crate::api::testing::BEARER),
                Some(&serde_json::json!({
                    "stream_id": STREAM,
                    "batch_id": 1,
                    "ops": [],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::OK);

        // Read only the replay: the stream stays open for live frames, so it is
        // read with a deadline rather than to completion.
        let body = client
            .read_stream(
                "/api/v1/sync/events",
                &[("x-sunrise-session", &id)],
                std::time::Duration::from_millis(250),
            )
            .await;

        assert!(body.contains("\"kind\":\"ops\""), "no replay in: {body}");
        assert!(
            body.contains("\"kind\":\"caught_up\""),
            "no caught-up marker in: {body}"
        );
        let ops_at = body.find("\"kind\":\"ops\"").unwrap();
        let caught_at = body.find("\"kind\":\"caught_up\"").unwrap();
        assert!(
            ops_at < caught_at,
            "caught up must not precede the replay it completes: {body}"
        );
        assert!(
            body.contains("id: 1"),
            "each replayed frame must carry its durable id for Last-Event-ID: {body}"
        );
    }

    /// `Last-Event-ID` resumes rather than replaying from the start.
    #[tokio::test]
    async fn a_resumed_stream_does_not_replay_what_it_already_had() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(crate::api::testing::BEARER),
                Some(&serde_json::json!({
                    "streams": [{ "stream_id": STREAM, "cursors": [] }]
                })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::NO_CONTENT);

        for batch_id in 1..=3u64 {
            client
                .send_with(
                    Method::POST,
                    "/api/v1/sync/ops",
                    Some(crate::api::testing::BEARER),
                    Some(&serde_json::json!({
                        "stream_id": STREAM,
                        "batch_id": batch_id,
                        "ops": [],
                    })),
                    &[("x-sunrise-session", &id)],
                )
                .await
                .assert_status(StatusCode::OK);
        }

        let body = client
            .read_stream(
                "/api/v1/sync/events",
                &[("x-sunrise-session", &id), ("last-event-id", "2")],
                std::time::Duration::from_millis(250),
            )
            .await;

        assert!(!body.contains("id: 1"), "frame 1 was already had: {body}");
        assert!(!body.contains("id: 2"), "frame 2 was already had: {body}");
        assert!(body.contains("id: 3"), "frame 3 was not: {body}");
    }
}
