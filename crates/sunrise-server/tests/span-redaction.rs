//! No exported span can carry a credential, an identifier or a path, and the
//! spans that are exported have the shape `observability.md` §Tracing states.
//!
//! The gate `docs/06-server/observability.md` §Tracing names. It drives the
//! public assembly — [`sunrise_server::build_service`] over a [`ServerState`]
//! given telemetry with [`ServerState::with_telemetry`], the seam the binary
//! uses — through an in-memory exporter sampling every trace, and holds:
//!
//! 1. **Redaction.** Every operation in the published description, with a
//!    sentinel bearer, a sentinel `?access_token=`, a sentinel session id, a
//!    sentinel device id and signature, and a sentinel email in the body, plus
//!    a real session whose id is a sentinel too. No sentinel appears anywhere
//!    in any exported span — its name, attributes, events, status, links or
//!    trace state — because the whole `Debug` rendering of each span is
//!    searched rather than the fields this test thought to name. Every
//!    attribute key is on the allowlist, and every span name is a root's
//!    `"{METHOD} {template}"` or a literal from the closed set below.
//! 2. **The span tree** of one `POST /sync/ops` with two subscribers.
//! 3. **Sampling**: the ratio holds, and a client's `traceparent` can lower it
//!    but not raise it.
//! 4. **Off by default**, and **log correlation** when on.
//!
//! Driving every route rather than a chosen few is the point, as it is in
//! `metric-label-safety.rs`: a route added later is in the description, so it
//! is driven here without anyone remembering to add it.

use http_body_util::BodyExt as _;
use kynos::http::body::Body;
use kynos::http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use kynos::router::service::Service;
use std::collections::BTreeSet;
use std::time::Duration;
use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};
use sunrise_server::telemetry::testing::{recording, InMemorySpanExporter, SpanData};
use sunrise_server::telemetry::{attr::KEYS, Telemetry};
use sunrise_server::{ServerConfig, ServerState};

/// Each sentinel is distinct, so a failure names which one leaked.
const BEARER: &str = "eyJhbGciOiJIUzI1NiJ9.SENTINELBEARERHEADER.sig";
const QUERY_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.SENTINELQUERYTOKEN.sig";
const SESSION: &str = "SENTINELSESSIONID0123456789abcdef";
const DEVICE: &str = "01SENTINELDEVICE00000000000";
const SIGNATURE: &str = "SENTINELSIGNATUREc2lnbmF0dXJl";
const EMAIL: &str = "sentinel.person@example.invalid";
/// What goes in each path parameter.
const PATH_ID: &str = "0123456789abcdef0123456789abcdef";

/// Every child span name the server can produce. A root is
/// `"{METHOD} {template}"` instead.
const CHILD_NAMES: &[&str] = &[
    "auth.verify_token",
    "auth.verify_signature",
    "relay.append",
    "relay.fanout",
    "blob.chunk_read",
    "blob.chunk_write",
    "sync.stream",
    "push.dispatch",
    "push.attempt",
];

/// The public surface over `state`.
struct Server {
    service: Service<ServerState>,
}

impl Server {
    fn new(state: ServerState) -> Self {
        Self {
            service: sunrise_server::build_service(state).expect("the typed surface builds"),
        }
    }

    /// Drive one request and hand back the response unread, for a stream.
    async fn call(
        &self,
        method: Method,
        target: &str,
        body: Option<&serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> Response {
        let encoded = body.map(|v| serde_json::to_vec(v).expect("serializable"));
        let mut request = Request::new(match &encoded {
            Some(bytes) => Body::from_bytes(bytes.clone().into()),
            None => Body::empty(),
        });
        *request.method_mut() = method;
        *request.uri_mut() = target.parse().expect("a well-formed target");
        if encoded.is_some() {
            request.headers_mut().insert(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            );
        }
        for (name, value) in headers {
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                HeaderValue::from_str(value).expect("a header value"),
            );
        }
        self.service.call(request).await
    }

    /// Drive one request and collect its body, giving up on a body that is
    /// still open after a second — an event stream that was let through.
    async fn send(
        &self,
        method: Method,
        target: &str,
        body: Option<&serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Vec<u8>) {
        let response = self.call(method, target, body, headers).await;
        let status = response.status();
        let bytes = tokio::time::timeout(Duration::from_secs(1), response.into_body().collect())
            .await
            .ok()
            .and_then(Result::ok)
            .map(|c| c.to_bytes().to_vec())
            .unwrap_or_default();
        (status, bytes)
    }
}

fn traced(ratio: f64) -> (Server, InMemorySpanExporter) {
    let (telemetry, exporter) = recording(ratio);
    let state = ServerState::new(ServerConfig::default()).with_telemetry(telemetry);
    (Server::new(state), exporter)
}

