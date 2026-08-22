//! Client sync driver.
//!
//! The driver is the long-lived task that connects a [`Core`] to the relay,
//! negotiates the session, subscribes to the vault's streams, drains the
//! persistent outbox, and materializes inbound remote ops via
//! [`Core::apply_remote`]. It owns the live [`crate::events::SyncStatus`] and
//! publishes every state transition on `Core::sync_status()`.
//!
//! # Determinism
//!
//! Core command/query logic stays deterministic (injected clock / RNG). The
//! driver is I/O: it uses tokio timers for backoff and the injected RNG only
//! for backoff jitter. All database access happens through synchronous
//! [`Core`] helper methods that lock, read/write, and release **before** any
//! `.await` — the db mutex is never held across an await point.
//!
//! # Ownership (no Arc cycle)
//!
//! [`Core`] owns the driver's [`JoinHandle`](tokio::task::JoinHandle) and an
//! [`Arc<SyncShared>`]. The driver task holds a [`Weak<Core>`] (upgraded per
//! session) plus a clone of the same `Arc<SyncShared>`. `SyncShared` holds no
//! back-reference to `Core`, so there is no reference cycle; dropping the
//! `Core` (or calling [`Core::close`]) cancels the task.
//!
//! # Transport factory
//!
//! The driver is transport-agnostic: it takes a [`TransportFactory`] closure
//! that yields a fresh [`Box<dyn Transport>`] per connection attempt. The
//! production WebSocket factory (built on `sunrise_sync::WsTransport`) and the
//! in-process loopback used by the tests are both just factories.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{broadcast, Notify};

use crate::config::Rng;
use crate::core::Core;
use crate::events::{DomainEvent, SyncStatus};
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_id::EntityKind;
use sunrise_sync::{Backoff, SyncState, Transport, TransportError};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, CaughtUpPayload, FrameFlags, Hello, MsgKind,
    OpBatchPayload, SubscribeEntry, SubscribePayload, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};

/// Boxed transport produced by a [`TransportFactory`].
pub type BoxTransport = Box<dyn Transport>;

/// Future returned by a [`TransportFactory`]: yields a connected transport.
pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<BoxTransport, TransportError>> + Send>>;

/// A factory that opens a fresh transport on every call. Called once per
/// connect attempt (initial connect and every reconnect after a drop), so it
/// must be able to produce a brand-new connection each time.
pub type TransportFactory = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

/// Client-side sync configuration carried in [`crate::CoreConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// Relay `/sync` endpoint URL (e.g. `wss://relay.example/sync`). Used by
    /// the production WebSocket factory the app assembles; the driver itself
    /// takes an already-built [`TransportFactory`].
    pub url: String,
}

// ---------------------------------------------------------------------------
// Shared live state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SharedState {
    state: SyncState,
    outbox_pending: u64,
    last_sync_ms: Option<u64>,
    /// Distinct originating device ids observed in inbound ops this session.
    peers: HashSet<[u8; 16]>,
}

/// Live, shared sync state. Read by `Core::query(SyncStatus)`; written by the
/// driver, which broadcasts a fresh [`SyncStatus`] snapshot on every change.
#[derive(Debug)]
pub(crate) struct SyncShared {
    inner: Mutex<SharedState>,
    sync_tx: broadcast::Sender<SyncStatus>,
    /// Poked by `Core::submit` (when sync is active) so the driver drains the
    /// outbox immediately instead of waiting for the next inbound frame.
    submit: Notify,
    shutdown_notify: Notify,
    shutdown: AtomicBool,
    active: AtomicBool,
}

