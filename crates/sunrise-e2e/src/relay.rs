//! Relay boot: the real `sunrise-server` router in-process, and the transport
//! factories that reach it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::chaos::{FaultHandle, Toxic, ToxicConfig};
use async_trait::async_trait;
use sunrise_core::{BoxTransport, CredentialRead, TransportFactory};
use sunrise_server::{ServerConfig, ServerState};
use sunrise_sync::transport::{
    BlobCommit, IdentityChainPinned, KeyBundlePin, KeyBundlePinned, RevokeOutcome, SnapshotPin,
    Transport, TransportError,
};
use sunrise_sync::{DeviceSigner, SseTransport};
use tokio::task::JoinHandle;

/// Boot the real `sunrise-server` router on an ephemeral loopback port,
/// in-process. Returns the bound address plus the serving task's handle (abort
/// it to shut the relay down). Follows `server_health_e2e.rs`'s boot pattern.
pub async fn spawn_relay() -> (SocketAddr, JoinHandle<()>) {
    spawn_relay_with(ServerConfig::default(), |s| s).await
}

/// Boot the relay as [`spawn_relay`] does, with its rate limits off.
///
/// For the CLI scenarios. Every `sunrise sync` subcommand, `sync status`
/// included, opens a sync session, and `cli::settle` runs two per device
/// each round. A scenario that converges slowly runs past the relay's
/// default budget of 10 new sessions per device per five minutes, which
/// refuses the rest `RATE_LIMITED` and costs a `sync --once` its whole 30 s
/// budget (#799, #1004). Rate limits are not what those scenarios test.
pub async fn spawn_relay_unlimited() -> (SocketAddr, JoinHandle<()>) {
    let mut config = ServerConfig::default();
    config.limits.enabled = false;
    spawn_relay_with(config, |s| s).await
}

/// Boot the relay with the config and state tweaks a scenario needs — a tiny
/// retained ring, a file-backed `SQLite` path to survive a restart, and so on.
///
/// A config with no `blob_root` gets one of its own, under a fresh temporary
/// directory that lives as long as the serving task: aborting the handle
/// deletes it. Without this every relay fell back to the server's one fixed
/// directory under the system temp directory, shared by every relay and every
/// user on the machine (#850). A config that names a `blob_root` keeps it.
pub async fn spawn_relay_with(
    mut config: ServerConfig,
    tweak: impl FnOnce(ServerState) -> ServerState,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let blob_dir = if config.blob_root.is_none() {
        let dir = tempfile::tempdir().expect("create the relay's blob directory");
        config.blob_root = Some(dir.path().join("blobs"));
        Some(dir)
    } else {
        None
    };
    let state = tweak(ServerState::new(config));
    let handle = tokio::spawn(async move {
        // Owned by the task so the directory is removed when it ends or is
        // aborted, and not before.
        let _blob_dir = blob_dir;
        let _ = sunrise_server::serve(state, listener).await;
    });
    (addr, handle)
}

/// Build a [`TransportFactory`] that reaches `http://{addr}` with the real
/// [`SseTransport`] on every connect attempt (initial connect + every
/// reconnect), presenting the bearer the driver read for that attempt.
///
/// The harness cores open with no credential, so that bearer is absent unless
/// a test writes one through [`sunrise_core::Core::sync_credential`]; the
/// harness relay runs the self-host `NullVerifier`, which accepts an absent
/// bearer. Authenticated access is covered by the sync surface's own tests in
/// `sunrise-server::api::sync`, and by [`signed_ws_factory`]'s suites.
#[must_use]
pub fn ws_factory(addr: SocketAddr) -> TransportFactory {
    let url = format!("http://{addr}");
    Arc::new(move |read: CredentialRead| {
        let url = url.clone();
        Box::pin(async move {
            let t = SseTransport::connect_with_bearer(&url, read.bearer());
            Ok(Box::new(t) as BoxTransport)
        }) as sunrise_core::ConnectFuture
    })
}

