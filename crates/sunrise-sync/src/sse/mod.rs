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
//! supplies one, and so is every other request this module makes. The route
//! families reached from here are:
//!
//! - `/sync`: the four `POST`s above and the `GET /sync/events` that opens the
//!   stream;
//! - `/devices/by-vault-id/{id}`: the revocation `DELETE`;
//! - `/blobs`: upload init, chunk `PUT`, finalize, and the blob `GET`;
//! - `/snapshots/{stream}`: `GET` and `PUT`;
//! - `/accounts/me`: the key bundle and identity chain (`GET` and `PUT`) and
//!   the recovery blob `PUT`.
//!
//! The relay verifies each binding through `sunrise_server::api::signed`; grep
//! this directory for `/api/v1/` for the exact routes rather than trusting a count
//! here. The two bootstrap routes are not reached from here:
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
//!
//! # Where each concern lives
//!
//! - `http`: the bounds, the device binding, and one request and its reply;
//! - `events`: opening the stream and turning its events into frames;
//! - `session`: the [`Transport`] impl, the frame-to-operation table above, and
//!   the read loop;
//! - `account`: revocation, the key bundle, the identity chain and the recovery
//!   blob;
//! - `blobs`: attachment transfer and the snapshot pin.
//!
//! [`Transport`]: crate::transport::Transport

mod account;
mod blobs;
mod events;
mod http;
mod session;
#[cfg(test)]
mod tests;

use crate::signer::DeviceSigner;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::collections::VecDeque;
use std::sync::Arc;

// What `tests` reaches through `super::`, kept in scope here so the test module
// is the one the single file carried.
#[cfg(test)]
use self::events::parse_id;
#[cfg(test)]
use self::http::{
    await_head, binding_advice, field_str, field_u32, field_u64, read_body, server_date, Reply,
    BLOB_IDLE_TIMEOUT, BODY_IDLE_TIMEOUT, CHUNK_HEAD_TIMEOUT, HEAD_TIMEOUT, MAX_BLOB_BYTES,
    MAX_EVENT_BYTES,
};
#[cfg(test)]
use self::session::cursor_json;
#[cfg(test)]
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, BacklogPayload, CaughtUpPayload, ClosePayload,
    CursorEntry, ErrorPayload, FrameFlags, Hello, HelloAck, MsgKind, OpBatchPayload,
    RefreshTokenAckPayload, RefreshTokenPayload, SubscribePayload,
};

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
    /// driver sends, and that is where its failure is reported.
    ///
    /// Because nothing fails here, every `TransportFactory` closure in this
    /// workspace wraps this constructor in `Ok(…)`, and a connect that resolves
    /// `Ok` says nothing about the relay. `sunrise_core::sync_driver`'s `run`
    /// therefore resets its reconnect backoff only once the relay has answered
    /// the `Hello` — `Backoff::reset`'s own precondition, "call after a
    /// successful operation" — so a relay that refuses it is retried on the
    /// published schedule rather than on its first step forever (#283).
    ///
    /// The client built here carries no timeout of its own, because the pinned
    /// `hyper-util` legacy client has none to set; every bound this transport
    /// has is stated per request instead — `HEAD_TIMEOUT` and
    /// `CHUNK_HEAD_TIMEOUT` on the head, and `BODY_IDLE_TIMEOUT` and
    /// `BLOB_IDLE_TIMEOUT` on the body.
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
}