impl SyncShared {
    pub(crate) fn new(sync_tx: broadcast::Sender<SyncStatus>, pending: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(SharedState {
                state: SyncState::Disconnected,
                outbox_pending: pending,
                last_sync_ms: None,
                peers: HashSet::new(),
            }),
            sync_tx,
            submit: Notify::new(),
            shutdown_notify: Notify::new(),
            shutdown: AtomicBool::new(false),
            active: AtomicBool::new(false),
        })
    }

    fn status_of(s: &SharedState) -> SyncStatus {
        SyncStatus {
            state: s.state,
            outbox_pending: u32::try_from(s.outbox_pending).unwrap_or(u32::MAX),
            peer_devices: u32::try_from(s.peers.len()).unwrap_or(u32::MAX),
            last_sync_ms: s.last_sync_ms,
        }
    }

    pub(crate) fn set_state(&self, new: SyncState) {
        let mut s = self.inner.lock();
        if s.state == new {
            return;
        }
        s.state = new;
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    pub(crate) fn set_pending(&self, n: u64) {
        let mut s = self.inner.lock();
        if s.outbox_pending == n {
            return;
        }
        s.outbox_pending = n;
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    fn note_peer(&self, device_id: [u8; 16]) {
        let mut s = self.inner.lock();
        if !s.peers.insert(device_id) {
            return;
        }
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    fn mark_synced(&self, ms: u64) {
        let mut s = self.inner.lock();
        s.last_sync_ms = Some(ms);
        let snap = Self::status_of(&s);
        drop(s);
        let _ = self.sync_tx.send(snap);
    }

    /// Fields served to `Core::query(SyncStatus)` alongside the DB-authoritative
    /// outbox count: `(state, last_sync_ms, peer_devices)`.
    pub(crate) fn status_fields(&self) -> (SyncState, Option<u64>, u32) {
        let s = self.inner.lock();
        (
            s.state,
            s.last_sync_ms,
            u32::try_from(s.peers.len()).unwrap_or(u32::MAX),
        )
    }

    fn current_state(&self) -> SyncState {
        self.inner.lock().state
    }

    pub(crate) fn mark_active(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) fn poke_submit(&self) {
        self.submit.notify_one();
    }

    async fn submit_notified(&self) {
        self.submit.notified().await;
    }

    pub(crate) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
    }

    fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Resolves when shutdown has been requested. Lost-wakeup-safe: re-checks
    /// the flag after arming the notification.
    async fn shutdown_notified(&self) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let fut = self.shutdown_notify.notified();
            tokio::pin!(fut);
            fut.as_mut().enable();
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            fut.await;
        }
    }
}

// ---------------------------------------------------------------------------
// Driver task
// ---------------------------------------------------------------------------

/// In-flight (sent, not yet acked) outbox batch: the op ids it carried.
struct InflightBatch {
    op_ids: Vec<[u8; 16]>,
}

enum SessionEnd {
    /// Shutdown requested; stop the driver.
    Shutdown,
    /// Transport dropped / closed; reconnect after backoff.
    Disconnected,
}

/// One event the session pump reacts to.
enum SessionEvent {
    Shutdown,
    Submit,
    Recv(Result<Option<Vec<u8>>, TransportError>),
}

/// Driver entry point. Runs until shutdown is requested or the `Core` is
/// dropped. Reconnects with exponential backoff between sessions.
pub(crate) async fn run(
    weak: Weak<Core>,
    shared: Arc<SyncShared>,
    factory: TransportFactory,
    rng: Arc<dyn Rng>,
) {
    let mut backoff = Backoff::canonical();
    while !shared.is_shutdown() {
        // Connect (cancellable by shutdown).
        let connect_fut = factory();
        let connected = tokio::select! {
            biased;
            () = shared.shutdown_notified() => break,
            res = connect_fut => res,
        };
        let transport = match connected {
            Ok(t) => {
                backoff.reset();
                t
            }
            Err(e) => {
                // "The client isn't syncing" is the single most common
                // support question, and this is the line that answers it:
                // the relay is unreachable, here is what the socket said.
                tracing::warn!(
                    ev = "sync.session.error",
                    err_code = "SYNC_CONNECT_FAILED",
                    err_kind = "transient",
                    retryable = true,
                    result = "failed",
                    cause = %e,
                    "relay connect failed"
                );
                if !backoff_sleep(&mut backoff, rng.as_ref(), &shared).await {
                    break;
                }
                continue;
            }
        };

        // A session needs the Core alive; if it's gone, stop.
        let Some(core) = weak.upgrade() else { break };
        let end = session(&core, &shared, transport).await;
        drop(core);
        shared.set_state(SyncState::Disconnected);
        tracing::info!(
            ev = "sync.session.closed",
            result = match end {
                SessionEnd::Shutdown => "ok",
                SessionEnd::Disconnected => "failed",
            },
            "sync session ended"
        );
        match end {
            SessionEnd::Shutdown => break,
            SessionEnd::Disconnected => {
                if !backoff_sleep(&mut backoff, rng.as_ref(), &shared).await {
                    break;
                }
            }
        }
    }
    shared.set_state(SyncState::Disconnected);
}

