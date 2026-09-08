//! What the typed surface is allowed to say about a request.
//!
//! Everything here exists because the obvious thing to log is the thing we must
//! not. A stock request log records the URI, and the query string is where a
//! bearer reached this server's log once already — browser clients put
//! `?access_token=` there, back when a `WebSocket` upgrade could not carry an
//! `Authorization` header. That is why `crate::logging`'s span was
//! hand-assembled rather than configured. No route reads the parameter today
//! (`docs/05-sync/wire-protocol.md` records it as reserved), and the hazard
//! outlives the transport: a browser `EventSource` cannot set headers either.
//!
//! kynos narrows the hazard rather than mitigating it after the fact. An
//! [`Observer`] is handed the matched [`Route`], whose `path()` is "the `paths`
//! key this request matched, exactly as the description spells it — with its
//! `{}` expressions intact, never the request's own path", and that key is what
//! `endpoint` is built from. There is no raw path in the computation at all,
//! which is a stronger property than the scrubbing step it replaced
//! (`sunrise_log::templatize_path`, which nothing on this route calls).
//!
//! Read "the concrete URI is never consulted" precisely, though. It is
//! *structural* only for `on_response`, `on_disconnect` and `on_panic`, which
//! are handed no request at all. `on_request` **is** handed one — it reads
//! `request.method()` off the same value — so what keeps its `.uri()` unread is
//! the tests, not the signature. `the_query_string_never_reaches_the_log` below
//! and `a_bearer_in_the_query_string_and_in_the_header_both_stay_out_of_the_log`
//! in `tests/logging.rs` each drive a real `?access_token=` through and assert
//! the sentinel never appears; neither may be dropped on the grounds that the
//! leak is impossible by construction. `docs/06-server/observability.md` records
//! the same distinction.
//!
//! What is kept from the surface this replaces is the *record shape*.
//! `docs/10-cross-cutting/log-events.md` catalogues `srv.req.start` and
//! `srv.req.end`, `sunrise-log`'s allowlist vets their field names, and an
//! ingest pipeline parses them. kynos's own `Trace` observer emits a different
//! shape, so this is written rather than mounted: the guarantee is inherited,
//! the contract is preserved.
//!
//! `docs/10-cross-cutting/logging.md` §6.3 bans `Plain::expose` in this module;
//! the `log-redaction` CI gate greps this path.

use crate::state::ServerState;
use kynos::http::{Request, Response};
use kynos::middleware::Observer;
use kynos::router::operation::Route;
use std::time::Duration;
use tracing::Span;

/// The `endpoint` recorded for a request that matched no operation.
///
/// A 404 has no `paths` key to name, and inventing one from the URI is exactly
/// the read this module exists to avoid.
const UNMATCHED: &str = "unmatched";

/// Rewrite a description path template into the log's `:id` spelling.
///
/// `docs/10-cross-cutting/log-events.md` records `endpoint` as a templated
/// target — `/api/v1/devices/:id` — and kynos spells the same template
/// `/api/v1/devices/{device_id}`. Translating keeps one endpoint spelling in
/// the logs across the port, so a dashboard grouping by it does not split into
/// two series on the day the server changed.
fn templated(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut in_param = false;
    for ch in path.chars() {
        match ch {
            '{' => {
                in_param = true;
                out.push_str(":id");
            }
            '}' => in_param = false,
            _ if in_param => {}
            _ => out.push(ch),
        }
    }
    out
}

/// The span both records carry their identifying fields in.
///
/// A span rather than event fields because that is the shape already on the
/// wire: `sunrise-log` renders span fields under `"span"`, and
/// `request_records_carry_status_and_latency` reads
/// `record["span"]["endpoint"]`.
fn span_for(method: &str, endpoint: &str) -> Span {
    tracing::info_span!("http.request", method = %method, endpoint = %endpoint)
}

/// Emits `srv.req.start` and `srv.req.end`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestLog;

impl Observer<ServerState> for RequestLog {
    fn on_request(&self, request: &Request, route: Option<Route<'_>>, _context: &ServerState) {
        let endpoint = route.map_or_else(|| UNMATCHED.to_owned(), |r| templated(r.path()));
        span_for(request.method().as_str(), &endpoint)
            .in_scope(|| tracing::debug!(ev = "srv.req.start", "request received"));
    }

    /// `srv.req.end` — debug for a served request, warn for a server-side
    /// failure.
    ///
    /// The level split is what makes a default `info` deployment useful:
    /// healthy traffic stays out of the log, and a 5xx shows up without anyone
    /// having turned anything on.
    fn on_response(&self, response: &Response, route: Option<Route<'_>>, elapsed: Duration) {
        let (method, endpoint) = route.map_or_else(
            || ("-".to_owned(), UNMATCHED.to_owned()),
            |r| (r.method().as_wire_str().to_owned(), templated(r.path())),
        );
        let status = response.status().as_u16();
        let lat_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        span_for(&method, &endpoint).in_scope(|| {
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
        });
    }
}

