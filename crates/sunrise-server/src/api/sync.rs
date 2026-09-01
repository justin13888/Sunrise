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
use crate::relay_log::Appended;
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
#[kynos::post("/api/v1/sync/session", operation_id = "openSyncSession")]
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
                cause = %e,
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
#[kynos::post("/api/v1/sync/subscribe", operation_id = "subscribeStreams")]
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
    tracing::debug!(
        ev = "srv.sync.subscribe",
        n_streams = streams.len() as u64,
        "stream set replaced"
    );
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
//
// A batch the channel already holds is acked without being stored or fanned out
// a second time, and the ack carries the timestamp the *first* copy got. The
// client cannot avoid re-sending: a session that dies between the append and
// the ack leaves the batch in the outbox, and every reconnect re-drains it. See
// `batch_ops_hash` for why the key is the content rather than the `batch_id`.
//
// Deliberately a `//` comment rather than a `///` one: kynos publishes a
// handler's doc comment as the operation `description`, and
// `schemas/openapi.v1.json` is a committed artefact this change has no business
// touching — the request and response shapes are identical either way.
#[kynos::post("/api/v1/sync/ops", operation_id = "publishOps")]
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
    let ops_h = batch_ops_hash(&batch.ops);

    let appended = state
        .store
        .relay_append(
            (session.account, stream_id),
            &frame,
            &heads,
            ops_h.as_ref(),
            body.batch_id,
            now_ms,
            state.durable_caps,
        )
        .map_err(|e| {
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
            ApiError::unavailable("relay could not durably store the batch")
        })?;

    let first_seen_ms = match appended {
        Appended::Fresh { first_seen_ms } => {
            tracing::debug!(
                ev = "srv.relay.fanout",
                stream_h = %crate::logging::id_h(&stream_id),
                n_bytes = frame.len() as u64,
                "op batch republished"
            );
            state.relay.publish(
                (session.account, stream_id),
                RelayFrame {
                    from: session.conn,
                    bytes: frame,
                    heads,
                },
            );
            first_seen_ms
        }
        // No publish: a second fan-out would hand every live subscriber an op
        // it already applied. Idempotent, but it is bandwidth and log noise
        // proportional to how flaky the *sender's* link is.
        Appended::Duplicate { first_seen_ms } => {
            state.metrics.incr("sunrise_relay_batch_duplicate_total");
            tracing::debug!(
                ev = "srv.relay.batch_duplicate",
                stream_h = %crate::logging::id_h(&stream_id),
                batch_id = body.batch_id,
                first_seen_ms,
                "op batch already stored; re-acking the first copy"
            );
            first_seen_ms
        }
    };

    Ok(Json(OpsResponse {
        batch_id: body.batch_id,
        stream_id: body.stream_id,
        server_first_seen_ms: first_seen_ms,
    }))
}

