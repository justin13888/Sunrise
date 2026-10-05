//! What each traced call site marks on its span when it fails, and what the
//! publish append records as its outcome.
//!
//! `tests/span-redaction.rs` holds every exported span to the allowlist and
//! the publish tree to its order, but drives the happy path: nothing there
//! fails a bearer, a signature, an append or a chunk write. These do, each
//! through the public surface over recording telemetry, and read back the
//! status and attributes the site set. They live in the crate because the
//! failures need what only the crate reaches: the in-process [`Client`] with
//! a checking verifier and device registration, and the store's connection.

use crate::api::testing::{register_device, send_signed, Client, BEARER};
use crate::state::ServerState;
use crate::{ServerConfig, StaticVerifier, Subject};
use kynos::http::{Method, StatusCode};
use std::sync::Arc;
use sunrise_telemetry::testing::{recording, InMemorySpanExporter, SpanData, Status};

fn finished(exporter: &InMemorySpanExporter) -> Vec<SpanData> {
    exporter.get_finished_spans().expect("the recorder answers")
}

/// The one span named `name`, among those recorded since the last reset.
fn only<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let named: Vec<&SpanData> = spans.iter().filter(|s| s.name == name).collect();
    assert_eq!(named.len(), 1, "expected one {name}: {spans:#?}");
    named[0]
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.to_string())
}

/// A `5xx` fails the request's root span; a `4xx` records its status and
/// leaves the root unfailed, because the caller was refused rather than the
/// server failing.
#[tokio::test]
async fn a_server_error_fails_the_root_and_a_refusal_does_not() {
    let dir = tempfile::tempdir().unwrap();
    // A blob root under a regular file cannot be created, so the deep probe
    // answers 503 whoever runs the test.
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"").unwrap();
    let (telemetry, exporter) = recording(1.0);
    let client = Client::from_state(
        ServerState::new(ServerConfig {
            blob_root: Some(file.join("blobs")),
            ..ServerConfig::default()
        })
        .with_telemetry(telemetry),
    );

    let res = client
        .send(Method::GET, "/api/v1/health?deep=1", None)
        .await;
    res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    let spans = finished(&exporter);
    let root = only(&spans, "GET /api/v1/health");
    assert_eq!(attr(root, "status").as_deref(), Some("503"));
    assert_eq!(root.status, Status::error("server error"));

    exporter.reset();
    let res = client
        .send(Method::GET, "/api/v1/health?deep=yes", None)
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    let spans = finished(&exporter);
    let root = only(&spans, "GET /api/v1/health");
    assert_eq!(attr(root, "status").as_deref(), Some("400"));
    assert_eq!(root.status, Status::Unset);
}

/// A bearer the verifier refuses fails `auth.verify_token`; one it accepts
/// leaves it unfailed.
#[tokio::test]
async fn a_refused_bearer_fails_its_verify_span() {
    let (telemetry, exporter) = recording(1.0);
    let client = Client::from_state(
        ServerState::new(ServerConfig::default())
            .with_verifier(Arc::new(
                StaticVerifier::default().with("test", Subject::new("https://idp.example", "a")),
            ))
            .with_telemetry(telemetry),
    );

    let res = client
        .send_as(
            Method::GET,
            "/api/v1/accounts/me",
            Some("Bearer wrong"),
            None,
        )
        .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    let spans = finished(&exporter);
    let verify = only(&spans, "auth.verify_token");
    assert_eq!(verify.status, Status::error("bearer refused"));
    let root = only(&spans, "GET /api/v1/accounts/me");
    assert_eq!(verify.parent_span_id, root.span_context.span_id());
    assert_eq!(root.status, Status::Unset, "a 401 is not a server failure");

    exporter.reset();
    let res = client
        .send_as(Method::GET, "/api/v1/accounts/me", Some(BEARER), None)
        .await;
    res.assert_status(StatusCode::OK);
    let spans = finished(&exporter);
    assert_eq!(only(&spans, "auth.verify_token").status, Status::Unset);
}

/// A signature from the wrong key fails `auth.verify_signature`; the right
/// key leaves it unfailed.
#[tokio::test]
async fn a_refused_signature_fails_its_verify_span() {
    let (telemetry, exporter) = recording(1.0);
    let client =
        Client::from_state(ServerState::new(ServerConfig::default()).with_telemetry(telemetry));
    let (device_id, key) = register_device(&client, 7, "laptop", None).await;
    let impostor = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);

    exporter.reset();
    let res = send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &device_id,
        &impostor,
        None,
    )
    .await;
    res.assert_status(StatusCode::UNAUTHORIZED);
    let spans = finished(&exporter);
    assert_eq!(
        only(&spans, "auth.verify_signature").status,
        Status::error("device signature refused")
    );

    exporter.reset();
    let res = send_signed(
        &client,
        "GET",
        "/api/v1/accounts/me",
        &device_id,
        &key,
        None,
    )
    .await;
    res.assert_status(StatusCode::OK);
    let spans = finished(&exporter);
    assert_eq!(only(&spans, "auth.verify_signature").status, Status::Unset);
}

