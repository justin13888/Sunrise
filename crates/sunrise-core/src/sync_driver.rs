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
//!
//! # Recovering inside a session
//!
//! A live session is not a link that works — it is a link that has not
//! *visibly* broken. Two mechanisms make the driver recover from a lossy one
//! without waiting for the socket to die (issue #20):
//!
//! **Outbound: bounded per-batch retransmit.** An `OpBatch` that is not acked
//! within a [`Backoff`] delay is sent again, up to the policy's five attempts,
//! after which the session is torn down and a fresh one re-drains the outbox.
//! Retransmitting is safe because an op batch is idempotent at the receiver —
//! the `OpLog` gate is keyed on `(stream, device, seq)` — which is also why
//! only `OpBatch` is ever retransmitted. Before this, an unacked op sat in the
//! outbox until the session *ended*: on a lossy-but-not-broken link it could
//! stay stranded indefinitely while the UI showed `Live`, which is the worst
//! pair — no progress and no signal.
//!
//! **Inbound: resync.** Nothing acknowledges an inbound frame, so a dropped or
//! mangled one leaves no trace at all; an outbound retry cannot help. The
//! remedy is anti-entropy: re-send `Subscribe` with the current per-device
//! cursors and let the relay replay whatever those cursors do not cover. Since
//! issue #19 that costs the relay only the frames actually missing, so it is
//! cheap enough to run on a timer ([`SyncConfig::resync_interval`]) as a
//! backstop, and immediately — rate-limited by [`MIN_RESYNC_GAP`] — whenever
//! the session sees evidence of loss: a retransmit, an undecodable frame, or a
//! remote op that fails its integrity checks.
//!
//! The timer is the guarantee and the evidence is the latency optimisation.
//! Evidence alone would miss the case where the *last* frame in each direction
//! is the one that vanished, leaving nothing to notice.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{broadcast, Notify};
use tokio::time::Instant;

use crate::config::Rng;
use crate::core::Core;
use crate::events::{DomainEvent, SyncStatus};
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_error::ErrorCode;
use sunrise_id::EntityKind;
use sunrise_sync::{Backoff, SyncState, Transport, TransportError};

pub use sunrise_sync::{TokenSource, TokenWatch};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, Capability, CapabilityBits, CaughtUpPayload,
    ErrorPayload, FrameFlags, Hello, HelloAck, MsgKind, OpBatchPayload, RefreshTokenAckPayload,
    RefreshTokenPayload, SubscribeEntry, SubscribePayload, REQUIRED_CLIENT_BITS,
    REQUIRED_SERVER_BITS,
};

/// Boxed transport produced by a [`TransportFactory`].
pub type BoxTransport = Box<dyn Transport>;

/// Future returned by a [`TransportFactory`]: yields a connected transport.
pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<BoxTransport, TransportError>> + Send>>;

/// A factory that opens a fresh transport on every call. Called once per
/// connect attempt (initial connect and every reconnect after a drop), so it
/// must be able to produce a brand-new connection each time.
pub type TransportFactory = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

/// Default anti-entropy interval: how often a live session re-subscribes with
/// its current cursors even with no evidence anything went wrong.
///
/// Long, because it is a backstop, not the delivery path — live fan-out is
/// what makes a peer's change appear in seconds, and a resync past a
/// cursor-filtering relay usually replays nothing at all.
pub const DEFAULT_RESYNC_INTERVAL: Duration = Duration::from_secs(30);

/// Floor on the spacing between two resyncs.
///
/// Loss evidence arrives in bursts — a lossy link produces a retransmit and a
/// mangled frame in the same breath — and each one wants a resync. Without a
/// floor, a link dropping half its frames would spend the session
/// re-subscribing.
pub const MIN_RESYNC_GAP: Duration = Duration::from_millis(250);

/// Client-side sync configuration carried in [`crate::CoreConfig`].
///
/// `Debug` is hand-written because [`SyncConfig::credential`] holds a live
/// bearer and this type is reachable from `CoreConfig`, which is logged.
///
/// Deliberately not `PartialEq`. Once it carries a credential handle there is
/// no equality worth defining: comparing by *value* compares secrets, and
/// comparing by shared cell would make two configs built the same way unequal.
/// Callers that need to compare configuration compare the fields they mean.
#[derive(Clone)]
pub struct SyncConfig {
    /// Relay `/sync` endpoint URL (e.g. `wss://relay.example/sync`). Used by
    /// the production WebSocket factory the app assembles; the driver itself
    /// takes an already-built [`TransportFactory`].
    pub url: String,
    /// How often a live session re-subscribes with its current cursors as an
    /// anti-entropy backstop. Tests shorten it; see [`DEFAULT_RESYNC_INTERVAL`].
    pub resync_interval: Duration,
    /// The bearer to present on the `/sync` upgrade.
    ///
    /// A [`TokenSource`] rather than a `String` because a session outlives its
    /// tokens: the transport factory is built once and called on every
    /// reconnect for the life of the process, so a captured string would be
    /// frozen at whatever the token was when sync started. Writing a renewed
    /// token here is what makes the *next* reconnect present a live
    /// credential.
    ///
    /// Empty means unauthenticated, which only a self-host relay running
    /// `NullVerifier` accepts.
    pub credential: TokenSource,
}

impl std::fmt::Debug for SyncConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncConfig")
            .field("url", &self.url)
            .field("resync_interval", &self.resync_interval)
            .field("credential", &self.credential)
            .finish()
    }
}

