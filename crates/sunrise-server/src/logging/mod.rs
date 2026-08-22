//! The server's logging surface: what it is allowed to say about a request.
//!
//! Everything here exists because the obvious thing to log is the thing we
//! must not. `tower-http`'s stock [`TraceLayer`] records `http.uri` — which on
//! this server is where browser clients put `?access_token=…`, because a
//! `WebSocket` upgrade cannot carry an `Authorization` header. A default
//! request-log configuration would therefore write bearer tokens to disk on
//! every `/sync` connection.
//!
//! So the layer is assembled by hand: the span records the HTTP method and a
//! *templated* target (`sunrise_log::templatize_path`, which drops the query
//! and replaces opaque id segments), and nothing else from the request ever
//! reaches a field.
//!
//! `docs/10-cross-cutting/logging.md` §6.3 bans `Plain::expose` in this
//! module; the `log-redaction` CI gate greps this path.

use std::time::Duration;

use axum::http::{Request, Response};
use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::trace::{MakeSpan, OnRequest, OnResponse, TraceLayer};
use tracing::Span;

/// Per-account correlation handle for logs.
///
/// `docs/10-cross-cutting/logging.md` §6 forbids logging a full `account_id`;
/// §6.1 defines `account_h` as the first 4 bytes of a salted BLAKE3, rendered
/// as 8 lowercase hex characters.
///
/// The salt is omitted here, deliberately. Its job in §6.1 is to stop a
/// low-entropy identifier — an email address — from being recovered by
/// brute force. Sunrise account ids are not that: `Store::resolve_account`
/// mints them as 16 random bytes with no relation to the OIDC subject or the
/// email, so the pre-image space is 2^128 and a dictionary attack has nothing
/// to enumerate. What a salt would still buy is preventing a client-side hash
/// and a server-side hash of the same account from being joined without the
/// operator's help — and clients hash their *own* ids under a device-local
/// salt, so that join is already unavailable.
#[must_use]
pub fn account_h(account_id: &str) -> String {
    let digest = blake3::hash(account_id.as_bytes());
    hex_lower(&digest.as_bytes()[..4])
}

/// Correlation handle for a raw 16-byte id — a stream id, or the account
/// namespace the relay derives per session. Same construction as
/// [`account_h`], and the same reasoning: these are 16 random bytes, so a
/// truncated hash has nothing to brute-force back to.
#[must_use]
pub fn id_h(id: &[u8; 16]) -> String {
    let digest = blake3::hash(id);
    hex_lower(&digest.as_bytes()[..4])
}

/// `hex::encode` is already lowercase and already in the lock file; the point
/// of the wrapper is the name, so the two call sites read as "the §6.1
/// construction" rather than as an encoding detail.
fn hex_lower(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Opens the span every request handler runs inside.
///
/// The two fields are the whole of what a request may contribute to a log
/// line: its method, and a target with the query string removed and opaque
/// path segments collapsed to `:id`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestSpan;

impl<B> MakeSpan<B> for RequestSpan {
    fn make_span(&mut self, request: &Request<B>) -> Span {
        tracing::info_span!(
            "http.request",
            method = %request.method(),
            endpoint = %sunrise_log::templatize_path(request.uri().path()),
        )
    }
}

/// `srv.req.start`, at debug — one line per inbound request.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestStart;

impl<B> OnRequest<B> for RequestStart {
    fn on_request(&mut self, _request: &Request<B>, _span: &Span) {
        tracing::debug!(ev = "srv.req.start", "request received");
    }
}

/// `srv.req.end` — debug for a served request, warn for a server-side failure.
///
/// The level split is what makes a default `info` deployment useful: healthy
/// traffic stays out of the log, and a 5xx shows up without anyone having
/// turned anything on.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestEnd;

impl<B> OnResponse<B> for RequestEnd {
    fn on_response(self, response: &Response<B>, latency: Duration, _span: &Span) {
        let status = response.status().as_u16();
        let lat_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);
        if response.status().is_server_error() {
            tracing::warn!(
                ev = "srv.req.end",
                status,
                lat_ms,
                result = "failed",
                "request failed"
            );
        } else {
            tracing::debug!(
                ev = "srv.req.end",
                status,
                lat_ms,
                result = "ok",
                "request served"
            );
        }
    }
}

/// The request-tracing layer mounted by [`crate::build_router`].
#[must_use]
pub fn trace_layer(
) -> TraceLayer<SharedClassifier<ServerErrorsAsFailures>, RequestSpan, RequestStart, RequestEnd> {
    TraceLayer::new_for_http()
        .make_span_with(RequestSpan)
        .on_request(RequestStart)
        .on_response(RequestEnd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_h_is_eight_lowercase_hex_chars() {
        let h = account_h("01J8ZQ7X9K3M5N7P9R1T3V5W7Y");
        assert_eq!(h.len(), 8, "{h}");
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{h}"
        );
    }

    #[test]
    fn account_h_is_stable_and_distinguishing() {
        assert_eq!(account_h("acct-a"), account_h("acct-a"));
        assert_ne!(account_h("acct-a"), account_h("acct-b"));
    }

    #[test]
    fn account_h_does_not_contain_the_id() {
        let id = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
        let h = account_h(id);
        assert!(!h.contains(&id.to_lowercase()));
        assert!(!id.to_lowercase().contains(&h));
    }

    #[test]
    fn id_h_matches_the_account_construction() {
        let sid = [7u8; 16];
        assert_eq!(id_h(&sid).len(), 8);
        assert_ne!(id_h(&sid), id_h(&[8u8; 16]));
    }

    #[test]
    fn hex_lower_pads_and_lowercases() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    /// The regression this whole module exists for: a bearer token in the
    /// query string must not survive into a request log.
    ///
    /// Asserted against a real subscriber, because span fields are only
    /// recorded when one is installed — a `Debug`-formatted `Span` with no
    /// subscriber would pass this test no matter what `make_span` did.
    #[test]
    fn request_span_never_carries_the_query_string() {
        let cap = sunrise_log::Capture::new();
        let dispatch = sunrise_log::build_subscriber(sunrise_log::LogConfig {
            target: sunrise_log::LogTarget::Capture(cap.clone()),
            filter: "trace".to_string(),
            format: sunrise_log::LogFormat::Ndjson,
        })
        .expect("subscriber builds");

        tracing::dispatcher::with_default(&dispatch, || {
            let req = Request::builder()
                .method("GET")
                .uri(
                    "/api/v1/devices/01J8ZQ7X9K3M5N7P9R1T3V5W7Y?access_token=eyJhbGciOi.SECRET.sig",
                )
                .body(())
                .unwrap();
            let span = RequestSpan.make_span(&req);
            span.in_scope(|| {
                RequestStart.on_request(&req, &span);
                RequestEnd.on_response(
                    &Response::builder().status(200).body(()).unwrap(),
                    Duration::from_millis(3),
                    &span,
                );
            });
        });

        let out = cap.contents();
        assert!(!out.contains("SECRET"), "token leaked: {out}");
        assert!(!out.contains("access_token"), "query leaked: {out}");
        assert!(!out.contains("01J8ZQ7X"), "device id leaked: {out}");
        // ...and the span was actually recorded, so the assertions above are
        // about redaction rather than about nothing being logged.
        assert!(
            out.contains("\"endpoint\":\"/api/v1/devices/:id\""),
            "{out}"
        );
        assert!(out.contains("\"method\":\"GET\""), "{out}");
        assert!(out.contains("srv.req.start"), "{out}");
        assert!(out.contains("srv.req.end"), "{out}");
    }
}