const STREAM: &str = "11111111111111111111111111111111";

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

async fn publish(client: &Client, session: &str, batch_id: u64) -> StatusCode {
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/ops",
            Some(BEARER),
            Some(&serde_json::json!({
                "stream_id": STREAM,
                "batch_id": batch_id,
                "ops": ["AAECAwQFBgc="],
            })),
            &[("x-sunrise-session", session)],
        )
        .await
        .status
}

/// `relay.append` records `fresh` for a new batch and `duplicate` for its
/// retry. When the store cannot take the batch it fails, and so does the
/// request's root, which answered 503.
#[tokio::test]
async fn an_append_records_its_outcome_and_fails_with_the_store() {
    let (telemetry, exporter) = recording(1.0);
    let state = ServerState::new(ServerConfig::default()).with_telemetry(telemetry);
    let store = Arc::clone(&state.store);
    let client = Client::from_state(state);
    let res = client
        .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
        .await;
    res.assert_status(StatusCode::CREATED);
    let session = res.json()["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned();
    client
        .send_with(
            Method::POST,
            "/api/v1/sync/subscribe",
            Some(BEARER),
            Some(&serde_json::json!({ "streams": [{ "stream_id": STREAM, "cursors": [] }] })),
            &[("x-sunrise-session", &session)],
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);

    let mut outcomes = Vec::new();
    for _ in 0..2 {
        exporter.reset();
        assert_eq!(publish(&client, &session, 1).await, StatusCode::OK);
        let spans = finished(&exporter);
        let append = only(&spans, "relay.append");
        assert_eq!(append.status, Status::Unset);
        outcomes.push(attr(append, "result"));
    }
    assert_eq!(
        outcomes,
        [Some("fresh".to_owned()), Some("duplicate".to_owned())]
    );

    // Gone from under the relay, so the next append's insert fails.
    store
        .conn
        .lock()
        .execute_batch("DROP TABLE relay_frames")
        .expect("the table drops");
    exporter.reset();
    assert_eq!(
        publish(&client, &session, 2).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let spans = finished(&exporter);
    let append = only(&spans, "relay.append");
    assert_eq!(append.status, Status::error("relay storage unavailable"));
    assert_eq!(attr(append, "result"), None);
    let root = only(&spans, "POST /api/v1/sync/ops");
    assert_eq!(attr(root, "status").as_deref(), Some("503"));
    assert_eq!(root.status, Status::error("server error"));
}

/// A chunk the blob store cannot write fails `blob.chunk_write`, and the
/// request's root with it; a chunk it writes leaves both unfailed.
#[tokio::test]
async fn a_failed_chunk_write_fails_its_span() {
    let dir = tempfile::tempdir().unwrap();
    let (telemetry, exporter) = recording(1.0);
    let client = Client::from_state(
        ServerState::new(ServerConfig {
            blob_root: Some(dir.path().to_path_buf()),
            ..ServerConfig::default()
        })
        .with_telemetry(telemetry),
    );
    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/init",
            Some(&serde_json::json!({
                "stream_id": "str_test",
                "chunk_count": 2,
                "size_bytes": 64,
            })),
        )
        .await;
    res.assert_status(StatusCode::OK);
    let upload_id = res.json()["upload_id"]
        .as_str()
        .expect("an upload id")
        .to_owned();
    let put = |idx: u32| {
        let target = format!("/api/v1/blobs/{upload_id}/{idx}");
        let client = &client;
        async move {
            client
                .send_bytes(
                    Method::PUT,
                    &target,
                    "application/octet-stream",
                    b"chunk-ciphertext",
                    &[],
                )
                .await
                .status
        }
    };

    exporter.reset();
    assert_eq!(put(0).await, StatusCode::NO_CONTENT);
    let spans = finished(&exporter);
    assert_eq!(only(&spans, "blob.chunk_write").status, Status::Unset);

    // Init made the upload's store; a regular file where its chunk
    // directories go leaves every later chunk nowhere to be written.
    let hex = upload_id.trim_start_matches("up_");
    let blobs = find_dir(dir.path(), &upload_id)
        .expect("init created the upload's directory")
        .join("blobs");
    let shard = blobs.join(&hex[..2]);
    std::fs::remove_dir_all(&shard).expect("chunk 0's shard exists");
    std::fs::write(&shard, b"").unwrap();

    exporter.reset();
    assert_eq!(put(1).await, StatusCode::INTERNAL_SERVER_ERROR);
    let spans = finished(&exporter);
    assert_eq!(
        only(&spans, "blob.chunk_write").status,
        Status::error("chunk write failed")
    );
    let root = only(&spans, "PUT /api/v1/blobs/:id/:id");
    assert_eq!(root.status, Status::error("server error"));
}

/// The directory named `name` somewhere under `root`.
fn find_dir(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if entry.file_name() == name {
            return Some(path);
        }
        if let Some(found) = find_dir(&path, name) {
            return Some(found);
        }
    }
    None
}