/// Sleep for the next backoff delay, interruptible by shutdown. Returns
/// `false` if shutdown was requested (caller should stop).
async fn backoff_sleep(backoff: &mut Backoff, rng: &dyn Rng, shared: &SyncShared) -> bool {
    if shared.is_shutdown() {
        return false;
    }
    // Never give up on a long-lived client: once the policy is exhausted, cap
    // at the max delay and keep retrying.
    let delay = if let Some(d) = backoff.next_delay(rng_unit(rng)) {
        backoff.record_attempt();
        d
    } else {
        backoff.reset();
        Duration::from_millis(30_000)
    };
    tracing::debug!(
        ev = "sync.backoff",
        attempt = u64::from(backoff.attempt()),
        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        "waiting before reconnect"
    );
    tokio::select! {
        biased;
        () = shared.shutdown_notified() => false,
        () = tokio::time::sleep(delay) => true,
    }
}

/// One connected session: handshake → subscribe → drain + pump frames.
#[allow(clippy::too_many_lines)]
async fn session(core: &Arc<Core>, shared: &SyncShared, mut transport: BoxTransport) -> SessionEnd {
    // ---- Handshake: Hello → HelloAck ----
    let hello = build_hello(core.app_string());
    let Ok(hello_bytes) = encode_hello(&hello) else {
        return SessionEnd::Disconnected;
    };
    let Ok(hello_frame) = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hello_bytes) else {
        return SessionEnd::Disconnected;
    };
    if transport.send_frame(hello_frame).await.is_err() {
        return SessionEnd::Disconnected;
    }
    loop {
        let recv = tokio::select! {
            biased;
            () = shared.shutdown_notified() => {
                let _ = transport.close().await;
                return SessionEnd::Shutdown;
            }
            r = transport.recv_frame() => r,
        };
        match recv {
            Ok(Some(bytes)) => match decode_frame(&bytes) {
                Ok((h, _)) if h.msg_kind == MsgKind::HelloAck => break,
                Ok((h, _)) if h.msg_kind == MsgKind::Error => return SessionEnd::Disconnected,
                // Any other frame before HelloAck: keep waiting.
                Ok(_) => {}
                Err(_) => return SessionEnd::Disconnected,
            },
            Ok(None) | Err(_) => return SessionEnd::Disconnected,
        }
    }

    // ---- Subscribe to all known streams with their cursors ----
    let Ok(entries) = core.sync_subscribe_entries() else {
        return SessionEnd::Disconnected;
    };
    let mut subscribed: HashSet<[u8; 16]> = entries.iter().map(|e| e.stream_id).collect();
    let sub = SubscribePayload { streams: entries };
    let Ok(sub_bytes) = sub.encode() else {
        return SessionEnd::Disconnected;
    };
    let Ok(sub_frame) = encode_frame(MsgKind::Subscribe, FrameFlags::EMPTY, &sub_bytes) else {
        return SessionEnd::Disconnected;
    };
    if transport.send_frame(sub_frame).await.is_err() {
        return SessionEnd::Disconnected;
    }
    shared.set_state(SyncState::CatchingUp);

    let mut caught_up: HashSet<[u8; 16]> = HashSet::new();
    let mut inflight: HashMap<u64, InflightBatch> = HashMap::new();
    let mut inflight_ops: HashSet<[u8; 16]> = HashSet::new();
    let mut batch_counter: u64 = 0;
    let mut pending_sends: Vec<Vec<u8>> = Vec::new();

    // Initial outbox drain (fresh session: everything unacked is (re)sent —
    // idempotent apply on the peer tolerates replays).
    if build_outbox_frames(
        core,
        &mut subscribed,
        &mut inflight_ops,
        &mut inflight,
        &mut batch_counter,
        &mut pending_sends,
    )
    .is_err()
    {
        return SessionEnd::Disconnected;
    }
    if let Ok(p) = core.sync_pending() {
        shared.set_pending(p);
    }
    maybe_live(core, shared, &subscribed, &caught_up);

    // ---- Main pump ----
    //
    // Sends happen sequentially at the top of the loop (no recv future alive),
    // so the transport is never mutably borrowed by two branches at once.
    loop {
        for frame in pending_sends.drain(..) {
            if transport.send_frame(frame).await.is_err() {
                return SessionEnd::Disconnected;
            }
        }

        let ev = tokio::select! {
            biased;
            () = shared.shutdown_notified() => SessionEvent::Shutdown,
            () = shared.submit_notified() => SessionEvent::Submit,
            r = transport.recv_frame() => SessionEvent::Recv(r),
        };

        match ev {
            SessionEvent::Shutdown => {
                let _ = transport.close().await;
                return SessionEnd::Shutdown;
            }
            SessionEvent::Submit => {
                if build_outbox_frames(
                    core,
                    &mut subscribed,
                    &mut inflight_ops,
                    &mut inflight,
                    &mut batch_counter,
                    &mut pending_sends,
                )
                .is_err()
                {
                    return SessionEnd::Disconnected;
                }
            }
            SessionEvent::Recv(Ok(Some(bytes))) => {
                if handle_frame(
                    core,
                    shared,
                    &bytes,
                    &mut subscribed,
                    &mut caught_up,
                    &mut inflight,
                    &mut inflight_ops,
                    &mut pending_sends,
                )
                .await
                .is_err()
                {
                    return SessionEnd::Disconnected;
                }
                maybe_live(core, shared, &subscribed, &caught_up);
            }
            SessionEvent::Recv(Ok(None) | Err(_)) => return SessionEnd::Disconnected,
        }
    }
}

