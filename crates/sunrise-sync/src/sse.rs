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
//! # The device binding
//!
//! Every one of those operations is bound to a [`DeviceSigner`] when the caller
//! supplies one. Six routes are reached from here — the four `POST`s above, the
//! revocation `DELETE`, and the `GET /sync/events` that opens the stream — and
//! `sunrise_server::api::signed` demands a binding on all six under
//! `require_device_sig`. The two bootstrap routes are not reached from here:
//! `sunrise_relay_client::bootstrap` makes those, and a device cannot sign
//! before the relay has a row for it, which is the exemption ADR-0022 records.
//!
//! A transport built without a signer sends no binding at all, which is what a
//! self-host relay running `NullVerifier` expects and what
//! `ServerConfig::validate` guarantees is the configured state there. So the
//! signer is an option rather than a constructor argument: the unauthenticated
//! path is a real deployment, not an omission.
//!
//! # Why the frames survive at all
//!
//! Because the payload types and their canonical CBOR encoding are unchanged —
//! ADR-0023 retires the *frame header* (magic, versions, `msg_kind`, flags,
//! length), which HTTP and SSE event types subsume, not the payloads. Keeping
//! the header on this seam means one adapter changes instead of every caller,
//! and `Hello::negotiate`'s frozen fixtures keep testing the thing they were
//! written for.

