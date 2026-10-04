//! The maintenance pass, driven through the real routes on an injected clock.

use super::maintenance::{self, Report};
use crate::api::testing::{register_device, send_signed_with, Client, Res};
use crate::state::{Clock, ServerState};
use crate::ServerConfig;
use kynos::http::{Method, StatusCode};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const T0_MS: u64 = 1_800_000_000_000;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const STREAM: &str = "11111111111111111111111111111111";
const ORIGIN: &str = "22222222222222222222222222222222";

#[derive(Debug)]
struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Harness {
    client: Client,
    state: ServerState,
    clock: Arc<TestClock>,
    dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock(AtomicU64::new(T0_MS)));
        let state = ServerState::with_clock(
            ServerConfig {
                blob_root: Some(dir.path().to_path_buf()),
                ..ServerConfig::default()
            },
            clock.clone(),
        );
        Self {
            client: Client::from_state(state.clone()),
            state,
            clock,
            dir,
        }
    }

    fn at(&self, ms: u64) {
        self.clock.0.store(ms, Ordering::SeqCst);
    }

    fn pass(&self) -> Report {
        maintenance::run(&self.state, self.clock.now_ms(), false).unwrap()
    }

    fn account_id(&self) -> String {
        let accounts = self.state.store.account_summaries().unwrap();
        assert_eq!(
            accounts.len(),
            1,
            "the self-host verifier maps to one account"
        );
        accounts[0].account_id.clone()
    }

    async fn send(&self, method: Method, path: &str, body: Option<serde_json::Value>) -> Res {
        self.client.send(method, path, body.as_ref()).await
    }

    /// Init, one chunk, finalize: the committed blob's id.
    async fn upload(&self, bytes: &[u8]) -> String {
        let upload_id = self.start_upload(bytes).await;
        let hash = hex::encode(blake3::hash(bytes).as_bytes());
        let res = self
            .send(
                Method::POST,
                "/api/v1/blobs/finalize",
                Some(serde_json::json!({
                    "upload_id": upload_id,
                    "content_hash": hash,
                    "chunk_hashes": [hash],
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        res.json()["blob_id"].as_str().unwrap().to_owned()
    }

    /// Init and one chunk, never finalized: the upload id.
    async fn start_upload(&self, bytes: &[u8]) -> String {
        let res = self
            .send(
                Method::POST,
                "/api/v1/blobs/init",
                Some(serde_json::json!({
                    "stream_id": "str_test",
                    "chunk_count": 1,
                    "size_bytes": bytes.len(),
                })),
            )
            .await;
        res.assert_status(StatusCode::OK);
        let upload_id = res.json()["upload_id"].as_str().unwrap().to_owned();
        let put = self
            .client
            .send_bytes(
                Method::PUT,
                &format!("/api/v1/blobs/{upload_id}/0"),
                "application/octet-stream",
                bytes,
                &[],
            )
            .await;
        put.assert_status(StatusCode::NO_CONTENT);
        upload_id
    }

    async fn tombstone(&self, blob_id: &str, seq: u64) -> Res {
        self.send(
            Method::DELETE,
            &format!("/api/v1/blobs/{blob_id}"),
            Some(serde_json::json!({
                "stream_id": STREAM,
                "device_id": ORIGIN,
                "seq": seq,
            })),
        )
        .await
    }

    async fn fetch(&self, blob_id: &str) -> StatusCode {
        self.send(Method::GET, &format!("/api/v1/blobs/{blob_id}"), None)
            .await
            .status
    }

    fn area(&self, area: &str) -> std::path::PathBuf {
        crate::api::blobs::account_dir(self.dir.path(), area, &self.account_id())
    }
}

fn hello() -> serde_json::Value {
    serde_json::json!({
        "client_app_v": "0.1.0",
        "client_platform": "test",
        "wire_proto_supported": [u32::from(sunrise_cbor::version::WIRE_PROTO_V)],
        "doc_schema_min": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
        "doc_schema_max": u32::from(sunrise_cbor::version::DOC_SCHEMA_V),
        "crypto_suite_supported": [u32::from(sunrise_cbor::version::CRYPTO_SUITE_V)],
        "capabilities": sunrise_wire_protocol::REQUIRED_CLIENT_BITS.0,
        "trace": "01J000000000000000000000000",
    })
}

/// Open a session as a signed device and subscribe with one cursor on
/// `(STREAM, ORIGIN)`: the acknowledgement a tombstone's quorum reads.
async fn declare(h: &Harness, device: &(String, ed25519_dalek::SigningKey), applied_seq: u64) {
    let (id, key) = device;
    let res = send_signed_with(
        &h.client,
        "POST",
        "/api/v1/sync/session",
        id,
        key,
        Some(&hello()),
        &[],
    )
    .await;
    res.assert_status(StatusCode::CREATED);
    let session = res.json()["session_id"].as_str().unwrap().to_owned();
    let body = serde_json::json!({
        "streams": [{
            "stream_id": STREAM,
            "cursors": [{ "device_id": ORIGIN, "last_applied_seq": applied_seq }],
        }]
    });
    send_signed_with(
        &h.client,
        "POST",
        "/api/v1/sync/subscribe",
        id,
        key,
        Some(&body),
        &[("x-sunrise-session", &session)],
    )
    .await
    .assert_status(StatusCode::NO_CONTENT);
}

/// **The whole deletion, end to end.** An account with devices, a push
/// token, relay frames and both a committed blob and an unfinished upload is
/// deleted through the two routes; nothing changes inside the grace period;
/// past it one pass leaves no row carrying the account's id or its hash, and
/// neither blob directory.
#[tokio::test]
async fn a_deleted_account_is_erased_completely_after_the_grace_period() {
    let h = Harness::new();
    let (device_id, _key) = register_device(&h.client, 7, "phone", None).await;
    let _second = register_device(&h.client, 8, "laptop", None).await;
    let account_id = h.account_id();
    h.state
        .store
        .upsert_push_token(&device_id, "apns", "tok", T0_MS)
        .unwrap();
    let account_h = crate::relay_log::account_key(&account_id);
    h.state
        .store
        .relay_append(
            (account_h, [0x11; 16]),
            b"ciphertext",
            &[],
            None,
            1,
            T0_MS,
            h.state.durable_caps,
        )
        .unwrap();
    let blob = h.upload(b"attachment").await;
    h.start_upload(b"abandoned").await;
    assert!(h.area("pending").exists() && h.area("committed").exists());

    let res = h
        .send(Method::POST, "/api/v1/accounts/me/delete/initiate", None)
        .await;
    let phrase = res.json()["confirm_phrase"].as_str().unwrap().to_owned();
    let res = h
        .send(
            Method::DELETE,
            "/api/v1/accounts/me",
            Some(serde_json::json!({ "confirm_phrase": phrase })),
        )
        .await;
    res.assert_status(StatusCode::ACCEPTED);
    let erase_after = res.json()["erase_after_ms"].as_u64().unwrap();
    assert_eq!(erase_after, T0_MS + 30 * DAY_MS);

    h.at(erase_after - 1);
    assert_eq!(h.pass().accounts_erased, 0, "inside the grace period");
    assert_eq!(h.fetch(&blob).await, StatusCode::OK);

    h.at(erase_after);
    let report = h.pass();
    assert_eq!(report.accounts_erased, 1);
    assert_eq!(report.failures, 0);
    assert_eq!(h.state.metrics.get("sunrise_account_delete_total"), 1);

    let conn = h.state.store.conn.lock();
    let by_id = |table: &str, column: &str, value: &str| -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
            [value],
            |r| r.get(0),
        )
        .unwrap()
    };
    for table in [
        "accounts",
        "devices",
        "account_delete_tokens",
        "blob_tombstones",
    ] {
        assert_eq!(by_id(table, "account_id", &account_id), 0, "{table}");
    }
    for table in ["push_tokens", "device_cursors"] {
        assert_eq!(by_id(table, "device_id", &device_id), 0, "{table}");
    }
    for table in ["relay_frames", "relay_evicted", "relay_batches"] {
        let n: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE account_h = ?1"),
                [&account_h[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    drop(conn);
    for area in ["pending", "committed"] {
        let dir = crate::api::blobs::account_dir(h.dir.path(), area, &account_id);
        assert!(!dir.exists(), "{} survived", dir.display());
    }
}

/// **GC, one condition at a time**, on the injected clock: a tombstoned blob
/// is kept and still readable inside the grace period, kept past it while an
/// active device has not acknowledged the op, and collected once both hold.
#[tokio::test]
async fn a_tombstoned_blob_waits_for_its_grace_period_and_its_quorum() {
    let h = Harness::new();
    let deleter = register_device(&h.client, 1, "phone", None).await;
    let peer = register_device(&h.client, 2, "laptop", None).await;
    let blob = h.upload(b"to be detached").await;
    declare(&h, &peer, 4).await;
    declare(&h, &deleter, 0).await;

    let res = send_signed_with(
        &h.client,
        "DELETE",
        &format!("/api/v1/blobs/{blob}"),
        &deleter.0,
        &deleter.1,
        Some(&serde_json::json!({ "stream_id": STREAM, "device_id": ORIGIN, "seq": 5 })),
        &[],
    )
    .await;
    res.assert_status(StatusCode::ACCEPTED);
    assert_eq!(res.json()["collect_after_ms"], T0_MS + 30 * DAY_MS);

    h.at(T0_MS + 30 * DAY_MS - 1);
    assert_eq!(h.pass().blobs_collected, 0, "inside the grace period");
    assert_eq!(h.fetch(&blob).await, StatusCode::OK, "still readable");

    h.at(T0_MS + 30 * DAY_MS);
    assert_eq!(
        h.pass().blobs_collected,
        0,
        "the peer has applied seq 4, not the tombstone's 5"
    );
    assert_eq!(h.fetch(&blob).await, StatusCode::OK);

    declare(&h, &peer, 5).await;
    let report = h.pass();
    assert_eq!(report.blobs_collected, 1);
    assert_eq!(h.fetch(&blob).await, StatusCode::NOT_FOUND);
    assert_eq!(h.state.metrics.get("sunrise_blob_gc_deleted_total"), 1);
    assert_eq!(h.pass().blobs_collected, 0, "collected once");
}

/// Re-uploading tombstoned ciphertext brings the attachment back, so the
/// tombstone goes; an unknown blob cannot be tombstoned at all.
#[tokio::test]
async fn a_reupload_lifts_the_tombstone_and_an_unknown_blob_is_not_found() {
    let h = Harness::new();
    let blob = h.upload(b"same bytes").await;
    h.tombstone(&blob, 1)
        .await
        .assert_status(StatusCode::ACCEPTED);
    assert_eq!(h.state.store.stats().unwrap().blob_tombstones, 1);
    assert_eq!(h.upload(b"same bytes").await, blob);
    assert_eq!(h.state.store.stats().unwrap().blob_tombstones, 0);

    h.at(T0_MS + 365 * DAY_MS);
    assert_eq!(h.pass().blobs_collected, 0);

    let missing = h.tombstone(&format!("blb_{}", "0".repeat(32)), 1).await;
    missing.assert_status(StatusCode::NOT_FOUND);
}

/// **An abandoned upload is swept once untouched past its TTL**, and one
/// still inside it is not. The TTL is measured from the newest file in the
/// upload, read off the disk, against the pass's clock.
#[tokio::test]
async fn an_abandoned_upload_is_swept_after_its_ttl() {
    let h = Harness::new();
    let committed = h.upload(b"keep me").await;
    h.start_upload(b"half done").await;
    let pending = h.area("pending");
    let newest = newest_ms(&pending);
    let ttl = 24 * 60 * 60 * 1000;

    let kept = maintenance::run(&h.state, newest + ttl, false).unwrap();
    assert_eq!(kept.uploads_swept, 0);
    assert_eq!(std::fs::read_dir(&pending).unwrap().count(), 1);

    let dry = maintenance::run(&h.state, newest + ttl + 1, true).unwrap();
    assert_eq!((dry.dry_run, dry.uploads_swept), (true, 1));
    assert_eq!(
        std::fs::read_dir(&pending).unwrap().count(),
        1,
        "a dry run deletes nothing"
    );

    let swept = maintenance::run(&h.state, newest + ttl + 1, false).unwrap();
    assert_eq!(swept.uploads_swept, 1);
    assert_eq!(
        swept.orphans_swept, 0,
        "the account's own directories are not orphans"
    );
    assert_eq!(std::fs::read_dir(&pending).unwrap().count(), 0);
    assert_eq!(h.fetch(&committed).await, StatusCode::OK);
}

/// Directories left by an account that no longer exists — a crash between an
/// erasure's commit and its file deletion — are swept once stale.
#[tokio::test]
async fn an_orphaned_account_directory_is_swept() {
    let h = Harness::new();
    h.upload(b"keep me").await;
    let orphan = h.dir.path().join("committed").join("ab".repeat(16));
    std::fs::create_dir_all(orphan.join("manifests")).unwrap();
    let newest = newest_ms(&orphan);

    let report = maintenance::run(&h.state, newest + 25 * 60 * 60 * 1000, false).unwrap();
    assert_eq!(report.orphans_swept, 1);
    assert!(!orphan.exists());
    assert!(h.area("committed").exists());
}

/// A signed subscribe records the device's cursors; an unsigned one has no
/// device to record them against and records nothing.
#[tokio::test]
async fn a_signed_subscribe_records_the_devices_cursors() {
    let h = Harness::new();
    let device = register_device(&h.client, 3, "phone", None).await;
    declare(&h, &device, 9).await;
    let rows: Vec<(Vec<u8>, i64, i64)> = {
        let conn = h.state.store.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT origin_device, applied_seq, reported_at_ms FROM device_cursors
                 WHERE device_id = ?1",
            )
            .unwrap();
        stmt.query_map([&device.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        rows,
        vec![(vec![0x22; 16], 9, i64::try_from(T0_MS).unwrap())]
    );
}

fn newest_ms(path: &Path) -> u64 {
    let own = std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let own = u64::try_from(own).unwrap();
    if !path.is_dir() {
        return own;
    }
    std::fs::read_dir(path)
        .unwrap()
        .map(|e| newest_ms(&e.unwrap().path()))
        .fold(own, u64::max)
}