impl SyncConfig {
    /// Sync against `url` with the default resync interval.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            resync_interval: DEFAULT_RESYNC_INTERVAL,
            credential: TokenSource::empty(),
        }
    }

    /// Override the anti-entropy interval.
    #[must_use]
    pub fn with_resync_interval(mut self, interval: Duration) -> Self {
        self.resync_interval = interval;
        self
    }

    /// Present `credential` on the upgrade. The source is shared, so a later
    /// write reaches the next reconnect.
    #[must_use]
    pub fn with_credential(mut self, credential: TokenSource) -> Self {
        self.credential = credential;
        self
    }
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

    /// Latch [`SyncState::Degraded`]: the relay has reported ops it can no
    /// longer supply.
    ///
    /// Deliberately *not* cleared within the session — nothing that happens on
    /// this connection can refill the hole, so any later `Live` would be a
    /// lie. It does clear on reconnect, because the next `Subscribe` re-asks
    /// the question: if the ops are still unavailable the relay reports the
    /// gap again and this re-latches, and if a durable log has since made them
    /// servable the state was correctly transient.
    fn mark_degraded(&self) {
        let mut s = self.inner.lock();
        if s.state == SyncState::Degraded {
            return;
        }
        s.state = SyncState::Degraded;
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

/// In-flight (sent, not yet acked) outbox batch.
///
/// Keeps the encoded frame so it can be sent again: without it "retry" would
/// mean re-reading and re-encoding the outbox, which would produce a *different*
/// `batch_id` and defeat the relay's idempotency key.
struct InflightBatch {
    op_ids: Vec<[u8; 16]>,
    frame: Vec<u8>,
    /// Retry policy for this batch alone. Per batch, not per session, so one
    /// unlucky batch does not consume another's attempts.
    backoff: Backoff,
    /// When this batch is considered lost and worth sending again.
    due_at: Instant,
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
    /// A retransmit deadline or the resync deadline came due.
    Timer,
    /// The bearer was replaced while this session was live.
    CredentialRenewed,
    Recv(Result<Option<Vec<u8>>, TransportError>),
}

/// Why a session decided the link is losing data.
///
/// Not an error in itself — each of these is survivable on its own — but each
/// one is evidence that something *inbound* may have been lost too, which
/// nothing else in the protocol would reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LossEvidence {
    /// An op batch went unacked long enough to be sent again.
    Retransmit,
    /// An inbound frame could not be decoded at the frame or payload level.
    UndecodableFrame,
    /// A remote op failed signature, AEAD, or CBOR checks. Distinct from an op
    /// from an untrusted device, which is a policy outcome, not corruption.
    CorruptOp,
}

impl LossEvidence {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Retransmit => "retransmit",
            Self::UndecodableFrame => "undecodable_frame",
            Self::CorruptOp => "corrupt_op",
        }
    }
}

/// Deadlines a live session is waiting on, and the loss evidence that pulls
/// the resync deadline forward.
struct Deadlines {
    /// Next anti-entropy resync.
    resync_at: Instant,
    /// Earliest a resync may happen at all, from [`MIN_RESYNC_GAP`].
    resync_floor: Instant,
    interval: Duration,
}

impl Deadlines {
    fn new(interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            resync_at: now + interval,
            resync_floor: now,
            interval,
        }
    }

    /// Record that a resync just happened.
    fn resynced(&mut self) {
        let now = Instant::now();
        self.resync_floor = now + MIN_RESYNC_GAP;
        self.resync_at = now + self.interval;
    }

    /// Bring the next resync forward to the earliest the floor allows.
    fn note_loss(&mut self, evidence: LossEvidence) {
        tracing::debug!(
            ev = "sync.loss_evidence",
            cause = evidence.as_str(),
            "scheduling a resync"
        );
        self.resync_at = self.resync_at.min(self.resync_floor.max(Instant::now()));
    }
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
    let resync_interval = weak
        .upgrade()
        .and_then(|c| c.sync_resync_interval())
        .unwrap_or(DEFAULT_RESYNC_INTERVAL);
    // Held for the driver's whole life, not per session: a renewal written
    // while the client is disconnected must still reach the next connect.
    let credential = weak
        .upgrade()
        .map_or_else(TokenSource::empty, |c| c.sync_credential());
    // One watch handle for the driver's life, so a renewal that lands between
    // two sessions — or while a session is busy — is still observed.
    let mut renewals = credential.watch();
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
        let end = session(
            &core,
            &shared,
            transport,
            rng.as_ref(),
            resync_interval,
            &credential,
            &mut renewals,
        )
        .await;
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

/// Hello → HelloAck.
///
/// `Ok(refresh_negotiated)` on success — whether the relay agreed
/// [`Capability::SrvTokenRefresh`], which decides whether this session may
/// renew in band. `Err` carries the reason the session never started.
async fn handshake(
    core: &Arc<Core>,
    shared: &SyncShared,
    transport: &mut BoxTransport,
) -> Result<bool, SessionEnd> {
    let hello = build_hello(core.app_string());
    let Ok(hello_bytes) = encode_hello(&hello) else {
        return Err(SessionEnd::Disconnected);
    };
    let Ok(hello_frame) = encode_frame(MsgKind::Hello, FrameFlags::EMPTY, &hello_bytes) else {
        return Err(SessionEnd::Disconnected);
    };
    if transport.send_frame(hello_frame).await.is_err() {
        return Err(SessionEnd::Disconnected);
    }
    // Whether the relay agreed to take an in-band refresh. A relay that did
    // not gets no `0x12` at all: it would ignore the frame, and the client
    // would have no way to tell that from acceptance.
    loop {
        let recv = tokio::select! {
            biased;
            () = shared.shutdown_notified() => {
                let _ = transport.close().await;
                return Err(SessionEnd::Shutdown);
            }
            r = transport.recv_frame() => r,
        };
        match recv {
            Ok(Some(bytes)) => match decode_frame(&bytes) {
                Ok((h, payload)) if h.msg_kind == MsgKind::HelloAck => {
                    // An ack we cannot parse is treated as agreeing to nothing
                    // optional, rather than as a failed session: the required
                    // bits were already checked by the relay, and degrading
                    // quietly is what an unknown-capability field is for.
                    return Ok(decode_hello_ack(&payload).is_some_and(|ack| {
                        CapabilityBits(ack.capabilities).has(Capability::SrvTokenRefresh)
                    }));
                }
                Ok((h, _)) if h.msg_kind == MsgKind::Error => return Err(SessionEnd::Disconnected),
                // Any other frame before HelloAck: keep waiting.
                Ok(_) => {}
                Err(_) => return Err(SessionEnd::Disconnected),
            },
            Ok(None) | Err(_) => return Err(SessionEnd::Disconnected),
        }
    }
}