/// Build a [`TransportFactory`] that reaches `http://{addr}` bound to `signer`
/// and presenting the bearer the driver read for each attempt — the shape a
/// real deployment uses.
///
/// [`ws_factory`] is the self-host shape: no binding, against a relay running
/// `NullVerifier`. Nothing in this harness had ever been device-bound, which is
/// why the relay's refusal of a revoked device could only be covered through
/// `sunrise-server`'s own HTTP tests.
///
/// The bearer is the core's own [`sunrise_core::Core::sync_credential`], which
/// the driver reads per attempt: write it before
/// [`sunrise_core::Core::start_sync`], as the FFI seam does, and a later write
/// reaches both the live session and the next reconnect. It used to be an
/// `Option<String>` fixed when the factory was built, which presented a bearer
/// the driver never knew about and could not carry a renewal (#273).
///
/// The signer is cloned per attempt because every reconnect is a new transport
/// and has to carry the binding too.
#[must_use]
pub fn signed_ws_factory(addr: SocketAddr, signer: Arc<dyn DeviceSigner>) -> TransportFactory {
    let url = format!("http://{addr}");
    Arc::new(move |read: CredentialRead| {
        let url = url.clone();
        let signer = Arc::clone(&signer);
        Box::pin(async move {
            let t =
                SseTransport::connect_with_bearer(&url, read.bearer()).with_device_signer(signer);
            Ok(Box::new(t) as BoxTransport)
        }) as sunrise_core::ConnectFuture
    })
}

/// Build a [`TransportFactory`] that reaches the relay with a real [`SseTransport`]
/// and wraps every freshly-connected transport in a [`Toxic`] fault injector.
///
/// All connections a single factory opens share the one [`FaultHandle`]
/// returned alongside it, so a test can retune drop/corrupt probabilities or
/// toggle a partition and have it steer the live connection **and** every
/// reconnect. Each connection gets its own RNG stream, seeded from `seed` plus a
/// monotonic connection counter, so a reconnect does not replay the identical
/// fault pattern the previous connection saw.
///
/// It presents the bearer the driver read for each attempt, as [`ws_factory`]
/// does.
#[must_use]
pub fn toxic_ws_factory(
    addr: SocketAddr,
    config: ToxicConfig,
    seed: u64,
) -> (TransportFactory, FaultHandle) {
    let url = format!("http://{addr}");
    let handle = FaultHandle::from_config(config);
    let counter = Arc::new(AtomicU64::new(0));
    let conn_handle = handle.clone();
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        let url = url.clone();
        let faults = conn_handle.clone();
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let conn_seed = seed.wrapping_add(n);
        Box::pin(async move {
            let inner = SseTransport::connect_with_bearer(&url, read.bearer());
            let toxic = Toxic::with_handle(inner, faults, config, conn_seed);
            Ok(Box::new(toxic) as BoxTransport)
        }) as sunrise_core::ConnectFuture
    });
    (factory, handle)
}

