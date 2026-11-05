//! Relay boot: the real `sunrise-server` router in-process, and the transport
//! factories that reach it.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::chaos::{FaultHandle, Toxic, ToxicConfig};
use sunrise_core::{BoxTransport, CredentialRead, TransportFactory};
use sunrise_server::{ServerConfig, ServerState};
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
