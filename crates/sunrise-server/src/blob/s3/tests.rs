//! [`S3Blobs`] offline, and against the store `SUNRISE_TEST_S3_ENDPOINT`
//! names (ADR-0062 §1). CI's `s3` job provides a MinIO; without it each store
//! test prints why it did nothing and passes, as the Postgres suites do.

use std::sync::Arc;

use kynos::http::{Method as HttpMethod, StatusCode as HttpStatus};

use super::keys::{manifest_key, stale_name, BLOBS};
use super::*;
use crate::api::testing::Client;
use crate::blob::conformance;

fn offline() -> S3Blobs {
    // Port 1 on loopback: nothing listens, so a request is refused at once
    // rather than waiting out the timeout.
    S3Blobs::new(
        "http://127.0.0.1:1",
        "sunrise",
        "us-east-1",
        S3Credentials::new("k", "s"),
    )
    .unwrap()
}

/// A forged sweep name is refused before any request is made.
#[tokio::test]
async fn remove_upload_refuses_a_name_it_did_not_list() {
    let blobs = offline();
    for forged in [
        "../../etc",
        "blobs/aa/pending/up_bb",
        &format!("blobs/{}/committed/x", "a".repeat(32)),
        &format!("blobs/{}/pending/up_{}/0", "a".repeat(32), "b".repeat(32)),
    ] {
        let err = blobs
            .remove_upload(&StaleUpload(forged.to_owned()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is not a pending upload"), "{err}");
    }
}

/// A store that does not answer is the typed, retryable error.
#[tokio::test]
async fn an_unreachable_store_is_unavailable() {
    let blobs = offline();
    assert!(matches!(
        blobs.manifest([0; 16], [0; 16]).await,
        Err(BlobError::Unavailable(_))
    ));
    assert!(matches!(
        blobs.probe().await,
        Err(BlobError::Unavailable(_))
    ));
    assert_eq!(blobs.disk_space().await.unwrap(), None);
    assert_eq!(blobs.location(), "s3://sunrise at http://127.0.0.1:1");
}

/// `[blobs] backend = "s3"` is what the state opens, and a credentials file
/// it cannot read refuses the start rather than falling back to the
/// filesystem.
#[test]
fn the_state_opens_the_object_store_the_config_names() {
    use crate::config::{BlobBackendKind, BlobsConfig};
    use crate::state::{ServerState, StartError};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("s3.toml");
    let config = |file: &std::path::Path| crate::ServerConfig {
        blobs: BlobsConfig {
            backend: BlobBackendKind::S3,
            s3_endpoint: Some("http://127.0.0.1:1".into()),
            s3_bucket: Some("sunrise".into()),
            s3_region: Some("us-east-1".into()),
            s3_credentials_file: Some(file.to_owned()),
        },
        ..crate::ServerConfig::default()
    };
    assert!(matches!(
        ServerState::try_new(config(&file)),
        Err(StartError::Blobs(S3OpenError::CredentialsFile { .. }))
    ));
    std::fs::write(&file, "access_key_id = \"k\"\nsecret_access_key = \"s\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let state = ServerState::try_new(config(&file)).unwrap();
    assert_eq!(state.blobs.location(), "s3://sunrise at http://127.0.0.1:1");
}

/// The variable naming the test store.
const ENV: &str = "SUNRISE_TEST_S3_ENDPOINT";

/// A fresh bucket of its own on the test store, or `None` with the reason
/// printed. Credentials are MinIO's defaults unless
/// `SUNRISE_TEST_S3_ACCESS_KEY_ID` and `SUNRISE_TEST_S3_SECRET_ACCESS_KEY` say
/// otherwise: a test's, never the relay's, way of finding them.
async fn fresh() -> Option<S3Blobs> {
    let Some(endpoint) = std::env::var(ENV).ok().filter(|e| !e.trim().is_empty()) else {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("skipped: {ENV} is not set, so there is no object store to run against");
        }
        return None;
    };
    let var =
        |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_owned());
    let mut raw = [0u8; 8];
    getrandom::getrandom(&mut raw).unwrap();
    let blobs = S3Blobs::new(
        &endpoint,
        &format!("sunrise-test-{}", hex::encode(raw)),
        "us-east-1",
        S3Credentials::new(
            var("SUNRISE_TEST_S3_ACCESS_KEY_ID", "minioadmin"),
            var("SUNRISE_TEST_S3_SECRET_ACCESS_KEY", "minioadmin"),
        ),
    )
    .unwrap();
    blobs.create_bucket().await.unwrap();
    Some(blobs)
}

/// The object store owes everything any backend owes: the interrupted
/// finalize that leaves no readable blob, cross-account isolation, and removal
/// manifest first.
#[tokio::test]
async fn the_object_store_passes_the_conformance_suite() {
    let Some(blobs) = fresh().await else { return };
    conformance::run(&blobs).await;
}

/// Finalize through the router, on the object store: a chunk that does not
/// hash to its claim is refused and leaves no readable blob; one that does
/// reads back whole.
#[tokio::test]
async fn finalize_on_the_object_store_refuses_a_hash_mismatch_and_commits_a_match() {
    let Some(blobs) = fresh().await else { return };
    let blobs = Arc::new(blobs);
    let client = Client::from_state(
        crate::state::ServerState::new(crate::ServerConfig::default())
            .with_blob_backend(Arc::clone(&blobs) as Arc<dyn BlobBackend>),
    );
    let chunk: &[u8] = b"ciphertext on an object store";
    let hash = |b: &[u8]| hex::encode(blake3::hash(b).as_bytes());
    let finalize = |claimed: String| {
        let client = &client;
        async move {
            let init = client
                .send(
                    HttpMethod::POST,
                    "/api/v1/blobs/init",
                    Some(&serde_json::json!({
                        "stream_id": "str_test",
                        "chunk_count": 1,
                        "size_bytes": chunk.len(),
                    })),
                )
                .await;
            init.assert_status(HttpStatus::OK);
            let upload = init.json()["upload_id"].as_str().unwrap().to_owned();
            let put = client
                .send_bytes(
                    HttpMethod::PUT,
                    &format!("/api/v1/blobs/{upload}/0"),
                    "application/octet-stream",
                    chunk,
                    &[],
                )
                .await;
            assert_eq!(put.status, HttpStatus::NO_CONTENT);
            client
                .send(
                    HttpMethod::POST,
                    "/api/v1/blobs/finalize",
                    Some(&serde_json::json!({
                        "upload_id": upload,
                        "content_hash": claimed,
                        "chunk_hashes": [claimed],
                    })),
                )
                .await
        }
    };

    let refused = finalize(hash(b"something else")).await;
    refused.assert_status(HttpStatus::BAD_REQUEST);
    assert_eq!(
        refused.json()["code"],
        crate::api::error::codes::BLOB_HASH_MISMATCH
    );
    assert!(
        blobs.committed_manifests().await.unwrap().is_empty(),
        "a refused finalize makes nothing readable"
    );

    let committed = finalize(hash(chunk)).await;
    committed.assert_status(HttpStatus::OK);
    let blob_id = committed.json()["blob_id"].as_str().unwrap().to_owned();
    let fetched = client
        .send(HttpMethod::GET, &format!("/api/v1/blobs/{blob_id}"), None)
        .await;
    fetched.assert_status(HttpStatus::OK);
    assert_eq!(fetched.bytes, chunk);
    assert_eq!(blobs.committed_manifests().await.unwrap().len(), 1);
}

/// The maintenance pass's listings: an abandoned upload and an idle account
/// tree are listed once old enough, and removing them leaves nothing of
/// either.
#[tokio::test]
async fn the_sweeps_list_and_remove_what_has_gone_idle() {
    let Some(blobs) = fresh().await else { return };
    let (a, u, b) = ([0xa1; 16], [0x01; 16], [0xbb; 16]);
    blobs.open_upload(a, u, 0).await.unwrap();
    blobs.put_pending(a, u, 0, b"p").await.unwrap();
    blobs.put_committed(a, b, 0, b"c").await.unwrap();
    // A manifest that does not parse is not listed as a blob.
    blobs
        .put(&manifest_key(a, [0xcc; 16]), b"junk")
        .await
        .unwrap();

    let now = u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap();
    let hour = 3_600_000;
    assert!(blobs.stale_uploads(now, hour).await.unwrap().is_empty());
    let later = now + 2 * hour;
    let stale = blobs.stale_uploads(later, hour).await.unwrap();
    assert_eq!(stale, vec![StaleUpload(stale_name(a, u))]);
    assert!(blobs.committed_manifests().await.unwrap().is_empty());
    blobs.remove_upload(&stale[0]).await.unwrap();
    assert_eq!(blobs.usage().await.unwrap().pending_uploads, 0);

    let trees = blobs.stale_account_trees(later, hour).await.unwrap();
    assert_eq!(
        trees,
        vec![AccountTree {
            area: Area::Committed,
            account: a
        }]
    );
    blobs.remove_account_tree(trees[0]).await.unwrap();
    assert!(blobs.list(BLOBS).await.unwrap().is_empty());
}

/// A backup is a filesystem tree [`FsBlobs`] reads back.
#[tokio::test]
async fn a_backup_restores_onto_the_filesystem_backend() {
    let Some(blobs) = fresh().await else { return };
    let (a, b) = ([0xa1; 16], [0xbb; 16]);
    let manifest = Manifest {
        chunk_count: 2,
        size_bytes: 6,
    };
    blobs.put_committed(a, b, 0, b"one").await.unwrap();
    blobs.put_committed(a, b, 1, b"two").await.unwrap();
    blobs.write_manifest(a, b, manifest).await.unwrap();
    // Chunks with no manifest: copied, and still not a blob.
    blobs
        .put_committed(a, [0xdd; 16], 0, b"half")
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let totals = blobs.backup_to(dir.path()).await.unwrap();
    assert_eq!(totals.files, 4);
    let restored = FsBlobs::new(dir.path());
    assert_eq!(restored.manifest(a, b).await.unwrap(), Some(manifest));
    assert_eq!(
        restored.get_committed(a, b, 1).await.unwrap().as_deref(),
        Some(&b"two"[..])
    );
    assert_eq!(restored.manifest(a, [0xdd; 16]).await.unwrap(), None);
}
