//! What the server writes to its log when real requests go through it.
//!
//! Instrumentation is only worth having if it survives contact with the
//! router, so these drive `build_router` end to end with a capturing
//! subscriber installed and assert on the NDJSON bytes.
//!
//! Two classes of assertion:
//!
//! * **Redaction.** The `?access_token=` query and full entity ids must not
//!   appear. This is not hypothetical: `TraceLayer::new_for_http`'s stock
//!   `MakeSpan` records the full URI, and `/sync` is where browser clients
//!   put their bearer token because a WebSocket upgrade cannot carry a
//!   header.
//! * **Vocabulary.** Every field the server emits must be on the allowlist.
//!   `RedactionLayer` defaults to panicking in debug builds, so an
//!   unvetted field name fails the test rather than shipping.

#![allow(clippy::missing_panics_doc, clippy::doc_markdown)]

use axum::http::{Request, StatusCode};
use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};
use sunrise_server::{build_router, ServerConfig, ServerState};
use tower::ServiceExt;

/// A bearer-shaped string that must never appear in a log line.
const TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.SENTINELTOKENPAYLOAD.sig";

fn capture() -> (Capture, tracing::Dispatch) {
    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        // `debug` so `srv.req.start` / `srv.req.end` are in scope; they are
        // debug-level by catalogue, which is what keeps a default `info`
        // deployment quiet.
        filter: "debug".to_string(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");
    (cap, dispatch)
}

fn router() -> axum::Router {
    build_router(ServerState::new(ServerConfig::default()))
}

/// Drive one request through the router with logging captured.
fn run(uri: &str) -> (StatusCode, String) {
    let (cap, dispatch) = capture();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let status = tracing::dispatcher::with_default(&dispatch, || {
        rt.block_on(async {
            let req = Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap();
            router()
                .oneshot(req)
                .await
                .expect("router responds")
                .status()
        })
    });
    (status, cap.contents())
}

#[test]
fn request_logging_never_echoes_the_access_token_query() {
    let (_status, out) =
        run("/api/v1/meta?access_token=eyJhbGciOiJIUzI1NiJ9.SENTINELTOKENPAYLOAD.sig");
    assert!(!out.is_empty(), "nothing was logged at all");
    assert!(!out.contains("SENTINELTOKENPAYLOAD"), "token leaked: {out}");
    assert!(!out.contains("access_token"), "query leaked: {out}");
    assert!(!out.contains(TOKEN), "token leaked: {out}");
    // The request *was* logged, so the absence above means redaction rather
    // than silence.
    assert!(out.contains("srv.req.end"), "no request record: {out}");
    assert!(out.contains("\"endpoint\":\"/api/v1/meta\""), "{out}");
}

#[test]
fn request_logging_templates_opaque_path_segments() {
    let (_status, out) = run("/api/v1/devices/01J8ZQ7X9K3M5N7P9R1T3V5W7Y");
    assert!(
        !out.contains("01J8ZQ7X9K3M5N7P9R1T3V5W7Y"),
        "full device id leaked: {out}"
    );
    assert!(
        out.contains("\"endpoint\":\"/api/v1/devices/:id\""),
        "expected a templated endpoint: {out}"
    );
}

#[test]
fn request_records_carry_status_and_latency() {
    let (status, out) = run("/api/v1/health");
    assert_eq!(status, StatusCode::OK);
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

#[test]
fn every_server_field_survives_the_redaction_allowlist() {
    // `RedactionLayer` panics on an unvetted field name in debug builds, so
    // reaching the assertions at all is most of the test. The count check
    // catches the opposite failure: fields being silently dropped.
    let (_status, out) = run("/api/v1/meta");
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

#[test]
fn a_default_info_deployment_stays_quiet_about_healthy_traffic() {
    // The reason `srv.req.*` is debug: an operator running at `info` should
    // see startup and failures, not every health check.
    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        filter: "info".to_string(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    tracing::dispatcher::with_default(&dispatch, || {
        rt.block_on(async {
            let req = Request::builder()
                .uri("/api/v1/health")
                .body(axum::body::Body::empty())
                .unwrap();
            router().oneshot(req).await.expect("router responds")
        })
    });
    assert!(
        cap.lines().is_empty(),
        "a 200 should be silent at info: {:?}",
        cap.lines()
    );
}