/// Process one inbound frame. `Err(())` signals a disconnect (reconnect);
/// malformed / non-fatal frames are logged-and-skipped as `Ok(())`.
#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    core: &Arc<Core>,
    shared: &SyncShared,
    bytes: &[u8],
    subscribed: &mut HashSet<[u8; 16]>,
    caught_up: &mut HashSet<[u8; 16]>,
    inflight: &mut HashMap<u64, InflightBatch>,
    inflight_ops: &mut HashSet<[u8; 16]>,
    pending_sends: &mut Vec<Vec<u8>>,
) -> Result<(), ()> {
    let Ok((header, payload)) = decode_frame(bytes) else {
        // Junk frame: ignore rather than tearing down the session.
        return Ok(());
    };
    match header.msg_kind {
        MsgKind::Ack => {
            if let Ok(ack) = AckPayload::decode(&payload) {
                if let Some(batch) = inflight.remove(&ack.batch_id) {
                    for id in &batch.op_ids {
                        inflight_ops.remove(id);
                    }
                    let pending = core.sync_mark_acked(&batch.op_ids).map_err(|_| ())?;
                    shared.set_pending(pending);
                    shared.mark_synced(core.now_ms());
                }
            }
        }
        MsgKind::OpBatch => {
            if let Ok(batch) = OpBatchPayload::decode(&payload) {
                for env in &batch.ops {
                    // Peer accounting (best-effort): decode the envelope header.
                    if let Ok(oe) = sunrise_crypto::decode_envelope(env) {
                        shared.note_peer(oe.device_id);
                    }
                    match core.apply_remote(env).await {
                        Ok(Some(DomainEvent::Created(r))) if r.kind() == EntityKind::Stream => {
                            // Newly-learned stream (StreamCreate): subscribe to
                            // its channel so its task ops flow.
                            let sid = *r.bytes();
                            if subscribed.insert(sid) {
                                if let Ok(frame) = encode_subscribe_one(sid) {
                                    pending_sends.push(frame);
                                }
                            }
                            shared.mark_synced(core.now_ms());
                        }
                        Ok(Some(_)) => shared.mark_synced(core.now_ms()),
                        // Idempotent re-receive (None) or untrusted / invalid
                        // remote op (Err): skip and keep the session.
                        Ok(None) | Err(_) => {}
                    }
                }
            }
        }
        // CaughtUp rides the StreamUpdate kind (see wire-protocol payloads).
        MsgKind::StreamUpdate => {
            if let Ok(cu) = CaughtUpPayload::decode(&payload) {
                caught_up.insert(cu.stream_id);
            }
        }
        MsgKind::Ping => {
            if let Ok(frame) = encode_frame(MsgKind::Pong, FrameFlags::EMPTY, &[]) {
                pending_sends.push(frame);
            }
        }
        // Server-initiated close: reconnect.
        MsgKind::Close => return Err(()),
        // Nack / Error are non-fatal in v1 self-host; every other kind is
        // ignored. All fall through to a no-op.
        _ => {}
    }
    Ok(())
}

