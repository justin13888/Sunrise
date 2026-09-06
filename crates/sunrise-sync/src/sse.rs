//! Concrete SSE + `POST` client [`Transport`] (feature `sse`).
//!
//! ADR-0023 replaced the `/sync` WebSocket with an event stream downstream and
//! typed operations upstream. The [`Transport`] trait did not change, which is
//! the point of it: the sync driver still hands this layer whole encoded wire
//! frames and still reads whole encoded wire frames back, so `sync_driver`, the
//! outbox, the cursor bookkeeping and the reconnect backoff are untouched.
//!
//! What this module owns is the translation between the two shapes:
//!
//! | Frame the driver sends | Operation |
//! |---|---|
//! | `Hello` | `POST /sync/session`, whose reply becomes a `HelloAck` frame |
//! | `Subscribe` | `POST /sync/subscribe` |
//! | `OpBatch` | `POST /sync/ops`, whose reply becomes an `Ack` frame |
//! | `RefreshToken` | `POST /sync/session/refresh` → `RefreshTokenAck` |
//! | `Ping` | answered locally with a `Pong`; the stream has keep-alives |
//!
//! and, in the other direction, each SSE event back into the frame the driver
//! already knows how to handle. An `ops` event carries the relay's verbatim
//! frame bytes, so that direction is a base64 decode rather than a re-encoding:
//! what the driver applies is exactly what the relay stored.
//!
//! # Why the frames survive at all
//!
//! Because the payload types and their canonical CBOR encoding are unchanged —
//! ADR-0023 retires the *frame header* (magic, versions, `msg_kind`, flags,
//! length), which HTTP and SSE event types subsume, not the payloads. Keeping
//! the header on this seam means one adapter changes instead of every caller,
//! and `Hello::negotiate`'s frozen fixtures keep testing the thing they were
//! written for.

use crate::transport::{Transport, TransportError};
use async_trait::async_trait;
use base64::Engine as _;
use http_body_util::{BodyExt as _, Full};
use hyper::body::{Bytes, Incoming};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::collections::VecDeque;
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, CaughtUpPayload, ClosePayload, CursorEntry,
    ErrorPayload, FrameFlags, Hello, HelloAck, MsgKind, OpBatchPayload, RefreshTokenAckPayload,
    RefreshTokenPayload, SubscribePayload,
};

/// The largest SSE event this client will buffer before giving up.
///
/// A relay that never emits a blank line would otherwise grow this buffer
/// without bound. The frame cap is the natural ceiling: an event carries one
/// frame, base64-encoded.
const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

/// SSE + `POST` client transport over `http://` / `https://`.
pub struct SseTransport {
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Full<Bytes>,
    >,
    /// Origin, no trailing slash: `http://127.0.0.1:8443`.
    base: String,
    bearer: Option<String>,
    /// Set by the reply to `Hello`. Every later operation presents it.
    session: Option<String>,
    /// Frames produced by an operation's own reply, waiting to be read.
    inbound: VecDeque<Vec<u8>>,
    /// The live event stream, opened on the first read after a subscribe.
    events: Option<Incoming>,
    /// Bytes of a partially-received event.
    buf: Vec<u8>,
    /// The last `id:` seen, so a reconnect resumes rather than replays.
    last_event_id: Option<String>,
    closed: bool,
}

