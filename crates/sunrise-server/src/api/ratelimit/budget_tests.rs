//! The per-device and per-account budgets, through the handlers that charge
//! them.
//!
//! Each test sets one budget low enough to reach in a few requests and leaves
//! the per-address groups at their defaults, so the refusal it sees is the
//! budget's and nothing else's.

use crate::api::error::codes;
use crate::api::testing::{code_of, register_device, send_signed, Client, Res};
use crate::config::LimitsConfig;
use crate::state::ServerState;
use crate::ServerConfig;
use kynos::http::body::Body;
use kynos::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use sunrise_wire_protocol::{Capability, CapabilityBits};

fn client(limits: LimitsConfig) -> (Client, crate::api::ratelimit::Limiter, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let state = ServerState::new(ServerConfig {
        blob_root: Some(dir.path().to_path_buf()),
        limits,
        ..ServerConfig::default()
    });
    let limiter = state.limiter.clone();
    (Client::from_state(state), limiter, dir)
}

fn hello() -> serde_json::Value {
    use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
    serde_json::json!({
        "client_app_v": "0.1.0",
        "client_platform": "test",
        "wire_proto_supported": [u32::from(WIRE_PROTO_V)],
        "doc_schema_min": u32::from(DOC_SCHEMA_V),
        "doc_schema_max": u32::from(DOC_SCHEMA_V),
        "crypto_suite_supported": [u32::from(CRYPTO_SUITE_V)],
        "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0
            | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
        "trace": "01J000000000000000000000000",
    })
}

async fn session(client: &Client) -> Res {
    client
        .send(Method::POST, "/api/v1/sync/session", Some(&hello()))
        .await
}

fn assert_limited(res: &Res, scope: &str, client: &Client, endpoint: &str) {
    res.assert_status(StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code_of(res), codes::RATE_LIMITED);
    assert!(res.headers.contains_key("retry-after"));
    assert_eq!(
        client.metrics.get_with(
            "sunrise_ratelimit_rejected_total",
            &[("endpoint", endpoint), ("scope", scope)]
        ),
        1,
        "the refusal is counted under {endpoint} / {scope}"
    );
}

/// New sessions are budgeted per device: one device spending its budget does
/// not spend another's.
#[tokio::test]
async fn sessions_are_budgeted_per_device() {
    let (client, _, _dir) = client(LimitsConfig {
        sessions_per_5min: 1,
        ..LimitsConfig::default()
    });
    let (a, ka) = register_device(&client, 7, "a", None).await;
    let (b, kb) = register_device(&client, 8, "b", None).await;
    let open = |id, key| {
        let client = &client;
        let body = hello();
        async move { send_signed(client, "POST", "/api/v1/sync/session", id, key, Some(&body)).await }
    };

    open(&a, &ka).await.assert_status(StatusCode::CREATED);
    let refused = open(&a, &ka).await;
    assert_limited(&refused, "device", &client, "/api/v1/sync/session");
    open(&b, &kb).await.assert_status(StatusCode::CREATED);
}

/// A caller that signs with no device — the single-tenant self-host case — is
/// budgeted by its account, and the label says so.
#[tokio::test]
async fn a_caller_with_no_device_is_budgeted_by_account() {
    let (client, _, _dir) = client(LimitsConfig {
        sessions_per_5min: 1,
        ..LimitsConfig::default()
    });
    session(&client).await.assert_status(StatusCode::CREATED);
    assert_limited(
        &session(&client).await,
        "account",
        &client,
        "/api/v1/sync/session",
    );
}

/// An op batch costs one unit per op, so the budget bounds ops rather than
/// batches.
#[tokio::test]
async fn ops_are_charged_per_op() {
    // One op a second, bursting to ten.
    let (client, _, _dir) = client(LimitsConfig {
        ops_per_sec: 1,
        ..LimitsConfig::default()
    });
    let id = session(&client).await.json()["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let batch = |n: usize, batch_id: u64| {
        let client = &client;
        let id = id.clone();
        async move {
            client
                .send_with(
                    Method::POST,
                    "/api/v1/sync/ops",
                    Some(crate::api::testing::BEARER),
                    Some(&serde_json::json!({
                        "stream_id": hex::encode([0x11; 16]),
                        "batch_id": batch_id,
                        "ops": vec!["AAAA"; n],
                    })),
                    &[("x-sunrise-session", &id)],
                )
                .await
        }
    };

    assert_ne!(batch(10, 1).await.status, StatusCode::TOO_MANY_REQUESTS);
    assert_limited(&batch(1, 2).await, "account", &client, "/api/v1/sync/ops");
}