/// Drain the persistent outbox (skipping already-in-flight ops), grouping by
/// stream into `OpBatch` frames with monotonically increasing `batch_id`s.
///
/// For every stream we emit an `OpBatch` on we also ensure a `Subscribe` frame
/// has been sent for its channel (tracked via `subscribed`). Without this, a
/// device that *creates* a stream mid-session — and therefore only ever *sends*
/// on its channel — would never subscribe to *receive* peers' ops for that
/// stream until the next reconnect (the initial `Subscribe` covered only the
/// streams that existed at connect time, and inbound `StreamCreate` handling
/// only subscribes the *other* side). Subscribing here makes two-way sync on a
/// locally-authored stream converge immediately.
fn build_outbox_frames(
    core: &Core,
    subscribed: &mut HashSet<[u8; 16]>,
    inflight_ops: &mut HashSet<[u8; 16]>,
    inflight: &mut HashMap<u64, InflightBatch>,
    batch_counter: &mut u64,
    out: &mut Vec<Vec<u8>>,
) -> Result<(), ()> {
    let groups = core.sync_outbox_grouped(inflight_ops).map_err(|_| ())?;
    for (stream_id, ops) in groups {
        if ops.is_empty() {
            continue;
        }
        if subscribed.insert(stream_id) {
            let frame = encode_subscribe_one(stream_id)?;
            out.push(frame);
        }
        *batch_counter += 1;
        let batch_id = *batch_counter;
        let op_ids: Vec<[u8; 16]> = ops.iter().map(|(id, _)| *id).collect();
        let envelopes: Vec<Vec<u8>> = ops.into_iter().map(|(_, env)| env).collect();
        let payload = OpBatchPayload {
            ops: envelopes,
            batch_id,
            stream_id,
        };
        let Ok(bytes) = payload.encode() else {
            return Err(());
        };
        let Ok(frame) = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &bytes) else {
            return Err(());
        };
        for id in &op_ids {
            inflight_ops.insert(*id);
        }
        inflight.insert(batch_id, InflightBatch { op_ids });
        out.push(frame);
    }
    Ok(())
}

/// Transition to `Live` once every subscribed stream is caught up and the
/// outbox is empty (all local ops acked).
fn maybe_live(
    core: &Core,
    shared: &SyncShared,
    subscribed: &HashSet<[u8; 16]>,
    caught_up: &HashSet<[u8; 16]>,
) {
    if shared.current_state() != SyncState::CatchingUp {
        return;
    }
    let pending = core.sync_pending().unwrap_or(0);
    if pending == 0 && subscribed.iter().all(|s| caught_up.contains(s)) {
        shared.set_state(SyncState::Live);
        // `Live` is the state a user cares about ("am I synced?"), and it is
        // the only one the driver reaches by *inference* rather than by a
        // wire event — so it is worth a line saying what the inference was
        // over.
        tracing::info!(
            ev = "sync.session.opened",
            n_streams = subscribed.len() as u64,
            "sync session live"
        );
    }
}

fn build_hello(app: &str) -> Hello {
    let (client_app_v, client_platform) = match app.split_once('+') {
        Some((v, p)) => (v.to_string(), p.to_string()),
        None => (app.to_string(), std::env::consts::OS.to_string()),
    };
    Hello {
        client_app_v,
        client_platform,
        wire_proto_supported: vec![u32::from(WIRE_PROTO_V)],
        doc_schema_min: u32::from(DOC_SCHEMA_V),
        doc_schema_max: u32::from(DOC_SCHEMA_V),
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
        trace: String::new(),
    }
}

fn encode_hello(h: &Hello) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(h, &mut buf).map_err(|_| ())?;
    Ok(buf)
}

fn encode_subscribe_one(stream_id: [u8; 16]) -> Result<Vec<u8>, ()> {
    let sub = SubscribePayload {
        streams: vec![SubscribeEntry {
            cursors: Vec::new(),
            stream_id,
        }],
    };
    let bytes = sub.encode().map_err(|_| ())?;
    encode_frame(MsgKind::Subscribe, FrameFlags::EMPTY, &bytes).map_err(|_| ())
}

