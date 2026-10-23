//! `GET /api/v1/health` — liveness, and with `?deep=1` readiness.
//!
//! One route rather than a separate `/ready`, because `docs/06-server/api.md`
//! already specified the query and a probe configuration that names it is
//! what operators have been told to write. `deep` is the first query parameter
//! on the surface; the route is unsigned, so the canonical-target note in
//! [`crate::api::signed`] still holds for every signed operation.

use crate::blob::BlobBackend;
use crate::state::ServerState;
use crate::store::MetadataStore;
use kynos::di::inject::Inject;
use kynos::extract::params::query::Query;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// How long the store gets to answer `SELECT 1`, per `api.md`.
const STORE_DEADLINE: Duration = Duration::from_secs(2);

/// How long the blob root gets to take a probe write and its removal.
const BLOB_DEADLINE: Duration = Duration::from_secs(5);

/// Health response.
///
/// `Deserialize` is here for the generated client, not for the server: spargen
/// reads this shape out of the document and the client decodes into it.
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct HealthResponse {
    /// `"ok"` while the server is healthy, `"unavailable"` when a readiness
    /// check failed.
    pub status: String,
    /// Each readiness check's outcome. Present only on `?deep=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<ReadinessChecks>,
    /// The names of the checks that failed, in `checks`' order. Empty, and
    /// omitted, when every check passed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
}

/// What `?deep=1` checked, one member per check, `true` where it passed.
// One `bool` per check is the wire shape `api.md` documents and probes read,
// so the lint's suggestion of an enum or a bit set does not apply.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, kynos::Schema)]
pub struct ReadinessChecks {
    /// The server is not draining. `false` from the moment shutdown begins.
    pub accepting: bool,
    /// The store answered `SELECT 1` within 2 s.
    pub store: bool,
    /// The blob root took a probe write and its removal within 5 s.
    pub blob_root: bool,
    /// The filesystem holding the blob root has more than 5% of its blocks
    /// available to the relay, read within 5 s.
    pub disk: bool,
}

impl ReadinessChecks {
    fn failed(&self) -> Vec<String> {
        [
            ("accepting", self.accepting),
            ("store", self.store),
            ("blob_root", self.blob_root),
            ("disk", self.disk),
        ]
        .into_iter()
        .filter(|(_, ok)| !ok)
        .map(|(name, _)| name.to_owned())
        .collect()
    }
}

/// The query `GET /api/v1/health` reads.
#[derive(Debug, Clone, kynos::Schema, kynos::QueryParams)]
pub struct HealthQuery {
    /// `1` runs the readiness checks; absent or `0` is liveness only.
    pub deep: Option<u8>,
}

/// What the route answers.
#[derive(Debug, kynos::Reply)]
pub enum HealthReply {
    /// Alive, and on `?deep=1` ready.
    #[reply(
        status = 200,
        description = "alive, and on ?deep=1 every readiness check passed"
    )]
    Ok(HealthResponse),
    /// A readiness check failed; `failed` names which.
    #[reply(
        status = 503,
        description = "a readiness check failed; `failed` names which"
    )]
    Unavailable(HealthResponse),
}

/// Liveness probe, and readiness probe with `?deep=1`.
///
/// Unauthenticated on purpose: a load balancer has no token.
///
/// Without `deep` it reads no state and always answers `200 {"status":"ok"}`:
/// it answers "is the process up", and a liveness probe that failed on a full
/// disk would restart a process that restarting cannot fix.
///
/// With `?deep=1` it answers `200` only when the server is not draining, the
/// store answers within 2 s, the blob root is writable within 5 s, and its
/// filesystem has more than 5% free, and otherwise `503` naming the failed
/// checks.
#[kynos::get("/api/v1/health", operation_id = "health")]
pub async fn health(
    Inject(state): Inject<ServerState>,
    Query(q): Query<HealthQuery>,
) -> HealthReply {
    if q.deep.is_none_or(|d| d == 0) {
        return HealthReply::Ok(HealthResponse {
            status: "ok".to_owned(),
            checks: None,
            failed: Vec::new(),
        });
    }
    let (store, blob_root, disk) = tokio::join!(
        probe_store(Arc::clone(&state.store)),
        probe_blob_root(Arc::clone(&state.blobs)),
        probe_disk(Arc::clone(&state.blobs)),
    );
    let checks = ReadinessChecks {
        accepting: !state.drain.is_draining(),
        store,
        blob_root,
        disk,
    };
    let failed = checks.failed();
    if failed.is_empty() {
        return HealthReply::Ok(HealthResponse {
            status: "ok".to_owned(),
            checks: Some(checks),
            failed,
        });
    }
    // A drain is expected and probed every few seconds; logging it would bury
    // the line that matters, which is a dependency failing while serving.
    if !(checks.store && checks.blob_root && checks.disk) {
        tracing::warn!(
            ev = "srv.health.unready",
            result = "failed",
            cause = %failed.join(","),
            "a readiness check failed"
        );
    }
    HealthReply::Unavailable(HealthResponse {
        status: "unavailable".to_owned(),
        checks: Some(checks),
        failed,
    })
}