/// Build a [`TransportFactory`] like [`ws_factory`]'s, except that the first
/// upload id the relay reserves for an upload of `chunk_count` chunks reaches
/// the core spoiled, so the relay refuses that upload's chunks.
///
/// The relay answers a `PUT` of a chunk under an id that is not `up_` plus 32
/// lowercase hex characters with `400 VALIDATION_INVALID`. That code is not
/// in the wire registry, so the client reads the refusal as
/// `SYNC_OP_INVALID`: the one refusal the core treats as being about the
/// upload id itself, dropping the id and reserving a fresh one on its next
/// attempt (`sunrise_core::blob_sync`). Spoiling the id is the seam because
/// the relay hands out only ids it accepts, and accepts a chunk under any
/// well-formed one; the refusal and its reading are the real relay's and the
/// real client's.
///
/// The chunk count picks the upload out from the core's own uploads, a
/// one-chunk snapshot record among them. Every transport the factory opens
/// shares the returned [`UploadRefusal`], so the one spoiled id is the first
/// across reconnects.
#[must_use]
pub fn refusing_upload_ws_factory(
    addr: SocketAddr,
    chunk_count: u32,
) -> (TransportFactory, UploadRefusal) {
    let url = format!("http://{addr}");
    let log = UploadRefusal {
        inner: Arc::new(RefusalLog {
            chunk_count,
            ..RefusalLog::default()
        }),
    };
    let shared = log.clone();
    let factory: TransportFactory = Arc::new(move |read: CredentialRead| {
        let url = url.clone();
        let log = shared.clone();
        Box::pin(async move {
            let inner = SseTransport::connect_with_bearer(&url, read.bearer());
            Ok(Box::new(RefusesFirstUpload { inner, log }) as BoxTransport)
        }) as sunrise_core::ConnectFuture
    });
    (factory, log)
}

/// What [`refusing_upload_ws_factory`]'s transports did.
#[derive(Debug, Clone)]
pub struct UploadRefusal {
    inner: Arc<RefusalLog>,
}

#[derive(Debug, Default)]
struct RefusalLog {
    /// The chunk count of the uploads this log is about.
    chunk_count: u32,
    /// Upload ids reserved for such an upload.
    inits: AtomicU64,
    /// The spoiled id, once there is one.
    spoiled: Mutex<Option<String>>,
    /// The code of each refused `PUT` under the spoiled id.
    refusals: Mutex<Vec<String>>,
}

impl UploadRefusal {
    /// How many upload ids the relay has reserved for an upload of the
    /// factory's chunk count, the spoiled one included.
    #[must_use]
    pub fn inits(&self) -> u64 {
        self.inner.inits.load(Ordering::Relaxed)
    }

    /// The wire code of each refusal of a chunk under the spoiled id.
    #[must_use]
    pub fn refusals(&self) -> Vec<String> {
        lock(&self.inner.refusals).clone()
    }
}

/// A poisoned lock still holds what was recorded before the panic.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An [`SseTransport`] that spoils the first upload id it is handed for an
/// upload of its log's chunk count.
struct RefusesFirstUpload {
    inner: SseTransport,
    log: UploadRefusal,
}