#[cfg(test)]
mod tests {
    use super::templated;
    use crate::api::testing::Client;
    use crate::ServerConfig;
    use kynos::http::Method;
    use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};

    /// A bearer-shaped string that must never appear in a log line.
    const TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.SENTINELTOKENPAYLOAD.sig";

    /// The one thing a template rewrite must not do is invent a path segment.
    #[test]
    fn a_template_becomes_the_logs_endpoint_spelling() {
        assert_eq!(templated("/api/v1/health"), "/api/v1/health");
        assert_eq!(
            templated("/api/v1/devices/{device_id}"),
            "/api/v1/devices/:id"
        );
        assert_eq!(
            templated("/api/v1/blobs/{upload_id}/{chunk_idx}"),
            "/api/v1/blobs/:id/:id"
        );
    }

    /// Drive one request through the typed surface with logging captured.
    ///
    /// A plain `#[test]` with its own runtime rather than `#[tokio::test]`,
    /// because installing a dispatcher is synchronous and has to wrap the whole
    /// request rather than sit inside it.
    fn run(method: Method, uri: &str) -> String {
        let cap = Capture::new();
        let dispatch = build_subscriber(LogConfig {
            target: LogTarget::Capture(cap.clone()),
            // `debug` so `srv.req.start` / `srv.req.end` are in scope; they are
            // debug-level by catalogue, which is what keeps a default `info`
            // deployment quiet.
            filter: "debug".to_owned(),
            format: LogFormat::Ndjson,
        })
        .expect("subscriber builds");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        tracing::dispatcher::with_default(&dispatch, || {
            rt.block_on(async {
                let client = Client::new(ServerConfig::default());
                let _ = client.send(method, uri, None).await;
            });
        });
        cap.contents()
    }

    /// The hazard this module exists for.
    ///
    /// A `?access_token=` query is where a browser client puts its bearer, and
    /// a stock request log records the URI. Here the URI is never read: the
    /// endpoint comes from the matched operation's `paths` key, so there is no
    /// redaction step to get wrong.
    #[test]
    fn the_query_string_never_reaches_the_log() {
        let out = run(Method::GET, &format!("/api/v1/meta?access_token={TOKEN}"));
        assert!(!out.is_empty(), "nothing was logged at all");
        assert!(!out.contains("SENTINELTOKENPAYLOAD"), "token leaked: {out}");
        assert!(!out.contains("access_token"), "query leaked: {out}");
        // The request *was* logged, so the absence above is redaction rather
        // than silence.
        assert!(out.contains("srv.req.end"), "no request record: {out}");
        assert!(out.contains("\"endpoint\":\"/api/v1/meta\""), "{out}");
    }

    /// An opaque id in a path is a correlation handle nobody asked for, so the
    /// log records the template rather than the segment.
    #[test]
    fn an_opaque_path_segment_is_templated() {
        let out = run(Method::DELETE, "/api/v1/devices/01J8ZQ7X9K3M5N7P9R1T3V5W7Y");
        assert!(
            !out.contains("01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
            "full device id leaked: {out}"
        );
        assert!(
            out.contains("\"endpoint\":\"/api/v1/devices/:id\""),
            "expected a templated endpoint: {out}"
        );
    }

    /// The record shape `docs/10-cross-cutting/log-events.md` catalogues, which
    /// an ingest pipeline parses.
    #[test]
    fn request_records_carry_status_and_latency() {
        let out = run(Method::GET, "/api/v1/health");
        let line = out
            .lines()
            .find(|l| l.contains("srv.req.end"))
            .unwrap_or_else(|| panic!("no srv.req.end record in {out}"));
        let record: serde_json::Value = serde_json::from_str(line).expect("record is JSON");
        assert_eq!(record["ev"], "srv.req.end");
        assert_eq!(record["status"], 200);
        assert_eq!(record["result"], "ok");
        assert!(record["lat_ms"].is_u64(), "{record}");
        assert_eq!(record["span"]["method"], "GET");
        assert_eq!(record["span"]["endpoint"], "/api/v1/health");
    }

    /// Every field the server emits must be on `sunrise-log`'s allowlist.
    #[test]
    fn every_server_field_survives_the_redaction_allowlist() {
        let out = run(Method::GET, "/api/v1/meta");
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let record: serde_json::Value = serde_json::from_str(line).expect("record is JSON");
            let obj = record.as_object().expect("object");
            for key in obj.keys() {
                assert!(
                    matches!(key.as_str(), "timestamp" | "level" | "target" | "span")
                        || sunrise_log::is_allowed(key),
                    "field {key:?} is not allowlisted: {line}"
                );
            }
        }
        assert!(out.contains("srv.req.start"), "{out}");
        assert!(out.contains("srv.req.end"), "{out}");
    }
}