/// The metadata store's own probe, within [`STORE_DEADLINE`].
///
/// On a blocking thread, because a probe may block for its whole deadline —
/// the SQLite store waits on its one connection's lock, so the failure this
/// can see is a wedged lock as much as a broken file — and that wait must not
/// hold an executor thread. The deadline is the store's to keep, so a wedged
/// store does not leave a blocking thread parked behind it for every probe.
async fn probe_store(store: Arc<dyn MetadataStore>) -> bool {
    let runtime = tokio::runtime::Handle::current();
    let probe =
        tokio::task::spawn_blocking(move || runtime.block_on(store.ping(STORE_DEADLINE)).is_ok());
    matches!(probe.await, Ok(true))
}

/// The blob backend's write-and-remove probe, within [`BLOB_DEADLINE`], on a
/// blocking thread for the same reason as [`probe_store`]. For the filesystem
/// backend that is a probe file under the blob root, created first, since a
/// root that cannot be created is exactly the condition a blob upload would
/// fail on.
async fn probe_blob_root(blobs: Arc<dyn BlobBackend>) -> bool {
    let runtime = tokio::runtime::Handle::current();
    let probe = tokio::task::spawn_blocking(move || runtime.block_on(blobs.probe()).is_ok());
    matches!(
        tokio::time::timeout(BLOB_DEADLINE, probe).await,
        Ok(Ok(true))
    )
}