impl std::fmt::Debug for SseTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseTransport")
            .field("base", &self.base)
            .field("session", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl SseTransport {
    /// Point at `base` (e.g. `https://relay.example`) unauthenticated.
    ///
    /// Only a self-host relay running `NullVerifier` accepts this; every other
    /// deployment answers 401.
    #[must_use]
    pub fn connect(base: &str) -> Self {
        Self::connect_with_bearer(base, None)
    }

    /// Point at `base`, presenting `bearer` on every request.
    ///
    /// Nothing is dialled here. Unlike the socket this replaces there is no
    /// handshake to fail at construction: the first request is the `Hello` the
    /// driver sends, and its failure is reported there — which is where the
    /// driver's backoff already expects to see one.
    #[must_use]
    pub fn connect_with_bearer(base: &str, bearer: Option<&str>) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        Self {
            client: Client::builder(TokioExecutor::new()).build(https),
            base: base.trim_end_matches('/').to_owned(),
            bearer: bearer.map(ToOwned::to_owned),
            session: None,
            inbound: VecDeque::new(),
            events: None,
            buf: Vec::new(),
            last_event_id: None,
            closed: false,
        }
    }

    /// Issue one JSON request and read the whole reply.
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(hyper::StatusCode, Vec<u8>), TransportError> {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base));
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(session) = &self.session {
            request = request.header("x-sunrise-session", session);
        }
        let request = match body {
            Some(value) => request
                .header(hyper::header::CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&value).map_err(|e| protocol(&e))?,
                ))),
            None => request.body(Full::new(Bytes::new())),
        }
        .map_err(|e| protocol(&e))?;

        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))?
            .to_bytes()
            .to_vec();
        Ok((status, bytes))
    }

    /// Turn a non-2xx into the typed error the driver branches on.
    ///
    /// The relay's own code wins wherever it is one this build knows. It was
    /// parsed out of the problem document and then thrown away for a status
    /// map, which flattened every `401` to `AUTH_TOKEN_INVALID` — so a client
    /// told "your signature is stale, fix your clock" heard "your bearer is
    /// bad" and refreshed a token that was never the problem. The status map
    /// stays as the fallback for a code this build does not know and for a body
    /// that carries none, which is what keeps an older client working against a
    /// newer relay.
    fn refuse(status: hyper::StatusCode, body: &[u8]) -> TransportError {
        // The problem document carries the stable code as an extension member,
        // which is exactly what a client is meant to switch on.
        let code = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("code")
                    .and_then(|c| c.as_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| status.as_str().to_owned());
        let by_status = match status.as_u16() {
            401 | 403 => "AUTH_TOKEN_INVALID",
            503 => "RELAY_STORAGE_UNAVAILABLE",
            _ => "SYNC_OP_INVALID",
        };
        TransportError::Server {
            code: sunrise_error::ErrorCode::from_wire_str(&code)
                .map_or(by_status, sunrise_error::ErrorCode::as_str),
            message: format!("{status}: {code}"),
        }
    }

    /// Open the event stream, resuming from the last id if there is one.
    async fn open_events(&mut self) -> Result<(), TransportError> {
        let Some(session) = self.session.clone() else {
            return Err(TransportError::Protocol(
                "no sync session: send Hello first".to_owned(),
            ));
        };
        let mut request = hyper::Request::builder()
            .method("GET")
            .uri(format!("{}/api/v1/sync/events", self.base))
            .header("x-sunrise-session", session)
            .header(hyper::header::ACCEPT, "text/event-stream");
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(id) = &self.last_event_id {
            request = request.header("last-event-id", id);
        }
        let request = request
            .body(Full::new(Bytes::new()))
            .map_err(|e| protocol(&e))?;

        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))?;
        if !response.status().is_success() {
            return Err(Self::refuse(response.status(), &[]));
        }
        self.events = Some(response.into_body());
        Ok(())
    }

    /// Pull the next complete event out of the buffer, if one is there.
    ///
    /// SSE separates events with a blank line, so a complete event is
    /// everything up to the first `\n\n`. Comments — the keep-alives that
    /// replaced `Ping` — start with `:` and are dropped here rather than
    /// reaching the driver, which has no frame for them.
    fn take_event(&mut self) -> Option<(Option<String>, String)> {
        let end = self
            .buf
            .windows(2)
            .position(|w| w == b"\n\n")
            .map(|i| i + 2)?;
        let raw = self.buf.drain(..end).collect::<Vec<u8>>();
        let text = String::from_utf8_lossy(&raw);

        let mut id = None;
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("id:") {
                id = Some(rest.trim().to_owned());
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            // A comment or a bare keep-alive: nothing for the driver.
            return Some((id, String::new()));
        }
        Some((id, data))
    }

    /// Convert one decoded event into the frame the driver expects.
    fn frame_for(event: &serde_json::Value) -> Result<Option<Vec<u8>>, TransportError> {
        let kind = event
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        match kind {
            // Verbatim: the relay stored the frame the publisher sent, and this
            // is that frame. Nothing is re-encoded, so nothing can drift.
            "ops" => {
                let raw = event
                    .get("frame")
                    .and_then(|f| f.as_str())
                    .ok_or_else(|| TransportError::Protocol("ops event has no frame".to_owned()))?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(raw)
                    .map_err(|e| protocol(&e))?;
                Ok(Some(bytes))
            }
            "caught_up" => {
                let stream_id = parse_id(event.get("stream_id"))?;
                let payload = CaughtUpPayload { stream_id }
                    .encode()
                    .map_err(|e| protocol(&e))?;
                Ok(Some(
                    encode_frame(MsgKind::StreamUpdate, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                ))
            }
            "gap" => Ok(Some(error_frame(
                sunrise_error::ErrorCode::SyncCursorGap,
                event
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("cursor gap"),
            )?)),
            "closed" => {
                let payload = ClosePayload {
                    code: sunrise_error::ErrorCode::AuthTokenExpired,
                    reason: event
                        .get("reason")
                        .and_then(|r| r.as_str())
                        .unwrap_or("session closed")
                        .to_owned(),
                }
                .encode()
                .map_err(|e| protocol(&e))?;
                Ok(Some(
                    encode_frame(MsgKind::Close, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                ))
            }
            _ => Ok(None),
        }
    }
}

#[async_trait]
impl Transport for SseTransport {
    // One arm per frame kind, each of them short. The length is the size of the
    // protocol rather than complexity in any one branch, and splitting it would
    // scatter the mapping this module exists to state in one place.
    #[allow(clippy::too_many_lines)]
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Cancelled);
        }
        let (head, payload) = decode_frame(&frame).map_err(|e| protocol(&e))?;

        match head.msg_kind {
            MsgKind::Hello => {
                // `Hello` and `HelloAck` ride plain ciborium rather than the
                // canonical codec every other payload uses — one of the five
                // defects ADR-0023 catalogues, and fixed in place rather than
                // here, so this matches what the driver encodes today.
                let hello: Hello =
                    ciborium::de::from_reader(&payload[..]).map_err(|e| protocol(&e))?;
                let (status, body) = self
                    .call(
                        "POST",
                        "/api/v1/sync/session",
                        Some(serde_json::json!({
                            "client_app_v": hello.client_app_v,
                            "client_platform": hello.client_platform,
                            "wire_proto_supported": hello.wire_proto_supported,
                            "doc_schema_min": hello.doc_schema_min,
                            "doc_schema_max": hello.doc_schema_max,
                            "crypto_suite_supported": hello.crypto_suite_supported,
                            "capabilities": hello.capabilities,
                            "trace": hello.trace,
                        })),
                    )
                    .await?;
                if !status.is_success() {
                    return Err(Self::refuse(status, &body));
                }
                let reply: serde_json::Value =
                    serde_json::from_slice(&body).map_err(|e| protocol(&e))?;
                self.session = reply
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(ToOwned::to_owned);

                let ack = HelloAck {
                    server_app_v: field_str(&reply, "server_app_v"),
                    wire_proto: field_u32(&reply, "wire_proto")?,
                    crypto_suite: field_u32(&reply, "crypto_suite")?,
                    doc_schema_floor: field_u32(&reply, "doc_schema_floor")?,
                    capabilities: field_u64(&reply, "capabilities"),
                    server_time_ms: field_u64(&reply, "server_time_ms"),
                };
                let mut payload = Vec::new();
                ciborium::ser::into_writer(&ack, &mut payload).map_err(|e| protocol(&e))?;
                self.inbound.push_back(
                    encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                );
            }

            MsgKind::Subscribe => {
                let sub = SubscribePayload::decode(&payload).map_err(|e| protocol(&e))?;
                let streams: Vec<serde_json::Value> = sub
                    .streams
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "stream_id": hex::encode(s.stream_id),
                            "cursors": s.cursors.iter().map(cursor_json).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                let (status, body) = self
                    .call(
                        "POST",
                        "/api/v1/sync/subscribe",
                        Some(serde_json::json!({ "streams": streams })),
                    )
                    .await?;
                if !status.is_success() {
                    return Err(Self::refuse(status, &body));
                }
                // The stream set changed, so the open stream is stale. Dropping
                // it makes the next read reopen against the new set.
                self.events = None;
                self.buf.clear();
                // And the resume point goes with it, which is the load-bearing
                // half. `Last-Event-ID` says "I received everything up to here";
                // the cursors in a `Subscribe` say "I have *applied* everything
                // up to here". Those differ exactly when delivery succeeded and
                // application did not — a dropped frame, a client that restarted
                // mid-batch — and that is precisely when the driver re-sends
                // `Subscribe` to recover. Keeping the id would resume past the
                // ops the cursors are asking for, so the recovery path would
                // silently skip what it exists to fetch. The stricter of the two
                // statements wins, and a client restating its cursors is making
                // the stricter one.
                self.last_event_id = None;
            }

            MsgKind::OpBatch => {
                let batch = OpBatchPayload::decode(&payload).map_err(|e| protocol(&e))?;
                let ops: Vec<String> = batch
                    .ops
                    .iter()
                    .map(|o| base64::engine::general_purpose::STANDARD.encode(o))
                    .collect();
                let (status, body) = self
                    .call(
                        "POST",
                        "/api/v1/sync/ops",
                        Some(serde_json::json!({
                            "stream_id": hex::encode(batch.stream_id),
                            "batch_id": batch.batch_id,
                            "ops": ops,
                        })),
                    )
                    .await?;
                if !status.is_success() {
                    return Err(Self::refuse(status, &body));
                }
                let reply: serde_json::Value =
                    serde_json::from_slice(&body).map_err(|e| protocol(&e))?;
                let ack = AckPayload {
                    batch_id: batch.batch_id,
                    stream_id: batch.stream_id,
                    server_first_seen_ms: field_u64(&reply, "server_first_seen_ms"),
                };
                let payload = ack.encode().map_err(|e| protocol(&e))?;
                self.inbound.push_back(
                    encode_frame(MsgKind::Ack, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                );
            }

            MsgKind::RefreshToken => {
                let refresh = RefreshTokenPayload::decode(&payload).map_err(|e| protocol(&e))?;
                let (status, body) = self
                    .call(
                        "POST",
                        "/api/v1/sync/session/refresh",
                        Some(serde_json::json!({ "token": refresh.token })),
                    )
                    .await?;
                if !status.is_success() {
                    return Err(Self::refuse(status, &body));
                }
                let reply: serde_json::Value =
                    serde_json::from_slice(&body).map_err(|e| protocol(&e))?;
                let ack = RefreshTokenAckPayload {
                    expires_at_ms: field_u64(&reply, "expires_at_ms"),
                };
                let payload = ack.encode().map_err(|e| protocol(&e))?;
                self.inbound.push_back(
                    encode_frame(MsgKind::RefreshTokenAck, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                );
            }

            // The stream keeps itself alive with comments, so a liveness probe
            // needs no round trip: answering locally keeps the driver's
            // ping/pong bookkeeping working with nothing on the wire.
            MsgKind::Ping => {
                self.inbound.push_back(
                    encode_frame(MsgKind::Pong, FrameFlags::EMPTY, &[])
                        .map_err(|e| protocol(&e))?,
                );
            }

            MsgKind::Close => {
                self.closed = true;
                self.events = None;
            }

            other => {
                return Err(TransportError::Protocol(format!(
                    "no operation carries {other:?} upstream"
                )));
            }
        }
        Ok(())
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            if let Some(frame) = self.inbound.pop_front() {
                return Ok(Some(frame));
            }
            if self.closed {
                return Ok(None);
            }
            if self.events.is_none() {
                self.open_events().await?;
            }

            // Drain what is already buffered before waiting on the socket.
            while let Some((id, data)) = self.take_event() {
                if let Some(id) = id {
                    self.last_event_id = Some(id);
                }
                if data.is_empty() {
                    continue;
                }
                let event: serde_json::Value =
                    serde_json::from_str(&data).map_err(|e| protocol(&e))?;
                if let Some(frame) = Self::frame_for(&event)? {
                    return Ok(Some(frame));
                }
            }

            let Some(body) = self.events.as_mut() else {
                return Ok(None);
            };
            match body.frame().await {
                None => {
                    // The relay closed the stream. The driver's reconnect is
                    // what decides whether to come back, so this is a graceful
                    // end rather than an error.
                    self.events = None;
                    return Ok(None);
                }
                Some(Err(e)) => {
                    self.events = None;
                    return Err(TransportError::Unavailable(e.to_string()));
                }
                Some(Ok(chunk)) => {
                    if let Some(data) = chunk.data_ref() {
                        if self.buf.len() + data.len() > MAX_EVENT_BYTES {
                            self.events = None;
                            return Err(TransportError::Protocol(
                                "event exceeded the frame cap without terminating".to_owned(),
                            ));
                        }
                        self.buf.extend_from_slice(data);
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.closed = true;
        self.events = None;
        self.buf.clear();
        Ok(())
    }
}

/// Everything that is a malformed exchange rather than an unreachable one.
fn protocol<E: std::fmt::Display>(e: &E) -> TransportError {
    TransportError::Protocol(e.to_string())
}

fn field_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|f| f.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn field_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

/// A negotiated version that does not fit a `u32` is a malformed reply rather
/// than a number to quietly truncate: the whole point of negotiation is that
/// both sides agree on the value.
fn field_u32(v: &serde_json::Value, key: &str) -> Result<u32, TransportError> {
    u32::try_from(field_u64(v, key))
        .map_err(|_| TransportError::Protocol(format!("{key} does not fit a u32")))
}

fn cursor_json(c: &CursorEntry) -> serde_json::Value {
    serde_json::json!({
        "device_id": hex::encode(c.device_id),
        "last_applied_seq": c.last_applied_seq,
    })
}

fn parse_id(v: Option<&serde_json::Value>) -> Result<[u8; 16], TransportError> {
    let s = v
        .and_then(|f| f.as_str())
        .ok_or_else(|| TransportError::Protocol("event has no stream_id".to_owned()))?;
    let mut out = [0u8; 16];
    hex::decode_to_slice(s, &mut out).map_err(|e| protocol(&e))?;
    Ok(out)
}

fn error_frame(code: sunrise_error::ErrorCode, reason: &str) -> Result<Vec<u8>, TransportError> {
    let payload = ErrorPayload {
        code,
        reason: reason.to_owned(),
    }
    .encode()
    .map_err(|e| protocol(&e))?;
    encode_frame(MsgKind::Error, FrameFlags::EMPTY, &payload).map_err(|e| protocol(&e))
}

#[cfg(test)]
mod tests {
    use super::SseTransport;
    use crate::transport::{Transport, TransportError};

    /// Compile-time assertion that this still fits the driver's transport slot.
    #[test]
    fn it_is_a_transport() {
        fn assert_transport<T: Transport>() {}
        assert_transport::<SseTransport>();
    }

    /// A frame sent before `Hello` has nowhere to go, and saying so beats
    /// issuing a request with no session that the relay would refuse anyway.
    #[tokio::test]
    async fn a_read_before_hello_reports_the_missing_session() {
        let mut t = SseTransport::connect("http://127.0.0.1:1");
        let err = t.recv_frame().await.expect_err("no session yet");
        assert!(
            format!("{err}").contains("session"),
            "expected a session complaint, got {err}"
        );
    }

    fn code_of(err: &TransportError) -> &'static str {
        match err {
            TransportError::Server { code, .. } => code,
            other => panic!("expected a server refusal, got {other}"),
        }
    }

    /// The regression: a `401` whose body names a code this build knows keeps
    /// that code. Flattening it to the status map is what left a client
    /// refreshing a bearer to cure a clock.
    #[test]
    fn a_typed_code_survives_the_status_map() {
        let body = br#"{"type":"https://sunrise.app/problems/unauthenticated","status":401,"code":"AUTH_DEVICE_SIG_INVALID"}"#;
        let err = SseTransport::refuse(hyper::StatusCode::UNAUTHORIZED, body);
        assert_eq!(code_of(&err), "AUTH_DEVICE_SIG_INVALID");
    }

    /// And the fallback still holds, so a relay that answers with a code this
    /// build has never heard of — or with no body at all — is still classified
    /// rather than dropped on the floor.
    #[test]
    fn an_untyped_401_still_maps_to_the_token_code() {
        let err = SseTransport::refuse(hyper::StatusCode::UNAUTHORIZED, b"");
        assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");

        let unknown = br#"{"status":401,"code":"AUTH_SOMETHING_FROM_THE_FUTURE"}"#;
        let err = SseTransport::refuse(hyper::StatusCode::UNAUTHORIZED, unknown);
        assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
    }
}
