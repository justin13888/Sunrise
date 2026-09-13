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

mod credential;
mod cursors;
mod publish;
mod stream;
#[cfg(test)]
mod suite;

pub use credential::{
    refresh, session, RefreshRequest, RefreshResponse, SessionHeader, SessionRequest,
    SessionResponse,
};
pub use cursors::{subscribe, DeviceCursor, StreamSubscription, SubscribeRequest};
pub use publish::{ops, OpsRequest, OpsResponse};
pub use stream::{events, EventStream, SyncEvent};