#[async_trait]
impl Transport for RefusesFirstUpload {
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.inner.send_frame(frame).await
    }

    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner.recv_frame().await
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        self.inner.close().await
    }

    async fn revoke_device(
        &mut self,
        device_id: [u8; 16],
    ) -> Result<RevokeOutcome, TransportError> {
        self.inner.revoke_device(device_id).await
    }

    async fn blob_init(
        &mut self,
        stream_id: &[u8; 16],
        chunk_count: u32,
        size_bytes: u64,
    ) -> Result<String, TransportError> {
        let id = self
            .inner
            .blob_init(stream_id, chunk_count, size_bytes)
            .await?;
        let log = &self.log.inner;
        if chunk_count != log.chunk_count || log.inits.fetch_add(1, Ordering::Relaxed) > 0 {
            return Ok(id);
        }
        // One character too many: no longer 32 hex characters after `up_`.
        let spoiled = format!("{id}0");
        *lock(&log.spoiled) = Some(spoiled.clone());
        Ok(spoiled)
    }

    async fn blob_put_chunk(
        &mut self,
        upload_id: &str,
        chunk_idx: u32,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        let put = self.inner.blob_put_chunk(upload_id, chunk_idx, bytes).await;
        let log = &self.log.inner;
        if let Err(TransportError::Server { code, .. }) = &put {
            if lock(&log.spoiled).as_deref() == Some(upload_id) {
                lock(&log.refusals).push((*code).to_owned());
            }
        }
        put
    }

    async fn blob_finalize(
        &mut self,
        upload_id: &str,
        ciphertext_hash: &[u8; 32],
        chunk_hashes: &[[u8; 32]],
    ) -> Result<BlobCommit, TransportError> {
        self.inner
            .blob_finalize(upload_id, ciphertext_hash, chunk_hashes)
            .await
    }

    async fn blob_fetch(&mut self, blob_id: &[u8; 16]) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner.blob_fetch(blob_id).await
    }

    async fn snapshot_latest(
        &mut self,
        stream_id: &[u8; 16],
    ) -> Result<Option<SnapshotPin>, TransportError> {
        self.inner.snapshot_latest(stream_id).await
    }

    async fn snapshot_pin(
        &mut self,
        stream_id: &[u8; 16],
        blob_id: &[u8; 16],
    ) -> Result<SnapshotPin, TransportError> {
        self.inner.snapshot_pin(stream_id, blob_id).await
    }

    async fn key_bundle_latest(&mut self) -> Result<Option<KeyBundlePin>, TransportError> {
        self.inner.key_bundle_latest().await
    }

    async fn key_bundle_pin(
        &mut self,
        blob_id: &[u8; 16],
        manifest_digest: &[u8; 32],
        replaces: Option<&[u8; 16]>,
    ) -> Result<KeyBundlePinned, TransportError> {
        self.inner
            .key_bundle_pin(blob_id, manifest_digest, replaces)
            .await
    }

    async fn identity_chain_latest(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        self.inner.identity_chain_latest().await
    }

    async fn identity_chain_pin(
        &mut self,
        chain: &[u8],
        replaces: Option<&[u8; 32]>,
    ) -> Result<IdentityChainPinned, TransportError> {
        self.inner.identity_chain_pin(chain, replaces).await
    }

    async fn recovery_blob_replace(&mut self, blob: &[u8]) -> Result<(), TransportError> {
        self.inner.recovery_blob_replace(blob).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Boot a relay through the harness and hand back the blob root its state
    /// was built with.
    async fn relay_blob_root(config: ServerConfig) -> (Option<PathBuf>, JoinHandle<()>) {
        let mut seen = None;
        let (_, handle) = spawn_relay_with(config, |s| {
            seen = Some(s.config.blob_root.clone());
            s
        })
        .await;
        (seen.expect("the harness runs the tweak"), handle)
    }

    /// #850: two relays never share a blob directory, neither uses the
    /// server's fixed fallback under the system temp directory, and each one's
    /// directory goes away with its relay.
    #[tokio::test]
    async fn each_relay_gets_its_own_blob_root_removed_on_abort() {
        let (a, relay_a) = relay_blob_root(ServerConfig::default()).await;
        let (b, relay_b) = relay_blob_root(ServerConfig::default()).await;
        let a = a.expect("the harness sets a blob root");
        let b = b.expect("the harness sets a blob root");
        assert_ne!(a, b, "two relays share one blob root");
        let shared = std::env::temp_dir().join("sunrise-self-host-blobs");
        assert_ne!(a, shared);
        assert_ne!(b, shared);

        let parent_a = a.parent().expect("blob root has a parent").to_path_buf();
        assert!(parent_a.is_dir(), "the blob directory exists while serving");
        relay_a.abort();
        let _ = relay_a.await;
        assert!(
            !parent_a.exists(),
            "aborting the relay leaves its blob directory"
        );

        let parent_b = b.parent().expect("blob root has a parent").to_path_buf();
        assert!(
            parent_b.is_dir(),
            "aborting one relay removed the other's directory"
        );
        relay_b.abort();
        let _ = relay_b.await;
    }

    /// A config that names its own blob root keeps it.
    #[tokio::test]
    async fn an_explicit_blob_root_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mine");
        let (seen, relay) = relay_blob_root(ServerConfig {
            blob_root: Some(root.clone()),
            ..ServerConfig::default()
        })
        .await;
        assert_eq!(seen, Some(root));
        relay.abort();
        let _ = relay.await;
    }
}