/// Domain-separated content hash of a batch's ops, or `None` for an empty one.
///
/// The relay dedups on this rather than on the request's `batch_id` because the
/// `batch_id` is not durable: `sync_driver.rs` initialises the counter inside
/// `session()`, so it restarts at 1 on every reconnect and a key containing it
/// would read session 2's first batch as session 1's — dropping it while acking
/// it, which is how an acked batch disappears from the client's outbox and the
/// server at once.
///
/// The hash covers the op count and each op's length before its bytes, so no
/// re-partitioning of the same concatenated ops collides with another batch.
/// `None` for an empty batch: there is no content to be the same as, and three
/// empty batches are three events.
fn batch_ops_hash(ops: &[Vec<u8>]) -> Option<[u8; 32]> {
    if ops.is_empty() {
        return None;
    }
    let mut h = blake3::Hasher::new();
    h.update(b"sunrise.relay.batch.v1");
    h.update(&(ops.len() as u64).to_le_bytes());
    for op in ops {
        h.update(&(op.len() as u64).to_le_bytes());
        h.update(op);
    }
    Some(*h.finalize().as_bytes())
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
#[kynos::post("/api/v1/sync/session/refresh", operation_id = "refreshSyncSession")]
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

    let account_h = crate::logging::account_h(&session.account_id);
    let verified = state
        .token_verifier
        .verify(&body.token)
        .await
        .map_err(|e| {
            // Recoverable: the session keeps the credential it still has.
            tracing::warn!(
                ev = "srv.sync.refresh_rejected",
                err_kind = "user",
                account_h = %account_h,
                cause = %e,
                "refresh token failed verification"
            );
            ApiError::unauthenticated()
        })?;
    if verified.subject.principal_key() != session.subject.principal_key()
        || verified.subject.device_id != session.subject.device_id
    {
        // A session handed another principal's token is not one to keep
        // serving: the channel namespace was fixed at establishment.
        tracing::warn!(
            ev = "srv.sync.refresh_identity_mismatch",
            err_kind = "user",
            account_h = %account_h,
            "refresh token names a different principal or device; ending the session"
        );
        state.sessions.remove(&id);
        return Err(ApiError::unauthenticated());
    }

    let deadline = verified.expires_at_ms;
    state
        .sessions
        .update(&id, now_ms, |s| s.deadline_ms = deadline);
    state.metrics.incr("sunrise_sync_refresh_total");
    tracing::debug!(
        ev = "srv.sync.refreshed",
        account_h = %account_h,
        "session deadline extended without a reconnect"
    );
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
#[kynos::get("/api/v1/sync/events", operation_id = "syncEvents")]
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

        let account_h = crate::logging::account_h(&session.account_id);
        live_loop(state, id, session, receivers, tx).await;
        tracing::info!(ev = "srv.sync.stream_closed", account_h = %account_h, "event stream ended");
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
    let recheck = std::time::Duration::from_millis(state.config.device_recheck_ms.max(1));
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
                    // Answers "why did a working client drop hourly".
                    tracing::warn!(
                        ev = "srv.sync.token_expired",
                        err_kind = "user",
                        account_h = %crate::logging::account_h(&session.account_id),
                        "session bearer passed its exp; ending the stream"
                    );
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
                        // Distinct from `token_expired` on purpose: that one
                        // means "renew and reconnect", this one means "access
                        // was withdrawn, ask the user".
                        tracing::warn!(
                            ev = "srv.sync.device_revoked",
                            err_kind = "user",
                            account_h = %crate::logging::account_h(&session.account_id),
                            "session device is no longer active; ending the stream"
                        );
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
    //! The WebSocket suite, carried over.
    //!
    //! Every scenario the five `ws_*` integration files covered lives here,
    //! against the operations that replaced the frames. They are unit tests now
    //! rather than integration ones because `Service::call` needs no port: what
    //! used to require booting a listener and dialling it is an in-process call,
    //! so the same coverage costs no sockets and no timing.
    //!
    //! Three scenarios changed shape rather than moving, and each says so where
    //! it sits: `Ping` is a keep-alive comment now, a malformed batch is refused
    //! by the schema rather than nacked, and the handshake round trip is an HTTP
    //! response rather than a frame exchange.

    use crate::api::testing::{Client, BEARER};
    use crate::relay::RingCaps;
    use crate::relay_log::DurableCaps;
    use crate::state::{Clock, ServerState};
    use crate::{ServerConfig, StaticVerifier, Subject};
    use kynos::http::{Method, StatusCode};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use sunrise_cbor::magic::{write_prefix, MagicKind, MAGIC_LEN};
    use sunrise_cbor::version::{ENVELOPE_FORMAT_V, WIRE_PROTO_V};
    use sunrise_wire_protocol::{Capability, CapabilityBits};

    const STREAM_BYTES: [u8; 16] = [0x11; 16];
    const ISSUER: &str = "https://idp.example";
    const T0_MS: u64 = 1_704_067_200_000;

    fn stream_hex() -> String {
        hex::encode(STREAM_BYTES)
    }

    /// A clock a test moves by hand, so expiry is a value rather than a wait.
    #[derive(Debug)]
    struct TestClock(AtomicU64);

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl TestClock {
        fn at(ms: u64) -> Arc<Self> {
            Arc::new(Self(AtomicU64::new(ms)))
        }
        fn set(&self, ms: u64) {
            self.0.store(ms, Ordering::SeqCst);
        }
    }

    fn hello() -> serde_json::Value {
        serde_json::json!({
            "client_app_v": "0.1.0",
            "client_platform": "test",
            "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
            "doc_schema_min": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
            "doc_schema_max": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
            "crypto_suite_supported": [u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)],
            // `SrvTokenRefresh` is agreed by AND, so a client that wants a
            // refresh has to offer it too. A real one does; this mirrors that.
            "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0
                | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
            "trace": "01J000000000000000000000000",
        })
    }

    async fn session_as(client: &Client, bearer: &str) -> String {
        let res = client
            .send_as(
                Method::POST,
                "/api/v1/sync/session",
                Some(bearer),
                Some(&hello()),
            )
            .await;
        res.assert_status(StatusCode::CREATED);
        res.json()["session_id"]
            .as_str()
            .expect("a session id")
            .to_owned()
    }

    async fn establish(client: &Client) -> String {
        session_as(client, BEARER).await
    }

    async fn subscribe_as(
        client: &Client,
        bearer: &str,
        id: &str,
        cursor: Option<(String, u64)>,
    ) -> StatusCode {
        let cursors = cursor.map_or_else(Vec::new, |(device_id, last_applied_seq)| {
            vec![serde_json::json!({
                "device_id": device_id,
                "last_applied_seq": last_applied_seq,
            })]
        });
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(bearer),
                Some(&serde_json::json!({
                    "streams": [{ "stream_id": stream_hex(), "cursors": cursors }]
                })),
                &[("x-sunrise-session", id)],
            )
            .await
            .status
    }

    async fn subscribe(client: &Client, id: &str, cursor: Option<(String, u64)>) {
        assert_eq!(
            subscribe_as(client, BEARER, id, cursor).await,
            StatusCode::NO_CONTENT
        );
    }

    /// One op envelope from `device` at `seq`.
    ///
    /// Header-shaped rather than a real sealed envelope: that is exactly the
    /// surface the relay reads — `stream_id`, `device_id`, `seq`, all cleartext
    /// — and building real ones would put `sunrise-crypto`, the crate holding
    /// the keys a relay must not have, into the server's test graph.
    fn envelope(device: [u8; 16], seq: u64) -> String {
        use base64::Engine as _;
        use ciborium::value::Value;
        let entries: Vec<(Value, Value)> = vec![
            (Value::Integer(1.into()), Value::Integer(3.into())),
            (
                Value::Integer(2.into()),
                Value::Bytes(STREAM_BYTES.to_vec()),
            ),
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
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    async fn publish_as(
        client: &Client,
        bearer: &str,
        id: &str,
        ops: Vec<String>,
        batch_id: u64,
    ) -> StatusCode {
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(bearer),
                Some(&serde_json::json!({
                    "stream_id": stream_hex(),
                    "batch_id": batch_id,
                    "ops": ops,
                })),
                &[("x-sunrise-session", id)],
            )
            .await
            .status
    }

    async fn publish(client: &Client, id: &str, ops: Vec<String>, batch_id: u64) -> StatusCode {
        publish_as(client, BEARER, id, ops, batch_id).await
    }

    /// [`publish`], keeping the `Ack` body — the dedup tests assert on
    /// `server_first_seen_ms`, which a status code cannot carry.
    async fn publish_acked(
        client: &Client,
        id: &str,
        ops: Vec<String>,
        batch_id: u64,
    ) -> serde_json::Value {
        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": stream_hex(),
                    "batch_id": batch_id,
                    "ops": ops,
                })),
                &[("x-sunrise-session", id)],
            )
            .await;
        res.assert_status(StatusCode::OK);
        res.json()
    }

    async fn read_as(client: &Client, bearer: &str, id: &str, extra: &[(&str, &str)]) -> String {
        let mut headers = vec![("x-sunrise-session", id), ("authorization", bearer)];
        headers.extend_from_slice(extra);
        client
            .read_stream(
                "/api/v1/sync/events",
                &headers,
                std::time::Duration::from_millis(250),
            )
            .await
    }

    async fn read(client: &Client, id: &str, extra: &[(&str, &str)]) -> String {
        read_as(client, BEARER, id, extra).await
    }

    // -- ws_auth ------------------------------------------------------------

    /// Was `unauthenticated_upgrade_is_refused`.
    ///
    /// Against a verifier that actually checks, as the socket test's own relay
    /// did. `NullVerifier` accepts an absent header by design — that is what
    /// makes self-host work and what makes enabling authentication purely a
    /// matter of configuring a verifier.
    #[tokio::test]
    async fn an_unauthenticated_session_is_refused() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
        ));
        let client = Client::from_state(state);
        client
            .send_as(Method::POST, "/api/v1/sync/session", None, Some(&hello()))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Was `unknown_bearer_is_refused`.
    #[tokio::test]
    async fn an_unknown_bearer_is_refused() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
        ));
        let client = Client::from_state(state);

        client
            .send_as(
                Method::POST,
                "/api/v1/sync/session",
                Some("Bearer nope"),
                Some(&hello()),
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Was `valid_bearer_is_accepted`.
    #[tokio::test]
    async fn a_valid_bearer_establishes_a_session() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("good", Subject::new(ISSUER, "alice")),
        ));
        let client = Client::from_state(state);
        let _ = session_as(&client, "Bearer good").await;
    }

    /// Was `one_tenant_never_receives_another_tenants_frames`.
    ///
    /// The channel key is `(account_hash, stream_id)` and the account half comes
    /// from the verified token, never from anything the client sends — so naming
    /// another tenant's stream id reaches a different channel entirely.
    #[tokio::test]
    async fn one_tenant_never_receives_another_tenants_frames() {
        let verifier = StaticVerifier::default()
            .with("alice", Subject::new(ISSUER, "alice"))
            .with("bob", Subject::new(ISSUER, "bob"));
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);

        let alice = session_as(&client, "Bearer alice").await;
        let bob = session_as(&client, "Bearer bob").await;

        assert_eq!(
            subscribe_as(&client, "Bearer bob", &bob, None).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            publish_as(&client, "Bearer alice", &alice, vec![], 1).await,
            StatusCode::OK
        );

        let body = read_as(&client, "Bearer bob", &bob, &[]).await;
        assert!(
            !body.contains("\"kind\":\"ops\""),
            "another tenant's frame reached this stream: {body}"
        );
        assert!(
            body.contains("\"kind\":\"caught_up\""),
            "the stream must still open and report its own emptiness: {body}"
        );
    }

    /// Was `two_sessions_of_one_tenant_still_fan_out`: the isolation above is
    /// per account, not per session.
    #[tokio::test]
    async fn two_sessions_of_one_tenant_both_receive() {
        let client = Client::new(ServerConfig::default());
        let publisher = establish(&client).await;
        let reader = establish(&client).await;

        subscribe(&client, &reader, None).await;
        assert_eq!(
            publish(&client, &publisher, vec![], 1).await,
            StatusCode::OK
        );

        let body = read(&client, &reader, &[]).await;
        assert!(
            body.contains("\"kind\":\"ops\""),
            "a sibling session's batch must fan out: {body}"
        );
    }

    // -- ws_handshake -------------------------------------------------------

    /// Was `ws_handshake_round_trip`.
    #[tokio::test]
    async fn a_session_negotiates_the_same_versions_the_handshake_did() {
        let client = Client::new(ServerConfig::default());
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
            .await;
        res.assert_status(StatusCode::CREATED);
        let body = res.json();

        assert_eq!(body["wire_proto"], u32::from(WIRE_PROTO_V));
        assert_eq!(
            body["crypto_suite"],
            u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)
        );
        assert!(
            body["session_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("ses_")),
            "a session id must come back: {body}"
        );
    }

    /// A client the server cannot agree with is refused for the same reason it
    /// was refused at the socket.
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

    /// Was `ws_subscribe_receives_op_batch_fanout`.
    #[tokio::test]
    async fn a_subscribed_stream_receives_a_published_batch() {
        let client = Client::new(ServerConfig::default());
        let reader = establish(&client).await;
        let writer = establish(&client).await;
        subscribe(&client, &reader, None).await;

        assert_eq!(publish(&client, &writer, vec![], 1).await, StatusCode::OK);

        let body = read(&client, &reader, &[]).await;
        assert!(body.contains("\"kind\":\"ops\""), "no fan-out in: {body}");
    }

    /// Was `ws_malformed_op_batch_nacked_no_fanout`.
    ///
    /// Shape changed: a `Nack` frame becomes a refusal status, and the body is
    /// caught before the handler rather than after a decode.
    #[tokio::test]
    async fn a_malformed_batch_is_refused_and_never_fans_out() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        let status = publish(&client, &id, vec!["not base64!!".to_owned()], 1).await;
        assert!(status.is_client_error(), "got {status}");

        let body = read(&client, &id, &[]).await;
        assert!(
            !body.contains("\"kind\":\"ops\""),
            "a refused batch must not fan out: {body}"
        );
    }

    // -- ws_cursors ---------------------------------------------------------

    /// Was `no_cursor_still_replays_the_whole_ring`.
    #[tokio::test]
    async fn no_cursor_replays_everything_retained() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &id, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            3,
            "every retained frame must replay: {body}"
        );
    }

    /// Was `a_cursor_narrows_the_replay_to_what_the_subscriber_missed`.
    #[tokio::test]
    async fn a_cursor_narrows_the_replay_to_what_was_missed() {
        let client = Client::new(ServerConfig::default());
        let writer = establish(&client).await;
        let device = [7u8; 16];

        for seq in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![envelope(device, seq)], seq).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        subscribe(&client, &reader, Some((hex::encode(device), 2))).await;

        let body = read(&client, &reader, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            1,
            "only what the cursor does not cover may replay: {body}"
        );
    }

    /// Was `eviction_of_already_applied_ops_is_not_a_gap`.
    #[tokio::test]
    async fn a_cursor_at_the_head_replays_nothing_and_reports_no_gap() {
        let client = Client::new(ServerConfig::default());
        let writer = establish(&client).await;
        let device = [8u8; 16];
        assert_eq!(
            publish(&client, &writer, vec![envelope(device, 1)], 1).await,
            StatusCode::OK
        );

        let reader = establish(&client).await;
        subscribe(&client, &reader, Some((hex::encode(device), 1))).await;

        let body = read(&client, &reader, &[]).await;
        assert!(
            !body.contains("\"kind\":\"ops\""),
            "nothing was missed: {body}"
        );
        assert!(
            !body.contains("\"kind\":\"gap\""),
            "an applied op falling out of retention is not a loss: {body}"
        );
        assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
    }

    /// Was `a_cursor_past_durable_retention_gets_a_typed_gap_not_silence`.
    ///
    /// The load-bearing half is the ordering: the gap precedes the partial
    /// replay, so a client cannot read `caught_up` as `complete`.
    #[tokio::test]
    async fn a_cursor_past_retention_gets_a_typed_gap_before_the_replay() {
        let state = ServerState::new(ServerConfig::default())
            .with_ring_caps(RingCaps {
                max_frames: 1,
                max_bytes: 1 << 20,
            })
            .with_durable_caps(DurableCaps {
                max_bytes: 1,
                max_age_ms: u64::MAX,
            });
        let client = Client::from_state(state);
        let writer = establish(&client).await;
        let device = [9u8; 16];

        for seq in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![envelope(device, seq)], seq).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        subscribe(&client, &reader, Some((hex::encode(device), 0))).await;

        let body = read(&client, &reader, &[]).await;
        assert!(
            body.contains("\"kind\":\"gap\""),
            "eviction past a cursor must be reported, not passed over: {body}"
        );
        let gap_at = body.find("\"kind\":\"gap\"").expect("a gap");
        let caught_at = body.find("\"kind\":\"caught_up\"").expect("a marker");
        assert!(
            gap_at < caught_at,
            "a gap must precede the caught-up it qualifies: {body}"
        );
    }

    /// Was `resubscribing_replaces_the_receiver_rather_than_duplicating_it`.
    #[tokio::test]
    async fn resubscribing_replaces_the_stream_set() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        subscribe(&client, &id, None).await;
        subscribe(&client, &id, None).await;

        let session = client
            .sessions
            .get(&id, client.clock_now_ms())
            .expect("a live session");
        assert_eq!(
            session.streams.len(),
            1,
            "a second subscribe replaces rather than duplicating"
        );
    }

    /// Was `a_cursor_past_the_ring_bound_is_served_from_the_durable_log` and
    /// `ops_survive_a_relay_restart`.
    ///
    /// The in-memory ring is bounded to one frame, so anything replayed past
    /// that came off the disk — which is the property a restart relies on.
    #[tokio::test]
    async fn a_replay_past_the_ring_bound_is_served_from_the_durable_log() {
        let state = ServerState::new(ServerConfig::default()).with_ring_caps(RingCaps {
            max_frames: 1,
            max_bytes: 1 << 20,
        });
        let client = Client::from_state(state);
        let writer = establish(&client).await;

        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &writer, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        let reader = establish(&client).await;
        subscribe(&client, &reader, None).await;

        let body = read(&client, &reader, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            3,
            "the durable log, not the ring, is the authority for replay: {body}"
        );
    }

    // -- ws_device_binding --------------------------------------------------

    /// Was `an_unregistered_device_cannot_open_a_sync_session`.
    ///
    /// Also covers `a_revoked_device_cannot_open_a_sync_session`: a revoked
    /// device is not an active device, so both resolve to nothing through the
    /// same lookup.
    #[tokio::test]
    async fn an_unregistered_device_cannot_open_a_session() {
        let client = Client::new(ServerConfig::default());
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session",
                Some(BEARER),
                Some(&hello()),
                &[
                    ("x-sunrise-device", "01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
                    ("x-sunrise-device-sig", "AAAA"),
                    ("date", "Mon, 01 Jan 2024 00:00:00 GMT"),
                ],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    // -- ws_token_expiry ----------------------------------------------------

    /// Was `the_relay_advertises_the_token_refresh_capability`.
    #[tokio::test]
    async fn the_relay_advertises_the_token_refresh_capability() {
        let client = Client::new(ServerConfig::default());
        let res = client
            .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
            .await;
        res.assert_status(StatusCode::CREATED);

        let agreed = res.json()["capabilities"].as_u64().expect("a bitfield");
        assert!(
            CapabilityBits(agreed).has(Capability::SrvTokenRefresh),
            "a client only asks for a refresh if it sees this bit come back"
        );
    }

    /// Was `an_idle_session_is_closed_when_its_token_expires` and
    /// `an_inbound_frame_on_an_expired_token_ends_the_session`.
    ///
    /// One test now, because the difference between them is gone: every
    /// operation resolves the session, and an expired one resolves to nothing
    /// whether or not anything arrived.
    #[tokio::test]
    async fn an_expired_token_ends_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default().with_expiring(
            "good",
            Subject::new(ISSUER, "alice"),
            T0_MS + 60_000,
        );
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone())
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer good").await;

        clock.set(T0_MS + 60_001);
        assert_eq!(
            subscribe_as(&client, "Bearer good", &id, None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    /// Was `a_session_with_no_deadline_is_never_closed`.
    #[tokio::test]
    async fn a_session_with_no_deadline_is_never_closed() {
        let clock = TestClock::at(T0_MS);
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone());
        let client = Client::from_state(state);
        let id = establish(&client).await;

        clock.set(T0_MS + 10_000_000);
        assert_eq!(
            subscribe_as(&client, BEARER, &id, None).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Was `a_refresh_extends_the_session_without_a_reconnect`.
    #[tokio::test]
    async fn a_refresh_extends_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default()
            .with_expiring("first", Subject::new(ISSUER, "alice"), T0_MS + 60_000)
            .with_expiring("second", Subject::new(ISSUER, "alice"), T0_MS + 600_000);
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone())
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer first").await;

        let refreshed = client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer first"),
                Some(&serde_json::json!({ "token": "second" })),
                &[("x-sunrise-session", &id)],
            )
            .await;
        refreshed.assert_status(StatusCode::OK);
        assert_eq!(refreshed.json()["expires_at_ms"], T0_MS + 600_000);

        // Past the first deadline, the session is still live on the second.
        clock.set(T0_MS + 60_001);
        assert_eq!(
            subscribe_as(&client, "Bearer second", &id, None).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Was `a_refresh_with_an_unverifiable_token_is_refused_but_keeps_the_session`.
    #[tokio::test]
    async fn an_unverifiable_refresh_is_refused_but_keeps_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default().with_expiring(
            "good",
            Subject::new(ISSUER, "alice"),
            T0_MS + 60_000,
        );
        let state = ServerState::with_clock(ServerConfig::default(), clock)
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer good").await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer good"),
                Some(&serde_json::json!({ "token": "garbage" })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);

        // Still running on the credential it already had.
        assert_eq!(
            subscribe_as(&client, "Bearer good", &id, None).await,
            StatusCode::NO_CONTENT
        );
    }

    /// Was `a_refresh_naming_another_principal_ends_the_session`.
    ///
    /// The channel namespace was fixed at establishment and is never
    /// re-derived, so a token naming someone else must not be allowed to keep
    /// reading this account's stream.
    #[tokio::test]
    async fn a_refresh_naming_another_principal_ends_the_session() {
        let clock = TestClock::at(T0_MS);
        let verifier = StaticVerifier::default()
            .with_expiring("alice", Subject::new(ISSUER, "alice"), T0_MS + 60_000)
            .with_expiring("bob", Subject::new(ISSUER, "bob"), T0_MS + 600_000);
        let state = ServerState::with_clock(ServerConfig::default(), clock)
            .with_verifier(Arc::new(verifier));
        let client = Client::from_state(state);
        let id = session_as(&client, "Bearer alice").await;

        client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some("Bearer alice"),
                Some(&serde_json::json!({ "token": "bob" })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);

        assert!(
            client.sessions.get(&id, T0_MS).is_none(),
            "the session must be gone, not merely refused"
        );
    }

    /// Was `an_ack_for_a_token_with_no_expiry_reports_zero`.
    #[tokio::test]
    async fn a_refresh_for_a_token_with_no_expiry_reports_zero() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some(BEARER),
                Some(&serde_json::json!({ "token": "anything" })),
                &[("x-sunrise-session", &id)],
            )
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(
            res.json()["expires_at_ms"],
            0,
            "the self-host verifier issues no deadline"
        );
    }

    /// Was `a_refresh_with_an_undecodable_payload_is_refused`.
    #[tokio::test]
    async fn a_refresh_with_an_undecodable_body_is_refused() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;

        let status = client
            .send_with(
                Method::POST,
                "/api/v1/sync/session/refresh",
                Some(BEARER),
                Some(&serde_json::json!({ "not_a_token": 1 })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .status;
        assert!(status.is_client_error(), "got {status}");
    }

    // -- the surface's own invariants ---------------------------------------

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

    /// A session id is a bearer-equivalent, so it is checked against the caller
    /// rather than merely looked up.
    #[tokio::test]
    async fn an_unknown_session_is_refused() {
        let client = Client::new(ServerConfig::default());
        client
            .send_with(
                Method::POST,
                "/api/v1/sync/subscribe",
                Some(BEARER),
                Some(&serde_json::json!({ "streams": [] })),
                &[("x-sunrise-session", "ses_00000000000000000000000000000000")],
            )
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    /// Durable first, then the ack.
    #[tokio::test]
    async fn a_batch_is_acked_after_it_is_durable() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        let res = client
            .send_with(
                Method::POST,
                "/api/v1/sync/ops",
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": stream_hex(), "batch_id": 7, "ops": [],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await;

        res.assert_status(StatusCode::OK);
        let body = res.json();
        assert_eq!(body["batch_id"], 7);
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
                Some(BEARER),
                Some(&serde_json::json!({
                    "stream_id": "not-a-stream", "batch_id": 1, "ops": [],
                })),
                &[("x-sunrise-session", &id)],
            )
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    /// The replay, then the marker that completes it, each frame carrying the
    /// durable id `Last-Event-ID` resumes from.
    #[tokio::test]
    async fn the_event_stream_replays_then_reports_caught_up() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        assert_eq!(publish(&client, &id, vec![], 1).await, StatusCode::OK);

        let body = read(&client, &id, &[]).await;
        assert!(body.contains("\"kind\":\"ops\""), "no replay in: {body}");
        assert!(body.contains("\"kind\":\"caught_up\""), "{body}");
        let ops_at = body.find("\"kind\":\"ops\"").expect("a replay");
        let caught_at = body.find("\"kind\":\"caught_up\"").expect("a marker");
        assert!(ops_at < caught_at, "{body}");
        assert!(body.contains("id: 1"), "{body}");
    }

    /// `Last-Event-ID` resumes rather than replaying from the start.
    #[tokio::test]
    async fn a_resumed_stream_does_not_replay_what_it_already_had() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &id, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        let body = read(&client, &id, &[("last-event-id", "2")]).await;
        assert!(!body.contains("id: 1"), "already had: {body}");
        assert!(!body.contains("id: 2"), "already had: {body}");
        assert!(body.contains("id: 3"), "not yet had: {body}");
    }

    // -- ws_batch_dedup -----------------------------------------------------

    /// The defect: a client that re-sends a batch it never saw acked — every
    /// reconnect re-drains the outbox — had the relay store and fan out a
    /// second copy of ops it already held.
    #[tokio::test]
    async fn a_resent_batch_is_appended_once_and_acked_once() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        let ops = vec![envelope([3u8; 16], 1)];

        let first = publish_acked(&client, &id, ops.clone(), 1).await;
        let second = publish_acked(&client, &id, ops, 1).await;

        assert_eq!(
            first["server_first_seen_ms"], second["server_first_seen_ms"],
            "a re-send is acked with the timestamp the first copy got"
        );
        assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 1);

        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            1,
            "the batch must be retained once, not twice: {body}"
        );
    }

    /// Why the key cannot be the `batch_id`.
    ///
    /// `sync_driver.rs` initialises its counter inside `session()`, so a
    /// reconnect re-sends the very same ops under a *different* number — and,
    /// worse, later ops under a number an earlier session already used. Only a
    /// content key catches the first and spares the second.
    #[tokio::test]
    async fn a_resent_batch_reacked_after_a_reconnect_carries_the_first_timestamp() {
        let clock = TestClock::at(T0_MS);
        let state = ServerState::with_clock(ServerConfig::default(), clock.clone());
        let client = Client::from_state(state);
        let ops = vec![envelope([4u8; 16], 9)];

        let first = publish_acked(&client, &establish(&client).await, ops.clone(), 7).await;
        assert_eq!(first["server_first_seen_ms"], T0_MS);

        // A new session, a counter that restarted, and a later clock.
        clock.set(T0_MS + 60_000);
        let second = publish_acked(&client, &establish(&client).await, ops, 1).await;

        assert_eq!(
            second["server_first_seen_ms"], T0_MS,
            "'first seen' is when the relay first saw it, not when the copy arrived"
        );
    }

    /// The other half of the same rule: the same `batch_id` over different ops
    /// is two batches, and reading them as one would be the data loss the
    /// `batch_id` key was rejected for.
    #[tokio::test]
    async fn a_batch_with_different_ops_is_never_deduped() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        publish_acked(&client, &id, vec![envelope([5u8; 16], 1)], 1).await;
        publish_acked(&client, &id, vec![envelope([5u8; 16], 2)], 1).await;

        assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 0);
        let body = read(&client, &id, &[]).await;
        assert_eq!(
            body.matches("\"kind\":\"ops\"").count(),
            2,
            "two distinct batches must both be retained: {body}"
        );
    }

    /// An empty batch has no content to be the same as. Collapsing them would
    /// silently drop the three-event expectation the cursor tests hold.
    #[tokio::test]
    async fn an_empty_batch_is_never_deduped() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;

        for batch_id in 1..=3u64 {
            assert_eq!(
                publish(&client, &id, vec![], batch_id).await,
                StatusCode::OK
            );
        }

        assert_eq!(client.metrics.get("sunrise_relay_batch_duplicate_total"), 0);
        let body = read(&client, &id, &[]).await;
        assert_eq!(body.matches("\"kind\":\"ops\"").count(), 3, "{body}");
    }

    /// A duplicate must not reach the ring either. Storing it once and fanning
    /// it out twice would leave every live subscriber re-applying ops in
    /// proportion to how flaky the *sender's* link is.
    #[tokio::test]
    async fn a_duplicate_is_not_fanned_out_to_a_live_subscriber() {
        let client = Client::new(ServerConfig::default());
        let id = establish(&client).await;
        subscribe(&client, &id, None).await;
        let ops = vec![envelope([6u8; 16], 1)];

        publish_acked(&client, &id, ops.clone(), 1).await;
        let body = read(&client, &id, &[]).await;
        assert!(body.contains("id: 1"), "the first copy is frame 1: {body}");

        publish_acked(&client, &id, ops, 2).await;

        // Resuming past frame 1 is how a live subscriber that already had the
        // first copy sees the stream: a second frame would show up here.
        let after = read(&client, &id, &[("last-event-id", "1")]).await;
        assert!(
            !after.contains("\"kind\":\"ops\""),
            "a duplicate produced a second frame: {after}"
        );
    }
}