/// One connected session: handshake → subscribe → drain + pump frames.
#[allow(clippy::too_many_lines)]
async fn session(
    core: &Arc<Core>,
    shared: &SyncShared,
    mut transport: BoxTransport,
    rng: &dyn Rng,
    resync_interval: Duration,
    credential: &TokenSource,
    renewals: &mut TokenWatch,
) -> SessionEnd {
    let refresh_negotiated = match handshake(core, shared, &mut transport).await {
        Ok(negotiated) => negotiated,
        Err(end) => return end,
    };

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
    let mut deadlines = Deadlines::new(resync_interval);
    // A renewal that landed while this client was disconnected is already in
    // the credential the connect used, so it is marked seen rather than
    // re-sent as a refresh the relay does not need.
    if renewals.seen() != credential.version() {
        let _ = renewals.changed().await;
    }

    // Initial outbox drain (fresh session: everything unacked is (re)sent —
    // idempotent apply on the peer tolerates replays).
    if build_outbox_frames(
        core,
        &mut subscribed,
        &mut inflight_ops,
        &mut inflight,
        &mut batch_counter,
        &mut pending_sends,
        rng,
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

        let wake_at = next_deadline(&inflight, &deadlines);
        let ev = tokio::select! {
            biased;
            () = shared.shutdown_notified() => SessionEvent::Shutdown,
            () = shared.submit_notified() => SessionEvent::Submit,
            _ = renewals.changed() => SessionEvent::CredentialRenewed,
            () = tokio::time::sleep_until(wake_at) => SessionEvent::Timer,
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
                    rng,
                )
                .is_err()
                {
                    return SessionEnd::Disconnected;
                }
            }
            // The renewal reaches the relay in-band. The alternative — waiting
            // for `AUTH_TOKEN_EXPIRED` and reconnecting — costs a full
            // handshake, a re-subscribe, and a catch-up window in which the
            // session is not live, all on a schedule the client already knew.
            SessionEvent::CredentialRenewed => {
                on_credential_renewed(refresh_negotiated, credential, &mut pending_sends);
            }
            SessionEvent::Timer => {
                // Exhausting a batch's retries means this link is not carrying
                // our ops at all. Reconnecting is the escalation: a fresh
                // session re-drains the outbox and re-subscribes from scratch.
                if !retransmit_due(&mut inflight, &mut pending_sends, &mut deadlines, rng) {
                    tracing::warn!(
                        ev = "sync.session.error",
                        err_code = "SYNC_NETWORK_UNAVAILABLE",
                        err_kind = "transient",
                        retryable = true,
                        result = "failed",
                        cause = "op batch unacked after the full retry policy",
                        "tearing the session down to reconnect"
                    );
                    return SessionEnd::Disconnected;
                }
                if deadlines.resync_at <= Instant::now() {
                    match encode_subscribe_all(core) {
                        Ok(frame) => pending_sends.push(frame),
                        Err(()) => return SessionEnd::Disconnected,
                    }
                    deadlines.resynced();
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
                    &mut deadlines,
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

/// Queue an in-band `0x12`, or record why the renewal is waiting.
///
/// The frame goes only to a relay that agreed `SrvTokenRefresh`. One that did
/// not would ignore it silently, which the client could not distinguish from
/// acceptance — so the new bearer rides the next reconnect instead, a path
/// every relay understands.
fn on_credential_renewed(
    refresh_negotiated: bool,
    credential: &TokenSource,
    pending_sends: &mut Vec<Vec<u8>>,
) {
    if !refresh_negotiated {
        tracing::debug!(
            ev = "sync.credential.renewed.deferred",
            "relay did not negotiate in-band refresh; the new bearer waits for the next connect"
        );
        return;
    }
    if let Some(frame) = encode_refresh_token(credential) {
        pending_sends.push(frame);
    }
}

/// Record a relay's acceptance of a refreshed bearer.
///
/// The deadline is the relay's, not our own reading of the token's `exp`: the
/// two differ by the relay's configured leeway, and the relay's is the one
/// that ends the session.
fn note_refresh_ack(payload: &[u8]) {
    let expires_at_ms = RefreshTokenAckPayload::decode(payload)
        .map(|p| p.expires_at_ms)
        .unwrap_or_default();
    tracing::debug!(
        ev = "sync.credential.accepted",
        expires_at_ms,
        "relay accepted the refreshed bearer"
    );
}

/// Encode a `0x12 RefreshToken` frame for the source's current bearer.
///
/// `None` when there is no token to send — clearing the credential is not
/// something to tell the relay about, and an empty `RefreshToken` would be
/// rejected as an unverifiable one, ending a session that was working.
fn encode_refresh_token(credential: &TokenSource) -> Option<Vec<u8>> {
    let token = credential.get()?;
    let payload = RefreshTokenPayload { token }.encode().ok()?;
    let frame = encode_frame(MsgKind::RefreshToken, FrameFlags::EMPTY, &payload).ok()?;
    tracing::debug!(
        ev = "sync.credential.renewed",
        "sending a refreshed bearer to the relay"
    );
    Some(frame)
}

/// When the pump next has something to do on its own: the soonest retransmit
/// deadline, or the next resync, whichever comes first.
fn next_deadline(inflight: &HashMap<u64, InflightBatch>, deadlines: &Deadlines) -> Instant {
    inflight
        .values()
        .map(|b| b.due_at)
        .min()
        .map_or(deadlines.resync_at, |d| d.min(deadlines.resync_at))
}

/// Queue a retransmit for every batch whose ack never arrived.
///
/// Returns false when a batch has exhausted the retry policy — the caller ends
/// the session rather than retrying forever on a link that is clearly not
/// delivering.
fn retransmit_due(
    inflight: &mut HashMap<u64, InflightBatch>,
    out: &mut Vec<Vec<u8>>,
    deadlines: &mut Deadlines,
    rng: &dyn Rng,
) -> bool {
    let now = Instant::now();
    for (batch_id, batch) in inflight.iter_mut() {
        if batch.due_at > now {
            continue;
        }
        let Some(delay) = batch.backoff.next_delay(rng_unit(rng)) else {
            return false;
        };
        batch.backoff.record_attempt();
        batch.due_at = now + delay;
        tracing::debug!(
            ev = "sync.op.retransmit",
            batch_id = *batch_id,
            attempt = u64::from(batch.backoff.attempt()),
            n_ops = batch.op_ids.len() as u64,
            "op batch unacked; sending again"
        );
        out.push(batch.frame.clone());
        // An ack that never came is the clearest evidence this link drops
        // frames, and the inbound direction has no ack of its own to miss.
        deadlines.note_loss(LossEvidence::Retransmit);
    }
    true
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
    deadlines: &mut Deadlines,
) -> Result<(), ()> {
    let Ok((header, payload)) = decode_frame(bytes) else {
        // Junk frame: keep the session, but a frame that will not decode means
        // the link mangled it — and whatever mangled this one may have
        // destroyed another outright, leaving no trace at all.
        deadlines.note_loss(LossEvidence::UndecodableFrame);
        return Ok(());
    };
    match header.msg_kind {
        MsgKind::Ack => {
            let Ok(ack) = AckPayload::decode(&payload) else {
                deadlines.note_loss(LossEvidence::UndecodableFrame);
                return Ok(());
            };
            {
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
            let Ok(batch) = OpBatchPayload::decode(&payload) else {
                deadlines.note_loss(LossEvidence::UndecodableFrame);
                return Ok(());
            };
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
                    // Idempotent re-receive: nothing to do, nothing wrong.
                    Ok(None) => {}
                    Err(e) => {
                        // An op that fails its own integrity checks was
                        // damaged in transit; an op from a device this vault
                        // has not trusted is a policy outcome and says nothing
                        // about the link. Only the first is evidence.
                        if is_corruption(&e) {
                            deadlines.note_loss(LossEvidence::CorruptOp);
                        }
                    }
                }
            }
        }
        // CaughtUp rides the StreamUpdate kind (see wire-protocol payloads).
        MsgKind::StreamUpdate => {
            if let Ok(cu) = CaughtUpPayload::decode(&payload) {
                caught_up.insert(cu.stream_id);
            } else {
                deadlines.note_loss(LossEvidence::UndecodableFrame);
            }
        }
        MsgKind::Ping => {
            if let Ok(frame) = encode_frame(MsgKind::Pong, FrameFlags::EMPTY, &[]) {
                pending_sends.push(frame);
            }
        }
        // Server-initiated close: reconnect.
        MsgKind::Close => return Err(()),
        MsgKind::Error => {
            let Ok(err) = ErrorPayload::decode(&payload) else {
                deadlines.note_loss(LossEvidence::UndecodableFrame);
                return Ok(());
            };
            // A cursor gap is the one error the session cannot recover from by
            // trying harder: the relay has said the ops between our cursor and
            // its watermark are gone, so neither a retransmit nor a resync can
            // produce them. Ignoring it — which is what this arm used to do —
            // meant accepting the `CaughtUp` that follows and reporting `Live`
            // while permanently missing data. Latch a degraded state instead;
            // it outlives the session because the loss does.
            if err.code == ErrorCode::SyncCursorGap {
                tracing::warn!(
                    ev = "sync.gap",
                    err_code = %err.code,
                    err_kind = "permanent",
                    retryable = false,
                    result = "degraded",
                    cause = %err.reason,
                    "relay cannot supply ops this device never received"
                );
                shared.mark_degraded();
            } else {
                tracing::warn!(
                    ev = "sync.session.error",
                    err_code = %err.code,
                    err_kind = "transient",
                    retryable = true,
                    result = "failed",
                    cause = %err.reason,
                    "relay reported an error"
                );
            }
        }
        MsgKind::RefreshTokenAck => note_refresh_ack(&payload),
        // Nack and every other kind are non-fatal in v1 self-host: they fall
        // through to a no-op.
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
    rng: &dyn Rng,
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
        let backoff = Backoff::canonical();
        // First deadline uses the policy's own initial delay, without consuming
        // an attempt: attempt 0 is the original send.
        let due_at = Instant::now() + backoff.next_delay(rng_unit(rng)).unwrap_or(MIN_RESYNC_GAP);
        inflight.insert(
            batch_id,
            InflightBatch {
                op_ids,
                frame: frame.clone(),
                backoff,
                due_at,
            },
        );
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
        // The floor is what this build can READ; DOC_SCHEMA_V is what it
        // WRITES. Advertising the same number for both would tell a peer we
        // cannot read our own predecessor, which is exactly the forward
        // compatibility the split exists to keep.
        doc_schema_min: u32::from(DOC_SCHEMA_FLOOR),
        doc_schema_max: u32::from(DOC_SCHEMA_V),
        crypto_suite_supported: vec![u32::from(CRYPTO_SUITE_V)],
        // The optional bit rides alongside the required ones. `HelloAck`
        // carries the AND of the two sides, so asking is how we find out
        // whether the relay can take an in-band refresh at all.
        capabilities: REQUIRED_CLIENT_BITS.0
            | REQUIRED_SERVER_BITS.0
            | CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0,
        trace: String::new(),
    }
}

fn decode_hello_ack(payload: &[u8]) -> Option<HelloAck> {
    ciborium::de::from_reader(payload).ok()
}

fn encode_hello(h: &Hello) -> Result<Vec<u8>, ()> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(h, &mut buf).map_err(|_| ())?;
    Ok(buf)
}

/// A `Subscribe` covering every known stream with its current cursors — the
/// anti-entropy resync frame.
///
/// Identical in shape to the one the handshake sends, and safe to repeat: the
/// relay replaces a stream's subscription rather than adding a second one, and
/// replays only what the cursors do not already cover.
fn encode_subscribe_all(core: &Core) -> Result<Vec<u8>, ()> {
    let entries = core.sync_subscribe_entries().map_err(|_| ())?;
    let sub = SubscribePayload { streams: entries };
    let bytes = sub.encode().map_err(|_| ())?;
    encode_frame(MsgKind::Subscribe, FrameFlags::EMPTY, &bytes).map_err(|_| ())
}

/// Whether an `apply_remote` failure means the bytes were damaged, as opposed
/// to the op being well-formed but from a device this vault does not trust.
fn is_corruption(e: &crate::core::CoreError) -> bool {
    matches!(
        e,
        crate::core::CoreError::Engine(crate::engine::EngineError::RemoteOpInvalid(_))
    )
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

    use super::{BoxTransport, ConnectFuture, SyncConfig, TokenSource, TransportFactory};
    use crate::config::Clock;
    use crate::{Command, Core, CoreConfig, DomainEvent, Query, QueryResult, SystemRng, Unlock};
    use async_trait::async_trait;
    use std::collections::{HashSet, VecDeque};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::TaskDraft;
    use sunrise_error::ErrorCode;
    use sunrise_sync::{SyncState, Transport, TransportError};
    use sunrise_wire_protocol::{
        decode_frame, encode_frame, AckPayload, Capability, CapabilityBits, CaughtUpPayload,
        ErrorPayload, FrameFlags, HelloAck, MsgKind, OpBatchPayload, SubscribePayload,
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
            sync: Some(SyncConfig::new("ws://unused/sync")),
            ..CoreConfig::with_clock(
                dir.to_path_buf(),
                "0.1.0+test",
                Arc::new(TestClock(T0)),
                Arc::new(SystemRng),
            )
        }
    }

    async fn open_arc(dir: &Path) -> Arc<Core> {
        Arc::new(
            Core::open(
                make_cfg(dir),
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes(ROOT),
                    paired: None,
                },
            )
            .await
            .unwrap(),
        )
    }

    /// Open a core whose anti-entropy timer fires on a test timescale rather
    /// than [`DEFAULT_RESYNC_INTERVAL`].
    async fn open_arc_with_resync(dir: &Path, interval: Duration) -> Arc<Core> {
        let mut cfg = make_cfg(dir);
        cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_resync_interval(interval));
        Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes(ROOT),
                    paired: None,
                },
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
        /// Receive this many OpBatches without acking them — a lossy link that
        /// is not a broken one, which is precisely the case issue #20 names.
        swallow_first_batches: usize,
        /// Inject these OpBatches on the SECOND and later Subscribe frames, so
        /// a test can tell an in-session resync from the initial subscribe.
        inject_on_resync: Vec<([u8; 16], Vec<Vec<u8>>)>,
        /// Send a `SYNC_CURSOR_GAP` Error before `CaughtUp`, exactly as the
        /// relay does when a subscriber's cursor predates the retained ring.
        gap_on_subscribe: bool,
    }

    struct ServerInner {
        connect_count: u64,
        scripts: VecDeque<Script>,
    }
    type SharedServer = Arc<parking_lot::Mutex<ServerInner>>;

    struct RecvBatch {
        ops: Vec<Vec<u8>>,
    }

    /// Bearer tokens the fake relay received in `0x12 RefreshToken` frames,
    /// in order. Shared so a test can assert the renewal reached a *live*
    /// session rather than the next reconnect.
    type SeenRefreshes = Arc<parking_lot::Mutex<Vec<String>>>;

    /// How many Subscribe frames one fake server has seen, across all its
    /// connections. Shared so a test can assert a re-subscribe happened
    /// *within* a session rather than via a reconnect.
    type SubCount = Arc<std::sync::atomic::AtomicU64>;

    fn harness(
        scripts: Vec<Script>,
    ) -> (
        TransportFactory,
        SharedServer,
        mpsc::UnboundedReceiver<RecvBatch>,
    ) {
        let (f, s, rx, _) = harness_counting(scripts);
        (f, s, rx)
    }

    fn harness_counting(
        scripts: Vec<Script>,
    ) -> (
        TransportFactory,
        SharedServer,
        mpsc::UnboundedReceiver<RecvBatch>,
        SubCount,
    ) {
        let (f, s, rx, subs, _) = harness_full(scripts, true);
        (f, s, rx, subs)
    }

    #[allow(clippy::type_complexity)]
    fn harness_full(
        scripts: Vec<Script>,
        negotiate_refresh: bool,
    ) -> (
        TransportFactory,
        SharedServer,
        mpsc::UnboundedReceiver<RecvBatch>,
        SubCount,
        SeenRefreshes,
    ) {
        let server: SharedServer = Arc::new(parking_lot::Mutex::new(ServerInner {
            connect_count: 0,
            scripts: scripts.into_iter().collect(),
        }));
        let (batch_tx, batch_rx) = mpsc::unbounded_channel();
        let subs: SubCount = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let refreshes: SeenRefreshes = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let factory_server = server.clone();
        let factory_subs = subs.clone();
        let factory_refreshes = refreshes.clone();
        let factory: TransportFactory = Arc::new(move || {
            let server = factory_server.clone();
            let batch_tx = batch_tx.clone();
            let subs = factory_subs.clone();
            let refreshes = factory_refreshes.clone();
            let fut = async move {
                let (client_end, server_end) = duplex();
                let script = {
                    let mut s = server.lock();
                    s.connect_count += 1;
                    s.scripts.pop_front().unwrap_or_default()
                };
                tokio::spawn(run_fake_server(
                    server_end,
                    script,
                    batch_tx,
                    subs,
                    refreshes,
                    negotiate_refresh,
                ));
                Ok(Box::new(client_end) as BoxTransport)
            };
            Box::pin(fut) as ConnectFuture
        });
        (factory, server, batch_rx, subs, refreshes)
    }

    /// Push each `(stream, envelopes)` group to the client as one `OpBatch`.
    /// Returns false once the client end is gone, so callers stop the server.
    async fn send_op_batches(
        t: &mut ChannelTransport,
        groups: &[([u8; 16], Vec<Vec<u8>>)],
    ) -> bool {
        for (stream_id, envs) in groups {
            let payload = OpBatchPayload {
                ops: envs.clone(),
                batch_id: 0,
                stream_id: *stream_id,
            };
            let bytes = payload.encode().unwrap();
            let f = encode_frame(MsgKind::OpBatch, FrameFlags::EMPTY, &bytes).unwrap();
            if t.send_frame(f).await.is_err() {
                return false;
            }
        }
        true
    }

    /// The fake server's Subscribe handling: optional resync injection, then a
    /// `CaughtUp` per newly-subscribed stream. Returns false once the client is
    /// gone.
    async fn serve_subscribe(
        t: &mut ChannelTransport,
        script: &Script,
        payload: &[u8],
        subscribed: &mut HashSet<[u8; 16]>,
        sub_count: &SubCount,
        injected: &mut bool,
    ) -> bool {
        let n = sub_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Ok(sub) = SubscribePayload::decode(payload) {
            // Only on a re-subscribe, so a test can attribute the
            // delivery to an in-session resync and nothing else.
            if n > 0 && !send_op_batches(t, &script.inject_on_resync).await {
                return false;
            }
            for entry in &sub.streams {
                if subscribed.insert(entry.stream_id) {
                    // Ordered exactly as the relay orders it: the
                    // gap lands BEFORE CaughtUp, so a client that
                    // ignores it goes on to read "caught up" as
                    // "complete".
                    if script.gap_on_subscribe {
                        let e = ErrorPayload {
                            code: ErrorCode::SyncCursorGap,
                            reason: "retained ring no longer covers stream".into(),
                        };
                        let bytes = e.encode().unwrap();
                        let f = encode_frame(MsgKind::Error, FrameFlags::EMPTY, &bytes).unwrap();
                        if t.send_frame(f).await.is_err() {
                            return false;
                        }
                    }
                    let cu = CaughtUpPayload {
                        stream_id: entry.stream_id,
                    };
                    let bytes = cu.encode().unwrap();
                    let f = encode_frame(MsgKind::StreamUpdate, FrameFlags::EMPTY, &bytes).unwrap();
                    if t.send_frame(f).await.is_err() {
                        return false;
                    }
                }
            }
        }
        if !*injected {
            *injected = true;
            if !send_op_batches(t, &script.inject).await {
                return false;
            }
            if script.close_after_subscribe {
                return false; // simulate a transport drop
            }
        }
        true
    }

    async fn run_fake_server(
        mut t: ChannelTransport,
        script: Script,
        batch_tx: mpsc::UnboundedSender<RecvBatch>,
        sub_count: SubCount,
        refreshes: SeenRefreshes,
        negotiate_refresh: bool,
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
            capabilities: if negotiate_refresh {
                CapabilityBits::EMPTY.with(Capability::SrvTokenRefresh).0
            } else {
                0
            },
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
        let mut swallowed = 0usize;
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
                    if !serve_subscribe(
                        &mut t,
                        &script,
                        &payload,
                        &mut subscribed,
                        &sub_count,
                        &mut injected,
                    )
                    .await
                    {
                        return;
                    }
                }
                MsgKind::OpBatch => {
                    if let Ok(batch) = OpBatchPayload::decode(&payload) {
                        let _ = batch_tx.send(RecvBatch {
                            ops: batch.ops.clone(),
                        });
                        if swallowed < script.swallow_first_batches {
                            swallowed += 1;
                            continue; // received, deliberately not acked
                        }
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
                MsgKind::RefreshToken => {
                    if let Ok(p) = sunrise_wire_protocol::RefreshTokenPayload::decode(&payload) {
                        refreshes.lock().push(p.token);
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
    /// A second device on the same account, reduced to what a test needs of
    /// it: the pairing payload that makes the receiver a sibling, and the
    /// vault-meta ops that announce it.
    struct RemotePeer {
        /// Encoded `PairingPayload` — the identity and every Stream key.
        bundle: Vec<u8>,
        /// The peer's vault-meta ops, which carry its `device_cert` op.
        meta: ([u8; 16], Vec<Vec<u8>>),
    }

    /// Build a peer device's ops out of process, the way a real second device
    /// would produce them.
    ///
    /// Before ADR-0024 this returned a certificate and the receiver trusted it
    /// with one command, because a shared vault root already implied a shared
    /// key schedule. It does not any more: two vaults on one root are two
    /// accounts. So the peer hands over a real pairing payload, and its cert
    /// reaches the receiver the way it reaches every other replica — as a
    /// `device_cert` op in the vault-meta stream, self-authenticating against
    /// the account identity.
    async fn make_remote_tasks(titles: &[&str]) -> (RemotePeer, [u8; 16], Vec<Vec<u8>>) {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::open(
            make_cfg(dir.path()),
            Unlock::DevicePaired {
                root: VaultRootKey::from_bytes(ROOT),
                paired: None,
            },
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
        let bundle =
            sunrise_pairing::encode_pairing_payload(&core.export_pairing_payload().unwrap())
                .unwrap();
        let groups = core.sync_outbox_grouped(&HashSet::new()).unwrap();
        let mut meta: Option<([u8; 16], Vec<Vec<u8>>)> = None;
        let mut tasks: Option<([u8; 16], Vec<Vec<u8>>)> = None;
        for (stream, ops) in groups {
            let envs: Vec<Vec<u8>> = ops.into_iter().map(|(_, env)| env).collect();
            if stream == [0u8; 16] {
                meta = Some((stream, envs));
            } else {
                tasks = Some((stream, envs));
            }
        }
        let meta = meta.expect("the peer announced itself in the meta stream");
        let (stream, envs) = tasks.expect("all tasks land on the inbox stream");
        core.close().await.unwrap();
        drop(dir);
        (RemotePeer { bundle, meta }, stream, envs)
    }

    /// Ack this vault's opening announcement out of band.
    ///
    /// A freshly opened vault is no longer empty. `Core::open` publishes this
    /// device's identity-signed certificate and the identity-sealed copies of
    /// the first Stream keys it mints, and those queue in the outbox like any
    /// other op — which is the point: a peer that never receives them can
    /// neither verify this device's envelopes nor recover its content.
    ///
    /// The tests below are about the driver's handling of *one* op the test
    /// submitted, so the announcement is acked directly rather than counted.
    /// `paired_devices_converge` and `device_revocation` in `sunrise-e2e` are
    /// where the announcement travelling for real is asserted.
    fn drain_announcement(core: &Core) {
        let ids: Vec<[u8; 16]> = core
            .sync_outbox_grouped(&std::collections::HashSet::new())
            .unwrap()
            .into_iter()
            .flat_map(|(_, ops)| ops.into_iter().map(|(op_id, _)| op_id))
            .collect();
        core.sync_mark_acked(&ids).unwrap();
    }

    /// Open a core that is already a sibling of `peer`.
    async fn open_arc_paired(dir: &Path, peer: &RemotePeer) -> Arc<Core> {
        open_paired(make_cfg(dir), peer).await
    }

    /// [`open_arc_paired`] with an anti-entropy timer on a test timescale.
    async fn open_arc_paired_with_resync(
        dir: &Path,
        peer: &RemotePeer,
        interval: Duration,
    ) -> Arc<Core> {
        let mut cfg = make_cfg(dir);
        cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_resync_interval(interval));
        open_paired(cfg, peer).await
    }

    async fn open_paired(cfg: CoreConfig, peer: &RemotePeer) -> Arc<Core> {
        let payload = sunrise_pairing::decode_pairing_payload(&peer.bundle).unwrap();
        Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes(ROOT),
                    paired: Some(Box::new(payload)),
                },
            )
            .await
            .unwrap(),
        )
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
    /// A renewal written while a session is live reaches the relay **in that
    /// session**, as a `0x12 RefreshToken` frame.
    ///
    /// The alternative — let the token expire, take the `AUTH_TOKEN_EXPIRED`
    /// close, reconnect — costs a full handshake, a re-subscribe, and a
    /// catch-up window in which the client is not live, all on a schedule the
    /// client already knew in advance. That is the case this closes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renewed_bearer_reaches_a_live_session_without_reconnecting() {
        let dir = tempfile::tempdir().unwrap();
        let credential = TokenSource::new(Some("first-token".into()));
        let mut cfg = make_cfg(dir.path());
        cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
        let core = Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes(ROOT),
                    paired: None,
                },
            )
            .await
            .unwrap(),
        );

        let mut status_rx = core.sync_status();
        let (factory, server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
        core.start_sync(factory).unwrap();
        let _ = collect_until_live(&mut status_rx).await;
        let connects_when_live = server.lock().connect_count;

        credential.set(Some("renewed-token".into()));

        tokio::time::timeout(Duration::from_secs(10), async {
            while refreshes.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("no RefreshToken frame arrived");
        assert_eq!(
            refreshes.lock().as_slice(),
            ["renewed-token".to_string()],
            "the relay is told the new bearer, exactly once"
        );
        assert_eq!(
            server.lock().connect_count,
            connects_when_live,
            "and without tearing the session down to do it"
        );
        core.shutdown().await;
    }

    /// The credential a caller reaches through `Core` is the *same cell* the
    /// driver watches, even when sync was never configured at open.
    ///
    /// This is the FFI seam's path: `SunriseCore::open` builds a
    /// `CoreConfig::production`, which has `sync: None`, and the URL and first
    /// bearer only arrive later at `start_sync`. While `sync_credential()`
    /// minted a fresh `TokenSource` per call, that path had two cells — the
    /// caller wrote one, the driver watched the other — so in-band renewal was
    /// silently dead for every Swift caller while looking wired up.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renewal_reaches_the_driver_when_sync_was_unconfigured_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = CoreConfig::with_clock(
            dir.path().to_path_buf(),
            "0.1.0+test",
            Arc::new(TestClock(T0)),
            Arc::new(SystemRng),
        );
        assert!(cfg.sync.is_none(), "the FFI seam opens with sync off");
        let core = Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes(ROOT),
                    paired: None,
                },
            )
            .await
            .unwrap(),
        );

        core.sync_credential().set(Some("issued-after-open".into()));
        assert_eq!(
            core.sync_credential().get().as_deref(),
            Some("issued-after-open"),
            "a write through one handle is visible through the next"
        );

        let mut status_rx = core.sync_status();
        let (factory, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
        core.start_sync(factory).unwrap();
        let _ = collect_until_live(&mut status_rx).await;

        core.sync_credential()
            .set(Some("renewed-after-open".into()));
        tokio::time::timeout(Duration::from_secs(10), async {
            while refreshes.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the renewal must reach the live session on this path too");
        assert_eq!(
            refreshes.lock().as_slice(),
            ["renewed-after-open".to_string()]
        );
        core.shutdown().await;
    }

    /// A relay that did not agree `SrvTokenRefresh` is sent **no** `0x12` at
    /// all, and the renewal waits for the next connect.
    ///
    /// This is the reason the capability bit exists. An old relay ignores an
    /// unknown frame silently, and a relay that accepted the refresh used to
    /// be silent too — so the client could not tell "your session is good for
    /// another hour" from "nothing happened", which call for opposite
    /// behaviour. Negotiating first means the client only sends the frame to
    /// someone it knows will answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_relay_that_does_not_negotiate_refresh_is_sent_no_frame() {
        let dir = tempfile::tempdir().unwrap();
        let credential = TokenSource::new(Some("first-token".into()));
        let mut cfg = make_cfg(dir.path());
        cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
        let core = Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes(ROOT),
                    paired: None,
                },
            )
            .await
            .unwrap(),
        );

        let mut status_rx = core.sync_status();
        let (factory, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], false);
        core.start_sync(factory).unwrap();
        let _ = collect_until_live(&mut status_rx).await;

        credential.set(Some("renewed-token".into()));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            refreshes.lock().is_empty(),
            "a relay that did not advertise the capability must not be sent the frame"
        );
        // The credential is still updated; it simply rides the next connect.
        assert_eq!(credential.get().as_deref(), Some("renewed-token"));
        core.shutdown().await;
    }

    /// Clearing the credential is not a renewal. An empty `RefreshToken` would
    /// be rejected by the relay as an unverifiable token, ending a session
    /// that was working — so nothing is sent.
    #[tokio::test(flavor = "multi_thread")]
    async fn clearing_the_credential_sends_no_refresh_frame() {
        let dir = tempfile::tempdir().unwrap();
        let credential = TokenSource::new(Some("first-token".into()));
        let mut cfg = make_cfg(dir.path());
        cfg.sync = Some(SyncConfig::new("ws://unused/sync").with_credential(credential.clone()));
        let core = Arc::new(
            Core::open(
                cfg,
                Unlock::DevicePaired {
                    root: VaultRootKey::from_bytes(ROOT),
                    paired: None,
                },
            )
            .await
            .unwrap(),
        );

        let mut status_rx = core.sync_status();
        let (factory, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
        core.start_sync(factory).unwrap();
        let _ = collect_until_live(&mut status_rx).await;

        credential.set(None);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            refreshes.lock().is_empty(),
            "a cleared credential must not be sent as a refresh"
        );
        core.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_connect_drains_pending_outbox_and_reaches_live() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        // Warm the vault before measuring. A fresh one is not quiet: opening
        // it queues this device's announcement, and the first task in the
        // Inbox mints that stream's key and queues the `key_envelope` op that
        // distributes it. Both are real ops that must reach the relay; they
        // are simply not what this test is about.
        core.submit(Command::CreateTask(TaskDraft {
            title: "warm-up".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
        drain_announcement(&core);
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
        let (peer, stream, envs) = make_remote_tasks(&["from B"]).await;
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc_paired(dir.path(), &peer).await;

        let mut changes = core.changes();
        let script = Script {
            inject: vec![peer.meta.clone(), (stream, envs)],
            close_after_subscribe: false,
            ..Default::default()
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
        let (peer, stream, envs) = make_remote_tasks(&["dup", "sentinel"]).await;
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc_paired(dir.path(), &peer).await;

        let mut changes = core.changes();
        // Deliver the first op twice (duplicate), then a distinct sentinel op.
        let dup = envs[0].clone();
        let sentinel = envs[1].clone();
        let script = Script {
            inject: vec![
                peer.meta.clone(),
                (stream, vec![dup.clone(), dup, sentinel]),
            ],
            close_after_subscribe: false,
            ..Default::default()
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
        let (peer, stream, envs) = make_remote_tasks(&["missed"]).await;
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc_paired(dir.path(), &peer).await;

        let mut changes = core.changes();
        // First connection drops right after Subscribe; the missed op only
        // arrives on the second connection.
        let scripts = vec![
            Script {
                inject: vec![],
                close_after_subscribe: true,
                ..Default::default()
            },
            Script {
                inject: vec![peer.meta.clone(), (stream, envs)],
                close_after_subscribe: false,
                ..Default::default()
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
        // Warm the vault before measuring. A fresh one is not quiet: opening
        // it queues this device's announcement, and the first task in the
        // Inbox mints that stream's key and queues the `key_envelope` op that
        // distributes it. Both are real ops that must reach the relay; they
        // are simply not what this test is about.
        core.submit(Command::CreateTask(TaskDraft {
            title: "warm-up".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
        drain_announcement(&core);

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

    // ---- Test (f): an unacked op batch is retransmitted inside the session ----
    //
    // The heart of issue #20. The server *receives* the batch and deliberately
    // withholds the ack, which is a lossy link rather than a broken one: the
    // socket stays up, so nothing tears the session down and, before the retry
    // path existed, the op sat in the outbox indefinitely while the UI showed
    // `Live`. The assertion that matters is not merely that the op arrives, but
    // that it arrives *without a reconnect* — recovery has to happen in-session,
    // because on a link like this no reconnect is ever coming.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unacked_batch_is_retransmitted_within_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        // Warm the vault before measuring. A fresh one is not quiet: opening
        // it queues this device's announcement, and the first task in the
        // Inbox mints that stream's key and queues the `key_envelope` op that
        // distributes it. Both are real ops that must reach the relay; they
        // are simply not what this test is about.
        core.submit(Command::CreateTask(TaskDraft {
            title: "warm-up".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
        drain_announcement(&core);

        let mut status_rx = core.sync_status();
        let script = Script {
            swallow_first_batches: 1,
            ..Default::default()
        };
        let (factory, server, mut batch_rx) = harness(vec![script]);
        core.start_sync(factory).unwrap();

        let _ = collect_until_live(&mut status_rx).await;
        let connects_before = server.lock().connect_count;

        core.submit(Command::CreateTask(TaskDraft {
            title: "stranded".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

        // First delivery: swallowed, never acked.
        let first = timeout(Duration::from_secs(10), batch_rx.recv())
            .await
            .expect("server saw the original send")
            .unwrap();
        assert_eq!(first.ops.len(), 1);

        // Second delivery: the retransmit, carrying the same op.
        let second = timeout(Duration::from_secs(10), batch_rx.recv())
            .await
            .expect("server saw the retransmit")
            .unwrap();
        assert_eq!(second.ops, first.ops, "retransmit carried the same op");

        // The retransmit was acked, so the outbox drained.
        wait_pending_zero(&core).await;
        assert_eq!(
            server.lock().connect_count,
            connects_before,
            "recovered in-session: the link was never cycled"
        );
    }

    // ---- Test (g): loss evidence pulls a resync forward, in-session ----
    //
    // Nothing acks an inbound frame, so a dropped one leaves no trace and no
    // outbound retry can recover it. The driver instead treats a retransmit as
    // evidence that this link is dropping frames in *both* directions and
    // re-subscribes early. The injected batch is delivered only on the second
    // and later Subscribe, so materializing it proves an in-session resync
    // happened — a reconnect would show up as a second connect.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loss_evidence_triggers_an_in_session_resync() {
        let (peer, stream, envs) = make_remote_tasks(&["only on resync"]).await;
        let dir = tempfile::tempdir().unwrap();
        // Long timer, so anything observed here came from the evidence path.
        let core = open_arc_paired_with_resync(dir.path(), &peer, Duration::from_secs(300)).await;

        let script = Script {
            // Withholding this ack is what produces the loss evidence.
            swallow_first_batches: 1,
            inject_on_resync: vec![peer.meta.clone(), (stream, envs)],
            ..Default::default()
        };
        let (factory, server, _batch_rx, subs) = harness_counting(vec![script]);
        core.start_sync(factory).unwrap();

        // This local op is what gets swallowed, producing the retransmit that
        // is the loss evidence. It also lands in the inbox, so the assertion
        // below counts two: this one, and the remote op the resync delivers.
        core.submit(Command::CreateTask(TaskDraft {
            title: "provokes a retransmit".into(),
            ..Default::default()
        }))
        .await
        .unwrap();

        wait_inbox_len(&core, 2).await;
        assert!(
            subs.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "a second Subscribe was sent: the resync"
        );
        assert_eq!(
            server.lock().connect_count,
            1,
            "the resync happened inside the original session"
        );
    }

    // ---- Test (h): the resync timer fires with no evidence at all ----
    //
    // The evidence path cannot cover the case where the *last* frame in each
    // direction is the one that vanished: there is then nothing left to notice
    // it. Here the link is clean and nothing is withheld, so no evidence is
    // ever produced, and only the timer can account for the resync. The timer
    // is the guarantee; evidence is only the latency optimisation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resync_timer_fires_without_any_loss_evidence() {
        let (peer, stream, envs) = make_remote_tasks(&["timer backstop"]).await;
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc_paired_with_resync(dir.path(), &peer, Duration::from_millis(150)).await;

        let script = Script {
            inject_on_resync: vec![peer.meta.clone(), (stream, envs)],
            ..Default::default()
        };
        let (factory, server, _batch_rx, subs) = harness_counting(vec![script]);
        core.start_sync(factory).unwrap();

        // Nothing is ever submitted locally here, so the inbox can only reach
        // one via the injected remote op — which only the resync carries.
        wait_inbox_len(&core, 1).await;
        assert!(
            subs.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "a second Subscribe was sent: the timer resync"
        );
        assert_eq!(
            server.lock().connect_count,
            1,
            "the resync happened inside the original session"
        );
    }

    // ---- Test (i): a cursor gap degrades the session instead of going Live ----
    //
    // The relay sends `SYNC_CURSOR_GAP` *before* `CaughtUp` precisely so a
    // client cannot read "caught up" as "complete". Until this arm existed the
    // driver discarded the Error, accepted the `CaughtUp`, and reported `Live`
    // while permanently missing ops the relay can never resend — the same "no
    // progress, no signal" failure as issue #20, one layer up. Re-subscribing
    // cannot fix it, so the state has to survive the rest of the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cursor_gap_degrades_the_session_and_never_reports_live() {
        let dir = tempfile::tempdir().unwrap();
        // Short resync timer: proves the degraded latch survives re-subscribes,
        // which are the only thing that could plausibly clear it.
        let core = open_arc_with_resync(dir.path(), Duration::from_millis(100)).await;

        let script = Script {
            gap_on_subscribe: true,
            ..Default::default()
        };
        let (factory, _server, _batch_rx) = harness(vec![script]);
        core.start_sync(factory).unwrap();

        wait_state(&core, SyncState::Degraded).await;

        // Hold through several resync cycles: still degraded, never Live.
        for _ in 0..5u32 {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let s = sync_state(&core).await;
            assert_eq!(
                s,
                SyncState::Degraded,
                "a gap the relay cannot fill must not resolve to Live"
            );
        }
    }

    async fn sync_state(core: &Core) -> SyncState {
        match core.query(Query::SyncStatus).await.unwrap() {
            QueryResult::SyncStatus(s) => s.state,
            other => panic!("expected sync status, got {other:?}"),
        }
    }

    /// Poll until the driver reports `want`.
    async fn wait_state(core: &Core, want: SyncState) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if sync_state(core).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "never reached {want:?}; last = {:?}",
            sync_state(core).await
        );
    }

    /// Poll until the inbox holds exactly `n` tasks.
    async fn wait_inbox_len(core: &Core, n: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if inbox_len(core).await == n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("inbox never reached {n}; last = {}", inbox_len(core).await);
    }

    /// Poll until the DB-authoritative outbox count reaches zero.
    async fn wait_pending_zero(core: &Core) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if core.sync_pending().unwrap() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("outbox never drained");
    }
}