use crate::signer::DeviceSigner;
use crate::transport::{RevokeOutcome, Transport, TransportError};
use async_trait::async_trait;
use base64::Engine as _;
use http_body_util::{BodyExt as _, Full};
use hyper::body::{Bytes, Incoming};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::collections::VecDeque;
use std::sync::Arc;
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
    /// The device binding this transport presents, when it has one.
    signer: Option<Arc<dyn DeviceSigner>>,
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
            .field("bound", &self.signer.is_some())
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
            signer: None,
            session: None,
            inbound: VecDeque::new(),
            events: None,
            buf: Vec::new(),
            last_event_id: None,
            closed: false,
        }
    }

    /// Bind `signer`'s device to every request this transport makes.
    ///
    /// Builder rather than a constructor argument, so the unauthenticated
    /// self-host path keeps the two-argument `connect` it already had — and so
    /// the caller that *can* sign is the one that says so, rather than every
    /// caller having to pass a `None`.
    #[must_use]
    pub fn with_device_signer(mut self, signer: Arc<dyn DeviceSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// The three `header_sig_v2` headers for one request, or none if this
    /// transport carries no binding.
    ///
    /// `body` is the value that will be sent, not the bytes: ADR-0022 signs the
    /// RFC 8785 canonical form of the request *value*, which is what lets the
    /// relay recompute the same string from what it parsed. Signing the octets
    /// this client happens to emit would break the moment either side changed
    /// its key order.
    fn binding(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Vec<(&'static str, String)>, TransportError> {
        let Some(signer) = &self.signer else {
            return Ok(Vec::new());
        };
        let date = sunrise_http_sig::date_header(signer.now_ms());
        let signature =
            sunrise_http_sig::sign_with(|msg| signer.sign(msg), method, path, &date, body)
                .map_err(|e| protocol(&e))?;
        Ok(vec![
            (sunrise_http_sig::DEVICE_HEADER, signer.device_id()),
            (sunrise_http_sig::DEVICE_SIG_HEADER, signature),
            ("date", date),
        ])
    }

    /// Issue one JSON request and read the whole reply.
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Reply, TransportError> {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base));
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(session) = &self.session {
            request = request.header("x-sunrise-session", session);
        }
        for (name, value) in self.binding(method, path, body.as_ref())? {
            request = request.header(name, value);
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
        let date = server_date(response.headers());
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))?
            .to_bytes()
            .to_vec();
        Ok(Reply {
            status,
            date,
            bytes,
        })
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
    ///
    /// A refused *binding* additionally gets [`binding_advice`] appended, since
    /// the code alone still leaves the user with nothing to do.
    fn refuse(&self, reply: &Reply) -> TransportError {
        // The problem document carries the stable code as an extension member,
        // which is exactly what a client is meant to switch on.
        let reported = serde_json::from_slice::<serde_json::Value>(&reply.bytes)
            .ok()
            .and_then(|v| {
                v.get("code")
                    .and_then(|c| c.as_str())
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| reply.status.as_str().to_owned());
        let by_status = match reply.status.as_u16() {
            401 | 403 => "AUTH_TOKEN_INVALID",
            503 => "RELAY_STORAGE_UNAVAILABLE",
            _ => "SYNC_OP_INVALID",
        };
        let code = sunrise_error::ErrorCode::from_wire_str(&reported)
            .map_or(by_status, sunrise_error::ErrorCode::as_str);
        let mut message = format!("{}: {reported}", reply.status);
        if code == sunrise_error::ErrorCode::AuthDeviceSigInvalid.as_str() {
            message.push_str(&binding_advice(
                reply.date.as_deref(),
                self.signer.as_ref().map(|s| s.now_ms()),
            ));
        }
        TransportError::Server { code, message }
    }

    /// Open the event stream, resuming from the last id if there is one.
    async fn open_events(&mut self) -> Result<(), TransportError> {
        let Some(session) = self.session.clone() else {
            return Err(TransportError::Protocol(
                "no sync session: send Hello first".to_owned(),
            ));
        };
        // `SignedParts`: the stream is a bound route like every other, and
        // hashes the empty string because it carries no body.
        let target = "/api/v1/sync/events";
        let mut request = hyper::Request::builder()
            .method("GET")
            .uri(format!("{}{target}", self.base))
            .header("x-sunrise-session", session)
            .header(hyper::header::ACCEPT, "text/event-stream");
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        if let Some(id) = &self.last_event_id {
            request = request.header("last-event-id", id);
        }
        for (name, value) in self.binding("GET", target, None)? {
            request = request.header(name, value);
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
            // The body is left unread rather than collected: a stream refusal
            // is diagnosed from its status, its code and the relay's `Date`,
            // and this path never had the body anyway.
            return Err(self.refuse(&Reply {
                status: response.status(),
                date: server_date(response.headers()),
                bytes: Vec::new(),
            }));
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
                let reply = self
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
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
                }
                let parsed: serde_json::Value =
                    serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
                self.session = parsed
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(ToOwned::to_owned);

                let ack = HelloAck {
                    server_app_v: field_str(&parsed, "server_app_v"),
                    wire_proto: field_u32(&parsed, "wire_proto")?,
                    crypto_suite: field_u32(&parsed, "crypto_suite")?,
                    doc_schema_floor: field_u32(&parsed, "doc_schema_floor")?,
                    capabilities: field_u64(&parsed, "capabilities"),
                    server_time_ms: field_u64(&parsed, "server_time_ms"),
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
                let reply = self
                    .call(
                        "POST",
                        "/api/v1/sync/subscribe",
                        Some(serde_json::json!({ "streams": streams })),
                    )
                    .await?;
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
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
                let reply = self
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
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
                }
                let parsed: serde_json::Value =
                    serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
                let ack = AckPayload {
                    batch_id: batch.batch_id,
                    stream_id: batch.stream_id,
                    server_first_seen_ms: field_u64(&parsed, "server_first_seen_ms"),
                };
                let payload = ack.encode().map_err(|e| protocol(&e))?;
                self.inbound.push_back(
                    encode_frame(MsgKind::Ack, FrameFlags::EMPTY, &payload)
                        .map_err(|e| protocol(&e))?,
                );
            }

            MsgKind::RefreshToken => {
                let refresh = RefreshTokenPayload::decode(&payload).map_err(|e| protocol(&e))?;
                let reply = self
                    .call(
                        "POST",
                        "/api/v1/sync/session/refresh",
                        Some(serde_json::json!({ "token": refresh.token })),
                    )
                    .await?;
                if !reply.status.is_success() {
                    return Err(self.refuse(&reply));
                }
                let parsed: serde_json::Value =
                    serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
                let ack = RefreshTokenAckPayload {
                    expires_at_ms: field_u64(&parsed, "expires_at_ms"),
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

    async fn revoke_device(
        &mut self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        // `by-vault-id`, not `/{device_id}`: that route names the ULID the
        // relay minted, which this vault has never held for any peer. Naming
        // the vault id is the whole reason the route exists.
        let reply = self
            .call(
                "DELETE",
                &format!(
                    "/api/v1/devices/by-vault-id/{}",
                    sunrise_id::crockford::encode_bytes(&device_id)
                ),
                None,
            )
            .await?;
        if reply.status.is_success() {
            return Ok(RevokeOutcome::Revoked);
        }
        // Terminal but not success. The relay holds no active row with this
        // vault id, and no amount of retrying will make one appear — but the
        // device may still be registered under a row from before
        // `vault_device_id` existed, in which case it is still being accepted.
        // The caller says so out loud rather than reporting a revocation.
        if reply.status.as_u16() == 404 {
            return Ok(RevokeOutcome::Unknown);
        }
        Err(TransportError::Server {
            code: "AUTH_DEVICE_REVOKE_FAILED",
            message: format!(
                "relay refused the revocation: {} {}{}",
                reply.status,
                String::from_utf8_lossy(&reply.bytes),
                // The one request whose entire purpose is to work when a device
                // has been lost, so a refusal that is really a clock has to say
                // so here too rather than reading as "the relay would not".
                binding_advice(
                    reply.date.as_deref(),
                    self.signer.as_ref().map(|s| s.now_ms())
                )
            ),
        })
    }
}

/// One reply, already read.
struct Reply {
    /// The status line.
    status: hyper::StatusCode,
    /// The relay's own `Date`, when it sent one.
    ///
    /// The only statement of server time a *refused* request carries, and
    /// therefore the only way this side can tell "your clock is wrong" from
    /// "your key is not registered" without a second round trip. hyper's
    /// server writes one on every HTTP/1 response, but nothing in the scheme
    /// requires it, so it is an `Option` and its absence degrades the advice
    /// rather than the diagnosis.
    date: Option<String>,
    /// The whole body.
    bytes: Vec<u8>,
}

/// The `Date` a response carried, if it carried a readable one.
fn server_date(headers: &hyper::HeaderMap) -> Option<String> {
    headers
        .get(hyper::header::DATE)?
        .to_str()
        .ok()
        .map(ToOwned::to_owned)
}

/// What to tell a user whose device binding was refused.
///
/// `AUTH_DEVICE_SIG_INVALID` covers four different situations — a clock outside
/// the replay window, an unregistered key, a revoked device, and a binding that
/// was not sent at all — and only one of them is the user's to fix. A bare 401
/// sent them to `sunrise login`, which cannot help with any of the four.
///
/// The clock is separated out because it is both the commonest cause and the
/// only one this side can *measure*: the relay's `Date` on the very response
/// that refused the request is server time, so the skew is arithmetic rather
/// than a guess. Where it can be measured and exceeds the window, that is
/// stated as the cause; otherwise the remaining possibilities are listed, since
/// naming a clock that is fine would send the user to fix the wrong thing.
///
/// `now_ms` is `None` for a transport carrying no signer, which is the fourth
/// case and is named directly.
fn binding_advice(server_date: Option<&str>, now_ms: Option<u64>) -> String {
    let Some(now_ms) = now_ms else {
        return " — this client sent no device binding and the relay requires one; register                 this device and give its transport a DeviceSigner"
            .to_owned();
    };
    match server_date.and_then(|d| sunrise_http_sig::skew_secs(d, now_ms)) {
        Some(skew) if skew.abs() > sunrise_http_sig::MAX_CLOCK_SKEW_SECS => format!(
            " — this device's clock is {skew}s from the relay's, outside the {}s the              signature allows; set the system clock and retry",
            sunrise_http_sig::MAX_CLOCK_SKEW_SECS
        ),
        _ => " — the relay refused this device's signature; the device may not be registered,               or may have been revoked"
            .to_owned(),
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
    use super::{binding_advice, Reply, SseTransport};
    use crate::signer::DeviceSigner;
    use crate::transport::{Transport, TransportError};
    use std::sync::Arc;

    /// A signer over a fixed key and a fixed clock — the shape a test needs
    /// and, minus the fixtures, the shape `Core` provides.
    #[derive(Debug)]
    struct FixedSigner {
        key: ed25519_dalek::SigningKey,
        device_id: String,
        now_ms: u64,
    }

    impl DeviceSigner for FixedSigner {
        fn device_id(&self) -> String {
            self.device_id.clone()
        }
        fn sign(&self, message: &[u8]) -> [u8; 64] {
            use ed25519_dalek::Signer as _;
            self.key.sign(message).to_bytes()
        }
        fn now_ms(&self) -> u64 {
            self.now_ms
        }
    }

    fn signer(now_ms: u64) -> Arc<dyn DeviceSigner> {
        Arc::new(FixedSigner {
            key: ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]),
            device_id: "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y".to_owned(),
            now_ms,
        })
    }

    /// `NOW_MS` is the instant `DATE` names.
    const DATE: &str = "Mon, 31 Aug 2026 00:00:00 GMT";
    const NOW_MS: u64 = 1_788_134_400_000;

    fn reply(status: u16, body: &[u8], date: Option<&str>) -> Reply {
        Reply {
            status: hyper::StatusCode::from_u16(status).expect("a status"),
            date: date.map(ToOwned::to_owned),
            bytes: body.to_vec(),
        }
    }

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

    const SIG_INVALID: &[u8] = br#"{"type":"https://sunrise.app/problems/unauthenticated","status":401,"code":"AUTH_DEVICE_SIG_INVALID"}"#;

    /// The regression: a `401` whose body names a code this build knows keeps
    /// that code. Flattening it to the status map is what left a client
    /// refreshing a bearer to cure a clock.
    #[test]
    fn a_typed_code_survives_the_status_map() {
        let t = SseTransport::connect("http://127.0.0.1:1");
        let err = t.refuse(&reply(401, SIG_INVALID, None));
        assert_eq!(code_of(&err), "AUTH_DEVICE_SIG_INVALID");
    }

    /// And the fallback still holds, so a relay that answers with a code this
    /// build has never heard of — or with no body at all — is still classified
    /// rather than dropped on the floor.
    #[test]
    fn an_untyped_401_still_maps_to_the_token_code() {
        let t = SseTransport::connect("http://127.0.0.1:1");
        let err = t.refuse(&reply(401, b"", None));
        assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");

        let unknown = br#"{"status":401,"code":"AUTH_SOMETHING_FROM_THE_FUTURE"}"#;
        let err = t.refuse(&reply(401, unknown, None));
        assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
    }

    fn message_of(err: &TransportError) -> String {
        match err {
            TransportError::Server { message, .. } => message.clone(),
            other => panic!("expected a server refusal, got {other}"),
        }
    }

    /// A clock outside the replay window is the commonest cause of this
    /// refusal and the only one the user can fix, so the measurement the relay
    /// handed back on the refusing response is turned into an instruction. The
    /// bare code alone sent people to `sunrise login`, which cures nothing
    /// here.
    #[test]
    fn a_skewed_clock_is_named_and_measured() {
        let late = NOW_MS + (sunrise_http_sig::MAX_CLOCK_SKEW_SECS as u64 + 100) * 1000;
        let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(late));
        let err = t.refuse(&reply(401, SIG_INVALID, Some(DATE)));
        let message = message_of(&err);
        assert!(message.contains("400s from the relay's"), "{message}");
        assert!(message.contains("set the system clock"), "{message}");
    }

    /// And a clock that is *fine* must not be blamed: the same code then means
    /// the key is unregistered or the device revoked, and naming the clock
    /// would send the user to fix the one thing that is right.
    #[test]
    fn a_correct_clock_is_not_blamed_for_a_refused_key() {
        let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(NOW_MS));
        let message = message_of(&t.refuse(&reply(401, SIG_INVALID, Some(DATE))));
        assert!(!message.contains("clock is"), "{message}");
        assert!(message.contains("revoked"), "{message}");

        // A relay that sent no `Date` leaves the skew unmeasurable, which is
        // the same "cannot blame the clock" answer rather than a guess.
        let message = message_of(&t.refuse(&reply(401, SIG_INVALID, None)));
        assert!(!message.contains("clock is"), "{message}");
    }

    /// A transport with no signer against a relay that requires one gets the
    /// fourth case named, because "your signature is wrong" is misleading when
    /// none was sent.
    #[test]
    fn an_unbound_transport_is_told_it_sent_no_binding() {
        let t = SseTransport::connect("http://127.0.0.1:1");
        let message = message_of(&t.refuse(&reply(401, SIG_INVALID, Some(DATE))));
        assert!(message.contains("sent no device binding"), "{message}");
    }

    /// The advice is a pure function of the two facts it has, so the branch
    /// boundary is pinned rather than approached only through a transport.
    #[test]
    fn the_skew_advice_turns_on_the_scheme_s_own_tolerance() {
        let max = sunrise_http_sig::MAX_CLOCK_SKEW_SECS as u64;
        let inside = binding_advice(Some(DATE), Some(NOW_MS + max * 1000));
        assert!(!inside.contains("clock is"), "{inside}");
        let outside = binding_advice(Some(DATE), Some(NOW_MS + (max + 1) * 1000));
        assert!(outside.contains("clock is 301s"), "{outside}");
    }

    /// Every route this transport reaches is one `signed.rs` binds, so a
    /// binding is produced for all of them — including the bodyless `GET` that
    /// opens the stream and the revocation `DELETE`, which are the two easiest
    /// to leave out because neither has a body to sign over.
    #[test]
    fn every_operation_this_transport_makes_carries_a_binding() {
        let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(NOW_MS));
        for (method, path, body) in [
            (
                "POST",
                "/api/v1/sync/session",
                Some(serde_json::json!({"a": 1})),
            ),
            (
                "POST",
                "/api/v1/sync/subscribe",
                Some(serde_json::json!({"streams": []})),
            ),
            (
                "POST",
                "/api/v1/sync/ops",
                Some(serde_json::json!({"ops": []})),
            ),
            (
                "POST",
                "/api/v1/sync/session/refresh",
                Some(serde_json::json!({"token": "t"})),
            ),
            ("GET", "/api/v1/sync/events", None),
            (
                "DELETE",
                "/api/v1/devices/by-vault-id/01ARZ3NDEKTSV4RRFFQ69G5FAV",
                None,
            ),
        ] {
            let headers = t.binding(method, path, body.as_ref()).expect("a binding");
            let names: Vec<&str> = headers.iter().map(|(n, _)| *n).collect();
            assert_eq!(
                names,
                vec!["x-sunrise-device", "x-sunrise-device-sig", "date"],
                "{method} {path} must carry the whole binding"
            );

            // And the signature verifies against the key, over this exact
            // target — which is what makes the header more than three
            // well-named strings.
            let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
            sunrise_http_sig::verify(
                &sunrise_http_sig::device_pub_b64(&key.verifying_key().to_bytes()),
                &headers[1].1,
                method,
                path,
                &headers[2].1,
                body.as_ref(),
                NOW_MS,
            )
            .unwrap_or_else(|e| panic!("{method} {path} must verify: {e}"));
        }
    }

    /// A transport with no signer sends no binding at all, which is the
    /// self-host `NullVerifier` deployment rather than an omission: sending an
    /// empty or partial one would be refused where an absent one is accepted.
    #[test]
    fn an_unsigned_transport_sends_no_binding_headers() {
        let t = SseTransport::connect("http://127.0.0.1:1");
        assert!(t
            .binding("GET", "/api/v1/sync/events", None)
            .expect("no binding is not an error")
            .is_empty());
    }
}
