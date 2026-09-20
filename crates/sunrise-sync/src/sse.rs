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
use crate::transport::{BlobCommit, RevokeOutcome, Transport, TransportError};
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

/// The largest *blob* this client will hold out of one reply.
///
/// The blob surface is the one route whose body is content rather than a
/// document: `blob_fetch` reads a whole committed attachment into a `Vec<u8>`.
/// `crates/sunrise-server/src/api/blobs.rs:62` refuses to commit one larger
/// than this, so the number is the relay's own ceiling restated on the side
/// that has to allocate it. Capping it at [`MAX_EVENT_BYTES`] instead would
/// refuse every attachment over eight mebibytes, and leaving it uncapped — as
/// this route was — lets any relay decide how much memory this client holds.
const MAX_BLOB_BYTES: usize = 100 * 1000 * 1000;

/// How long this client will wait for the *next* piece of a response body.
///
/// An **idle** bound rather than a total one: every frame that arrives resets
/// it, so it bounds a peer that has stopped sending without putting a
/// throughput floor under a peer that is merely slow. That distinction is what
/// lets one deadline serve both a few-hundred-byte problem document and a
/// hundred megabytes of attachment ciphertext — a total deadline large enough
/// for the second would not bound the first at all, and one tight enough for
/// the first would refuse the second on any modest link.
///
/// Two seconds, measured against a connection that is already established and
/// a head that has already arrived. The relay writes a problem document from a
/// `format!`-bounded string and streams a blob one chunk per poll out of local
/// storage (`crates/sunrise-server/src/api/blobs.rs:423-453`), so two seconds
/// of *no bytes at all* is a stalled peer rather than a loaded one.
///
/// The alternative to expiring is not a slower read; it is no return at all. A
/// stall is not a failure the body reports, and nothing above supplies a bound:
/// there is no timeout on the client (see [`SseTransport::connect_with_bearer`]),
/// and `crates/sunrise-core/src/sync_driver.rs:723` awaits the `Hello` send
/// bare, outside any `tokio::select!`. An unbounded read under that await parks
/// the driver task with no error, no `SessionEnd::Disconnected`, no backoff and
/// no log line. On the stream route the driver's `tokio::select!` *cancels*
/// rather than times out, which drops the refusal unreported and reopens the
/// stream, so a stall there becomes a silent retry loop instead of a session
/// that ends and backs off.
const BODY_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

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
        // The `?` is unreachable from this module and stays for the signature's
        // sake. `sign_with` fails only with `SigError::NotCanonicalizable`,
        // which its own doc records as reachable for a map with non-string keys
        // — and `body` is a `serde_json::Value`, whose maps are
        // `Map<String, Value>` by construction. Non-finite floats do not reach
        // it either: `serde_jcs` follows `serde_json` and writes `null`. So a
        // mutant that deletes this error path survives because no input can
        // take it, not because nothing depends on it.
        let signature =
            sunrise_http_sig::sign_with(|msg| signer.sign(msg), method, path, &date, body)
                .map_err(|e| protocol(&e))?;
        Ok(vec![
            (sunrise_http_sig::DEVICE_HEADER, signer.device_id()),
            (sunrise_http_sig::DEVICE_SIG_HEADER, signature),
            ("date", date),
        ])
    }

    /// The three `header_sig_v2` headers for a request whose body is **not**
    /// JSON, or none if this transport carries no binding.
    ///
    /// Separate from [`Self::binding`] rather than a second parameter on it,
    /// because the two sign different things: that one canonicalizes a value,
    /// and here the bytes already are the canonical form. `sign_binary_with` is
    /// the encoder both sides of that split share with the relay's verifier.
    fn binding_bytes(&self, method: &str, path: &str, body: &[u8]) -> Vec<(&'static str, String)> {
        let Some(signer) = &self.signer else {
            return Vec::new();
        };
        let date = sunrise_http_sig::date_header(signer.now_ms());
        let signature =
            sunrise_http_sig::sign_binary_with(|msg| signer.sign(msg), method, path, &date, body);
        vec![
            (sunrise_http_sig::DEVICE_HEADER, signer.device_id()),
            (sunrise_http_sig::DEVICE_SIG_HEADER, signature),
            ("date", date),
        ]
    }

    /// Issue one request with a raw byte body (possibly empty) and read the
    /// whole reply.
    ///
    /// The blob surface is the only place this is needed: a chunk `PUT` sends
    /// opaque ciphertext and a blob `GET` receives it, and neither is
    /// describable as a JSON value. `content_type` is `None` for a request
    /// with no body, which is what keeps a bodiless `GET` from advertising a
    /// media type it is not sending.
    async fn call_bytes(
        &self,
        method: &str,
        path: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<Reply, TransportError> {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base));
        if let Some(bearer) = &self.bearer {
            request = request.header(hyper::header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        for (name, value) in self.binding_bytes(method, path, body) {
            request = request.header(name, value);
        }
        if let Some(ct) = content_type {
            request = request.header(hyper::header::CONTENT_TYPE, ct);
        }
        let request = request
            .body(Full::new(Bytes::copy_from_slice(body)))
            .map_err(|e| protocol(&e))?;

        let response = self
            .client
            .request(request)
            .await
            .map_err(|e| TransportError::Unavailable(e.to_string()))?;
        let status = response.status();
        let date = server_date(response.headers());
        // [`MAX_BLOB_BYTES`] rather than [`MAX_EVENT_BYTES`]: this is the one
        // route whose body is content rather than a document, and the relay
        // commits attachments four orders of magnitude larger than the event
        // cap. Sizing this route by the event cap would refuse every
        // attachment over eight mebibytes.
        let bytes = read_body(response.into_body(), MAX_BLOB_BYTES, BODY_IDLE_TIMEOUT).await?;
        Ok(Reply {
            status,
            date,
            bytes,
        })
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
        // Every reply on this route is a small JSON document, so the event cap
        // is far above anything legitimate — it is here so a relay cannot
        // choose how much memory this client holds, not to size the reply.
        //
        // The deadline is the load-bearing half. This is the route the
        // handshake rides: `send_frame`'s `Hello` arm calls it, and
        // `crates/sunrise-core/src/sync_driver.rs:723` awaits that send bare,
        // outside any `tokio::select!`. An unbounded read here was not a retry
        // loop the driver eventually notices — it was the driver task parked
        // for good, with no error, no backoff and nothing in the log.
        let bytes = read_body(response.into_body(), MAX_EVENT_BYTES, BODY_IDLE_TIMEOUT).await?;
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
            // A stream refusal is diagnosed from its status, its code and the
            // relay's `Date`, so the body is read rather than dropped: the
            // problem document is where the relay puts the typed code, and
            // leaving it unread flattened every `401` on this one route back
            // onto the status map — the exact defect [`SseTransport::refuse`]
            // exists to prevent on the other six.
            //
            // This is the one body read in this module that *degrades* instead
            // of propagating, and the asymmetry is the point rather than an
            // oversight. On this route the 2xx body is the event stream and is
            // never collected at all — it is handed to `self.events` below — so
            // the only body [`read_body`] ever reads here is a refusal's, and a
            // refusal whose document did not arrive is still a refusal. Every
            // way of not getting the document therefore lands on the status
            // map, which is where this route was before it read the body at
            // all. [`SseTransport::call`] collects on both sides of
            // `is_success`, where the same degrade would turn a success reply
            // that never arrived into an empty one that did, so it propagates.
            let status = response.status();
            let date = server_date(response.headers());
            let bytes = read_body(response.into_body(), MAX_EVENT_BYTES, BODY_IDLE_TIMEOUT)
                .await
                .unwrap_or_default();
            return Err(self.refuse(&Reply {
                status,
                date,
                bytes,
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
                // The relay's own code wins, exactly as [`SseTransport::refuse`]
                // keeps it for a refused request. `ClosePayload::is_recoverable`
                // is `matches!(self.code, AuthTokenExpired)`, and the stream
                // emits three distinct close reasons — `AUTH_TOKEN_EXPIRED`,
                // `AUTH_DEVICE_REVOKED` and `RELAY_STORAGE_UNAVAILABLE`
                // (`docs/05-sync/wire-protocol.md:379-383`). Collapsing them
                // onto the first is what `docs/05-sync/wire-protocol.md:203-206`
                // names as the defect: a client that cannot tell them apart
                // "retries forever against a revoked device".
                //
                // A close this build cannot read is **not** recoverable, and
                // the direction of that fallback is the whole point of it.
                // `docs/05-sync/wire-protocol.md:214-216` records adding a
                // close code as non-breaking, so a relay one version ahead
                // closing for a fourth reason is sanctioned rather than
                // malformed. Defaulting such a close onto `AUTH_TOKEN_EXPIRED`
                // — the single member `is_recoverable` admits — would put a
                // client into exactly the loop the paragraph above names: it
                // refreshes, reconnects, is closed again for the same unread
                // reason, and never stops. `INTERNAL_UNKNOWN_CODE` is the code
                // `docs/10-cross-cutting/error-handling.md:36` reserves for
                // this — "an older client receiving an unknown code maps it to
                // `INTERNAL_UNKNOWN_CODE` and preserves the original wire
                // string in `diagnostic`" — so the unparsed spelling is carried
                // into `reason`, which is `ClosePayload`'s diagnostic field.
                // This is the same direction [`SseTransport::refuse`] takes at
                // its own fallback, and for the same stated reason.
                let reported = event.get("code").and_then(|c| c.as_str());
                let parsed = reported.and_then(sunrise_error::ErrorCode::from_wire_str);
                let reason = event
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("session closed");
                let payload = ClosePayload {
                    code: parsed.unwrap_or(sunrise_error::ErrorCode::InternalUnknownCode),
                    reason: match (parsed, reported) {
                        (None, Some(wire)) => {
                            format!("{reason} (unrecognised close code {wire})")
                        }
                        _ => reason.to_owned(),
                    },
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
                // A reply naming no session is a failed handshake, not a
                // quiet one.
                // `crates/sunrise-server/src/api/sync/credential.rs:51`
                // declares `session_id` as a `String` rather than an
                // `Option<String>`, so a `200` without it is malformed by the
                // server's own contract. Accepting it used to leave
                // `self.session` `None` while a `HelloAck` went to the driver
                // as though the handshake had succeeded: every later `call`
                // then went up with no `x-sunrise-session` for the relay to
                // refuse, and the next stream open reported "no sync session:
                // send Hello first" — naming the wrong cause to whoever read
                // the log, because `Hello` *was* sent and *was* acked.
                let Some(session) = parsed.get("session_id").and_then(|s| s.as_str()) else {
                    return Err(TransportError::Protocol(
                        "handshake reply carries no session_id".to_owned(),
                    ));
                };
                self.session = Some(session.to_owned());

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

            // Unreachable, and kept because `as_mut` has to answer something:
            // the guard above opens the stream when `events` is `None`, and a
            // successful `open_events` assigns it, so control only arrives here
            // with `Some`. The drain loop touches `buf` alone. A mutant that
            // changes what this arm returns therefore survives by construction
            // rather than for want of a test.
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

    async fn blob_init(
        &mut self,
        stream_id: &[u8; 16],
        chunk_count: u32,
        size_bytes: u64,
    ) -> Result<String, TransportError> {
        let reply = self
            .call(
                "POST",
                "/api/v1/blobs/init",
                Some(serde_json::json!({
                    "stream_id": hex::encode(stream_id),
                    "chunk_count": chunk_count,
                    "size_bytes": size_bytes,
                })),
            )
            .await?;
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        serde_json::from_slice::<serde_json::Value>(&reply.bytes)
            .ok()
            .and_then(|v| {
                v.get("upload_id")
                    .and_then(|u| u.as_str())
                    .map(ToOwned::to_owned)
            })
            .ok_or_else(|| TransportError::Protocol("blobs/init returned no upload_id".to_owned()))
    }

    async fn blob_put_chunk(
        &mut self,
        upload_id: &str,
        chunk_idx: u32,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        let reply = self
            .call_bytes(
                "PUT",
                &format!("/api/v1/blobs/{upload_id}/{chunk_idx}"),
                Some("application/octet-stream"),
                bytes,
            )
            .await?;
        if reply.status.is_success() {
            return Ok(());
        }
        Err(self.refuse(&reply))
    }

    async fn blob_finalize(
        &mut self,
        upload_id: &str,
        ciphertext_hash: &[u8; 32],
        chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        let hashes: Vec<String> = chunk_hashes.iter().map(hex::encode).collect();
        let reply = self
            .call(
                "POST",
                "/api/v1/blobs/finalize",
                Some(serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hex::encode(ciphertext_hash),
                    "chunk_hashes": hashes,
                })),
            )
            .await?;
        if !reply.status.is_success() {
            return Err(self.refuse(&reply));
        }
        let body: serde_json::Value =
            serde_json::from_slice(&reply.bytes).map_err(|e| protocol(&e))?;
        let blob_id = body
            .get("blob_id")
            .and_then(|b| b.as_str())
            .and_then(|s| s.strip_prefix("blb_"))
            .and_then(|hexpart| {
                let mut out = [0u8; 16];
                hex::decode_to_slice(hexpart, &mut out).ok().map(|()| out)
            })
            .ok_or_else(|| {
                TransportError::Protocol("blobs/finalize returned no blob id".to_owned())
            })?;
        Ok(BlobCommit {
            blob_id,
            size_bytes: body
                .get("size_bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            chunk_count: body
                .get("chunk_count")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
        })
    }

    async fn blob_fetch(&mut self, blob_id: &[u8; 16]) -> Result<Option<Vec<u8>>, TransportError> {
        let reply = self
            .call_bytes(
                "GET",
                &format!("/api/v1/blobs/blb_{}", hex::encode(blob_id)),
                None,
                &[],
            )
            .await?;
        if reply.status.is_success() {
            return Ok(Some(reply.bytes));
        }
        // The relay answers "no such blob", "not yours" and "not finished yet"
        // with one 404, deliberately — a distinguishable response would make
        // this route an oracle for whether another account holds a given
        // ciphertext. All three mean the same thing to a fetching device: the
        // bytes are not here, try later.
        if reply.status.as_u16() == 404 {
            return Ok(None);
        }
        Err(self.refuse(&reply))
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

/// Read a response body whole, under a size cap and an idle deadline.
///
/// The one place this client reads a body it did not write. Three routes need
/// it — the JSON control plane ([`SseTransport::call`]), the blob surface
/// ([`SseTransport::call_bytes`]) and a refused stream open
/// ([`SseTransport::open_events`]) — and each of them used to hand-roll the
/// read. That is how two of them came to have a cap and a deadline and the
/// third to have neither: a bound nobody can see is missing is a bound that
/// goes missing, and stating it once per route is what made the omission
/// invisible.
///
/// `cap` is a parameter because the routes carry different things — a problem
/// document and an attachment are not bounded by the same number — and `idle`
/// is one because a caller wanting different patience should have to say so.
/// Both are now stated at the call rather than assembled out of a `timeout`
/// and a `Limited` at each site.
///
/// What a failure *means* is deliberately not decided here, because it differs:
/// a refusal whose problem document did not arrive is still a refusal and
/// degrades to the status map, while a success reply whose body did not arrive
/// is not a reply at all. So this reports the failure and the caller reads it.
///
/// Both bounds discard what already arrived. Half a document is not a document:
/// parsing a truncated body would turn a bounded read into a source of
/// arbitrary misreadings, which is a worse answer than the one the caller's
/// fallback already gives.
async fn read_body<B>(
    body: B,
    cap: usize,
    idle: std::time::Duration,
) -> Result<Vec<u8>, TransportError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let mut body = http_body_util::Limited::new(body, cap);
    let mut out = Vec::new();
    loop {
        // The timer is armed per frame rather than once around the whole read,
        // which is the difference between bounding a stall and bounding a
        // download: a body that keeps arriving keeps resetting it.
        let Ok(next) = tokio::time::timeout(idle, body.frame()).await else {
            return Err(TransportError::Unavailable(format!(
                "response body stalled: no data for {}s",
                idle.as_secs()
            )));
        };
        let Some(frame) = next else { return Ok(out) };
        let frame = frame.map_err(|e| TransportError::Unavailable(e.to_string()))?;
        // Trailers carry nothing this client reads. Dropping them here rather
        // than treating the frame as data is what keeps a trailing frame from
        // appending its encoding to a body somebody is about to parse.
        if let Ok(data) = frame.into_data() {
            out.extend_from_slice(&data);
        }
    }
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

    /// The byte-body binding is the blob path's, and it was the untested half.
    ///
    /// [`Self::binding`] signs a JSON *value* and the test above covers every
    /// route that uses it. `binding_bytes` signs bytes that already are the
    /// canonical form — the chunk `PUT` and the blob `GET`, which is the whole
    /// attachment upload and download surface — and nothing reached it. Five
    /// mutants rewriting it to return an empty or junk header list all
    /// survived, so the signing on that surface was asserted by nothing.
    ///
    /// Verified with `verify_canonical` rather than `verify`: these bytes are
    /// the message, and routing them through the value verifier would
    /// canonicalize them a second time and assert the wrong thing.
    #[test]
    fn the_byte_body_operations_carry_a_verifying_binding() {
        let t = SseTransport::connect("http://127.0.0.1:1").with_device_signer(signer(NOW_MS));
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        for (method, path, body) in [
            (
                "PUT",
                "/api/v1/blobs/01ARZ3NDEKTSV4RRFFQ69G5FAV/0",
                b"sealed-chunk-bytes".as_slice(),
            ),
            (
                "GET",
                "/api/v1/blobs/01ARZ3NDEKTSV4RRFFQ69G5FAV",
                b"".as_slice(),
            ),
        ] {
            let headers = t.binding_bytes(method, path, body);
            let names: Vec<&str> = headers.iter().map(|(n, _)| *n).collect();
            assert_eq!(
                names,
                vec!["x-sunrise-device", "x-sunrise-device-sig", "date"],
                "{method} {path} must carry the whole binding"
            );
            assert_eq!(
                headers[0].1, "dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y",
                "{method} {path} must name the device that signed it"
            );
            sunrise_http_sig::verify_canonical(
                &sunrise_http_sig::device_pub_b64(&key.verifying_key().to_bytes()),
                &headers[1].1,
                method,
                path,
                &headers[2].1,
                body,
                NOW_MS,
            )
            .unwrap_or_else(|e| panic!("{method} {path} must verify: {e}"));
        }
    }

    /// A transport with no signer sends no binding at all, which is the
    /// self-host `NullVerifier` deployment rather than an omission: sending an
    /// empty or partial one would be refused where an absent one is accepted.
    ///
    /// Both signing paths, because they carry the decision separately. The
    /// byte-body one was reached by no test at all: it is a bare `return` with
    /// no `Result` around it, so nothing above would have noticed it returning
    /// a partial binding on the unauthenticated path.
    #[test]
    fn an_unsigned_transport_sends_no_binding_headers() {
        let t = SseTransport::connect("http://127.0.0.1:1");
        assert!(t
            .binding("GET", "/api/v1/sync/events", None)
            .expect("no binding is not an error")
            .is_empty());
        assert!(t
            .binding_bytes("PUT", "/api/v1/blobs/up-1/0", b"sealed-chunk-bytes")
            .is_empty());
    }

    // ---- the buffer half: `take_event` needs no socket ----

    /// A transport whose receive buffer has been filled directly.
    ///
    /// [`SseTransport::take_event`] reads and drains `self.buf` and touches
    /// nothing else, so the buffer is its whole input. This module is a child
    /// of `sse`, which is what lets a test fill a field production code keeps
    /// private — and why these cases live here rather than under `tests/`.
    fn buffered(bytes: &[u8]) -> SseTransport {
        let mut t = SseTransport::connect("http://127.0.0.1:1");
        t.buf = bytes.to_vec();
        t
    }

    /// A terminated event yields its `id` and its `data`, and the bytes it was
    /// made of — separator included — leave the buffer.
    ///
    /// The separator is the load-bearing part: an event reported but not fully
    /// drained leaves its own terminator at the head of the buffer, and the
    /// next read finds an empty event nobody sent.
    #[test]
    fn a_complete_event_yields_its_id_and_data_and_drains_the_buffer() {
        let mut t = buffered(b"id: 42\ndata: {\"kind\":\"gap\"}\n\n");
        let (id, data) = t.take_event().expect("a terminated event");
        assert_eq!(id.as_deref(), Some("42"));
        assert_eq!(data, "{\"kind\":\"gap\"}");
        assert!(
            t.buf.is_empty(),
            "the event and its blank line are both consumed, not just the event"
        );
    }

    /// An event that has not arrived whole is not an event, and none of it is
    /// consumed: the rest of it is still coming.
    ///
    /// A single newline is not the separator. SSE ends an event with a blank
    /// line, which is two bytes, and treating one of them as the boundary
    /// would cut every multi-line event in half.
    #[test]
    fn a_partial_event_is_left_in_the_buffer_untouched() {
        let head = b"id: 42\ndata: {\"kind\"";
        let mut t = buffered(head);
        assert!(t.take_event().is_none(), "the event has not terminated");
        assert_eq!(
            t.buf, head,
            "nothing is consumed until the blank line lands"
        );

        let mut t = buffered(b"data: {}\n");
        assert!(t.take_event().is_none(), "one newline is not a blank line");
    }

    /// Two events that arrived in one read come out one at a time, in order.
    #[test]
    fn two_buffered_events_come_out_one_at_a_time_in_order() {
        let mut t = buffered(b"id: 1\ndata: one\n\nid: 2\ndata: two\n\n");
        let (id, data) = t.take_event().expect("the first event");
        assert_eq!((id.as_deref(), data.as_str()), (Some("1"), "one"));
        let (id, data) = t.take_event().expect("the second event");
        assert_eq!((id.as_deref(), data.as_str()), (Some("2"), "two"));
        assert!(t.take_event().is_none(), "and then the buffer is empty");
    }

    /// A keep-alive comment is consumed and reported as an event with no data,
    /// not as "nothing arrived".
    ///
    /// The distinction keeps [`SseTransport::recv_frame`]'s drain loop moving:
    /// `None` means "the buffer holds no terminated event" and sends it back to
    /// the socket, so a comment reported as `None` would leave the event behind
    /// it unread until the next chunk happened to arrive.
    #[test]
    fn a_keep_alive_comment_is_consumed_rather_than_reported_as_nothing() {
        let mut t = buffered(b": keep-alive\n\nid: 7\ndata: after\n\n");
        let (id, data) = t.take_event().expect("the comment is a terminated event");
        assert!(id.is_none(), "a comment carries no id");
        assert!(data.is_empty(), "and nothing for the driver");
        let (id, data) = t.take_event().expect("the event behind it");
        assert_eq!(id.as_deref(), Some("7"));
        assert_eq!(data, "after");
    }

    // ---- the mapping half: `frame_for` needs no socket either ----

    /// An `ops` event hands the driver the relay's bytes, byte for byte.
    ///
    /// The module documentation claims exactly this — what the driver applies
    /// is what the relay stored — and it is why this direction is a base64
    /// decode rather than a re-encoding. A re-encoding that happened to
    /// round-trip today would break the first time either side reordered a map
    /// key, and every signature over those bytes with it.
    #[test]
    fn an_ops_event_carries_the_relay_s_frame_verbatim() {
        use base64::Engine as _;
        let stored: &[u8] = b"\x01\x02\x03 whatever the publisher sent";
        let event = serde_json::json!({
            "kind": "ops",
            "frame": base64::engine::general_purpose::STANDARD.encode(stored),
        });
        let frame = SseTransport::frame_for(&event)
            .expect("a well-formed ops event")
            .expect("an ops event is a frame");
        assert_eq!(frame, stored);
    }

    /// An `ops` event with nothing to decode is a malformed exchange, not an
    /// empty frame the driver would then try to apply.
    #[test]
    fn an_ops_event_with_no_frame_is_a_protocol_error() {
        let err = SseTransport::frame_for(&serde_json::json!({"kind": "ops"}))
            .expect_err("there is nothing to decode");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("no frame")),
            "{err}"
        );
    }

    /// `caught_up` becomes the `StreamUpdate` the driver waits on, naming the
    /// stream that finished replaying.
    #[test]
    fn a_caught_up_event_becomes_a_stream_update_naming_its_stream() {
        let event = serde_json::json!({
            "kind": "caught_up",
            "stream_id": "ab".repeat(16),
        });
        let frame = SseTransport::frame_for(&event)
            .expect("a well-formed caught_up event")
            .expect("caught_up is a frame");
        let (head, payload) = super::decode_frame(&frame).expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::StreamUpdate);
        let parsed = super::CaughtUpPayload::decode(&payload).expect("a CaughtUpPayload");
        assert_eq!(parsed.stream_id, [0xabu8; 16]);
    }

    /// A `gap` becomes a coded error frame rather than a disconnection: the
    /// driver answers a cursor gap by resubscribing, which it can only do if it
    /// is told which condition it hit.
    #[test]
    fn a_gap_event_becomes_a_coded_error_frame_carrying_its_reason() {
        for (event, reason) in [
            (
                serde_json::json!({"kind": "gap", "reason": "cursor behind retention"}),
                "cursor behind retention",
            ),
            (serde_json::json!({"kind": "gap"}), "cursor gap"),
        ] {
            let frame = SseTransport::frame_for(&event)
                .expect("a well-formed gap event")
                .expect("a gap is a frame");
            let (head, payload) = super::decode_frame(&frame).expect("a decodable frame");
            assert_eq!(head.msg_kind, super::MsgKind::Error);
            let parsed = super::ErrorPayload::decode(&payload).expect("an ErrorPayload");
            assert_eq!(parsed.code, sunrise_error::ErrorCode::SyncCursorGap);
            assert_eq!(parsed.reason, reason);
        }
    }

    /// A `closed` event carries the relay's own close code through, so only the
    /// expiry is read as recoverable.
    ///
    /// One case per code `crates/sunrise-server/src/api/sync/stream.rs` emits —
    /// `AUTH_TOKEN_EXPIRED` (:338), `AUTH_DEVICE_REVOKED` (:362) and
    /// `RELAY_STORAGE_UNAVAILABLE` (:232) — because
    /// `ClosePayload::is_recoverable` is `matches!(self.code,
    /// AuthTokenExpired)` and collapsing the three onto the first is what
    /// `docs/05-sync/wire-protocol.md:203-206` calls "retries forever against a
    /// revoked device". `SyncEvent::Closed.code` is not an `Option`, so every
    /// one of these is an event the relay really sends.
    #[test]
    fn a_closed_event_carries_the_relay_s_own_close_code() {
        for (code, recoverable) in [
            ("AUTH_TOKEN_EXPIRED", true),
            ("AUTH_DEVICE_REVOKED", false),
            ("RELAY_STORAGE_UNAVAILABLE", false),
        ] {
            let event = serde_json::json!({
                "kind": "closed",
                "code": code,
                "reason": "the relay said so",
            });
            let frame = SseTransport::frame_for(&event)
                .expect("a well-formed closed event")
                .expect("a close is a frame");
            let (head, payload) = super::decode_frame(&frame).expect("a decodable frame");
            assert_eq!(head.msg_kind, super::MsgKind::Close);
            let parsed = super::ClosePayload::decode(&payload).expect("a ClosePayload");
            assert_eq!(
                parsed.code.as_str(),
                code,
                "the driver branches on the relay's code, not on a constant"
            );
            assert_eq!(parsed.reason, "the relay said so");
            assert_eq!(
                parsed.is_recoverable(),
                recoverable,
                "{code} decides whether the client refreshes or asks the user"
            );
        }
    }

    /// A `closed` event with no code, or one this build has never heard of, is
    /// **not** recoverable.
    ///
    /// `docs/05-sync/wire-protocol.md:214-216` records adding a close code as
    /// non-breaking, so a relay one version ahead closing for a reason this
    /// build has never heard of is sanctioned rather than malformed — and a
    /// client that read it as recoverable would refresh, reconnect, be closed
    /// again for the same unread reason and loop, which is the failure
    /// `docs/05-sync/wire-protocol.md:203-206` exists to name. Neither shape
    /// may land on `AUTH_TOKEN_EXPIRED`, the single code `is_recoverable`
    /// admits.
    ///
    /// The code it does land on is the one
    /// `docs/10-cross-cutting/error-handling.md:36` reserves for an older
    /// client meeting a newer code, and the unparsed spelling survives in the
    /// diagnostic the same paragraph asks for, so support tooling can still
    /// say which close this was.
    #[test]
    fn a_closed_event_this_build_cannot_read_is_terminal_rather_than_recoverable() {
        for (event, reason) in [
            (serde_json::json!({"kind": "closed"}), "session closed"),
            (
                serde_json::json!({"kind": "closed", "code": "FROM_THE_FUTURE", "reason": "who knows"}),
                "who knows (unrecognised close code FROM_THE_FUTURE)",
            ),
        ] {
            let frame = SseTransport::frame_for(&event)
                .expect("a well-formed closed event")
                .expect("a close is a frame");
            let (_, payload) = super::decode_frame(&frame).expect("a decodable frame");
            let parsed = super::ClosePayload::decode(&payload).expect("a ClosePayload");
            assert_eq!(
                parsed.code,
                sunrise_error::ErrorCode::InternalUnknownCode,
                "a close this build cannot read is named as such, not guessed at"
            );
            assert!(
                !parsed.is_recoverable(),
                "{event} must not read as a close the client may retry through"
            );
            assert_eq!(
                parsed.reason, reason,
                "the relay's own words survive, and so does the code nobody could parse"
            );
        }
    }

    /// An event kind this build has never heard of is dropped, not fatal.
    ///
    /// The relay is allowed to grow event kinds without every client being
    /// upgraded first; a client that tore the stream down over one would make
    /// the forward compatibility the stream is versioned for unusable.
    #[test]
    fn an_event_kind_this_build_does_not_know_is_dropped_rather_than_fatal() {
        for event in [
            serde_json::json!({"kind": "from_the_future"}),
            serde_json::json!({}),
        ] {
            assert!(
                SseTransport::frame_for(&event)
                    .expect("an unknown kind is not an error")
                    .is_none(),
                "nothing reaches the driver for {event}"
            );
        }
    }

    // ---- the reply readers ----

    /// The relay's own `Date` is read back when it is readable and never
    /// invented when it is not: it is the only server clock a *refused* request
    /// carries, and a fabricated one would make the skew advice a guess.
    #[test]
    fn the_server_s_date_is_read_back_and_never_invented() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(hyper::header::DATE, DATE.parse().expect("a header value"));
        assert_eq!(super::server_date(&headers).as_deref(), Some(DATE));

        assert!(super::server_date(&hyper::HeaderMap::new()).is_none());

        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::DATE,
            hyper::header::HeaderValue::from_bytes(&[0xff, 0xfe]).expect("a header value"),
        );
        assert!(
            super::server_date(&headers).is_none(),
            "an unreadable Date degrades the advice rather than the diagnosis"
        );
    }

    /// The advisory reply fields are total: absent or wrongly typed reads as
    /// the type's zero, because a relay that omits a capability bitfield should
    /// not sink a session that does not use it.
    #[test]
    fn the_reply_field_readers_return_the_value_or_a_zero() {
        let reply = serde_json::json!({
            "server_app_v": "1.2.3",
            "capabilities": 9,
            "wire_proto": 7,
            "text": "not a number",
        });
        assert_eq!(super::field_str(&reply, "server_app_v"), "1.2.3");
        assert_eq!(super::field_str(&reply, "absent"), "");
        assert_eq!(
            super::field_str(&reply, "capabilities"),
            "",
            "a number is not a string"
        );
        assert_eq!(super::field_u64(&reply, "capabilities"), 9);
        assert_eq!(super::field_u64(&reply, "absent"), 0);
        assert_eq!(super::field_u64(&reply, "text"), 0);
        assert_eq!(super::field_u32(&reply, "wire_proto").expect("a u32"), 7);
    }

    /// A negotiated version that does not fit a `u32` is refused rather than
    /// truncated. The whole point of the handshake is that both sides agree on
    /// the value, and a wrapped one is a number neither of them said.
    #[test]
    fn a_negotiated_version_too_large_for_a_u32_is_refused() {
        let reply = serde_json::json!({"wire_proto": u64::from(u32::MAX) + 1});
        let err = super::field_u32(&reply, "wire_proto").expect_err("it does not fit");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("wire_proto")),
            "{err}"
        );
    }

    /// A cursor goes up as the hex device id and the sequence already applied.
    #[test]
    fn a_cursor_is_sent_as_a_hex_device_id_and_its_applied_sequence() {
        let entry = super::CursorEntry {
            device_id: [0x7bu8; 16],
            last_applied_seq: 42,
        };
        assert_eq!(
            super::cursor_json(&entry),
            serde_json::json!({"device_id": "7b".repeat(16), "last_applied_seq": 42})
        );
    }

    /// A stream id is sixteen bytes of hex; anything else is a malformed event
    /// rather than an id of zeroes that would then address the wrong stream.
    #[test]
    fn a_stream_id_is_parsed_from_hex_and_anything_else_is_refused() {
        let id = serde_json::json!("0f".repeat(16));
        assert_eq!(
            super::parse_id(Some(&id)).expect("a stream id"),
            [0x0fu8; 16]
        );
        assert!(super::parse_id(None).is_err(), "an event with no stream id");
        assert!(
            super::parse_id(Some(&serde_json::json!("0f0f"))).is_err(),
            "a stream id of the wrong length"
        );
        assert!(
            super::parse_id(Some(&serde_json::json!(16))).is_err(),
            "a stream id that is not a string"
        );
    }

    /// A status carrying no typed code still lands in the right family, so a
    /// storage outage and a rejected operation do not read alike to a driver
    /// deciding whether to retry.
    #[test]
    fn a_status_with_no_typed_code_still_maps_to_the_right_family() {
        let t = SseTransport::connect("http://127.0.0.1:1");
        assert_eq!(
            code_of(&t.refuse(&reply(503, b"", None))),
            "RELAY_STORAGE_UNAVAILABLE"
        );
        assert_eq!(
            code_of(&t.refuse(&reply(403, b"", None))),
            "AUTH_TOKEN_INVALID"
        );
        assert_eq!(
            code_of(&t.refuse(&reply(400, b"", None))),
            "SYNC_OP_INVALID"
        );
    }

    // ---- the network half: a loopback relay ----

    /// A loopback HTTP/1.1 relay standing in for the real one.
    ///
    /// The transport's `events` field is a `hyper::body::Incoming`, and an
    /// `Incoming` can only be produced by hyper from a real response — there is
    /// nothing to fabricate and no seam to inject at. So the double is an
    /// actual server on `127.0.0.1:0` that the transport dials, which also
    /// makes every assertion below an assertion about what went **on the wire**
    /// rather than about what a private method returned.
    ///
    /// It costs no new dependency: the workspace already pins `hyper` with
    /// `server` on and `tokio` with `net`, both for other crates.
    /// A response body this module drives directly, one frame at a time.
    ///
    /// The relay double below proves each *route* reaches [`read_body`]; this
    /// proves what `read_body` itself does, at boundaries a relay would have to
    /// move a hundred megabytes or wait whole seconds to reach. Driving the
    /// body here rather than over loopback is also what lets these cases name
    /// their own deadline: `read_body` takes one as a parameter, so the
    /// mechanism can be exercised in milliseconds and the shipped value pinned
    /// separately, instead of every case paying for two real seconds.
    #[derive(Debug)]
    struct Scripted {
        /// Each frame, and how long the body withholds it first.
        rest: std::collections::VecDeque<(std::time::Duration, hyper::body::Bytes)>,
        /// Withhold the *end* of the body too, indefinitely.
        stall: bool,
        /// Fail instead of ending, once every frame has been delivered.
        abort: bool,
        /// The gap currently being waited out, if any.
        sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    }

    /// A body that yields `frames` — each a `(gap, data)` pair — and then ends,
    /// stalls or breaks.
    fn scripted(frames: &[(std::time::Duration, &str)], stall: bool, abort: bool) -> Scripted {
        Scripted {
            rest: frames
                .iter()
                .map(|(gap, data)| (*gap, hyper::body::Bytes::copy_from_slice(data.as_bytes())))
                .collect(),
            stall,
            abort,
            sleep: None,
        }
    }

    impl hyper::body::Body for Scripted {
        type Data = hyper::body::Bytes;
        /// Not `Infallible`, for the same reason the relay's body is not: a
        /// body that cannot fail forecloses the error arm by construction.
        type Error = std::io::Error;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<hyper::body::Frame<hyper::body::Bytes>, std::io::Error>>>
        {
            use std::future::Future as _;
            use std::task::Poll;

            let this = self.get_mut();
            loop {
                if let Some(sleep) = this.sleep.as_mut() {
                    match sleep.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(()) => this.sleep = None,
                    }
                }
                match this.rest.front_mut() {
                    // A gap still owed. Arm it, clear it, and go round, so the
                    // timer registers its waker before this returns `Pending`.
                    Some(owed) if !owed.0.is_zero() => {
                        let gap = std::mem::replace(&mut owed.0, std::time::Duration::ZERO);
                        this.sleep = Some(Box::pin(tokio::time::sleep(gap)));
                    }
                    Some(_) => {
                        let (_, data) = this.rest.pop_front().expect("a frame was just there");
                        return Poll::Ready(Some(Ok(hyper::body::Frame::data(data))));
                    }
                    None if this.stall => {
                        // A real timer rather than a bare `Pending`: a body that
                        // returns `Pending` without registering a waker is one
                        // the runtime is entitled never to poll again, and the
                        // stall would then be an artefact of this double.
                        let held = this.sleep.get_or_insert_with(|| {
                            Box::pin(tokio::time::sleep(std::time::Duration::from_secs(3_600)))
                        });
                        return match held.as_mut().poll(cx) {
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(()) => Poll::Ready(None),
                        };
                    }
                    None if this.abort => {
                        this.abort = false;
                        return Poll::Ready(Some(Err(std::io::Error::other(
                            "the peer broke the body",
                        ))));
                    }
                    None => return Poll::Ready(None),
                }
            }
        }
    }

    /// The deadline these cases drive `read_body` with.
    ///
    /// Not [`BODY_IDLE_TIMEOUT`]: the mechanism and the shipped value are
    /// separate claims, and tying them together would spend two real seconds
    /// per case to say nothing the value's own case does not.
    const TEST_IDLE: std::time::Duration = std::time::Duration::from_millis(300);

    /// [`read_body`]'s deadline bounds a *stall*, not a download: a body that
    /// keeps arriving keeps resetting it, however long the whole read takes.
    ///
    /// The load-bearing property of the whole helper. Four frames, each
    /// withheld for a third of the deadline, so every gap is comfortably inside
    /// it while the total is a third longer than it.
    ///
    /// A deadline wrapped around the whole read instead of around each frame
    /// passes every other case in this file and refuses this one — and with it
    /// any attachment large enough to take longer than [`BODY_IDLE_TIMEOUT`] to
    /// arrive, which on the blob route is most of them: [`MAX_BLOB_BYTES`] is a
    /// hundred megabytes and the relay serves it one chunk per poll. That is
    /// why the bound is idle rather than total, and this is the case that says
    /// so.
    #[tokio::test]
    async fn a_body_that_keeps_arriving_is_not_cut_off_by_the_idle_deadline() {
        let gap = TEST_IDLE / 3;
        let started = tokio::time::Instant::now();

        let read = super::read_body(
            scripted(
                &[(gap, "one "), (gap, "two "), (gap, "three "), (gap, "four")],
                false,
                false,
            ),
            1024,
            TEST_IDLE,
        )
        .await
        .expect("every gap is inside the deadline");

        assert_eq!(
            String::from_utf8(read).expect("the frames were UTF-8"),
            "one two three four",
            "and every frame is there, in order"
        );
        assert!(
            started.elapsed() > TEST_IDLE,
            "the read outlasted the deadline, which is the whole point of it"
        );
    }

    /// And a body that *stops* arriving is cut off.
    #[tokio::test]
    async fn a_body_that_stops_arriving_is_cut_off_at_the_idle_deadline() {
        let started = tokio::time::Instant::now();

        let err = super::read_body(
            scripted(
                &[(std::time::Duration::ZERO, r#"{"code":"AUTH_DEV"#)],
                true,
                false,
            ),
            1024,
            TEST_IDLE,
        )
        .await
        .expect_err("the rest of the body never came");

        assert!(
            matches!(&err, TransportError::Unavailable(m) if m.contains("stalled")),
            "a stall is named as one rather than reported as a broken read: {err}"
        );
        assert!(
            started.elapsed() >= TEST_IDLE,
            "and it waited the deadline out rather than giving up on the first `Pending`"
        );
    }

    /// The shipped deadline and the blob cap are these numbers.
    ///
    /// Spelled out rather than read from the constants they pin, for the reason
    /// [`FRAME_CAP`] is: a case sized from the constant moves with it and
    /// asserts the comparison instead of the value. The cases above exercise
    /// the mechanism at a deadline of their own, so without this one a mutant
    /// could move `BODY_IDLE_TIMEOUT` anywhere and nothing would notice — which
    /// matters, because "anywhere" includes long enough to park the handshake
    /// route for the rest of the session.
    ///
    /// [`MAX_BLOB_BYTES`] is the relay's own ceiling
    /// (`crates/sunrise-server/src/api/blobs.rs:62`) restated on the side that
    /// allocates it. Lowering it would refuse attachments the relay accepted,
    /// which is a failure no caller could diagnose from here.
    #[test]
    fn the_body_bounds_are_the_numbers_they_are_documented_as() {
        assert_eq!(super::BODY_IDLE_TIMEOUT, std::time::Duration::from_secs(2));
        assert_eq!(super::MAX_BLOB_BYTES, 100 * 1000 * 1000);
    }

    /// The cap is a ceiling, not a limit one byte lower — and what it refuses
    /// is discarded rather than short-read.
    ///
    /// Both sides of the boundary, because a cap that refused everything would
    /// pass a case written only against the oversized half. Ten bytes against a
    /// ten-byte cap are delivered; the same ten against nine are not delivered
    /// truncated, which is the answer that matters: a caller handed nine bytes
    /// of a ten-byte document cannot tell them from a document.
    #[tokio::test]
    async fn a_body_at_the_cap_is_read_and_one_byte_past_it_is_refused_whole() {
        let at_cap = super::read_body(
            scripted(&[(std::time::Duration::ZERO, "0123456789")], false, false),
            10,
            TEST_IDLE,
        )
        .await
        .expect("exactly the cap is under the cap");
        assert_eq!(at_cap, b"0123456789", "and arrives whole");

        let err = super::read_body(
            scripted(&[(std::time::Duration::ZERO, "0123456789")], false, false),
            9,
            TEST_IDLE,
        )
        .await
        .expect_err("one byte past the cap is not read");
        assert!(
            matches!(&err, TransportError::Unavailable(_)),
            "the peer sent more than was allowed, which is the peer's failure: {err}"
        );
    }

    /// A body that breaks part-way through is reported, not short-read.
    #[tokio::test]
    async fn a_body_that_breaks_part_way_through_is_reported_rather_than_short_read() {
        let err = super::read_body(
            scripted(
                &[(std::time::Duration::ZERO, "half a document")],
                false,
                true,
            ),
            1024,
            TEST_IDLE,
        )
        .await
        .expect_err("the body broke before it ended");
        assert!(
            matches!(&err, TransportError::Unavailable(m) if m.contains("broke")),
            "the peer's own failure comes through: {err}"
        );
    }

    mod relay {
        use http_body_util::BodyExt as _;
        use hyper::body::{Bytes, Frame};
        use hyper::service::service_fn;
        use hyper::{Request, Response, StatusCode};
        use hyper_util::rt::TokioIo;
        use std::collections::VecDeque;
        use std::convert::Infallible;
        use std::future::Future as _;
        use std::pin::Pin;
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll};
        use std::time::Duration;
        use tokio::net::TcpListener;

        /// How long the relay waits between the chunks of one response.
        ///
        /// A deterministic gap, so every chunk of a scripted response reaches
        /// the client as a separate body frame no matter how the two ends are
        /// scheduled, and reassembly is exercised across frames rather than
        /// within one. The value is not load-bearing beyond being non-zero:
        /// this relay writes one chunked-transfer chunk per [`Frame::data`] and
        /// hyper's decoder yields one frame per chunk regardless of arrival
        /// timing, so the gap removes a scheduling variable rather than a
        /// coalescing one.
        const CHUNK_GAP: Duration = Duration::from_millis(5);

        /// How long a stalling response withholds the rest of its body.
        ///
        /// Long enough that nothing in this suite can outwait it, so a test
        /// asserting a client-side deadline is asserting the *client's* timer
        /// and not this one. It is a real timer rather than a bare
        /// `Poll::Pending` because a body that returns `Pending` without
        /// registering a waker is a body the runtime is entitled never to poll
        /// again, and the stall would then be an artefact of this double rather
        /// than of the relay it stands in for.
        const STALL: Duration = Duration::from_secs(3_600);

        /// One canned response, in the order the relay will hand them out.
        #[derive(Debug, Clone)]
        pub(super) struct Canned {
            status: u16,
            content_type: Option<&'static str>,
            chunks: Vec<Bytes>,
            /// End the body with an error rather than with its last chunk.
            abort: bool,
            /// Never end the body at all: write `chunks`, then hold.
            stall: bool,
        }

        impl Canned {
            /// A whole JSON reply.
            pub(super) fn json(status: u16, body: &str) -> Self {
                Self {
                    status,
                    content_type: Some("application/json"),
                    chunks: vec![Bytes::copy_from_slice(body.as_bytes())],
                    abort: false,
                    stall: false,
                }
            }

            /// A reply with a status and nothing else.
            pub(super) fn empty(status: u16) -> Self {
                Self {
                    status,
                    content_type: None,
                    chunks: Vec::new(),
                    abort: false,
                    stall: false,
                }
            }

            /// A whole opaque byte reply, as the blob surface sends.
            pub(super) fn bytes(status: u16, body: &[u8]) -> Self {
                Self {
                    status,
                    content_type: Some("application/octet-stream"),
                    chunks: vec![Bytes::copy_from_slice(body)],
                    abort: false,
                    stall: false,
                }
            }

            /// An event stream delivered one chunk at a time.
            pub(super) fn stream(chunks: Vec<Bytes>) -> Self {
                Self {
                    status: 200,
                    content_type: Some("text/event-stream"),
                    chunks,
                    abort: false,
                    stall: false,
                }
            }

            /// A reply whose body starts and then never finishes.
            ///
            /// The status line and headers reach the client — a chunk has to
            /// be written for hyper to flush the head, which is why this takes
            /// a prefix rather than nothing — and the rest of the body never
            /// arrives. That is what a proxy, captive portal or relay holding a
            /// connection open looks like from here, and the only shape that
            /// exercises a *deadline* rather than an error: a stall is not a
            /// failure the body reports, so without a timer the read does not
            /// return at all.
            pub(super) fn json_then_stall(status: u16, prefix: &str) -> Self {
                Self {
                    status,
                    content_type: Some("application/json"),
                    chunks: vec![Bytes::copy_from_slice(prefix.as_bytes())],
                    abort: false,
                    stall: true,
                }
            }

            /// A reply that fails part-way through its body.
            ///
            /// The relay answers `status`, writes `chunks`, and then errors
            /// instead of ending the message — which is what a connection
            /// broken mid-body looks like to the client, and the only way this
            /// double can reach a `Some(Err(e))` on a response body. A body
            /// whose `Error` is `Infallible` forecloses those arms by
            /// construction, which is why the error type here is not.
            ///
            /// The status is a parameter rather than a fixed `200` because two
            /// different arms need this shape and they need it on opposite
            /// sides of `is_success`: `recv_frame`'s mid-stream error arm on a
            /// `200`, and `open_events`' failed-refusal-read fallback on a
            /// `4xx`. Fixing the status at `200` left the second unreachable by
            /// any script, which is a limit of the double rather than a
            /// property of the code. The media type follows the status for the
            /// same reason: a refusal carries a problem document, not a stream.
            pub(super) fn stream_then_abort(status: u16, chunks: Vec<Bytes>) -> Self {
                Self {
                    status,
                    content_type: Some(if (200..300).contains(&status) {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }),
                    chunks,
                    abort: true,
                    stall: false,
                }
            }
        }

        /// One request exactly as it reached the relay.
        #[derive(Debug, Clone)]
        pub(super) struct Seen {
            pub(super) method: String,
            pub(super) path: String,
            pub(super) body: Vec<u8>,
            headers: Vec<(String, String)>,
        }

        impl Seen {
            /// The value of `name`, which hyper has already lower-cased.
            pub(super) fn header(&self, name: &str) -> Option<&str> {
                self.headers
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.as_str())
            }
        }

        /// The body of a canned response: one HTTP chunk per element, spaced so
        /// the client sees them as separate body frames.
        #[derive(Debug)]
        struct Chunks {
            rest: VecDeque<Bytes>,
            gap: Option<Pin<Box<tokio::time::Sleep>>>,
            abort: bool,
            stall: bool,
        }

        impl hyper::body::Body for Chunks {
            type Data = Bytes;
            /// Not `Infallible`: a body that cannot fail forecloses the client's
            /// mid-stream error arm by construction, and that arm is the one
            /// that decides whether a broken connection is reported or read as
            /// a graceful end of stream.
            type Error = std::io::Error;

            fn poll_frame(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
                let this = self.get_mut();
                if let Some(gap) = this.gap.as_mut() {
                    match gap.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(()) => this.gap = None,
                    }
                }
                let Some(next) = this.rest.pop_front() else {
                    if this.stall {
                        let held = this
                            .gap
                            .get_or_insert_with(|| Box::pin(tokio::time::sleep(STALL)));
                        return match held.as_mut().poll(cx) {
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(()) => Poll::Ready(None),
                        };
                    }
                    if this.abort {
                        this.abort = false;
                        return Poll::Ready(Some(Err(std::io::Error::other(
                            "the relay dropped the body mid-stream",
                        ))));
                    }
                    return Poll::Ready(None);
                };
                // A gap after the last chunk too when an abort follows it: hyper
                // polls a response body ahead of writing it, so an error offered
                // in the same pass as the final chunk kills the connection
                // before the head is flushed, and the client reports a failed
                // *request* rather than a failed body. The gap makes the server
                // flush what it has first.
                if !this.rest.is_empty() || this.abort {
                    this.gap = Some(Box::pin(tokio::time::sleep(CHUNK_GAP)));
                }
                Poll::Ready(Some(Ok(Frame::data(next))))
            }
        }

        /// A running relay: where to dial it, and what it has been asked.
        #[derive(Debug)]
        pub(super) struct Relay {
            pub(super) base: String,
            seen: Arc<Mutex<Vec<Seen>>>,
        }

        impl Relay {
            /// Every request the relay has answered, in order.
            pub(super) fn seen(&self) -> Vec<Seen> {
                self.seen
                    .lock()
                    .expect("the relay's record outlives every request")
                    .clone()
            }
        }

        /// Start a relay that answers `script` in order and records what it was
        /// asked. A request past the end of the script gets a `500`, which
        /// makes an unscripted call a visible failure rather than a hang.
        pub(super) async fn start(script: Vec<Canned>) -> Relay {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback port");
            let base = format!(
                "http://{}",
                listener.local_addr().expect("the bound address")
            );
            let seen = Arc::new(Mutex::new(Vec::new()));
            let queue = Arc::new(Mutex::new(script.into_iter().collect::<VecDeque<_>>()));

            let accepted = Arc::clone(&seen);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let seen = Arc::clone(&accepted);
                    let queue = Arc::clone(&queue);
                    tokio::spawn(async move {
                        let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                            let seen = Arc::clone(&seen);
                            let queue = Arc::clone(&queue);
                            async move {
                                let method = req.method().to_string();
                                let path = req.uri().path().to_owned();
                                let headers = req
                                    .headers()
                                    .iter()
                                    .map(|(n, v)| {
                                        (
                                            n.as_str().to_owned(),
                                            v.to_str().unwrap_or_default().to_owned(),
                                        )
                                    })
                                    .collect();
                                let body = req
                                    .into_body()
                                    .collect()
                                    .await
                                    .map(|b| b.to_bytes().to_vec())
                                    .unwrap_or_default();
                                seen.lock().expect("the relay's record").push(Seen {
                                    method,
                                    path,
                                    body,
                                    headers,
                                });
                                let canned = queue
                                    .lock()
                                    .expect("the relay's script")
                                    .pop_front()
                                    .unwrap_or_else(|| Canned::empty(500));
                                let mut reply = Response::builder()
                                    .status(StatusCode::from_u16(canned.status).expect("a status"));
                                if let Some(ct) = canned.content_type {
                                    reply = reply.header(hyper::header::CONTENT_TYPE, ct);
                                }
                                Ok::<_, Infallible>(
                                    reply
                                        .body(Chunks {
                                            rest: canned.chunks.into_iter().collect(),
                                            gap: None,
                                            abort: canned.abort,
                                            stall: canned.stall,
                                        })
                                        .expect("a response"),
                                )
                            }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });

            Relay { base, seen }
        }
    }

    /// The reply to `Hello` every relay-backed test below starts from.
    const SESSION_OK: &str = r#"{"session_id":"sess-1","server_app_v":"9.9.9","wire_proto":1,"crypto_suite":1,"doc_schema_floor":3,"capabilities":5,"server_time_ms":1788134400000}"#;

    /// The public half of the key [`signer`] signs with.
    fn signing_pub() -> String {
        let key = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        sunrise_http_sig::device_pub_b64(&key.verifying_key().to_bytes())
    }

    /// The signature and date a recorded request carried, having first asserted
    /// that it named the device that signed it.
    fn binding_of(seen: &relay::Seen) -> (String, String) {
        assert_eq!(
            seen.header("x-sunrise-device"),
            Some("dev_01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
            "{} {} must name the signing device",
            seen.method,
            seen.path
        );
        let signature = seen
            .header("x-sunrise-device-sig")
            .unwrap_or_else(|| panic!("{} {} must carry a signature", seen.method, seen.path));
        let date = seen
            .header("date")
            .unwrap_or_else(|| panic!("{} {} must carry a date", seen.method, seen.path));
        (signature.to_owned(), date.to_owned())
    }

    /// A JSON request's binding verifies against the value the relay parsed
    /// back out of the body it was sent.
    ///
    /// ADR-0022 signs the canonical form of the request *value*, so re-parsing
    /// the emitted body and canonicalizing that is exactly what the relay's own
    /// verifier does — which is the property being asserted, rather than
    /// "three headers are present".
    fn assert_json_binding(seen: &relay::Seen) {
        let value: Option<serde_json::Value> = if seen.body.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&seen.body).expect("a JSON body"))
        };
        let (signature, date) = binding_of(seen);
        sunrise_http_sig::verify(
            &signing_pub(),
            &signature,
            &seen.method,
            &seen.path,
            &date,
            value.as_ref(),
            NOW_MS,
        )
        .unwrap_or_else(|e| panic!("{} {} must verify: {e}", seen.method, seen.path));
    }

    /// A byte-body request's binding verifies over the octets themselves: those
    /// bytes already are the canonical form, and routing them through the value
    /// verifier would canonicalize them twice.
    fn assert_byte_binding(seen: &relay::Seen) {
        let (signature, date) = binding_of(seen);
        sunrise_http_sig::verify_canonical(
            &signing_pub(),
            &signature,
            &seen.method,
            &seen.path,
            &date,
            &seen.body,
            NOW_MS,
        )
        .unwrap_or_else(|e| panic!("{} {} must verify: {e}", seen.method, seen.path));
    }

    /// A bound transport pointed at `relay`.
    fn dial(relay: &relay::Relay) -> SseTransport {
        SseTransport::connect(&relay.base).with_device_signer(signer(NOW_MS))
    }

    fn hello_frame() -> Vec<u8> {
        let hello = super::Hello {
            client_app_v: "1.4.2".to_owned(),
            client_platform: "linux-x86_64".to_owned(),
            wire_proto_supported: vec![1],
            doc_schema_min: 1,
            doc_schema_max: 2,
            crypto_suite_supported: vec![1],
            capabilities: 0,
            trace: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
        };
        let mut payload = Vec::new();
        ciborium::ser::into_writer(&hello, &mut payload).expect("a serialisable Hello");
        frame_of(super::MsgKind::Hello, &payload)
    }

    fn frame_of(kind: super::MsgKind, payload: &[u8]) -> Vec<u8> {
        super::encode_frame(kind, super::FrameFlags::EMPTY, payload).expect("an encodable frame")
    }

    /// The frame cap, written out rather than read from the constant it pins.
    ///
    /// A test that sizes its own stream from [`MAX_EVENT_BYTES`] moves with it:
    /// it would build an eight-byte stream against an eight-byte cap and pass,
    /// which makes it a test of the comparison and of nothing else. Spelling
    /// the number here is what lets the cases below say the cap is *this*
    /// large.
    const FRAME_CAP: usize = 8 * 1024 * 1024;

    /// And the constant is that number. Eight mebibytes is the ceiling a relay
    /// has to stay under for its events to be delivered at all, so moving it is
    /// a protocol-visible act rather than an implementation detail.
    #[test]
    fn the_frame_cap_is_the_documented_eight_mebibytes() {
        assert_eq!(super::MAX_EVENT_BYTES, FRAME_CAP);
    }

    /// [`SseTransport::recv_frame`], bounded.
    ///
    /// A read has no deadline of its own — in production the driver's own
    /// supervision is what bounds it — so a relay that goes quiet mid-exchange
    /// would hang the whole test binary rather than fail one test. Every read
    /// below is answered by a scripted relay in milliseconds, so anything near
    /// this deadline is a defect.
    ///
    /// It bounds *waiting*, not spinning. A defect that put the drain loop into
    /// a cycle with no `await` in it would block the runtime thread outright,
    /// and no timer running on that thread can fire to interrupt it; catching
    /// that class needs a deadline outside the process. See the frame-cap cases
    /// below for the part of this surface that is bounded by the code itself.
    async fn next_frame(t: &mut SseTransport) -> Result<Option<Vec<u8>>, TransportError> {
        tokio::time::timeout(std::time::Duration::from_secs(30), t.recv_frame())
            .await
            .expect("a read against a scripted relay must not hang")
    }

    /// One stream chunk that leaves the transport mid-stream: a whole event the
    /// client will take, followed by the beginning of a second it cannot.
    ///
    /// Reading it through [`next_frame`] puts all three pieces of stream state
    /// into the only configuration where dropping them means anything —
    /// `events` `Some`, `last_event_id` `Some("9")`, `buf` non-empty. A test
    /// that sets those fields by hand instead leaves `events` `None`, and every
    /// `self.events = None;` in this module stays deletable.
    fn half_read_stream() -> hyper::body::Bytes {
        hyper::body::Bytes::from(format!(
            "id: 9\ndata: {}\n\ndata: half an ev",
            serde_json::json!({"kind": "caught_up", "stream_id": "ab".repeat(16)})
        ))
    }

    /// Open a session against `relay` and swallow the `HelloAck`, so a test
    /// about something else starts where that test left off.
    async fn open_session(relay: &relay::Relay) -> SseTransport {
        let mut t = dial(relay);
        t.send_frame(hello_frame()).await.expect("a session opens");
        next_frame(&mut t)
            .await
            .expect("the ack is waiting")
            .expect("a HelloAck");
        t
    }

    /// The handshake reaches the relay signed, and its reply becomes the
    /// `HelloAck` the driver negotiated against.
    #[tokio::test]
    async fn hello_opens_a_session_and_its_reply_becomes_a_hello_ack() {
        let relay = relay::start(vec![relay::Canned::json(200, SESSION_OK)]).await;
        let mut t = dial(&relay);
        t.send_frame(hello_frame()).await.expect("a session opens");

        let (head, payload) = super::decode_frame(
            &next_frame(&mut t)
                .await
                .expect("a reply is waiting")
                .expect("a HelloAck"),
        )
        .expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::HelloAck);
        let ack: super::HelloAck =
            ciborium::de::from_reader(&payload[..]).expect("a decodable HelloAck");
        assert_eq!(ack.server_app_v, "9.9.9");
        assert_eq!(ack.wire_proto, 1);
        assert_eq!(ack.crypto_suite, 1);
        assert_eq!(ack.doc_schema_floor, 3);
        assert_eq!(ack.capabilities, 5);
        assert_eq!(ack.server_time_ms, NOW_MS);

        let seen = relay.seen();
        assert_eq!(seen.len(), 1, "one request, and the reply came from it");
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, "/api/v1/sync/session");
        assert_json_binding(&seen[0]);
        let sent: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("a JSON body");
        assert_eq!(sent["client_app_v"], "1.4.2");
        assert_eq!(sent["wire_proto_supported"], serde_json::json!([1]));
        assert_eq!(sent["trace"], "01ARZ3NDEKTSV4RRFFQ69G5FAV");
    }

    /// A relay that refuses an operation is reported as a refusal, with its own
    /// code. Reading a refusal as a success is the failure mode worth pinning:
    /// the driver would then wait on a session that was never opened.
    #[tokio::test]
    async fn a_refused_operation_is_reported_with_the_relay_s_own_code() {
        let relay = relay::start(vec![relay::Canned::json(
            503,
            r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#,
        )])
        .await;
        let mut t = dial(&relay);
        let err = t
            .send_frame(hello_frame())
            .await
            .expect_err("the relay refused");
        assert_eq!(code_of(&err), "RELAY_STORAGE_UNAVAILABLE");
    }

    /// Every route this transport reaches presents the bearer it was built
    /// with.
    ///
    /// Three separate pieces of code attach it — [`SseTransport::call`] for the
    /// JSON operations, [`SseTransport::call_bytes`] for the blob surface and
    /// [`SseTransport::open_events`] for the stream — so this asserts once per
    /// site rather than once for the transport. Nothing else that scores this
    /// crate reaches them: the only other bearer-setting tests are in
    /// `crates/sunrise-e2e`, which `mise.toml:1005`'s `cargo mutants -p
    /// sunrise-sync` does not build and `.cargo/mutants.toml` excludes besides.
    /// Deleting any one of the three bodies leaves the rest of this crate
    /// green, and a transport that dropped the bearer would earn a `401` on
    /// every route it makes.
    #[tokio::test]
    async fn every_route_presents_the_bearer_the_transport_was_built_with() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![half_read_stream()]),
            relay::Canned::bytes(200, b"the ciphertext"),
        ])
        .await;
        let mut t = SseTransport::connect_with_bearer(&relay.base, Some("the-bearer"))
            .with_device_signer(signer(NOW_MS));

        t.send_frame(hello_frame()).await.expect("a session opens");
        next_frame(&mut t)
            .await
            .expect("the ack is waiting")
            .expect("a HelloAck");
        next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("an event became a frame");
        assert_eq!(
            t.blob_fetch(&[0x11u8; 16])
                .await
                .expect("the fetch succeeds"),
            Some(b"the ciphertext".to_vec())
        );

        let seen = relay.seen();
        assert_eq!(seen[0].path, "/api/v1/sync/session");
        assert_eq!(
            seen[0].header("authorization"),
            Some("Bearer the-bearer"),
            "the JSON operations"
        );
        assert_eq!(seen[1].path, "/api/v1/sync/events");
        assert_eq!(
            seen[1].header("authorization"),
            Some("Bearer the-bearer"),
            "the stream"
        );
        assert_eq!(
            seen[2].path,
            format!("/api/v1/blobs/blb_{}", "11".repeat(16))
        );
        assert_eq!(
            seen[2].header("authorization"),
            Some("Bearer the-bearer"),
            "the blob surface"
        );
    }

    /// A transport built without one presents no `Authorization` at all, on
    /// the same three routes.
    ///
    /// The negative half of the case above, and it is not redundant with it: a
    /// header attached unconditionally — a `Bearer` built from an empty string,
    /// a default, a `unwrap_or_default` where the `Option` is read — satisfies
    /// every assertion there and is invisible to all of them. `connect` exists
    /// for the self-host relay running `NullVerifier`, where the request that
    /// carries a credential is the wrong request, so "no header" is the
    /// contract rather than the absence of one.
    #[tokio::test]
    async fn a_transport_built_without_a_bearer_presents_none() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![half_read_stream()]),
            relay::Canned::bytes(200, b"the ciphertext"),
        ])
        .await;
        let mut t = SseTransport::connect(&relay.base).with_device_signer(signer(NOW_MS));

        t.send_frame(hello_frame()).await.expect("a session opens");
        next_frame(&mut t)
            .await
            .expect("the ack is waiting")
            .expect("a HelloAck");
        next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("an event became a frame");
        assert_eq!(
            t.blob_fetch(&[0x11u8; 16])
                .await
                .expect("the fetch succeeds"),
            Some(b"the ciphertext".to_vec())
        );

        let seen = relay.seen();
        assert_eq!(seen[0].path, "/api/v1/sync/session");
        assert_eq!(seen[0].header("authorization"), None, "the JSON operations");
        assert_eq!(seen[1].path, "/api/v1/sync/events");
        assert_eq!(seen[1].header("authorization"), None, "the stream");
        assert_eq!(
            seen[2].path,
            format!("/api/v1/blobs/blb_{}", "11".repeat(16))
        );
        assert_eq!(seen[2].header("authorization"), None, "the blob surface");
    }

    /// A `Subscribe` restates the client's cursors, which drops both the open
    /// stream and the resume point that went with it.
    ///
    /// The resume point is the load-bearing half. `Last-Event-ID` says "I
    /// received everything up to here"; the cursors say "I have *applied*
    /// everything up to here". They differ exactly when delivery succeeded and
    /// application did not, which is precisely when the driver resubscribes —
    /// so keeping the id would resume past the ops the cursors are asking for.
    ///
    /// The stream is opened **for real** before the `Subscribe`, rather than
    /// the three fields being set by hand: with `events` already `None` on
    /// entry, `self.events = None` is deletable and every assertion below still
    /// holds. A client that kept the old body would go on reading events for a
    /// stream set it no longer subscribes to, and never open the new one.
    #[tokio::test]
    async fn a_subscribe_drops_the_open_stream_and_its_resume_point() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![half_read_stream()]),
            relay::Canned::json(200, "{}"),
        ])
        .await;
        let mut t = open_session(&relay).await;
        next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("an event became a frame");
        assert!(
            t.events.is_some(),
            "the stream is open before the subscribe"
        );
        assert_eq!(t.last_event_id.as_deref(), Some("9"));
        assert!(!t.buf.is_empty(), "and half an event is still buffered");

        let payload = super::SubscribePayload {
            streams: vec![sunrise_wire_protocol::SubscribeEntry {
                cursors: vec![super::CursorEntry {
                    device_id: [0x7bu8; 16],
                    last_applied_seq: 42,
                }],
                stream_id: [0xabu8; 16],
            }],
        }
        .encode()
        .expect("an encodable subscribe");
        t.send_frame(frame_of(super::MsgKind::Subscribe, &payload))
            .await
            .expect("the subscribe lands");

        assert!(
            t.events.is_none(),
            "the stream set changed, so the body opened against the old one goes"
        );
        assert!(
            t.last_event_id.is_none(),
            "a restated cursor is the stricter statement, so the resume point goes"
        );
        assert!(t.buf.is_empty(), "and the stale stream's bytes with it");

        let seen = relay.seen();
        assert_eq!(seen[2].path, "/api/v1/sync/subscribe");
        assert_json_binding(&seen[2]);
        let sent: serde_json::Value = serde_json::from_slice(&seen[2].body).expect("a JSON body");
        assert_eq!(sent["streams"][0]["stream_id"], "ab".repeat(16));
        assert_eq!(
            sent["streams"][0]["cursors"][0]["device_id"],
            "7b".repeat(16)
        );
        assert_eq!(sent["streams"][0]["cursors"][0]["last_applied_seq"], 42);
    }

    /// A batch goes up as base64 and comes back acked with the relay's
    /// first-seen time, which is what makes a re-send after a lost ack
    /// idempotent rather than a second publication.
    #[tokio::test]
    async fn an_op_batch_is_published_and_acked_against_the_batch_that_sent_it() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(200, r#"{"server_first_seen_ms":1234}"#),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let payload = super::OpBatchPayload {
            ops: vec![b"one".to_vec(), b"two".to_vec()],
            batch_id: 7,
            stream_id: [0xabu8; 16],
        }
        .encode()
        .expect("an encodable batch");
        t.send_frame(frame_of(super::MsgKind::OpBatch, &payload))
            .await
            .expect("the batch is published");

        let (head, payload) = super::decode_frame(
            &next_frame(&mut t)
                .await
                .expect("an ack is waiting")
                .expect("an Ack"),
        )
        .expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::Ack);
        let ack = super::AckPayload::decode(&payload).expect("an AckPayload");
        assert_eq!(ack.batch_id, 7);
        assert_eq!(ack.stream_id, [0xabu8; 16]);
        assert_eq!(ack.server_first_seen_ms, 1234);

        let seen = relay.seen();
        assert_eq!(seen[1].path, "/api/v1/sync/ops");
        assert_json_binding(&seen[1]);
        assert_eq!(
            seen[1].header("x-sunrise-session"),
            Some("sess-1"),
            "every post-handshake operation names the session the handshake minted; \
             the relay's SessionHeader extractor refuses one that does not"
        );
        let sent: serde_json::Value = serde_json::from_slice(&seen[1].body).expect("a JSON body");
        assert_eq!(sent["batch_id"], 7);
        assert_eq!(sent["ops"], serde_json::json!(["b25l", "dHdv"]));
    }

    /// A refreshed credential is acknowledged with the new deadline, so the
    /// driver knows when to refresh again rather than waiting for a close.
    #[tokio::test]
    async fn a_token_refresh_is_acked_with_the_new_deadline() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(200, r#"{"expires_at_ms":1788138000000}"#),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let payload = super::RefreshTokenPayload {
            token: "the-new-bearer".to_owned(),
        }
        .encode()
        .expect("an encodable refresh");
        t.send_frame(frame_of(super::MsgKind::RefreshToken, &payload))
            .await
            .expect("the refresh lands");

        let (head, payload) = super::decode_frame(
            &next_frame(&mut t)
                .await
                .expect("an ack is waiting")
                .expect("a RefreshTokenAck"),
        )
        .expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::RefreshTokenAck);
        let ack = super::RefreshTokenAckPayload::decode(&payload).expect("the ack payload");
        assert_eq!(ack.expires_at_ms, 1_788_138_000_000);

        let seen = relay.seen();
        assert_eq!(seen[1].path, "/api/v1/sync/session/refresh");
        assert_json_binding(&seen[1]);
    }

    // ---- the three refusals a success path would swallow ----
    //
    // `Subscribe`, `OpBatch` and `RefreshToken` each guard their reply with
    // `if !reply.status.is_success()`. Each guard is separate code and each
    // survives separately: replacing any one of them with `false` turns a
    // refusal into a success made out of a problem document, and the reply
    // readers are total — `field_u64` answers `0` for a field that is not
    // there — so nothing downstream notices.

    /// A batch the relay refused is a refusal, never an ack.
    ///
    /// The load-bearing one of the three, and therefore written first. Without
    /// the guard, the problem document is parsed as an ack: `field_u64` reads
    /// the missing `server_first_seen_ms` as `0`, an `Ack` naming this batch
    /// reaches the driver, and the outbox retires ops the relay never stored.
    /// That is data loss rather than a retry — after the ack the driver has
    /// nothing left to re-send.
    #[tokio::test]
    async fn an_op_batch_the_relay_refused_is_never_acked() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(409, r#"{"code":"SYNC_OP_INVALID"}"#),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let payload = super::OpBatchPayload {
            ops: vec![b"one".to_vec()],
            batch_id: 7,
            stream_id: [0xabu8; 16],
        }
        .encode()
        .expect("an encodable batch");
        let err = t
            .send_frame(frame_of(super::MsgKind::OpBatch, &payload))
            .await
            .expect_err("the relay refused the batch");

        assert_eq!(code_of(&err), "SYNC_OP_INVALID");
        assert!(
            t.inbound.is_empty(),
            "a refused batch synthesizes no Ack, so the outbox keeps its ops"
        );
    }

    /// A `Subscribe` the relay refused leaves the client subscribed to what it
    /// was subscribed to.
    ///
    /// Without the guard the refusal reads as success, and the three pieces of
    /// stream state are discarded for a stream set the relay never adopted: the
    /// driver believes it is subscribed to streams that were refused, and the
    /// resume point it would have needed to recover is gone.
    #[tokio::test]
    async fn a_subscribe_the_relay_refused_discards_no_resume_point() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(403, r#"{"code":"AUTH_DEVICE_REVOKED"}"#),
        ])
        .await;
        let mut t = open_session(&relay).await;
        t.last_event_id = Some("9".to_owned());

        let payload = super::SubscribePayload {
            streams: vec![sunrise_wire_protocol::SubscribeEntry {
                cursors: Vec::new(),
                stream_id: [0xabu8; 16],
            }],
        }
        .encode()
        .expect("an encodable subscribe");
        let err = t
            .send_frame(frame_of(super::MsgKind::Subscribe, &payload))
            .await
            .expect_err("the relay refused the subscribe");

        assert_eq!(code_of(&err), "AUTH_DEVICE_REVOKED");
        assert_eq!(
            t.last_event_id.as_deref(),
            Some("9"),
            "a refused subscribe changed no stream set, so it discards no resume point"
        );
    }

    /// A refused refresh is reported rather than acked with a deadline the
    /// relay never set.
    ///
    /// Without the guard, `field_u64` reads the absent `expires_at_ms` as `0`,
    /// which this protocol spells *no deadline* — so a client whose refresh was
    /// rejected would be told its credential never expires, and would stop
    /// refreshing until the relay closed the stream under it.
    #[tokio::test]
    async fn a_token_refresh_the_relay_refused_is_never_acked() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(401, r#"{"code":"AUTH_TOKEN_INVALID"}"#),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let payload = super::RefreshTokenPayload {
            token: "a token the relay will not take".to_owned(),
        }
        .encode()
        .expect("an encodable refresh");
        let err = t
            .send_frame(frame_of(super::MsgKind::RefreshToken, &payload))
            .await
            .expect_err("the relay refused the refresh");

        assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
        assert!(
            t.inbound.is_empty(),
            "a refused refresh names no new deadline"
        );
    }

    /// A `Ping` is answered locally. The stream keeps itself alive with
    /// comments, so a liveness probe costs no round trip — and a transport that
    /// issued one would make the driver's ping interval a request rate.
    #[tokio::test]
    async fn a_ping_is_answered_locally_with_nothing_on_the_wire() {
        let relay = relay::start(Vec::new()).await;
        let mut t = SseTransport::connect(&relay.base);
        t.send_frame(frame_of(super::MsgKind::Ping, &[]))
            .await
            .expect("a local answer");

        let (head, _) = super::decode_frame(
            &next_frame(&mut t)
                .await
                .expect("a reply is waiting")
                .expect("a Pong"),
        )
        .expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::Pong);
        assert!(relay.seen().is_empty(), "and the relay was never asked");
    }

    /// A `Close` ends the session in both directions at once: nothing more goes
    /// up, the open stream is dropped, and the read side reports end of stream
    /// rather than reopening.
    ///
    /// The stream is opened first, so `self.events = None` in the `Close` arm
    /// is executed with something to clear. `self.closed` alone satisfies the
    /// end-of-stream assertion, so without an open body this case says nothing
    /// about the body being released — and a `Close` that left it held would
    /// keep the relay's connection alive for a session that is over.
    #[tokio::test]
    async fn a_close_frame_ends_the_session_in_both_directions() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![half_read_stream()]),
        ])
        .await;
        let close = super::ClosePayload {
            code: sunrise_error::ErrorCode::AuthTokenExpired,
            reason: "done".to_owned(),
        }
        .encode()
        .expect("an encodable close");
        let mut t = open_session(&relay).await;
        next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("an event became a frame");
        assert!(t.events.is_some(), "the stream is open before the close");

        t.send_frame(frame_of(super::MsgKind::Close, &close))
            .await
            .expect("the close is local");

        assert!(
            t.events.is_none(),
            "the session is over, so the body it was reading is released"
        );
        assert!(
            matches!(
                t.send_frame(frame_of(super::MsgKind::Close, &close)).await,
                Err(TransportError::Cancelled)
            ),
            "a closed transport sends nothing more"
        );
        assert!(
            next_frame(&mut t)
                .await
                .expect("a closed transport is not an error")
                .is_none(),
            "and reads as end of stream"
        );
        assert_eq!(
            relay.seen().len(),
            2,
            "the close itself costs no request: the session and the stream are all of them"
        );
    }

    /// Closing the transport drops the stream and the half-event behind it.
    ///
    /// Keeping the buffer would hand a reopened stream the tail of the old
    /// one's last event, which parses as neither — and keeping the body would
    /// hold the relay's connection open past the close. Both need a stream that
    /// is really open, which is why this dials one rather than assigning `buf`.
    #[tokio::test]
    async fn closing_the_transport_drops_the_stream_and_its_buffer() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![half_read_stream()]),
        ])
        .await;
        let mut t = open_session(&relay).await;
        next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("an event became a frame");
        assert!(t.events.is_some() && !t.buf.is_empty());

        Transport::close(&mut t).await.expect("close succeeds");

        assert!(t.events.is_none(), "the stream goes");
        assert!(t.buf.is_empty(), "the partial event goes with the stream");
        assert!(
            next_frame(&mut t).await.expect("not an error").is_none(),
            "a closed transport reads as end of stream"
        );
        assert!(
            matches!(
                t.send_frame(hello_frame()).await,
                Err(TransportError::Cancelled)
            ),
            "and refuses to send"
        );
        assert_eq!(
            relay.seen().len(),
            2,
            "the session and the stream open, and closing adds nothing to them"
        );
    }

    /// A stream that breaks mid-body is reported, not read as a graceful end.
    ///
    /// `recv_frame` has two arms for a stream that stops: `None` is the relay
    /// closing the stream cleanly, which is not an error because the driver's
    /// reconnect decides whether to come back, and `Some(Err(e))` is the
    /// connection failing under it, which is. Collapsing the second onto the
    /// first would have a client treat a broken relay as a caught-up one and
    /// back off instead of retrying.
    ///
    /// Reaching it needs a double whose body can fail: with
    /// `Chunks::Error = Infallible` the arm is unreachable by construction, and
    /// a mutant that deletes it survives for that reason rather than because
    /// nothing depends on it.
    #[tokio::test]
    async fn a_stream_that_breaks_mid_body_is_reported_rather_than_ended() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream_then_abort(200, vec![half_read_stream()]),
        ])
        .await;
        let mut t = open_session(&relay).await;
        next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("an event became a frame");

        let err = next_frame(&mut t)
            .await
            .expect_err("a body that fails is not an end of stream");
        assert!(
            matches!(err, TransportError::Unavailable(_)),
            "a broken connection is retryable, not a clean close: {err}"
        );
        assert!(
            t.events.is_none(),
            "the failed body is released so the next read reopens"
        );
    }

    /// A frame the wire codec cannot decode is refused before any route is
    /// chosen.
    ///
    /// `send_frame` decodes the frame header first, and every arm below that
    /// point assumes it succeeded. A build that let a malformed frame through
    /// would dispatch on whatever `msg_kind` the garbage happened to hold, and
    /// a build that reported it as `Unavailable` would have the driver retry a
    /// frame that will never decode.
    #[tokio::test]
    async fn a_frame_that_does_not_decode_is_refused_before_any_route() {
        let relay = relay::start(Vec::new()).await;
        let mut t = SseTransport::connect(&relay.base);
        let err = t
            .send_frame(vec![0xffu8; 8])
            .await
            .expect_err("that is not a frame");
        assert!(matches!(err, TransportError::Protocol(_)), "{err}");
        assert!(
            relay.seen().is_empty(),
            "nothing reaches the relay for a frame that never decoded"
        );
    }

    /// A `Hello` reply this transport cannot read is a failed handshake, and
    /// says so.
    ///
    /// Two shapes, and the second is the one that used to be quiet. A reply
    /// that is not JSON has always been a protocol error the driver sees. A
    /// reply that *is* JSON but names no `session_id` was accepted:
    /// `self.session` stayed `None` and a `HelloAck` went to the driver as
    /// though the handshake had succeeded, so every later `call` went up with
    /// no `x-sunrise-session` for the relay to refuse and the next stream open
    /// reported "no sync session: send Hello first" — the wrong cause, since
    /// `Hello` was sent and acked.
    ///
    /// `crates/sunrise-server/src/api/sync/credential.rs:51` declares
    /// `session_id` non-`Option`, so such a reply is malformed by the server's
    /// own contract. Both shapes now fail where they are read, and neither
    /// leaves an ack behind: the driver's reconnect is what decides what
    /// happens next, which is what it does for every other failed handshake.
    #[tokio::test]
    async fn a_hello_reply_this_build_cannot_read_is_refused_rather_than_acked() {
        for body in [
            "not json at all",
            r#"{"server_app_v":"9.9.9","wire_proto":1,"crypto_suite":1,"doc_schema_floor":3}"#,
        ] {
            let relay = relay::start(vec![relay::Canned::json(200, body)]).await;
            let mut t = dial(&relay);
            let err = t
                .send_frame(hello_frame())
                .await
                .expect_err("a reply this build cannot read is not a session");
            assert!(matches!(err, TransportError::Protocol(_)), "{err}");
            assert!(
                t.session.is_none(),
                "and nothing is remembered from a handshake that failed"
            );
            assert!(
                t.inbound.is_empty(),
                "no HelloAck reaches the driver for a handshake that did not happen"
            );
        }
    }

    /// A frame kind that has no operation upstream is refused here rather than
    /// turned into a request the relay would have to reject.
    #[tokio::test]
    async fn a_frame_with_no_upstream_operation_is_refused_before_it_is_sent() {
        let relay = relay::start(Vec::new()).await;
        let mut t = SseTransport::connect(&relay.base);
        let err = t
            .send_frame(frame_of(super::MsgKind::Pong, &[]))
            .await
            .expect_err("nothing carries a Pong upstream");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("Pong")),
            "{err}"
        );
        assert!(relay.seen().is_empty());
    }

    /// The stream is a bound route like every other, and its events become the
    /// frames the driver already knows how to handle.
    ///
    /// It is the easiest binding to leave out, because it is the one request
    /// with no body to sign over.
    #[tokio::test]
    async fn the_event_stream_is_opened_bound_and_its_events_become_frames() {
        let event = format!(
            "id: 7\ndata: {}\n\n",
            serde_json::json!({"kind": "caught_up", "stream_id": "ab".repeat(16)})
        );
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![hyper::body::Bytes::from(event)]),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let (head, payload) = super::decode_frame(
            &next_frame(&mut t)
                .await
                .expect("the stream opens")
                .expect("an event became a frame"),
        )
        .expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::StreamUpdate);
        assert_eq!(
            super::CaughtUpPayload::decode(&payload)
                .expect("a CaughtUpPayload")
                .stream_id,
            [0xabu8; 16]
        );
        assert_eq!(
            t.last_event_id.as_deref(),
            Some("7"),
            "the resume point advances with every id the stream carried"
        );

        let seen = relay.seen();
        assert_eq!(seen[1].method, "GET");
        assert_eq!(seen[1].path, "/api/v1/sync/events");
        assert_eq!(seen[1].header("x-sunrise-session"), Some("sess-1"));
        assert_eq!(seen[1].header("accept"), Some("text/event-stream"));
        assert_json_binding(&seen[1]);
    }

    /// A stream that ends is a graceful end, and the reconnect resumes from the
    /// last id rather than replaying what it already applied.
    #[tokio::test]
    async fn a_reopened_stream_resumes_from_the_last_id_it_saw() {
        let event = |id: u32| {
            hyper::body::Bytes::from(format!(
                "id: {id}\ndata: {}\n\n",
                serde_json::json!({"kind": "caught_up", "stream_id": "ab".repeat(16)})
            ))
        };
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![event(7)]),
            relay::Canned::stream(vec![event(8)]),
        ])
        .await;
        let mut t = open_session(&relay).await;

        next_frame(&mut t)
            .await
            .expect("the stream opens")
            .expect("a frame");
        assert!(
            next_frame(&mut t)
                .await
                .expect("a relay that closes the stream is not an error")
                .is_none(),
            "the driver's backoff decides whether to come back, not this layer"
        );
        next_frame(&mut t)
            .await
            .expect("the stream reopens")
            .expect("a frame");

        let seen = relay.seen();
        assert_eq!(
            seen[1].header("last-event-id"),
            None,
            "nothing to resume from yet"
        );
        assert_eq!(seen[2].header("last-event-id"), Some("7"));
    }

    /// A refused stream is reported as a refusal rather than opened, since
    /// treating one as a successful open would leave the driver reading an
    /// error document as events — and it is reported with the relay's *own*
    /// code, like every other route.
    ///
    /// This is the flattening [`SseTransport::refuse`] exists to prevent: a
    /// client told "your signature is stale, fix your clock" heard "your bearer
    /// is bad" and refreshed a token that was never the problem. The stream was
    /// the one route still doing it, because it dropped the problem document
    /// before `refuse` could read a code out of it.
    ///
    /// The read goes through [`next_frame`] like the other relay reads: a
    /// refusal is answered by a relay whose script is exhausted by then, and an
    /// unbounded read there would hang the whole test binary rather than fail
    /// one test.
    #[tokio::test]
    async fn a_refused_stream_is_reported_with_the_relay_s_own_code() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(401, r#"{"code":"AUTH_DEVICE_SIG_INVALID"}"#),
        ])
        .await;
        let mut t = open_session(&relay).await;
        let err = next_frame(&mut t)
            .await
            .expect_err("the relay refused the stream");
        assert_eq!(code_of(&err), "AUTH_DEVICE_SIG_INVALID");

        // And the advice rides with it. A refused *binding* is the one refusal
        // whose code leaves the user nothing to do, so `refuse` appends what to
        // do about it — and until `open_events` read the body this route could
        // not reach that arm at all, because a refusal with no document never
        // resolves to `AUTH_DEVICE_SIG_INVALID`. The skew branch is the
        // deterministic one here: [`signer`] is fixed at [`NOW_MS`], a date in
        // the past, and the relay's own `Date` is the wall clock, so the
        // measured skew is always outside the tolerance.
        let message = message_of(&err);
        assert!(
            message.contains("AUTH_DEVICE_SIG_INVALID"),
            "the relay's own words come through: {message}"
        );
        assert!(
            message.contains("set the system clock and retry"),
            "and the user is told what to do about it: {message}"
        );
    }

    /// And a refusal carrying no code at all still lands on the status map,
    /// which is what keeps this client readable against a relay whose codes it
    /// does not know.
    #[tokio::test]
    async fn a_refused_stream_with_no_document_falls_back_to_the_status_map() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::empty(401),
        ])
        .await;
        let mut t = open_session(&relay).await;
        let err = next_frame(&mut t)
            .await
            .expect_err("the relay refused the stream");
        assert_eq!(code_of(&err), "AUTH_TOKEN_INVALID");
    }

    /// A refusal whose body *fails* is still a refusal.
    ///
    /// The third way the document can fail to arrive, beside empty and
    /// oversized, and the one no script could reach until the double could
    /// pair a non-2xx status with a failing body. The relay answers `401`,
    /// writes a **whole, valid** problem document naming
    /// `AUTH_DEVICE_SIG_INVALID` and then breaks the connection instead of
    /// ending the message: a failed read yields nothing, so what arrived is
    /// discarded rather than parsed, the read does not swallow the refusal,
    /// and the status map answers — the same degrade the empty and oversized
    /// bodies take.
    ///
    /// The document is valid on purpose. A truncated one would land on
    /// `AUTH_TOKEN_INVALID` whether the failed-read arm ran or the bytes were
    /// simply unparseable, and the test would pass without reaching the arm it
    /// names.
    #[tokio::test]
    async fn a_refusal_whose_body_fails_falls_back_to_the_status_map() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream_then_abort(
                401,
                vec![hyper::body::Bytes::from_static(
                    br#"{"code":"AUTH_DEVICE_SIG_INVALID"}"#,
                )],
            ),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let err = next_frame(&mut t)
            .await
            .expect_err("the relay refused the stream and then dropped the body");
        assert_eq!(
            code_of(&err),
            "AUTH_TOKEN_INVALID",
            "a refusal whose document failed is reported as a refusal, not as a broken read"
        );
    }

    /// A refusal whose body never finishes arriving is still a refusal.
    ///
    /// The relay answers `401`, writes the first few bytes of a problem
    /// document, and then holds the connection. Before the read was deadlined
    /// this await did not return: the driver's only bound is `tokio::select!`
    /// **cancelling** the whole future, and cancellation is not a timeout — the
    /// partial body and the refusal are both dropped, `events` is still `None`,
    /// and the next iteration opens a brand-new stream. No error reaches the
    /// driver, the session never ends and no backoff engages, so a stalling
    /// relay becomes a silent retry loop driven by whatever else wakes the
    /// driver.
    ///
    /// What comes back is the status map, which is where this route was before
    /// it read the body at all — and the truncated `"code":"AUTH_DEV` that did
    /// arrive is *not* read, because half a document is not a code.
    #[tokio::test]
    async fn a_refusal_whose_body_stalls_is_still_reported_as_a_refusal() {
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json_then_stall(401, r#"{"code":"AUTH_DEV"#),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let started = tokio::time::Instant::now();
        let err = next_frame(&mut t)
            .await
            .expect_err("the relay refused the stream and then went quiet");
        assert_eq!(
            code_of(&err),
            "AUTH_TOKEN_INVALID",
            "a body that never arrived falls back to the status map"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(1_500)
                && elapsed < std::time::Duration::from_secs(10),
            "the read is bounded by its own deadline, and that deadline is the two seconds \
             `the_body_bounds_are_the_numbers_they_are_documented_as` pins — an upper bound \
             alone leaves every shorter value passing too: {elapsed:?}"
        );
    }

    /// A handshake whose reply body never finishes arriving is reported rather
    /// than waited on for good.
    ///
    /// The `Hello` route is the one with nothing above it. The stream route's
    /// stall, above, was at least *cancelled*: `sync_driver.rs:848-855` selects
    /// over the read, so a stalled refusal there became a silent retry loop.
    /// This one is not. `sync_driver.rs:723` awaits the `Hello` send bare,
    /// outside any `tokio::select!`, `session(...)` is itself awaited bare at
    /// `sync_driver.rs:628`, and `Client::builder(...).build(https)` sets no
    /// timeout — so an unbounded read here parks the driver task with no error,
    /// no `SessionEnd::Disconnected`, no backoff and no log line. That is the
    /// "the client isn't syncing" state `sync_driver.rs:580-585` calls the
    /// commonest support question, and the only symptom is silence.
    ///
    /// The relay answers `200` — a refusal is not needed to reach this, and a
    /// success makes the point better: the handshake is going *well* right up
    /// to the body that never comes.
    #[tokio::test]
    async fn a_stalled_handshake_reply_is_reported_rather_than_parking_the_driver() {
        let relay = relay::start(vec![relay::Canned::json_then_stall(
            200,
            r#"{"session_id":"sess-"#,
        )])
        .await;
        let mut t = dial(&relay);

        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            t.send_frame(hello_frame()),
        )
        .await
        .expect("the read is bounded by the client, not by this harness")
        .expect_err("the relay answered and then went quiet");

        assert!(
            matches!(&err, TransportError::Unavailable(m) if m.contains("stalled")),
            "a stalled handshake is reported as unavailable, which is what the driver \
             turns into a disconnect and a backoff: {err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "and it came back on the client's own deadline"
        );
        assert!(
            t.session.is_none(),
            "no session is recorded out of a reply that never arrived"
        );
    }

    /// A handshake reply past the event cap is refused rather than held.
    ///
    /// The other half of the same bound. Nothing legitimate on this route is
    /// anywhere near eight mebibytes — every reply it reads is a small JSON
    /// document — so the cap is not sizing the reply; it is denying a relay the
    /// choice of how much memory this client holds. Before the read was capped
    /// that choice was the relay's alone.
    #[tokio::test]
    async fn a_handshake_reply_past_the_event_cap_is_refused_rather_than_held() {
        let oversized = format!(r#"{{"session_id":"{}"}}"#, "x".repeat(FRAME_CAP));
        assert!(oversized.len() > FRAME_CAP, "the reply is past the cap");
        let relay = relay::start(vec![relay::Canned::json(200, &oversized)]).await;
        let mut t = dial(&relay);

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            t.send_frame(hello_frame()),
        )
        .await
        .expect("a capped read returns")
        .expect_err("the reply is past the cap");

        assert!(
            matches!(&err, TransportError::Unavailable(_)),
            "the relay sent more than was allowed, which is the relay's failure: {err}"
        );
        assert!(
            t.session.is_none(),
            "and nothing is read out of a body that was refused"
        );
    }

    /// The blob surface is bounded too, and by the same deadline.
    ///
    /// The third of the three body reads. It is not on the handshake's footing
    /// — a blob fetch is driven by a caller who can be told it failed — but it
    /// had the same missing bound, and the shared read is what makes the three
    /// move together instead of each being remembered separately.
    ///
    /// The double's media type says JSON where a blob would say octets;
    /// `call_bytes` never reads it, and the stall is the subject.
    #[tokio::test]
    async fn a_stalled_blob_fetch_is_reported_rather_than_parking_the_caller() {
        let relay = relay::start(vec![relay::Canned::json_then_stall(
            200,
            "the first ciphertext",
        )])
        .await;
        let mut t = dial(&relay);

        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            t.blob_fetch(&[0x22; 16]),
        )
        .await
        .expect("the read is bounded by the client, not by this harness")
        .expect_err("the relay answered and then went quiet");

        assert!(
            matches!(&err, TransportError::Unavailable(m) if m.contains("stalled")),
            "a stalled fetch is reported, so the caller can retry it: {err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "and it came back on the client's own deadline"
        );
    }

    /// An attachment larger than the *event* cap is still delivered.
    ///
    /// The blob route's cap is the one argument in this module that cannot be
    /// reached from its refusing side: [`super::MAX_BLOB_BYTES`] is a hundred
    /// megabytes, and scripting that over loopback to watch it be refused would
    /// cost more than the boundary is worth. So it is pinned from the other
    /// side, which is the side that matters anyway — sizing this route by
    /// [`super::MAX_EVENT_BYTES`] would refuse every attachment over eight
    /// mebibytes, and the relay commits them up to a hundred megabytes
    /// (`crates/sunrise-server/src/api/blobs.rs:62`). A byte past the event cap
    /// is enough to tell the two constants apart, and it is the cheapest body
    /// that can.
    ///
    /// The ciphertext is opaque by construction here: what is asserted is that
    /// every byte the relay sent came back, which is what a content hash
    /// downstream depends on.
    #[tokio::test]
    async fn an_attachment_larger_than_the_event_cap_is_still_fetched_whole() {
        let ciphertext = vec![0xA5u8; FRAME_CAP + 1];
        let relay = relay::start(vec![relay::Canned::bytes(200, &ciphertext)]).await;
        let mut t = dial(&relay);

        let fetched = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            t.blob_fetch(&[0x33; 16]),
        )
        .await
        .expect("a bounded read returns")
        .expect("the relay served the blob")
        .expect("and it is there");

        assert_eq!(
            fetched.len(),
            FRAME_CAP + 1,
            "the blob route is not bounded by the event cap"
        );
        assert!(
            fetched.iter().all(|b| *b == 0xA5),
            "and every byte of it came back unaltered"
        );
    }

    /// A refusal document is read up to the frame cap and no further.
    ///
    /// Both sides of the boundary, because a cap that refused everything would
    /// pass a test written only against the oversized half. At exactly
    /// [`super::MAX_EVENT_BYTES`] the relay's own code still wins; one byte
    /// past it the read is abandoned and the status map answers instead — which
    /// is the same degrade every other unreadable refusal takes, so a relay
    /// cannot make this client hold an unbounded body by refusing it.
    #[tokio::test]
    async fn a_refusal_document_past_the_frame_cap_is_abandoned_for_the_status_map() {
        let document = |len: usize| {
            let head = r#"{"code":"AUTH_DEVICE_SIG_INVALID","detail":""#;
            let tail = r#""}"#;
            format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
        };

        let at_cap = document(FRAME_CAP);
        assert_eq!(at_cap.len(), FRAME_CAP);
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(401, &at_cap),
        ])
        .await;
        let mut t = open_session(&relay).await;
        let err = next_frame(&mut t).await.expect_err("the relay refused");
        assert_eq!(
            code_of(&err),
            "AUTH_DEVICE_SIG_INVALID",
            "exactly the cap is under the cap, so the relay's own code wins"
        );

        let past_cap = document(FRAME_CAP + 1);
        assert_eq!(past_cap.len(), FRAME_CAP + 1);
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::json(401, &past_cap),
        ])
        .await;
        let mut t = open_session(&relay).await;
        let err = next_frame(&mut t).await.expect_err("the relay refused");
        assert_eq!(
            code_of(&err),
            "AUTH_TOKEN_INVALID",
            "one byte past the cap is not read, so the status map answers"
        );
    }

    /// An event that arrives in two chunks is one event, and the frame cap does
    /// not fire on the way.
    ///
    /// Both halves are deliberately larger than the square root of the cap, so
    /// a cap check that multiplied the buffered length by the arriving one
    /// instead of adding them would refuse this stream. That is the arithmetic
    /// the check is made of, and it is invisible to any test whose chunks are
    /// small.
    #[tokio::test]
    async fn an_event_split_across_chunks_is_reassembled_under_the_cap() {
        let event = format!(
            "id: 3\ndata: {}\n\n",
            serde_json::json!({"kind": "gap", "reason": "x".repeat(8_000)})
        );
        let (head, tail) = event.as_bytes().split_at(event.len() / 2);
        assert!(
            head.len() * tail.len() > FRAME_CAP,
            "the halves must be large enough to separate a product from a sum"
        );
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![
                hyper::body::Bytes::copy_from_slice(head),
                hyper::body::Bytes::copy_from_slice(tail),
            ]),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let (head, payload) = super::decode_frame(
            &next_frame(&mut t)
                .await
                .expect("the halves make one event")
                .expect("a frame"),
        )
        .expect("a decodable frame");
        assert_eq!(head.msg_kind, super::MsgKind::Error);
        let parsed = super::ErrorPayload::decode(&payload).expect("an ErrorPayload");
        assert_eq!(parsed.code, sunrise_error::ErrorCode::SyncCursorGap);
        assert_eq!(parsed.reason.len(), 8_000);
    }

    /// A relay that never sends a blank line cannot grow this buffer without
    /// bound: past the frame cap the stream is dropped and the condition named.
    #[tokio::test]
    async fn an_event_that_never_terminates_is_refused_at_the_frame_cap() {
        let cap = FRAME_CAP;
        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![
                hyper::body::Bytes::from(vec![b'x'; cap - 4]),
                hyper::body::Bytes::from(vec![b'x'; 8]),
            ]),
        ])
        .await;
        let mut t = open_session(&relay).await;

        let err = next_frame(&mut t).await.expect_err("the cap fires");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("frame cap")),
            "{err}"
        );
        assert!(t.events.is_none(), "and the stream is dropped with it");
    }

    /// The cap is a ceiling, not a limit one byte lower: a stream that arrives
    /// at exactly the cap is delivered.
    ///
    /// Written against a keep-alive so the whole eight mebibytes are a single
    /// terminated event that yields nothing to the driver — the cap is the
    /// subject here, not the parsing.
    #[tokio::test]
    async fn a_stream_arriving_at_exactly_the_cap_is_not_refused() {
        let cap = FRAME_CAP;
        let mut comment = Vec::with_capacity(cap);
        comment.push(b':');
        comment.extend(std::iter::repeat_n(b'x', cap - 3));
        comment.extend_from_slice(b"\n\n");
        assert_eq!(comment.len(), cap);
        let (head, tail) = comment.split_at(cap - 4);

        let relay = relay::start(vec![
            relay::Canned::json(200, SESSION_OK),
            relay::Canned::stream(vec![
                hyper::body::Bytes::copy_from_slice(head),
                hyper::body::Bytes::copy_from_slice(tail),
            ]),
        ])
        .await;
        let mut t = open_session(&relay).await;

        assert!(
            next_frame(&mut t)
                .await
                .expect("exactly the cap is under the cap")
                .is_none(),
            "a keep-alive carries no frame, and the stream then ends"
        );
    }

    /// The three revocation outcomes are kept apart.
    ///
    /// "Revoked" and "no such row" are both terminal and are not the same
    /// answer: the second one covers a device still being accepted under a row
    /// from before `vault_device_id` existed, and reporting it as a revocation
    /// would tell a user their lost device had been locked out when it had not.
    #[tokio::test]
    async fn a_revocation_keeps_revoked_unknown_and_refused_apart() {
        let relay = relay::start(vec![
            relay::Canned::empty(204),
            relay::Canned::empty(404),
            relay::Canned::json(500, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#),
        ])
        .await;
        let mut t = dial(&relay);
        let device = [0x11u8; 16];

        assert_eq!(
            t.revoke_device(device).await.expect("a revocation"),
            crate::transport::RevokeOutcome::Revoked
        );
        assert_eq!(
            t.revoke_device(device).await.expect("no such row"),
            crate::transport::RevokeOutcome::Unknown
        );
        let err = t
            .revoke_device(device)
            .await
            .expect_err("the relay would not");
        assert_eq!(code_of(&err), "AUTH_DEVICE_REVOKE_FAILED");

        let seen = relay.seen();
        assert_eq!(seen[0].method, "DELETE");
        assert_eq!(
            seen[0].path,
            format!(
                "/api/v1/devices/by-vault-id/{}",
                sunrise_id::crockford::encode_bytes(&device)
            ),
            "the route names the vault id, not the ULID the relay minted"
        );
        assert_json_binding(&seen[0]);
        assert_eq!(
            seen[0].header("content-type"),
            None,
            "a `call` with no body advertises no media type either"
        );
    }

    /// The whole attachment upload: an id from the relay, a signed chunk, and a
    /// commit the caller can address the blob by.
    #[tokio::test]
    async fn a_blob_upload_signs_its_chunks_and_reads_back_the_commit() {
        let relay = relay::start(vec![
            relay::Canned::json(200, r#"{"upload_id":"up-1"}"#),
            relay::Canned::empty(204),
            relay::Canned::json(
                200,
                &format!(
                    r#"{{"blob_id":"blb_{}","size_bytes":9,"chunk_count":1}}"#,
                    "0f".repeat(16)
                ),
            ),
        ])
        .await;
        let mut t = dial(&relay);

        let upload = t
            .blob_init(&[0xabu8; 16], 1, 9)
            .await
            .expect("an upload id");
        assert_eq!(upload, "up-1");
        t.blob_put_chunk(&upload, 0, b"sealed-chunk-bytes")
            .await
            .expect("the chunk lands");
        let commit = t
            .blob_finalize(&upload, &[0x22u8; 32], &[[0x33u8; 32]])
            .await
            .expect("a commit");
        assert_eq!(commit.blob_id, [0x0fu8; 16]);
        assert_eq!(commit.size_bytes, 9);
        assert_eq!(commit.chunk_count, 1);

        let seen = relay.seen();
        assert_eq!(seen[0].path, "/api/v1/blobs/init");
        assert_json_binding(&seen[0]);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&seen[0].body).expect("JSON")
                ["chunk_count"],
            1
        );
        assert_eq!(seen[1].method, "PUT");
        assert_eq!(seen[1].path, "/api/v1/blobs/up-1/0");
        assert_eq!(seen[1].body, b"sealed-chunk-bytes");
        assert_byte_binding(&seen[1]);
        assert_eq!(
            seen[1].header("content-type"),
            Some("application/octet-stream"),
            "a chunk is opaque ciphertext, and says so"
        );
        assert_eq!(seen[2].path, "/api/v1/blobs/finalize");
        assert_json_binding(&seen[2]);
        assert_eq!(
            seen[2].header("content-type"),
            Some("application/json"),
            "a request with a JSON body advertises one"
        );
    }

    /// A commit the relay under-described still names its blob, and reads as
    /// zero rather than as a size nobody sent.
    ///
    /// `size_bytes` and `chunk_count` are read with `unwrap_or(0)`, and
    /// `chunk_count` narrows to `u32` first. All three fallbacks are reachable
    /// from a relay that answers the route and omits or overflows a field, and
    /// none of them was executed: the blob id is the only part of the commit
    /// this build refuses to guess at, which is the distinction the fallbacks
    /// encode.
    #[tokio::test]
    async fn a_commit_with_fields_missing_or_too_large_reads_as_zero() {
        let relay = relay::start(vec![
            relay::Canned::json(200, &format!(r#"{{"blob_id":"blb_{}"}}"#, "0f".repeat(16))),
            relay::Canned::json(
                200,
                &format!(
                    r#"{{"blob_id":"blb_{}","size_bytes":9,"chunk_count":4294967297}}"#,
                    "0f".repeat(16)
                ),
            ),
            relay::Canned::json(200, r#"{"size_bytes":9,"chunk_count":1}"#),
        ])
        .await;
        let mut t = dial(&relay);

        let bare = t
            .blob_finalize("up-1", &[0x22u8; 32], &[[0x33u8; 32]])
            .await
            .expect("a commit naming its blob");
        assert_eq!(bare.blob_id, [0x0fu8; 16]);
        assert_eq!(bare.size_bytes, 0, "a size nobody sent is not invented");
        assert_eq!(bare.chunk_count, 0);

        let wide = t
            .blob_finalize("up-1", &[0x22u8; 32], &[[0x33u8; 32]])
            .await
            .expect("a commit naming its blob");
        assert_eq!(wide.size_bytes, 9);
        assert_eq!(
            wide.chunk_count, 0,
            "a count past u32 is refused, not truncated into a plausible one: \
             the fixture is 2^32 + 1, so a narrowing cast would read 1"
        );

        let err = t
            .blob_finalize("up-1", &[0x22u8; 32], &[[0x33u8; 32]])
            .await
            .expect_err("a commit with no blob id names nothing");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("blob id")),
            "{err}"
        );
    }

    /// A fetch returns the ciphertext, and the relay's single 404 — "no such
    /// blob", "not yours" and "not finished yet" answered alike, so the route
    /// is not an oracle for whether another account holds a ciphertext — is
    /// "not here, try later" rather than a failure.
    #[tokio::test]
    async fn a_blob_fetch_separates_the_bytes_from_the_deliberate_404() {
        let relay = relay::start(vec![
            relay::Canned::bytes(200, b"sealed"),
            relay::Canned::empty(404),
            relay::Canned::json(503, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#),
        ])
        .await;
        let mut t = dial(&relay);
        let blob = [0x0fu8; 16];

        assert_eq!(
            t.blob_fetch(&blob).await.expect("the bytes"),
            Some(b"sealed".to_vec())
        );
        assert_eq!(
            t.blob_fetch(&blob).await.expect("not an error"),
            None,
            "the deliberate 404 is not here, try later"
        );
        let err = t
            .blob_fetch(&blob)
            .await
            .expect_err("an outage is not a 404");
        assert_eq!(code_of(&err), "RELAY_STORAGE_UNAVAILABLE");

        let seen = relay.seen();
        assert_eq!(seen[0].method, "GET");
        assert_eq!(
            seen[0].path,
            format!("/api/v1/blobs/blb_{}", "0f".repeat(16))
        );
        assert!(seen[0].body.is_empty(), "a bodiless GET sends no body");
        assert_eq!(
            seen[0].header("content-type"),
            None,
            "and advertises no media type it is not sending, which is what \
             `content_type: None` on `call_bytes` is for"
        );
        assert_byte_binding(&seen[0]);
    }

    /// Every blob operation reports a refusal rather than a quiet success.
    ///
    /// The chunk `PUT` is the one that matters most: it returns `Ok(())` on
    /// success, so a refusal read as success would leave `finalize` asking the
    /// relay to commit chunks it never received.
    #[tokio::test]
    async fn every_blob_operation_reports_a_refusal_rather_than_a_quiet_success() {
        let refused = relay::Canned::json(503, r#"{"code":"RELAY_STORAGE_UNAVAILABLE"}"#);
        let relay = relay::start(vec![refused.clone(), refused.clone(), refused]).await;
        let mut t = dial(&relay);

        assert_eq!(
            code_of(&t.blob_init(&[1u8; 16], 1, 9).await.expect_err("refused")),
            "RELAY_STORAGE_UNAVAILABLE"
        );
        assert_eq!(
            code_of(
                &t.blob_put_chunk("up-1", 0, b"x")
                    .await
                    .expect_err("refused")
            ),
            "RELAY_STORAGE_UNAVAILABLE"
        );
        assert_eq!(
            code_of(
                &t.blob_finalize("up-1", &[2u8; 32], &[[3u8; 32]])
                    .await
                    .expect_err("refused")
            ),
            "RELAY_STORAGE_UNAVAILABLE"
        );
    }

    /// A relay that answers an upload with no id at all is a malformed
    /// exchange, not an upload under the empty string.
    #[tokio::test]
    async fn an_upload_the_relay_named_nothing_is_a_protocol_error() {
        let relay = relay::start(vec![
            relay::Canned::json(200, "{}"),
            relay::Canned::json(200, r#"{"size_bytes":9}"#),
        ])
        .await;
        let mut t = dial(&relay);

        let err = t
            .blob_init(&[1u8; 16], 1, 9)
            .await
            .expect_err("no upload id");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("upload_id")),
            "{err}"
        );
        let err = t
            .blob_finalize("up-1", &[2u8; 32], &[[3u8; 32]])
            .await
            .expect_err("no blob id");
        assert!(
            matches!(&err, TransportError::Protocol(m) if m.contains("blob id")),
            "{err}"
        );
    }
}