fn finished(exporter: &InMemorySpanExporter) -> Vec<SpanData> {
    exporter.get_finished_spans().expect("the recorder answers")
}

/// Every `(method, path template)` the description publishes.
fn operations() -> Vec<(Method, String)> {
    let doc = sunrise_server::api::document().expect("the router describes");
    let doc: serde_json::Value =
        serde_json::from_str(&doc.to_json().expect("serializes")).expect("is JSON");
    let mut out = Vec::new();
    for (path, item) in doc["paths"].as_object().expect("a paths object") {
        for verb in item.as_object().expect("a path item").keys() {
            let method = match verb.as_str() {
                "get" => Method::GET,
                "post" => Method::POST,
                "put" => Method::PUT,
                "delete" => Method::DELETE,
                "patch" => Method::PATCH,
                _ => continue,
            };
            out.push((method, path.clone()));
        }
    }
    assert!(out.len() > 10, "too few operations: {out:?}");
    out
}

/// `template` with every `{param}` replaced by `value`.
fn fill(template: &str, value: &str) -> String {
    let mut out = String::new();
    let mut in_param = false;
    for ch in template.chars() {
        match ch {
            '{' => {
                in_param = true;
                out.push_str(value);
            }
            '}' => in_param = false,
            _ if in_param => {}
            c => out.push(c),
        }
    }
    out
}

/// The `Hello` a session opens with.
fn hello() -> serde_json::Value {
    use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
    serde_json::json!({
        "client_app_v": "0.1.0",
        "client_platform": "test",
        "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
        "doc_schema_min": u32::from(DOC_SCHEMA_V),
        "doc_schema_max": u32::from(DOC_SCHEMA_V),
        "crypto_suite_supported": [u32::from(CRYPTO_SUITE_V)],
        "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0,
        "trace": "01J000000000000000000000000",
    })
}

const STREAM: &str = "11111111111111111111111111111111";

