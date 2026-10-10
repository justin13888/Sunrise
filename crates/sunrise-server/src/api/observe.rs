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
use kynos::middleware::{Continued, Interceptor, Next, Observer};
use kynos::router::operation::Route;
use std::time::Duration;
use sunrise_telemetry::{Attr, FutureExt as _};
use tracing::{Instrument as _, Span};

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
pub(crate) fn templated(path: &str) -> String {
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

/// Opens the root span of each traced operation and runs the rest of the
/// request inside it (`docs/06-server/observability.md` §Tracing).
///
/// An interceptor rather than an observer, because a root span has to be
/// *current* while the handler runs for the verify, store, relay and blob
/// spans beneath it to find their parent, and only an interceptor wraps the
/// handler's future. It declares nothing — it reads no header it extracts,
/// adds none, and always continues — so mounting it leaves the published
/// description unchanged.
///
/// The span is named `"{method} {endpoint}"`, where `endpoint` is the matched
/// route's template in the log's `:id` spelling, so a span and a
/// `srv.req.*` record of the same request carry the same value under the same
/// key. The request is read for its `traceparent` and nothing else; its URI
/// is never consulted, the same rule [`RequestLog`] keeps.
///
/// While a sampled span is open the handler also runs inside an
/// `http.request` log span carrying `trace_id` and `span_id`, so every record
/// written during the request joins the trace it belongs to. Without
/// `[observability]`, or for a request the sampler dropped, there is no
/// telemetry span and no log span, and the request runs exactly as it would
/// with this interceptor absent.
#[derive(Debug, Clone, Copy, Default)]
pub struct TraceRequest;

impl Interceptor<ServerState> for TraceRequest {
    type Reads = ();
    type Adds = ();
    type Short = std::convert::Infallible;

    async fn intercept(
        &self,
        request: Request,
        reads: (),
        state: &ServerState,
        next: Next<'_, ServerState>,
    ) -> Result<Continued<()>, std::convert::Infallible> {
        let () = reads;
        let route = next.route();
        let method = route.method().as_wire_str();
        let endpoint = templated(route.path());
        let root = state
            .telemetry
            .server_span(method, &endpoint, request.headers());
        let Some(ids) = root.ids() else {
            return Ok(next.run(request).await);
        };
        let log = tracing::info_span!(
            "http.request",
            method = %method,
            endpoint = %endpoint,
            trace_id = %ids.trace_id,
            span_id = %ids.span_id,
        );
        let continued = next
            .run(request)
            .with_context(root.context())
            .instrument(log)
            .await;
        root.set(Attr::status(continued.status().as_u16()));
        if continued.status().is_server_error() {
            root.fail("server error");
        }
        Ok(continued)
    }
}

/// Holds a request's place in `sunrise_http_in_flight_requests` and meters
/// its time in the store into `sunrise_db_query_duration_seconds{endpoint}`.
///
/// An interceptor, because both facts belong to the request's *future*, which
/// only an interceptor wraps. The in-flight guard lives in that future, so it is
/// released however the future ends: the response, a panic unwinding, or a
/// client that left while the handler ran and had the future dropped — the
/// case no [`Observer`] hook reports, which is why the count could not be kept
/// there. The store time is every `Store` operation the handler took, lock
/// waits included, summed per request; a request that took none is not
/// observed, so the histogram reads as "requests that touched the store".
///
/// `endpoint` is the matched route's template in the description's `{param}`
/// spelling, the value `sunrise_http_requests_total` carries. Declares nothing,
/// so the published description is unchanged.
///
/// For the SSE `events` operation both end with the response head, as the
/// HTTP duration does: an open stream is `sunrise_sync_streams_active`, and
/// its replay reads are its own task's, not the request's.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestMeter;

impl Interceptor<ServerState> for RequestMeter {
    type Reads = ();
    type Adds = ();
    type Short = std::convert::Infallible;

    async fn intercept(
        &self,
        request: Request,
        reads: (),
        state: &ServerState,
        next: Next<'_, ServerState>,
    ) -> Result<Continued<()>, std::convert::Infallible> {
        let () = reads;
        let endpoint = next.route().path();
        let _in_flight = state.in_flight.enter();
        let (continued, time) = crate::store::metered(next.run(request)).await;
        if let Some(spent) = time.spent() {
            state.metrics.observe(
                "sunrise_db_query_duration_seconds",
                &[("endpoint", endpoint)],
                crate::metrics::LATENCY_BUCKETS,
                spent.as_secs_f64(),
            );
        }
        Ok(continued)
    }
}

/// Records `sunrise_http_requests_total` and
/// `sunrise_http_request_duration_seconds`.
///
/// The labels come from the same place `endpoint` does in the log: the matched
/// operation's `paths` key, in the description's own `{param}` spelling
/// (`docs/06-server/metrics.md` §Label allowlist), its method, and the status
/// actually returned. The request's URI is never read, so no id can reach a
/// label, and every value is drawn from the route table or the status codes
/// the surface returns.
///
/// It holds the registry rather than reading it off the context because
/// [`Observer::on_response`] is handed no context: the status is only known
/// there, and the context only in `on_request`.
///
/// Duration is kynos's `elapsed`: time to the response head. For a buffered
/// response that is the whole request; for the SSE `events` stream it is time
/// to the first byte, which is what `metrics.md` documents.
#[derive(Debug, Clone)]
pub struct HttpMetrics {
    metrics: crate::Metrics,
}

impl HttpMetrics {
    /// Record into `metrics`.
    #[must_use]
    pub const fn new(metrics: crate::Metrics) -> Self {
        Self { metrics }
    }
}

impl Observer<ServerState> for HttpMetrics {
    fn on_request(&self, _request: &Request, _route: Option<Route<'_>>, _context: &ServerState) {}

    fn on_response(&self, response: &Response, route: Option<Route<'_>>, elapsed: Duration) {
        let (method, endpoint) =
            route.map_or(("-", UNMATCHED), |r| (r.method().as_wire_str(), r.path()));
        self.metrics.incr_with(
            "sunrise_http_requests_total",
            &[
                ("endpoint", endpoint),
                ("method", method),
                ("status", response.status().as_str()),
            ],
        );
        self.metrics.observe(
            "sunrise_http_request_duration_seconds",
            &[("endpoint", endpoint), ("method", method)],
            crate::metrics::LATENCY_BUCKETS,
            elapsed.as_secs_f64(),
        );
    }
}

#[cfg(test)]
#[path = "observe_trace_tests.rs"]
mod trace_tests;

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
    ///
    /// # Never returns an empty capture
    ///
    /// A test here asserts on the *contents* of a record, so a capture that
    /// received nothing at all fails every one of them — with the offending
    /// `{out}` rendering as nothing, which is a panic message that says
    /// nothing. That is exactly how #116 was reported: two different tests in
    /// this module, twice, under a loaded whole-workspace run, each panicking
    /// with a blank message because `out` was `""`.
    ///
    /// The cause was `tracing`'s global per-callsite interest cache being
    /// computed against a thread that had no subscriber — see
    /// `sunrise_log::init`'s `pin_interest_cache`, which closes it. The guard
    /// stays regardless, and lives here rather than in each test so that a
    /// test added later cannot omit it: "nothing was logged" and "the wrong
    /// thing was logged" are different failures and must not share a message.
    #[track_caller]
    fn run(method: Method, uri: &str) -> String {
        let named = method.to_string();
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
        let out = cap.contents();
        assert!(
            !out.is_empty(),
            "the capture received no records at all for {named} {uri}; \
             every assertion below is about which records arrived, so this is \
             a broken subscriber rather than a broken server"
        );
        out
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