/// Whether the blob root's filesystem has room, within [`BLOB_DEADLINE`], on
/// a blocking thread for the same reason as [`probe_store`]: `statvfs` on a
/// hung network mount does not return. A backend with no filesystem to read
/// passes; one whose reading fails does not, since a disk the relay cannot
/// stat is not one it can be sure of writing to.
async fn probe_disk(blobs: Arc<dyn BlobBackend>) -> bool {
    let runtime = tokio::runtime::Handle::current();
    let probe = tokio::task::spawn_blocking(move || runtime.block_on(blobs.disk_space()));
    match tokio::time::timeout(BLOB_DEADLINE, probe).await {
        Ok(Ok(Ok(space))) => space.is_none_or(crate::blob::DiskSpace::has_room),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use crate::api::testing::Client;
    use crate::state::ServerState;
    use crate::{ServerConfig, StaticVerifier, Subject};
    use kynos::http::{Method, StatusCode};
    use std::sync::Arc;

    /// A state whose blob root is a fresh, writable directory.
    fn ready_state(dir: &tempfile::TempDir) -> ServerState {
        ServerState::new(ServerConfig {
            blob_root: Some(dir.path().join("blobs")),
            ..ServerConfig::default()
        })
    }

    /// Nothing asserted what this route returns, only that the router had one.
    ///
    /// The body is one member and a load balancer branches on it, so a handler
    /// that started answering `{"status":"degraded"}` — or `{}` — would have
    /// failed no test here.
    #[tokio::test]
    async fn health_answers_ok() {
        let client = Client::new(ServerConfig::default());
        let res = client.send(Method::GET, "/api/v1/health", None).await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json(), serde_json::json!({"status": "ok"}));
    }

    /// Unauthenticated on purpose: a load balancer has no token.
    ///
    /// Against a verifier that actually checks, because the default
    /// `NullVerifier` accepts an absent bearer and so cannot tell an open route
    /// from a closed one.
    #[tokio::test]
    async fn health_needs_no_credential() {
        let state = ServerState::new(ServerConfig::default()).with_verifier(Arc::new(
            StaticVerifier::default().with("test", Subject::new("https://idp.example", "alice")),
        ));
        let client = Client::from_state(state);
        for path in ["/api/v1/health", "/api/v1/health?deep=1"] {
            let res = client.send_as(Method::GET, path, None, None).await;
            res.assert_status(StatusCode::OK);
            assert_eq!(res.json()["status"], serde_json::json!("ok"), "{path}");
        }
    }

    #[tokio::test]
    async fn deep_answers_ok_with_every_check_when_ready() {
        let dir = tempfile::tempdir().unwrap();
        let client = Client::from_state(ready_state(&dir));
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(
            res.json(),
            serde_json::json!({
                "status": "ok",
                "checks": {"accepting": true, "store": true, "blob_root": true, "disk": true},
            })
        );
        // The probe cleans up after itself.
        let left: Vec<_> = std::fs::read_dir(dir.path().join("blobs"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "the probe file was left behind: {left:?}");
    }

    /// `deep=0` is liveness, not readiness: it must not run a check that could
    /// fail.
    #[tokio::test]
    async fn deep_zero_is_liveness() {
        let state = ServerState::new(ServerConfig::default());
        state.drain.begin();
        let client = Client::from_state(state);
        let res = client
            .send(Method::GET, "/api/v1/health?deep=0", None)
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json(), serde_json::json!({"status": "ok"}));
    }

    /// `api.md` promises a `400` for a `deep` that is not a `u8`, rather than
    /// reading it as either probe.
    #[tokio::test]
    async fn a_deep_that_is_not_a_number_is_refused() {
        let client = Client::new(ServerConfig::default());
        for path in ["/api/v1/health?deep=yes", "/api/v1/health?deep=256"] {
            let res = client.send(Method::GET, path, None).await;
            res.assert_status(StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn deep_answers_503_when_the_blob_root_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        // A root under a regular file cannot be created whoever runs the test,
        // root included, which a read-only permission bit would not ensure.
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"").unwrap();
        let client = Client::from_state(ServerState::new(ServerConfig {
            blob_root: Some(file.join("blobs")),
            ..ServerConfig::default()
        }));
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        let body = res.json();
        assert_eq!(body["status"], serde_json::json!("unavailable"));
        assert_eq!(body["failed"], serde_json::json!(["blob_root"]));
        assert_eq!(body["checks"]["store"], serde_json::json!(true));

        // Liveness stays unconditional.
        let res = client.send(Method::GET, "/api/v1/health", None).await;
        res.assert_status(StatusCode::OK);
    }

    /// The store is wedged — its one connection held past the deadline — so
    /// `SELECT 1` cannot run in time.
    #[tokio::test]
    async fn deep_answers_503_when_the_store_does_not_answer_in_time() {
        let dir = tempfile::tempdir().unwrap();
        let state = ready_state(&dir);
        let store = Arc::clone(&state.store);
        let client = Client::from_state(state);

        // Held on a thread of its own, so the guard never crosses an await.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _held = store.as_sqlite().expect("a SQLite store").conn.lock();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.json()["failed"], serde_json::json!(["store"]));

        let res = client.send(Method::GET, "/api/v1/health", None).await;
        res.assert_status(StatusCode::OK);
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::OK);
    }

    /// The probes go through the seams, so a backend that is not SQLite or
    /// not a filesystem is probed the same way, and one that does not answer
    /// is named.
    #[tokio::test]
    async fn deep_names_each_backend_that_does_not_answer() {
        let state = ServerState::new(ServerConfig::default())
            .with_metadata_store(Arc::new(crate::store::conformance::Unreachable::default()))
            .with_blob_backend(Arc::new(crate::blob::conformance::Unreachable));
        let client = Client::from_state(state);
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            res.json()["failed"],
            serde_json::json!(["store", "blob_root", "disk"])
        );
    }

    #[tokio::test]
    async fn deep_flips_to_503_once_the_drain_begins() {
        let dir = tempfile::tempdir().unwrap();
        let state = ready_state(&dir);
        let drain = state.drain.clone();
        let client = Client::from_state(state);

        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::OK);

        drain.begin();
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.json()["failed"], serde_json::json!(["accepting"]));
        assert_eq!(res.json()["checks"]["accepting"], serde_json::json!(false));

        // A draining process is still alive.
        let res = client.send(Method::GET, "/api/v1/health", None).await;
        res.assert_status(StatusCode::OK);
    }

    /// A state over a writable blob root whose filesystem reports `read`.
    fn with_disk(
        dir: &tempfile::TempDir,
        read: fn(&std::path::Path) -> std::io::Result<Option<crate::blob::DiskSpace>>,
    ) -> Client {
        let blobs = crate::blob::FsBlobs::new(dir.path().join("blobs")).with_disk_space(read);
        Client::from_state(ready_state(dir).with_blob_backend(Arc::new(blobs)))
    }

    /// 4% free, and exactly 5%, fail readiness and name only the disk;
    /// liveness does not look.
    #[tokio::test]
    async fn deep_answers_503_when_the_disk_has_5_percent_or_less_free() {
        let dir = tempfile::tempdir().unwrap();
        let reads: [fn(&std::path::Path) -> _; 2] = [
            |_| {
                Ok(Some(crate::blob::DiskSpace {
                    available: 4,
                    total: 100,
                }))
            },
            |_| {
                Ok(Some(crate::blob::DiskSpace {
                    available: 5,
                    total: 100,
                }))
            },
        ];
        for read in reads {
            let client = with_disk(&dir, read);
            let res = client
                .send(Method::GET, "/api/v1/health?deep=1", None)
                .await;
            res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(res.json()["failed"], serde_json::json!(["disk"]));
            assert_eq!(res.json()["checks"]["blob_root"], serde_json::json!(true));

            // Liveness stays unconditional: a restart frees no disk.
            let res = client.send(Method::GET, "/api/v1/health", None).await;
            res.assert_status(StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn deep_answers_ok_when_the_disk_has_more_than_5_percent_free() {
        let dir = tempfile::tempdir().unwrap();
        let client = with_disk(&dir, |_| {
            Ok(Some(crate::blob::DiskSpace {
                available: 6,
                total: 100,
            }))
        });
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::OK);
        assert_eq!(res.json()["checks"]["disk"], serde_json::json!(true));
    }

    /// A filesystem the relay cannot stat is not one it can count on.
    #[tokio::test]
    async fn deep_answers_503_when_the_disk_cannot_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let client = with_disk(&dir, |_| Err(std::io::Error::other("stale handle")));
        let res = client
            .send(Method::GET, "/api/v1/health?deep=1", None)
            .await;
        res.assert_status(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.json()["failed"], serde_json::json!(["disk"]));
    }

    #[test]
    fn room_is_strictly_more_than_5_percent() {
        let space = |available, total| crate::blob::DiskSpace { available, total };
        assert!(!space(0, 100).has_room());
        assert!(!space(5, 100).has_room());
        assert!(space(51, 1000).has_room());
        assert!(space(100, 100).has_room());
        // No blocks reported is no ratio to fail on.
        assert!(space(0, 0).has_room());
        // Wide enough not to overflow.
        assert!(space(u64::MAX, u64::MAX).has_room());
        assert!(!space(u64::MAX / 20, u64::MAX).has_room());
    }

    /// The real reading, against the filesystem the test runs on: a root that
    /// does not exist yet is read at its nearest ancestor.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_filesystem_backend_reads_its_disk_before_the_root_exists() {
        use crate::blob::BlobBackend as _;
        let dir = tempfile::tempdir().unwrap();
        let blobs = crate::blob::FsBlobs::new(dir.path().join("not/yet/blobs"));
        let space = blobs
            .disk_space()
            .await
            .unwrap()
            .expect("a reading on unix");
        assert!(space.available <= space.total, "{space:?}");
    }
}
