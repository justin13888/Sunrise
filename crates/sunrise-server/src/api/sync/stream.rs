//! The SSE downstream: replay, fan-out, and the reasons a stream ends.
//!
//! The only async, task-spawning half of this surface and the only consumer of
//! the relay's live subscriptions. [`events`] opens a stream, [`spawn_stream`]
//! replays the durable log into it, and [`live_loop`] drives the fan-out
//! alongside the token deadline and the revocation re-check. [`SyncEvent`] is
//! the event vocabulary all of that emits, and [`KEEP_ALIVE_SECS`] with
//! [`STREAM_BUFFER`] are the two bounds a long-lived stream is held to.

use crate::api::error::{codes, ApiError};
use crate::api::signed::SignedParts;
use crate::relay::{CursorGap, RelayFrame};
use crate::state::ServerState;
use crate::sync_session::Session;
use base64::Engine as _;
use kynos::di::inject::Inject;
use kynos::extract::params::header::Headers;
use kynos::extract::sse::LastEventId;
use kynos::response::stream::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use sunrise_error::ErrorCode;

use super::credential::{resolve, SessionHeader};

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
///
/// # The resume order, enforced
///
/// A `Last-Event-ID` and a `Subscribe`'s cursors are two different statements:
/// the id is what the client *received* on a stream, the cursors are what it
/// *applied* to its op log. They agree right up until the moment they matter —
/// a frame delivered but not committed, which is what a crash between receipt
/// and commit leaves behind, and which is exactly when a client re-sends
/// `Subscribe` to recover.
///
/// `relay_replay_after` selects on the id alone when one is present, so
/// serving that pair would skip the very frames the cursors asked for and
/// report `caught_up` over the hole. Rather than pick one silently, a resume
/// id presented on the **first stream after a `Subscribe`** is refused with
/// `400 SYNC_RESUME_CONFLICT`; the client drops the id and reopens, and gets
/// cursor-selected replay. An ordinary reconnect — a stream following another
/// stream of the same session — is untouched and still resumes on its id.
///
/// The alternative, unioning the two filters, was rejected because the cursors
/// only advance when a client sends a new `Subscribe`: a client that resumes
/// correctly for a week on `Last-Event-ID` would have every frame since its
/// last `Subscribe` replayed on every reconnect, which is not resumption at
/// all. The alternative of documenting the precedence and leaving the server
/// alone leaves a data-loss hazard as a client's responsibility to read
/// carefully, which is what the wire protocol document already declined to do
/// by calling clearing the id on `Subscribe` part of the client contract.
#[kynos::get("/api/v1/sync/events", operation_id = "syncEvents")]
pub async fn events(
    Inject(state): Inject<ServerState>,
    Headers(header): Headers<SessionHeader>,
    LastEventId(resume): LastEventId,
    SignedParts(caller): SignedParts,
) -> Result<Sse<EventStream>, ApiError> {
    let now_ms = state.clock.now_ms();
    let (id, session) = resolve(&state, &header, &caller, now_ms)?;
    // A zero is not a resume point: `relay_replay_after` already reads 0 as
    // "first connection", so it carries no claim to conflict with.
    let after = resume
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0);

    if after.is_some() && session.subscribe_unserved {
        state.metrics.incr("sunrise_sync_resume_conflict_total");
        tracing::warn!(
            ev = "srv.sync.resume_conflict",
            err_code = %ErrorCode::SyncResumeConflict,
            err_kind = "permanent",
            retryable = false,
            account_h = %crate::logging::account_h(&session.account_id),
            n_streams = session.streams.len() as u64,
            "a resume id was presented on the first stream after a subscribe"
        );
        return Err(ApiError::validation_coded(
            codes::SYNC_RESUME_CONFLICT,
            "Last-Event-ID and a fresh Subscribe are two different positions; \
             reopen without the id to replay from the cursors",
        ));
    }
    // The set is served from here on, so the ids this stream mints are a
    // statement about it and the next reconnect may resume on one.
    if session.subscribe_unserved {
        state
            .sessions
            .update(&id, now_ms, |s| s.subscribe_unserved = false);
    }

    state.metrics.incr("sunrise_sync_stream_total");
    // `resumed` is the reason this event is worth emitting rather than
    // deleting from the catalogue: a stream that opens cold is a client that
    // has no `Last-Event-ID` to present, and a step change in the cold rate
    // after a deploy is clients losing the id rather than choosing not to use
    // it. The counter beside this one cannot tell those apart.
    tracing::info!(
        ev = "srv.sync.stream_open",
        account_h = %crate::logging::account_h(&session.account_id),
        resumed = after.is_some(),
        "sync event stream opened"
    );

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