/// Map RNG output to a jitter unit in `[0, 1]` for [`Backoff::next_delay`].
fn rng_unit(rng: &dyn Rng) -> f64 {
    let mut b = [0u8; 8];
    rng.fill_bytes(&mut b);
    // 53-bit mantissa → uniform [0, 1).
    let v = u64::from_le_bytes(b) >> 11;
    #[allow(clippy::cast_precision_loss)]
    {
        v as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    //! Driver tests against an in-process fake relay over a channel transport
    //! (no sockets). Each `on_connect`/`inbound`/`duplicate`/`reconnect`/
    //! `submit_while_live` test drives the real driver task; the fake server
    //! scripts the protocol side.

    use super::{BoxTransport, ConnectFuture, SyncConfig, TransportFactory};
    use crate::config::Clock;
    use crate::{Command, Core, CoreConfig, DomainEvent, Query, QueryResult, SystemRng, Unlock};
    use async_trait::async_trait;
    use std::collections::{HashSet, VecDeque};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::TaskDraft;
    use sunrise_sync::{SyncState, Transport, TransportError};
    use sunrise_wire_protocol::{
        decode_frame, encode_frame, AckPayload, CaughtUpPayload, FrameFlags, HelloAck, MsgKind,
        OpBatchPayload, SubscribePayload,
    };
    use tokio::sync::{broadcast, mpsc};
    use tokio::time::timeout;

    const ROOT: [u8; 32] = [0x5a; 32];
    const T0: u64 = 1_700_000_000_000;

    #[derive(Debug)]
    struct TestClock(u64);
    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0
        }
    }

    fn make_cfg(dir: &Path) -> CoreConfig {
        CoreConfig {
            vault_dir: dir.to_path_buf(),
            clock: Arc::new(TestClock(T0)),
            rng: Arc::new(SystemRng),
            app: "0.1.0+test".into(),
            sync: Some(SyncConfig {
                url: "ws://unused/sync".into(),
            }),
        }
    }

    async fn open_arc(dir: &Path) -> Arc<Core> {
        Arc::new(
            Core::open(
                make_cfg(dir),
                Unlock::DevicePaired(VaultRootKey::from_bytes(ROOT)),
            )
            .await
            .unwrap(),
        )
    }

    // ---- channel-backed duplex transport ----

    struct ChannelTransport {
        tx: mpsc::UnboundedSender<Vec<u8>>,
        rx: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    #[async_trait]
    impl Transport for ChannelTransport {
        async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
            self.tx
                .send(frame)
                .map_err(|_| TransportError::Unavailable("peer gone".into()))
        }
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(self.rx.recv().await)
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            self.rx.close();
            Ok(())
        }
    }

    fn duplex() -> (ChannelTransport, ChannelTransport) {
        let (a_tx, b_rx) = mpsc::unbounded_channel();
        let (b_tx, a_rx) = mpsc::unbounded_channel();
        (
            ChannelTransport { tx: a_tx, rx: a_rx },
            ChannelTransport { tx: b_tx, rx: b_rx },
        )
    }

    // ---- fake server ----

    #[derive(Clone, Default)]
    struct Script {
        /// Remote OpBatches to inject after the first Subscribe: `(stream, envelopes)`.
        inject: Vec<([u8; 16], Vec<Vec<u8>>)>,
        /// Simulate a transport drop right after the first Subscribe.
        close_after_subscribe: bool,
    }

    struct ServerInner {
        connect_count: u64,
        scripts: VecDeque<Script>,
    }
    type SharedServer = Arc<parking_lot::Mutex<ServerInner>>;

    struct RecvBatch {
        ops: Vec<Vec<u8>>,
    }

    fn harness(
        scripts: Vec<Script>,
    ) -> (
        TransportFactory,
        SharedServer,
        mpsc::UnboundedReceiver<RecvBatch>,
    ) {
        let server: SharedServer = Arc::new(parking_lot::Mutex::new(ServerInner {
            connect_count: 0,
            scripts: scripts.into_iter().collect(),
        }));
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let factory_server = server.clone();
        let factory: TransportFactory = Arc::new(move || {
            let server = factory_server.clone();
            let batch_tx = batch_tx.clone();
            let fut = async move {
                let (client_end, server_end) = duplex();
                let script = {
                    let mut s = server.lock();
                    s.connect_count += 1;
                    s.scripts.pop_front().unwrap_or_default()
                };
                tokio::spawn(run_fake_server(server_end, script, batch_tx));
                Ok(Box::new(client_end) as BoxTransport)
            };
            Box::pin(fut) as ConnectFuture
        });
        (factory, server, batch_rx)
    }

    async fn run_fake_server(
        mut t: ChannelTransport,
        script: Script,
        batch_tx: mpsc::UnboundedSender<RecvBatch>,
    ) {
        // Expect Hello, reply HelloAck.
        let Ok(Some(frame)) = t.recv_frame().await else {
            return;
        };
        let Ok((h, _)) = decode_frame(&frame) else {
            return;
        };
        if h.msg_kind != MsgKind::Hello {
            return;
        }
        let ack = HelloAck {
            server_app_v: "fake".into(),
            wire_proto: 1,
            crypto_suite: 1,
            doc_schema_floor: 1,
            capabilities: 0,
            server_time_ms: T0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&ack, &mut buf).unwrap();
        let ack_frame = encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &buf).unwrap();
        if t.send_frame(ack_frame).await.is_err() {
            return;
        }

        let mut subscribed: HashSet<[u8; 16]> = HashSet::new();
        let mut injected = false;
        loop {
            let frame = match t.recv_frame().await {
                Ok(Some(f)) => f,
                Ok(None) | Err(_) => return,
            };
            let Ok((hh, payload)) = decode_frame(&frame) else {
                continue;
            };
            match hh.msg_kind {
                MsgKind::Subscribe => {
                    if let Ok(sub) = SubscribePayload::decode(&payload) {
                        for entry in &sub.streams {
                            if subscribed.insert(entry.stream_id) {
                                let cu = CaughtUpPayload {
                                    stream_id: entry.stream_id,
                                };
                                let bytes = cu.encode().unwrap();
                                let f =
                                    encode_frame(MsgKind::StreamUpdate, FrameFlags::EMPTY, &bytes)
                                        .unwrap();
                                if t.send_frame(f).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    if !injected {
                        injected = true;
                        for (stream_id, envs) in &script.inject {
                            let payload = OpBatchPayload {
                                ops: envs.clone(),
                                batch_id: 0,
                                stream_id: *stream_id,
                            };
                            let bytes = payload.encode().unwrap();
                            let f =
                                encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &bytes).unwrap();
                            if t.send_frame(f).await.is_err() {
                                return;
                            }
                        }
                        if script.close_after_subscribe {
                            return; // simulate a transport drop
                        }
                    }
                }
                MsgKind::OpBatch => {
                    if let Ok(batch) = OpBatchPayload::decode(&payload) {
                        let _ = batch_tx.send(RecvBatch {
                            ops: batch.ops.clone(),
                        });
                        let ack = AckPayload {
                            batch_id: batch.batch_id,
                            stream_id: batch.stream_id,
                            server_first_seen_ms: T0,
                        };
                        let bytes = ack.encode().unwrap();
                        let f = encode_frame(MsgKind::Ack, FrameFlags::EMPTY, &bytes).unwrap();
                        if t.send_frame(f).await.is_err() {
                            return;
                        }
                    }
                }
                MsgKind::Close => return,
                _ => {}
            }
        }
    }

    // ---- remote-op builder: a second Core (device B, same vault root) ----

    /// Create `titles.len()` tasks on device B's inbox and return
    /// `(B's cert, inbox stream id, sealed envelope per task in seq order)`.
    async fn make_remote_tasks(titles: &[&str]) -> (Vec<u8>, [u8; 16], Vec<Vec<u8>>) {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(
            make_cfg(dir.path()),
            Unlock::DevicePaired(VaultRootKey::from_bytes(ROOT)),
        )
        .await
        .unwrap();
        for title in titles {
            core.submit(Command::CreateTask(TaskDraft {
                title: (*title).into(),
                ..Default::default()
            }))
            .await
            .unwrap();
        }
        let cert = core.device_cert();
        let groups = core.sync_outbox_grouped(&HashSet::new()).unwrap();
        assert_eq!(groups.len(), 1, "all tasks land on the inbox stream");
        let (stream, ops) = groups.into_iter().next().unwrap();
        let envs: Vec<Vec<u8>> = ops.into_iter().map(|(_, env)| env).collect();
        core.close().await.unwrap();
        drop(dir);
        (cert, stream, envs)
    }

    // ---- assertion helpers (event-driven, generous timeouts) ----

    async fn collect_until_live(rx: &mut broadcast::Receiver<crate::SyncStatus>) -> Vec<SyncState> {
        let mut seen = Vec::new();
        timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Ok(s) => {
                        if seen.last() != Some(&s.state) {
                            seen.push(s.state);
                        }
                        if s.state == SyncState::Live {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        })
        .await
        .expect("timed out waiting for Live");
        seen
    }

    async fn wait_created(rx: &mut broadcast::Receiver<DomainEvent>) {
        timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Ok(DomainEvent::Created(_)) => return,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => panic!("changes closed"),
                }
            }
        })
        .await
        .expect("timed out waiting for Created");
    }

    async fn inbox_len(core: &Core) -> usize {
        match core.query(Query::Inbox).await.unwrap() {
            QueryResult::StreamTasks(v) => v.len(),
            other => panic!("expected StreamTasks, got {other:?}"),
        }
    }

    // ---- Test (a): pending outbox drains, acks, reaches Live ----
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_connect_drains_pending_outbox_and_reaches_live() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        core.submit(Command::CreateTask(TaskDraft {
            title: "pending".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
        assert_eq!(core.sync_pending().unwrap(), 1);

        let mut status_rx = core.sync_status();
        let (factory, _server, mut batch_rx) = harness(vec![]);
        core.start_sync(factory).unwrap();

        let rb = timeout(Duration::from_secs(10), batch_rx.recv())
            .await
            .expect("server received a batch")
            .unwrap();
        assert_eq!(rb.ops.len(), 1, "the one pending op is delivered");

        let seq = collect_until_live(&mut status_rx).await;
        assert!(
            seq.contains(&SyncState::CatchingUp),
            "went through CatchingUp: {seq:?}"
        );
        assert_eq!(
            *seq.last().unwrap(),
            SyncState::Live,
            "reached Live: {seq:?}"
        );
        // Reaching Live implies the ack was applied → outbox empty in the DB.
        assert_eq!(core.sync_pending().unwrap(), 0);

        match core.query(Query::SyncStatus).await.unwrap() {
            QueryResult::SyncStatus(s) => {
                assert_eq!(s.state, SyncState::Live);
                assert_eq!(s.outbox_pending, 0);
            }
            other => panic!("expected sync status, got {other:?}"),
        }
    }

    // ---- Test (b): inbound remote OpBatch materializes + advances cursor ----
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inbound_remote_op_materializes_and_advances_cursor() {
        let (cert_b, stream, envs) = make_remote_tasks(&["from B"]).await;
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        core.submit(Command::TrustDevice { cert_cbor: cert_b })
            .await
            .unwrap();

        let mut changes = core.changes();
        let script = Script {
            inject: vec![(stream, envs)],
            close_after_subscribe: false,
        };
        let (factory, _server, _batch_rx) = harness(vec![script]);
        core.start_sync(factory).unwrap();

        wait_created(&mut changes).await;
        assert_eq!(inbox_len(&core).await, 1, "remote task materialized");

        // Cursor advanced for device B on the inbox stream.
        let entries = core.sync_subscribe_entries().unwrap();
        let entry = entries
            .iter()
            .find(|e| e.stream_id == stream)
            .expect("inbox stream is subscribed");
        assert!(
            entry.cursors.iter().any(|c| c.last_applied_seq >= 1),
            "cursor advanced: {:?}",
            entry.cursors
        );
    }

    // ---- Test (c): duplicate inbound delivery is idempotent ----
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_inbound_delivery_is_idempotent() {
        let (cert_b, stream, envs) = make_remote_tasks(&["dup", "sentinel"]).await;
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        core.submit(Command::TrustDevice { cert_cbor: cert_b })
            .await
            .unwrap();

        let mut changes = core.changes();
        // Deliver the first op twice (duplicate), then a distinct sentinel op.
        let dup = envs[0].clone();
        let sentinel = envs[1].clone();
        let script = Script {
            inject: vec![(stream, vec![dup.clone(), dup, sentinel])],
            close_after_subscribe: false,
        };
        let (factory, _server, _batch_rx) = harness(vec![script]);
        core.start_sync(factory).unwrap();

        // Exactly two Created events: the duplicate second delivery emits none.
        wait_created(&mut changes).await;
        wait_created(&mut changes).await;
        assert_eq!(
            inbox_len(&core).await,
            2,
            "duplicate did not double-materialize"
        );
    }

    // ---- Test (d): transport drop → reconnect → convergence ----
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reconnect_after_drop_converges() {
        let (cert_b, stream, envs) = make_remote_tasks(&["missed"]).await;
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        core.submit(Command::TrustDevice { cert_cbor: cert_b })
            .await
            .unwrap();

        let mut changes = core.changes();
        // First connection drops right after Subscribe; the missed op only
        // arrives on the second connection.
        let scripts = vec![
            Script {
                inject: vec![],
                close_after_subscribe: true,
            },
            Script {
                inject: vec![(stream, envs)],
                close_after_subscribe: false,
            },
        ];
        let (factory, server, _batch_rx) = harness(scripts);
        core.start_sync(factory).unwrap();

        wait_created(&mut changes).await;
        assert_eq!(inbox_len(&core).await, 1, "converged after reconnect");
        assert!(
            server.lock().connect_count >= 2,
            "factory was called again to reconnect"
        );
    }

    // ---- Test (e): submit while Live pushes immediately (no reconnect) ----
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submit_while_live_pushes_without_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;

        let mut status_rx = core.sync_status();
        let (factory, server, mut batch_rx) = harness(vec![]);
        core.start_sync(factory).unwrap();

        let _ = collect_until_live(&mut status_rx).await;
        let connects_before = server.lock().connect_count;

        // Nothing sent yet (no pending outbox on connect).
        core.submit(Command::CreateTask(TaskDraft {
            title: "live submit".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

        let rb = timeout(Duration::from_secs(10), batch_rx.recv())
            .await
            .expect("server received the live submit")
            .unwrap();
        assert_eq!(rb.ops.len(), 1);
        assert_eq!(
            server.lock().connect_count,
            connects_before,
            "no reconnect was needed"
        );
    }
}