/// Open a session and subscribe it to [`STREAM`], returning its id.
async fn subscribed_session(server: &Server) -> String {
    let (status, body) = server
        .send(
            Method::POST,
            "/api/v1/sync/session",
            Some(&hello()),
            &[("authorization", &format!("Bearer {BEARER}"))],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let session: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let id = session["session_id"].as_str().expect("an id").to_owned();
    let (status, body) = server
        .send(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(&serde_json::json!({ "streams": [{ "stream_id": STREAM, "cursors": [] }] })),
            &[
                ("authorization", &format!("Bearer {BEARER}")),
                ("x-sunrise-session", &id),
            ],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    id
}

/// Open `session`'s event stream and read it until the replay is over, so its
/// live subscription is certainly in place. The body is returned unfinished:
/// dropping it is what ends the subscriber.
async fn open_stream(server: &Server, session: &str) -> Body {
    let response = server
        .call(
            Method::GET,
            "/api/v1/sync/events",
            None,
            &[
                ("authorization", &format!("Bearer {BEARER}")),
                ("x-sunrise-session", session),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut seen = String::new();
    while !seen.contains("caught_up") {
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("the replay ends promptly")
            .expect("the stream is open")
            .expect("a frame");
        if let Ok(data) = frame.into_data() {
            seen.push_str(&String::from_utf8_lossy(&data));
        }
    }
    body
}

/// The whole of a span, as text: name, attributes, events, status, links,
/// trace state, ids and scope, without this test choosing which.
fn rendered(span: &SpanData) -> String {
    format!("{span:?}")
}

/// 1. Nothing a request carried becomes span data.
#[tokio::test]
async fn no_credential_identifier_or_path_reaches_a_span() {
    let (server, exporter) = traced(1.0);
    // A real session, whose id is as much a credential as the bearer.
    let real_session = subscribed_session(&server).await;

    let ops = operations();
    for (method, template) in &ops {
        let target = format!("{}?access_token={QUERY_TOKEN}", fill(template, PATH_ID));
        let body = serde_json::json!({ "email": EMAIL, "stream_id": STREAM });
        let has_body = matches!(*method, Method::POST | Method::PUT | Method::PATCH);
        for session in [SESSION, real_session.as_str()] {
            server
                .send(
                    method.clone(),
                    &target,
                    has_body.then_some(&body),
                    &[
                        ("authorization", &format!("Bearer {BEARER}")),
                        ("x-sunrise-session", session),
                        ("x-sunrise-device", DEVICE),
                        ("x-sunrise-device-sig", SIGNATURE),
                        ("x-sunrise-date", "Thu, 01 Jan 2026 00:00:00 GMT"),
                        ("from", EMAIL),
                    ],
                )
                .await;
        }
    }

    let spans = finished(&exporter);
    let roots: BTreeSet<String> = spans
        .iter()
        .filter(|s| s.parent_span_id == opentelemetry_invalid_parent())
        .map(|s| s.name.to_string())
        .collect();
    for (method, template) in &ops {
        let name = format!("{method} {}", logged_template(template));
        assert!(roots.contains(&name), "{name} was never traced: {roots:?}");
    }

    let sentinels = [
        "SENTINELBEARERHEADER",
        "SENTINELQUERYTOKEN",
        "access_token",
        SESSION,
        real_session.as_str(),
        DEVICE,
        SIGNATURE,
        EMAIL,
        PATH_ID,
    ];
    for span in &spans {
        let text = rendered(span);
        for sentinel in sentinels {
            assert!(
                !text.contains(sentinel),
                "{sentinel:?} reached span {:?}: {text}",
                span.name
            );
        }
        for kv in &span.attributes {
            assert!(
                KEYS.contains(&kv.key.as_str()),
                "attribute {:?} on {:?} is off the allowlist",
                kv.key.as_str(),
                span.name
            );
        }
        for event in span.events.iter() {
            for kv in &event.attributes {
                assert!(KEYS.contains(&kv.key.as_str()), "{:?}", kv.key.as_str());
            }
        }
        let name = span.name.as_ref();
        assert!(
            roots.contains(name) || CHILD_NAMES.contains(&name) || name.starts_with("store."),
            "span name {name:?} is neither a route nor a known literal"
        );
        if let Some(op) = name.strip_prefix("store.") {
            assert!(
                op.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "a store span names an operation, never a value: {name:?}"
            );
        }
    }
}

/// The parent id of a root span.
fn opentelemetry_invalid_parent() -> sunrise_server::telemetry::testing::SpanId {
    sunrise_server::telemetry::testing::SpanId::INVALID
}

/// A description template in the log's `:id` spelling.
fn logged_template(template: &str) -> String {
    fill(template, "\u{0}").replace('\u{0}', ":id")
}

/// 2. `POST /sync/ops` with two live subscribers: the request's root, then
/// verification, then the store, then the append with its transaction beneath
/// it, then the fan-out that reached both subscribers.
#[tokio::test]
async fn a_publish_traces_verify_store_append_and_fanout_in_order() {
    let (server, exporter) = traced(1.0);
    let first = subscribed_session(&server).await;
    let second = subscribed_session(&server).await;
    let publisher = subscribed_session(&server).await;
    let _a = open_stream(&server, &first).await;
    let _b = open_stream(&server, &second).await;
    exporter.reset();

    let (status, body) = server
        .send(
            Method::POST,
            "/api/v1/sync/ops",
            Some(&serde_json::json!({
                "stream_id": STREAM,
                "batch_id": 1,
                "ops": ["AAECAwQFBgc="],
            })),
            &[
                ("authorization", &format!("Bearer {BEARER}")),
                ("x-sunrise-session", &publisher),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    let spans = finished(&exporter);
    let root = spans
        .iter()
        .find(|s| s.name == "POST /api/v1/sync/ops")
        .expect("the request is a root span");
    let trace = root.span_context.trace_id();
    let mut children: Vec<&SpanData> = spans
        .iter()
        .filter(|s| s.parent_span_id == root.span_context.span_id())
        .collect();
    children.sort_by_key(|s| s.start_time);
    assert!(spans
        .iter()
        .filter(|s| s.name != root.name)
        .all(|s| s.span_context.trace_id() == trace));
    let order: Vec<&str> = children.iter().map(|s| s.name.as_ref()).collect();
    let position = |name: &str| {
        order
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| panic!("{name} is not a child of the request: {order:?}"))
    };
    let verify_token = position("auth.verify_token");
    let verify_sig = position("auth.verify_signature");
    let store = position("store.relay_device_heads");
    let append = position("relay.append");
    let fanout = position("relay.fanout");
    assert!(
        verify_token < verify_sig && verify_sig < store && store < append && append < fanout,
        "children out of order: {order:?}"
    );

    let child_of = |parent: &SpanData, name: &str| {
        spans
            .iter()
            .any(|s| s.name == name && s.parent_span_id == parent.span_context.span_id())
    };
    assert!(
        child_of(children[append], "store.relay_append"),
        "{spans:#?}"
    );
    assert!(child_of(children[verify_token], "store.resolve_account"));

    let fanout = children[fanout];
    let reached: Vec<_> = fanout
        .attributes
        .iter()
        .map(|kv| (kv.key.as_str().to_owned(), kv.value.to_string()))
        .collect();
    assert_eq!(
        reached,
        [("n_streams".to_owned(), "2".to_owned())],
        "{fanout:?}"
    );
    assert!(root
        .attributes
        .iter()
        .any(|kv| kv.key.as_str() == "status" && kv.value.to_string() == "200"));
}

/// A client-chosen trace id the stock ratio sampler would always accept: its
/// low bits are zero.
const FAVOURED: &str = "00-ffffffffffffffff0000000000000000-00f067aa0ba902b7-01";

/// 3. The ratio holds, and a `traceparent` cannot raise it.
#[tokio::test]
async fn a_client_traceparent_cannot_raise_the_sampling_ratio() {
    // At zero, a client asking for its trace to be sampled is not.
    let (server, exporter) = traced(0.0);
    for _ in 0..50 {
        server
            .send(
                Method::GET,
                "/api/v1/health",
                None,
                &[("traceparent", FAVOURED)],
            )
            .await;
    }
    assert!(
        finished(&exporter).is_empty(),
        "a traceparent forced a sample"
    );

    // At one, a client asking for its trace *not* to be sampled is obeyed.
    let (server, exporter) = traced(1.0);
    let unsampled = FAVOURED.replace("-01", "-00");
    server
        .send(
            Method::GET,
            "/api/v1/health",
            None,
            &[("traceparent", &unsampled)],
        )
        .await;
    assert!(finished(&exporter).is_empty(), "sampled=0 was overridden");

    // Between, the fraction a client's sampled=1 gets is the ratio, not all.
    let (server, exporter) = traced(0.25);
    let n = 2000;
    for _ in 0..n {
        server
            .send(
                Method::GET,
                "/api/v1/health",
                None,
                &[("traceparent", FAVOURED)],
            )
            .await;
    }
    let sampled = finished(&exporter).len();
    // Binomial(2000, 0.25): mean 500, standard deviation 19. Ten deviations.
    assert!((306..=694).contains(&sampled), "{sampled} of {n}");

    // And with no traceparent the ratio applies to the server's own roots.
    let (server, exporter) = traced(0.25);
    for _ in 0..n {
        server.send(Method::GET, "/api/v1/health", None, &[]).await;
    }
    let sampled = finished(&exporter).len();
    assert!((306..=694).contains(&sampled), "{sampled} of {n}");
}

/// Run `body` with logging captured at `info`.
fn logged<F, Fut>(state: ServerState, body: F) -> String
where
    F: FnOnce(Server) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        filter: "info".to_owned(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    tracing::dispatcher::with_default(&dispatch, || {
        rt.block_on(async move { body(Server::new(state)).await });
    });
    cap.contents()
}

async fn open_session(server: Server) {
    let (status, _) = server
        .send(
            Method::POST,
            "/api/v1/sync/session",
            Some(&hello()),
            &[("authorization", &format!("Bearer {BEARER}"))],
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

/// 4a. Without `[observability]` there is no telemetry, and a request's log
/// records carry no trace ids and no extra span.
#[test]
fn without_observability_nothing_is_traced_and_the_logs_are_unchanged() {
    let state = ServerState::new(ServerConfig::default());
    assert!(!state.telemetry.is_enabled());
    assert!(state.config.trace_export("abc").is_none());
    let out = logged(state, open_session);
    let line = out
        .lines()
        .find(|l| l.contains("srv.sync.session_open"))
        .unwrap_or_else(|| panic!("no session_open record: {out}"));
    let record: serde_json::Value = serde_json::from_str(line).expect("JSON");
    assert!(record.get("span").is_none(), "{record}");
    assert!(!out.contains("trace_id"), "{out}");
}

/// 4b. With it, a record written during a traced request names the trace and
/// the span it was written in.
#[test]
fn a_log_record_inside_a_traced_request_carries_its_trace_and_span_ids() {
    let (telemetry, exporter): (Telemetry, _) = recording(1.0);
    // Held past the server, whose drop would otherwise shut the provider down
    // and empty the recorder.
    let _held = telemetry.clone();
    let state = ServerState::new(ServerConfig::default()).with_telemetry(telemetry);
    let out = logged(state, open_session);
    let line = out
        .lines()
        .find(|l| l.contains("srv.sync.session_open"))
        .unwrap_or_else(|| panic!("no session_open record: {out}"));
    let record: serde_json::Value = serde_json::from_str(line).expect("JSON");
    let spans = finished(&exporter);
    let root = spans
        .iter()
        .find(|s| s.name == "POST /api/v1/sync/session")
        .unwrap_or_else(|| panic!("the request was not traced: {spans:?}"));
    assert_eq!(record["span"]["name"], "http.request", "{record}");
    assert_eq!(
        record["span"]["trace_id"],
        root.span_context.trace_id().to_string(),
        "{record}"
    );
    assert_eq!(
        record["span"]["span_id"],
        root.span_context.span_id().to_string(),
        "{record}"
    );
    assert_eq!(record["span"]["endpoint"], "/api/v1/sync/session");
}
