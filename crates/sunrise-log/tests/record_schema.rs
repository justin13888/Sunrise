//! `schemas/log-record.v1.json` describes what this crate actually emits.
//!
//! The schema used to be a document nothing produced and nothing checked. It
//! is now validated against live subscriber output, so a change to the
//! formatter configuration that alters the record shape fails here rather
//! than surfacing in an ingest pipeline weeks later.

use jsonschema::JSONSchema;
use serde_json::Value;
use sunrise_log::{build_subscriber, Capture, LogConfig, LogFormat, LogTarget};

const TARGET: &str = "sunrise_log_schema_test";

fn schema() -> JSONSchema {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/log-record.v1.json");
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let doc: Value = serde_json::from_str(&raw).expect("schema is valid JSON");
    JSONSchema::compile(&doc).expect("schema compiles")
}

/// Emit one record of every shape the workspace produces.
fn sample_records() -> Vec<Value> {
    let cap = Capture::new();
    let dispatch = build_subscriber(LogConfig {
        target: LogTarget::Capture(cap.clone()),
        filter: "trace".to_string(),
        format: LogFormat::Ndjson,
    })
    .expect("subscriber builds");

    tracing::dispatcher::with_default(&dispatch, || {
        // Plain info with context.
        tracing::info!(target: TARGET, ev = "srv.start", bind = "127.0.0.1:8443", wire_v = 1u64, "listening");
        // Debug with counters.
        tracing::debug!(target: TARGET, ev = "srv.relay.fanout", n_bytes = 42u64, "fanned out");
        // Warn carrying the error envelope.
        tracing::warn!(
            target: TARGET,
            ev = "srv.auth.rejected",
            err_code = "AUTH_TOKEN_INVALID",
            err_kind = "user",
            retryable = false,
            cause = "signature verification failed",
            "auth rejected"
        );
        // Inside a span, so the `span` object appears.
        let span = tracing::info_span!(target: TARGET, "request", endpoint = "/api/v1/meta", method = "GET");
        span.in_scope(|| {
            tracing::info!(target: TARGET, ev = "srv.req.end", status = 200u64, lat_ms = 3u64, "handled");
        });
        // Error level.
        tracing::error!(target: TARGET, ev = "db.migrate.failed", from_v = 9u64, to_v = 10u64, err_code = "DB_MIGRATION_FAILED", "migration failed");
    });

    let lines = cap.lines();
    assert_eq!(lines.len(), 5, "expected 5 records, got {lines:?}");
    lines
        .iter()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("record is not JSON: {l} ({e})"))
        })
        .collect()
}

#[test]
fn live_records_validate_against_the_schema() {
    let schema = schema();
    for record in sample_records() {
        if let Err(errors) = schema.validate(&record) {
            let detail: Vec<String> = errors
                .map(|e| format!("{} at {}", e, e.instance_path))
                .collect();
            panic!("record {record} failed the schema: {detail:?}");
        }
    }
}

#[test]
fn every_context_key_is_on_the_redaction_allowlist() {
    // The schema deliberately allows extra top-level keys, because that is
    // where flattened context fields land. This is the assertion that keeps
    // "extra" from meaning "anything": every one of them must be a key the
    // redaction allowlist vetted, or part of the fixed envelope.
    const ENVELOPE: &[&str] = &["timestamp", "level", "target", "span"];
    for record in sample_records() {
        let obj = record.as_object().expect("record is an object");
        for key in obj.keys() {
            assert!(
                ENVELOPE.contains(&key.as_str()) || sunrise_log::is_allowed(key),
                "record key {key:?} is neither envelope nor allowlisted: {record}"
            );
        }
    }
}

#[test]
fn schema_rejects_a_record_missing_the_event_name() {
    // Guards the schema itself: a `required` list that had drifted to empty
    // would make the test above vacuous.
    let schema = schema();
    let bad = serde_json::json!({
        "timestamp": "2026-05-08T12:34:56.789Z",
        "level": "INFO",
        "message": "no ev field",
        "target": "sunrise_x"
    });
    assert!(schema.validate(&bad).is_err(), "schema must require `ev`");
}

#[test]
fn schema_rejects_a_malformed_event_name() {
    let schema = schema();
    let bad = serde_json::json!({
        "timestamp": "2026-05-08T12:34:56.789Z",
        "level": "INFO",
        "message": "bad ev",
        "target": "sunrise_x",
        "ev": "Srv.Req.End"
    });
    assert!(
        schema.validate(&bad).is_err(),
        "schema must enforce the ev grammar"
    );
}