/// Chunk bytes are budgeted, and an upload counts against the account's open
/// uploads until it is finalized.
#[tokio::test]
async fn chunk_bytes_and_open_uploads_are_budgeted() {
    let (client, _, _dir) = client(LimitsConfig {
        blob_upload_bytes_per_min: 16,
        open_uploads: 1,
        ..LimitsConfig::default()
    });
    let init = || {
        let client = &client;
        async move {
            client
                .send(
                    Method::POST,
                    "/api/v1/blobs/init",
                    Some(&serde_json::json!({
                        "stream_id": "str_test", "chunk_count": 2, "size_bytes": 32,
                    })),
                )
                .await
        }
    };
    let first = init().await;
    first.assert_status(StatusCode::OK);
    let upload = first.json()["upload_id"].as_str().unwrap().to_owned();
    assert_limited(&init().await, "account", &client, "/api/v1/blobs/init");

    let put = |idx: u32, body: &'static [u8]| {
        let client = &client;
        let upload = upload.clone();
        async move {
            client
                .send_bytes(
                    Method::PUT,
                    &format!("/api/v1/blobs/{upload}/{idx}"),
                    "application/octet-stream",
                    body,
                    &[],
                )
                .await
        }
    };
    put(0, &[1; 16]).await.assert_status(StatusCode::NO_CONTENT);
    assert_limited(
        &put(1, &[2; 1]).await,
        "account",
        &client,
        "/api/v1/blobs/{upload_id}/{chunk_idx}",
    );
}

/// Finalizing an upload frees its slot, and a fetch is charged the blob's
/// whole size.
#[tokio::test]
async fn finalize_frees_the_slot_and_fetch_is_charged_by_size() {
    let (client, _, _dir) = client(LimitsConfig {
        open_uploads: 1,
        blob_download_bytes_per_min: 8,
        ..LimitsConfig::default()
    });
    let chunk: &[u8] = b"ciphertext";
    let hash = |b: &[u8]| hex::encode(blake3::hash(b).as_bytes());
    let res = client
        .send(
            Method::POST,
            "/api/v1/blobs/init",
            Some(&serde_json::json!({
                "stream_id": "str_test", "chunk_count": 1, "size_bytes": 10,
            })),
        )
        .await;
    let upload = res.json()["upload_id"].as_str().unwrap().to_owned();
    client
        .send_bytes(
            Method::PUT,
            &format!("/api/v1/blobs/{upload}/0"),
            "application/octet-stream",
            chunk,
            &[],
        )
        .await
        .assert_status(StatusCode::NO_CONTENT);
    let done = client
        .send(
            Method::POST,
            "/api/v1/blobs/finalize",
            Some(&serde_json::json!({
                "upload_id": upload,
                "content_hash": hash(chunk),
                "chunk_hashes": [hash(chunk)],
            })),
        )
        .await;
    done.assert_status(StatusCode::OK);
    let blob = done.json()["blob_id"].as_str().unwrap().to_owned();

    // The slot is free again.
    client
        .send(
            Method::POST,
            "/api/v1/blobs/init",
            Some(&serde_json::json!({
                "stream_id": "str_test", "chunk_count": 1, "size_bytes": 10,
            })),
        )
        .await
        .assert_status(StatusCode::OK);

    // Ten bytes against a bucket of eight: admitted from a full bucket, and
    // the debt refuses the next.
    let target = format!("/api/v1/blobs/{blob}");
    let fetch = || client.send(Method::GET, &target, None);
    fetch().await.assert_status(StatusCode::OK);
    assert_limited(
        &fetch().await,
        "account",
        &client,
        "/api/v1/blobs/{blob_id}",
    );
}

/// A device holds at most `streams` event streams, and a slot is released when
/// the client goes away — without waiting for the stream's next event.
#[tokio::test]
async fn event_streams_are_capped_and_released_on_disconnect() {
    let (client, limiter, _dir) = client(LimitsConfig {
        streams: 1,
        ..LimitsConfig::default()
    });
    let id = session(&client).await.json()["session_id"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let open = || {
        let mut request = Request::new(Body::empty());
        *request.uri_mut() = "/api/v1/sync/events".parse().unwrap();
        request.headers_mut().insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static(crate::api::testing::BEARER),
        );
        request.headers_mut().insert(
            HeaderName::from_static("x-sunrise-session"),
            HeaderValue::from_str(&id).unwrap(),
        );
        client.call(request)
    };

    let held = open().await;
    assert_eq!(held.status(), StatusCode::OK);
    let refused = open().await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(refused.headers().contains_key("retry-after"));

    drop(held);
    // The stream's task notices the dropped receiver on its own schedule;
    // yield to it rather than sleep, under a bound.
    let mut released = false;
    for _ in 0..10_000 {
        if limiter.streams_held_total() == 0 {
            released = true;
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(released, "dropping the stream must release its slot");
    assert_eq!(open().await.status(), StatusCode::OK);
}
