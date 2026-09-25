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
//! production SSE factory (built on `sunrise_sync::SseTransport`,
//! `crates/sunrise-sync/src/sse.rs`) and the in-process loopback used by the
//! tests are both just factories.
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
use crate::engine::hex_short;
use crate::events::{DomainEvent, SyncStatus};
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_error::{ErrorCode, ErrorKind};
use sunrise_id::EntityKind;
use sunrise_sync::{Backoff, RevokeOutcome, SyncState, Transport, TransportError};

pub use sunrise_sync::{TokenSource, TokenWatch};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, Capability, CapabilityBits, CaughtUpPayload,
    ClosePayload, ErrorPayload, FrameFlags, Hello, HelloAck, MsgKind, OpBatchPayload,
    RefreshTokenAckPayload, RefreshTokenPayload, SubscribeEntry, SubscribePayload,
    REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS,
};

/// Boxed transport produced by a [`TransportFactory`].
pub type BoxTransport = Box<dyn Transport>;

/// Future returned by a [`TransportFactory`]: yields a connected transport.
pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<BoxTransport, TransportError>> + Send>>;

/// A factory that opens a fresh transport on every call. Called once per
/// connect attempt (initial connect and every reconnect after a drop), so it
/// must be able to produce a brand-new connection each time.
///
/// # Precondition: do not fix the bearer when the factory is built
///
/// A factory that presents a bearer must read it from its [`TokenSource`] on
/// **every call** — never once, when the factory is built.
///
/// This is a precondition of the driver, not a suggestion. `run` marks its
/// renewal handle current the instant this closure returns, taking whatever
/// the attempt is about to present as consumed. A factory that fixed its
/// bearer when it was built has every later renewal consumed by a connect
/// which did not carry it, and the relay is then never told: not in band,
/// because the pump has nothing pending, and not on the next reconnect either,
/// because that attempt marks the handle current too.
///
/// Reading in the closure's own body is what the driver is written against.
/// Reading inside the [`ConnectFuture`] the closure returns is *discouraged*
/// rather than forbidden, and the difference between the two is waste against
/// loss: that read happens **after** the mark, not before it. `run` marks at
/// some version `v_m` and the future then reads at `v_f >= v_m`, so the
/// attempt carries everything the mark consumed and possibly more. A write
/// landing between the two makes the pump fire and costs one redundant
/// `0x12 RefreshToken` — waste, never loss. Only a build-time capture loses a
/// renewal, and loses it silently.
///
/// The two factories that ship read in the closure body —
/// `sunrise_cli::livesync::ws_factory` and `sunrise_core_bindings::ws_factory`
/// both call [`TokenSource::get`] there. Most harnesses cannot break it at
/// all: the in-process harness in this file's `tests` module and the three
/// factories in `sunrise-e2e` present no renewable bearer, so there is nothing
/// for the handle to consume, and each says so in its own documentation. The
/// test factories in this file that *do* read a [`TokenSource`] read it in the
/// closure body, per attempt, which is the shipped shape — that is what makes
/// them evidence about the driver rather than about themselves. A harness that
/// grows a bearer must do the same.
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
    /// Relay `/sync` endpoint URL (e.g. `https://relay.example/sync`). Used
    /// by the production transport factory the app assembles — Server-Sent
    /// Events downstream and typed `POST` upstream, per ADR-0023; the driver
    /// itself takes an already-built [`TransportFactory`].
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
    pub(crate) async fn shutdown_notified(&self) {
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
/// Keeps the encoded frame so it can be sent again *byte for byte*. Without it
/// "retry" would mean re-reading the outbox, which is not the same operation:
/// `Core::sync_outbox_grouped` returns whatever is unacked *now*, so a re-read
/// picks up ops enqueued since and mints a fresh `batch_id`. That is a new
/// batch, not a retransmit — the relay dedups on the ops' content, so a
/// re-partitioned batch is stored again, and the `Ack` comes back under a
/// number the client is no longer waiting on. `batch_id` is an ack correlator
/// and not an idempotency key (`docs/05-sync/wire-protocol.md`).
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
    /// Transport dropped, or the session failed locally; reconnect after
    /// backoff.
    Disconnected,
    /// The relay refused the stream and said why: `recv_frame` returned
    /// [`TransportError::Server`] with the relay's own code. Reconnects after
    /// backoff exactly as `Disconnected` does; it is a separate variant so the
    /// code reaches `sync.session.error` instead of being discarded with the
    /// error that carried it.
    Refused { code: &'static str, message: String },
    /// The relay sent a `Close`. The code's catalogue `retryable` flag decides
    /// what happens next: a retryable close reconnects after backoff, any
    /// other parks the driver in [`SyncState::Stopped`]. See
    /// [`SessionEnd::is_terminal`].
    Closed(ClosePayload),
}

impl SessionEnd {
    /// The end a failed `recv_frame` means. A relay refusal keeps its code;
    /// every other transport error is a drop.
    fn from_recv_error(e: TransportError) -> Self {
        match e {
            TransportError::Server { code, message } => Self::Refused { code, message },
            _ => Self::Disconnected,
        }
    }

    /// The end a `Close` frame's payload means.
    ///
    /// A payload that does not decode is a drop, not a terminal close: the
    /// transport already maps an unreadable *code* to `INTERNAL_UNKNOWN_CODE`
    /// (which is terminal), so reaching here means the frame itself is
    /// malformed — the same answer the transport gives a close with no code at
    /// all, which is a protocol error and therefore a reconnect.
    fn from_close(payload: &[u8]) -> Self {
        ClosePayload::decode(payload).map_or(Self::Disconnected, Self::Closed)
    }

    /// Whether this end must stop the driver reconnecting until the user acts.
    ///
    /// A close is terminal exactly when its code is not `retryable` in
    /// `crates/sunrise-error/codes.toml` ([`ErrorCode::retryable`]). That is
    /// the question the driver is asking — will reconnecting help? — and it
    /// is not the one [`ClosePayload::is_recoverable`] answers, which is
    /// whether the client can repair its *credential* on its own. The two
    /// differ at `RELAY_STORAGE_UNAVAILABLE`: a failed durable-log read on the
    /// relay, `transient` and `retryable` in the catalogue and logged that
    /// way by the relay that sends the close. Parking on it would strand every
    /// device that opened a stream during the fault until its user signed in
    /// again, for a condition that fixes itself.
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed(close) if !close.code.retryable())
    }

    /// The state the driver is in once this session is over.
    fn state_after(&self) -> SyncState {
        if self.is_terminal() {
            SyncState::Stopped
        } else {
            SyncState::Disconnected
        }
    }
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

/// The monotonic instant source a session schedules its deadlines against.
///
/// Injected rather than read ambiently, so the resync deadline and the
/// retransmit sweep can be driven to an exact instant in a test instead of
/// waited out on real time — which is [`#207`], and which is why
/// `backoff_sleep`'s reset arm had no test: reaching it meant sleeping thirty
/// seconds.
///
/// [`#207`]: https://github.com/justin13888/Sunrise/issues/207
///
/// # Why this is not [`crate::config::Clock`]
///
/// That seam is the vault's **wall clock**: `now_ms` is unix milliseconds, and
/// it stamps HLCs, `created_at`, reminder windows — everything whose value a
/// peer has to agree with. A deadline is not that. It has to survive an NTP
/// step, a daylight-saving jump and a user correcting their clock by an hour,
/// none of which may make a pending retransmit fire an hour early or an hour
/// late. And the pump ultimately hands the result to
/// [`tokio::time::sleep_until`], which consumes a monotonic
/// [`tokio::time::Instant`] and nothing else.
///
/// So this is not a second notion of "now" sitting beside `Clock`. It is the
/// timeline the driver was already on — tokio's — with the `now()` call made
/// injectable. `Clock` keeps its monopoly on "what time is it in the world";
/// this answers only "how long until the next thing is due".
pub(crate) trait MonotonicClock: Send + Sync + std::fmt::Debug {
    /// Now, on the same monotonic timeline [`tokio::time::sleep_until`] reads.
    fn now(&self) -> Instant;
}

/// The production clock: tokio's own, which is real time outside a test and
/// virtual under a paused runtime.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TokioClock;

impl MonotonicClock for TokioClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// What a session needs in order to schedule: how often to resync, and the
/// clock to measure it on.
///
/// One parameter rather than two because `session` is already at the
/// `too-many-arguments` threshold, and because the pair is meaningless split:
/// an interval with no clock cannot be turned into a deadline.
struct SessionSchedule {
    resync_interval: Duration,
    clock: Arc<dyn MonotonicClock>,
}

/// The credential state a session borrows from the driver, for the same reason
/// [`SessionSchedule`] exists: `session` is at the `too-many-arguments`
/// threshold, and these are one concern: which bearer this attempt carries,
/// and whether the relay answered it.
///
/// They are the driver's, not the session's — each outlives every session and
/// is borrowed for the length of one.
struct SessionCredential<'a> {
    /// The cell the factory reads per attempt and the pump re-announces from.
    source: &'a TokenSource,
    /// The driver's one renewal handle. Per driver rather than per session, so
    /// a write landing between two sessions is still observed.
    renewals: &'a mut TokenWatch,
    /// A renewal consumed at connect and not yet reported. Borrowed rather
    /// than passed by value because a session that never gets a handshake must
    /// hand it back outstanding, for the attempt that does. See
    /// [`note_marked_at_connect`].
    marked_at_connect: &'a mut Option<u64>,
    /// Set once the relay answers the handshake — the same moment the mark
    /// above is reported, because both ask whether this attempt's bearer
    /// reached the relay. [`SessionEnd`] cannot carry it:
    /// `Disconnected` is returned both by an attempt the relay never answered
    /// and by a session that was live for an hour, and `run` resets its
    /// reconnect backoff only for the second.
    answered: &'a mut bool,
}

/// Deadlines a live session is waiting on, and the loss evidence that pulls
/// the resync deadline forward.
struct Deadlines {
    /// Next anti-entropy resync.
    resync_at: Instant,
    /// Earliest a resync may happen at all, from [`MIN_RESYNC_GAP`].
    resync_floor: Instant,
    interval: Duration,
    /// Where every instant below comes from. Held rather than passed per call
    /// because `note_loss` is reached from deep inside frame handling, and
    /// threading a `now` through five frame kinds to reach it would put the
    /// clock in signatures that have nothing to do with time.
    clock: Arc<dyn MonotonicClock>,
}

impl Deadlines {
    fn new(interval: Duration, clock: Arc<dyn MonotonicClock>) -> Self {
        let now = clock.now();
        Self {
            resync_at: now + interval,
            resync_floor: now,
            interval,
            clock,
        }
    }

    /// Now, per the injected clock. The one reader of it outside this type is
    /// the retransmit sweep, which must compare against the same instant these
    /// deadlines were computed from.
    fn now(&self) -> Instant {
        self.clock.now()
    }

    /// Whether the anti-entropy resync is due.
    ///
    /// `<=`, not `<`: a deadline at exactly `resync_at` has arrived. The pump
    /// sleeps *until* this instant, so a strict comparison would wake, decide
    /// nothing was due, and sleep again on a zero-length timer.
    fn resync_due(&self) -> bool {
        self.resync_at <= self.clock.now()
    }

    /// Record that a resync just happened.
    fn resynced(&mut self) {
        let now = self.clock.now();
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
        let now = self.clock.now();
        self.resync_at = self.resync_at.min(self.resync_floor.max(now));
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
    // The production clock, constructed here rather than taken as a parameter:
    // `run`'s callers spawn the driver and have no opinion about time, and the
    // seam exists for the scheduling unit tests below, which build `Deadlines`
    // directly.
    let clock: Arc<dyn MonotonicClock> = Arc::new(TokioClock);
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
    // A renewal consumed out of the offline window that has not been reported
    // yet, carried across attempts that never reached the relay so the event
    // lands on the session whose bearer it actually answered. See
    // [`note_marked_at_connect`].
    let mut marked_at_connect: Option<u64> = None;
    while !shared.is_shutdown() {
        // Connect (cancellable by shutdown).
        let connect_fut = factory();
        // The factory has just read the credential, so this attempt carries
        // every renewal that landed while the driver was disconnected. Bring
        // the handle forward to what it read, and the pump's renewal arm below
        // fires only for writes made after this connect.
        //
        // This NARROWS the window; it does not close it. The factory's read
        // and the mark below are two operations on two cells with no lock
        // spanning them, and `TokenSource::set` is called from other threads —
        // the FFI seam in `sunrise-core-bindings` and the CLI's login flow. A
        // `set` that completes wholly between the two is consumed here without
        // having been carried, so it reaches the relay on the next reconnect
        // rather than in band. What the move bought is the size of the window:
        // it used to span the dial, the handshake and the subscribe, and is
        // now the few instructions between these two statements.
        marked_at_connect = mark_renewals_current(&mut renewals, marked_at_connect);
        let connected = tokio::select! {
            biased;
            () = shared.shutdown_notified() => break,
            res = connect_fut => res,
        };
        let transport = match connected {
            Ok(t) => {
                // `Ok` here is a transport object, not a round trip. Every
                // factory in this workspace constructs one with no I/O — the
                // shipped two build an `SseTransport`, which dials nothing —
                // so this attempt has not yet reached the relay and a consumed
                // renewal is not reported here. `session` reports it once the
                // handshake comes back.
                //
                // Nor is the reconnect counter reset here, for the same
                // reason: this arm is taken on **every** attempt, so a reset
                // here zeroes the counter before it can advance and pins the
                // driver to the first step of the schedule
                // `docs/05-sync/offline-queue.md` §Backoff publishes (#283).
                // It is reset below, once `session` says the relay answered.
                t
            }
            Err(e) => {
                // "The client isn't syncing" is the single most common
                // support question, and these are the lines that answer it:
                // the relay is unreachable, here is what it said.
                //
                // `err_code` describes the *error*, not what the driver does
                // next. A device-signature refusal is permanent — reconnecting
                // re-presents the same wrong clock or the same wrong key — and
                // the driver still backs off and retries. `SyncState::Stopped`
                // exists, but only a relay's own terminal `Close` parks the
                // driver in it: a refusal's code can be one the transport
                // derived from an HTTP status, and parking on a guess would
                // strand a device the relay never meant to turn away. Naming
                // the code here is what lets an operator tell "the relay is
                // down" from "this device will never connect".
                match &e {
                    TransportError::Server { code, .. }
                        if *code == ErrorCode::AuthDeviceSigInvalid.as_str() =>
                    {
                        tracing::warn!(
                            ev = "sync.session.error",
                            err_code = %ErrorCode::AuthDeviceSigInvalid,
                            err_kind = "permanent",
                            retryable = false,
                            result = "failed",
                            cause = %e,
                            "relay refused this device's signature; check the clock, not the token"
                        );
                    }
                    // `SYNC_CONNECT_FAILED` used to sit here and is in no
                    // catalogue: nothing could map it, and a client switching
                    // on codes saw a string that does not exist.
                    _ => tracing::warn!(
                        ev = "sync.session.error",
                        err_code = %ErrorCode::SyncNetworkUnavailable,
                        err_kind = "transient",
                        retryable = true,
                        result = "failed",
                        cause = %e,
                        "relay connect failed"
                    ),
                }
                if !backoff_sleep(&mut backoff, rng.as_ref(), &shared).await {
                    break;
                }
                continue;
            }
        };

        // A session needs the Core alive; if it's gone, stop.
        let Some(core) = weak.upgrade() else { break };
        // Per attempt: set by `session` once the relay answers the handshake.
        let mut answered = false;
        let end = session(
            &core,
            &shared,
            transport,
            rng.as_ref(),
            SessionSchedule {
                resync_interval,
                clock: Arc::clone(&clock),
            },
            SessionCredential {
                source: &credential,
                renewals: &mut renewals,
                marked_at_connect: &mut marked_at_connect,
                answered: &mut answered,
            },
        )
        .await;
        // `reset`'s documented precondition is a successful operation, and a
        // constructed transport is not one: the handshake is the first round
        // trip the relay answers. An attempt that never got that far keeps
        // climbing the schedule, so an unreachable relay is retried in bursts
        // of five and then every 30 s rather than every 100 ms. A session that
        // did handshake — even one that then dropped at once — starts the
        // next reconnect from the first step, because a client connected for
        // a day that drops once should retry immediately.
        if answered {
            backoff.reset();
        }
        drop(core);
        shared.set_state(end.state_after());
        log_session_end(&end);
        if matches!(end, SessionEnd::Shutdown) {
            break;
        }
        if end.is_terminal() {
            let Some(to_v) = park_until_renewed(&shared, &mut renewals).await else {
                break;
            };
            // The wait consumed the renewal, so the next connect's mark finds
            // the handle current. Carry it as a renewal from the offline
            // window, which is what it is, so the handshake that presents it
            // still reports it.
            marked_at_connect = Some(to_v);
            continue;
        }
        if !backoff_sleep(&mut backoff, rng.as_ref(), &shared).await {
            break;
        }
    }
    shared.set_state(SyncState::Disconnected);
}

/// Wait in [`SyncState::Stopped`] for the credential to be replaced.
///
/// The relay has said reconnecting will not help, so nothing on a timer does.
/// What can help is the user: signing in again replaces the credential, and
/// that write is what this waits for. An app restart builds a new driver,
/// which is the other way out.
///
/// `Some(version)` of the new credential, after moving back to
/// `Disconnected` for the reconnect that follows; `None` on shutdown.
async fn park_until_renewed(shared: &SyncShared, renewals: &mut TokenWatch) -> Option<u64> {
    let to_v = tokio::select! {
        biased;
        () = shared.shutdown_notified() => return None,
        v = renewals.changed() => v,
    };
    shared.set_state(SyncState::Disconnected);
    tracing::info!(
        ev = "sync.session.resumed",
        to_v,
        "credential replaced after a terminal close; reconnecting"
    );
    Some(to_v)
}

/// `sync.session.closed` for every end, and `sync.session.error` for the ends
/// the relay chose and named.
///
/// The code is the point. A refused stream, a dropped connection and a close
/// for a revoked device used to produce the same line, so an operator could
/// not tell "the relay is down" from "this device will never be let back in".
fn log_session_end(end: &SessionEnd) {
    let (result, err_code) = match end {
        SessionEnd::Shutdown => ("ok", None),
        SessionEnd::Disconnected => ("failed", None),
        SessionEnd::Refused { code, .. } => ("failed", Some(*code)),
        SessionEnd::Closed(close) if end.is_terminal() => ("stopped", Some(close.code.as_str())),
        SessionEnd::Closed(close) => ("failed", Some(close.code.as_str())),
    };
    tracing::info!(
        ev = "sync.session.closed",
        result,
        err_code,
        "sync session ended"
    );
    match end {
        SessionEnd::Shutdown | SessionEnd::Disconnected => {}
        // Worded like the connect path's arm for the same code, because it
        // is the same refusal arriving by the other route.
        SessionEnd::Refused { code, message }
            if *code == ErrorCode::AuthDeviceSigInvalid.as_str() =>
        {
            tracing::warn!(
                ev = "sync.session.error",
                err_code = *code,
                err_kind = "permanent",
                retryable = false,
                result = "failed",
                cause = %message,
                "relay refused this device's signature; check the clock, not the token"
            );
        }
        SessionEnd::Refused { code, message } => tracing::warn!(
            ev = "sync.session.error",
            err_code = *code,
            err_kind = "transient",
            retryable = true,
            result = "failed",
            cause = %message,
            "relay refused the event stream"
        ),
        // `err_kind` and `retryable` are the catalogue's for the relay's code,
        // so this line agrees with the relay's own log of the same close.
        SessionEnd::Closed(close) if !end.is_terminal() => tracing::warn!(
            ev = "sync.session.error",
            err_code = close.code.as_str(),
            err_kind = error_kind_str(close.code.default_kind()),
            retryable = true,
            result = "failed",
            cause = %close.reason,
            "relay closed the session; reconnecting"
        ),
        SessionEnd::Closed(close) => tracing::warn!(
            ev = "sync.session.error",
            err_code = close.code.as_str(),
            err_kind = error_kind_str(close.code.default_kind()),
            retryable = false,
            result = "stopped",
            cause = %close.reason,
            "relay closed the session for a reason reconnecting cannot fix; waiting for a new credential"
        ),
    }
}

/// The `err_kind` log value for `kind`, spelled as `codes.toml` spells it.
const fn error_kind_str(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Transient => "transient",
        ErrorKind::Permanent => "permanent",
        ErrorKind::User => "user",
        ErrorKind::Internal => "internal",
    }
}

/// Sleep for the next backoff delay, interruptible by shutdown. Returns
/// `false` if shutdown was requested (caller should stop).
async fn backoff_sleep(backoff: &mut Backoff, rng: &dyn Rng, shared: &SyncShared) -> bool {
    if shared.is_shutdown() {
        return false;
    }
    let delay = next_backoff_delay(backoff, rng_unit(rng));
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

/// How long the next reconnect waits, and the attempt bookkeeping that goes
/// with it. Pure: it decides the delay, it does not wait it.
///
/// Split out of [`backoff_sleep`] so the schedule is testable without spending
/// the schedule. The `else` arm is the reason — a long-lived client never gives
/// up, so an exhausted policy resets and waits a flat, **un-jittered** 30 s
/// (`docs/05-sync/offline-queue.md` §backoff documents that as load-bearing).
/// Reaching it through `backoff_sleep` costs a real thirty-second sleep, so
/// nothing did, and the arm shipped untested. Here it costs six calls.
///
/// `jitter_unit` is in `[0, 1]`; see [`Backoff::next_delay`].
fn next_backoff_delay(backoff: &mut Backoff, jitter_unit: f64) -> Duration {
    // Never give up on a long-lived client: once the policy is exhausted, cap
    // at the max delay and keep retrying.
    if let Some(d) = backoff.next_delay(jitter_unit) {
        backoff.record_attempt();
        d
    } else {
        backoff.reset();
        Duration::from_millis(30_000)
    }
}

/// Bring the driver's renewal handle forward to the credential this connect
/// read, and fold the result into what this connect still owes the log.
///
/// `Some(v)` when a renewal out of the offline window has been consumed and
/// not yet reported — by this attempt, or by an earlier attempt the relay never
/// answered. `None` when the handle was already current and nothing is
/// outstanding.
///
/// Called once per connect attempt, immediately after the factory returned and
/// long before the handshake, because that return is the moment this attempt's
/// bearer is fixed — *provided the factory did not fix its bearer when it was
/// built*. That is a precondition, not an observation: it is stated on
/// [`TransportFactory`], and the two shipped factories
/// (`sunrise_cli::livesync::ws_factory` and `sunrise_core_bindings::ws_factory`)
/// meet it while every test harness in the tree presents no renewable bearer
/// at all and so cannot violate it. Bringing the handle forward here — and no
/// later — marks what such an attempt carries and little else, so a write
/// landing during the dial, the handshake or the session itself still reaches
/// the relay in band as a `0x12 RefreshToken` instead of waiting for the next
/// reconnect.
///
/// # Why an attempt's mark outlives the attempt
///
/// An attempt that then fails has still advanced the handle, and that is sound
/// for the *handle*: the next attempt re-reads the credential and so carries at
/// least as much as this one would have. It is not sound for the *record*,
/// which is why the carry-forward is here rather than at the call site.
/// Dropping it would make a renewal consumed by a failed attempt invisible —
/// the failing attempt cannot report a bearer the relay never answered, and the
/// attempt that does get a handshake finds the handle already current and has
/// nothing of its own to report.
///
/// "Failed" here means failed to be answered, not failed to be constructed. No
/// factory in this workspace can resolve its [`ConnectFuture`] to `Err`, so the
/// attempt this carry exists for is one that built a transport and then lost
/// its handshake — a relay that is down, or a refusal — which is the ordinary
/// failure and not a rare one.
///
/// The residual, stated because the call site reads like a proof and is not
/// one: the factory's read and this mark are two operations on two cells with
/// nothing ordering them, and `TokenSource::set` is called from other threads.
/// A `set` completing between them is consumed here without having been
/// carried, and rides the next reconnect instead of the live session. The move
/// shrank that window from "the dial, the handshake and the subscribe" to a
/// few instructions; it did not remove it.
fn mark_renewals_current(renewals: &mut TokenWatch, unreported: Option<u64>) -> Option<u64> {
    renewals.mark_current().or(unreported)
}

/// Report a renewal this connect carried out of the offline window.
///
/// The one credential transition in this file that used to be silent, and the
/// one that suppresses the other three: when this fires,
/// `sync.credential.renewed` will not, because the pump has nothing left
/// pending. Without it a swallowed renewal and a session in which no renewal
/// ever happened produce identical logs.
///
/// `None` is the steady state and says nothing, so a reconnect loop with no
/// renewal behind it does not carry one of these per attempt.
///
/// Called once the relay has answered the handshake, because the version is
/// evidence about a bearer the relay actually received.
///
/// Neither of the two earlier sites will do. Emitting before the dial
/// attributes the renewal to an attempt that may never connect, and then
/// leaves the attempt that did connect silent. Emitting when the
/// [`ConnectFuture`] resolves `Ok` is barely later: `Ok` is a transport
/// *object*, and every factory in this workspace — the two shipped ones
/// included — constructs one with no I/O, so it is evidence that a `struct`
/// was built and nothing more. Either way the event lands between
/// `sync.session.closed` and `sync.backoff` for an attempt whose header never
/// reached the relay, which is precisely the adjacency
/// `docs/10-cross-cutting/log-events.md` tells an operator to read it against.
fn note_marked_at_connect(marked: Option<u64>) {
    let Some(to_v) = marked else { return };
    tracing::debug!(
        ev = "sync.credential.marked_at_connect",
        to_v,
        "connect carried a renewal from the offline window; not re-announcing it"
    );
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
                // A close before the handshake completes is still a close, and
                // its code decides the same thing it decides mid-session.
                Ok((h, payload)) if h.msg_kind == MsgKind::Close => {
                    return Err(SessionEnd::from_close(&payload));
                }
                // Any other frame before HelloAck: keep waiting.
                Ok(_) => {}
                Err(_) => return Err(SessionEnd::Disconnected),
            },
            Ok(None) => return Err(SessionEnd::Disconnected),
            Err(e) => return Err(SessionEnd::from_recv_error(e)),
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
    schedule: SessionSchedule,
    credential: SessionCredential<'_>,
) -> SessionEnd {
    let SessionCredential {
        source: credential,
        renewals,
        marked_at_connect,
        answered,
    } = credential;
    let refresh_negotiated = match handshake(core, shared, &mut transport).await {
        Ok(negotiated) => negotiated,
        Err(end) => return end,
    };
    *answered = true;
    // The handshake is the first round trip this attempt makes: its
    // `Authorization` header has now reached the relay and been answered. That
    // is what the report claims, so it is made here and not at the connect,
    // where nothing has left the process yet. An attempt that got a transport
    // and no handshake leaves the mark outstanding for the attempt that does.
    note_marked_at_connect(marked_at_connect.take());

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
    let mut deadlines = Deadlines::new(schedule.resync_interval, schedule.clock);

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
        deadlines.now(),
    )
    .is_err()
    {
        return SessionEnd::Disconnected;
    }
    if let Ok(p) = core.sync_pending() {
        shared.set_pending(p);
    }
    // A session is the first moment a revocation can reach the relay, so this
    // runs once per session rather than on a timer: the ordinary case is that
    // the user revoked a device while offline, or on a device that then closed.
    // Reconnect backoff is therefore the retry schedule.
    drain_relay_revocations(core, transport.as_mut()).await;
    // A session is also the first moment an attachment sealed offline can
    // reach the relay, and the first moment one created elsewhere can be
    // fetched. Both directions run here for the same reason the revocation
    // does: reconnect backoff is the retry schedule.
    drain_blob_transfers(core, transport.as_mut()).await;
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
                // The same wake the outbox row uses, for the row written in
                // the same transaction. Without this a revocation made while
                // the session is already up would wait for the connection to
                // drop before the relay heard about it — which is the one case
                // where the user is watching, and the relay-side bound is the
                // whole point of pressing the button. The read is a single-row
                // query against an almost always empty table.
                drain_relay_revocations(core, transport.as_mut()).await;
                // `attach_file` writes its queue row and then submits the op,
                // which pokes this wake — so the upload starts in the same
                // breath as the metadata rather than waiting for a reconnect.
                drain_blob_transfers(core, transport.as_mut()).await;
                if build_outbox_frames(
                    core,
                    &mut subscribed,
                    &mut inflight_ops,
                    &mut inflight,
                    &mut batch_counter,
                    &mut pending_sends,
                    rng,
                    deadlines.now(),
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
                if deadlines.resync_due() {
                    match encode_subscribe_all(core) {
                        Ok(frame) => pending_sends.push(frame),
                        Err(()) => return SessionEnd::Disconnected,
                    }
                    // Anti-entropy for bytes, on the same backstop that covers
                    // frames. Both halves need it and for the same reason the
                    // resync itself does — the last event in each direction is
                    // the one nothing else re-triggers. A blob whose metadata
                    // arrived before its upload finished answers 404 once and
                    // would otherwise wait for the *next* inbound batch, which
                    // on a quiet vault is never; an upload refused by a relay
                    // that has since recovered would wait for the next local
                    // edit. This is the timer that makes both converge without
                    // one.
                    drain_blob_transfers(core, transport.as_mut()).await;
                    deadlines.resynced();
                }
            }
            SessionEvent::Recv(Ok(Some(bytes))) => {
                if let Err(end) = handle_frame(
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
                {
                    return end;
                }
                // An inbound batch is the only way an attachment created on
                // another device becomes known here, so it is the trigger the
                // fetch half needs: without it a device that stays connected
                // would not look for new bytes until its next reconnect, and a
                // long-lived session is the ordinary case rather than the
                // exception.
                drain_blob_transfers(core, transport.as_mut()).await;
                maybe_live(core, shared, &subscribed, &caught_up);
            }
            SessionEvent::Recv(Ok(None)) => return SessionEnd::Disconnected,
            SessionEvent::Recv(Err(e)) => return SessionEnd::from_recv_error(e),
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
    // The same clock the deadlines were computed from. Reading `Instant::now`
    // here instead would compare two different timelines the moment either one
    // is faked.
    let now = deadlines.now();
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

/// Process one inbound frame. `Err(end)` ends the session with `end`;
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
) -> Result<(), SessionEnd> {
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
                    let pending = core
                        .sync_mark_acked(&batch.op_ids)
                        .map_err(|_| SessionEnd::Disconnected)?;
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
                match core.apply_remote_all(env).await {
                    Ok(events) if !events.is_empty() => {
                        subscribe_to_new_streams(&events, subscribed, pending_sends);
                        shared.mark_synced(core.now_ms());
                    }
                    // Idempotent re-receive, or an op parked awaiting its key:
                    // nothing to do, nothing wrong.
                    Ok(_) => {}
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
        // Server-initiated close. Its code decides whether to reconnect:
        // `AUTH_TOKEN_EXPIRED` does, and a revoked device, unavailable relay
        // storage or an unreadable code parks the driver in `Stopped`
        // (`docs/05-sync/wire-protocol.md` §Connection lifecycle).
        MsgKind::Close => return Err(SessionEnd::from_close(&payload)),
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
        // Nack and every other kind are non-fatal in the self-host build: they fall
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
    now: Instant,
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
        let due_at = now + backoff.next_delay(rng_unit(rng)).unwrap_or(MIN_RESYNC_GAP);
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

/// Tell the relay about every device this vault has revoked and not yet
/// reported.
///
/// The relay cannot learn this from the op stream — `device_revoke` is sealed
/// under the vault-meta key and the relay holds no Stream keys — so the two
/// halves of a revocation travel separately, and this is the second one. Until
/// it lands the relay keeps accepting the revoked device's uploads and keeps
/// streaming it everyone else's, which is `#80`.
///
/// It sends only what the register still agrees with. A re-fold can unwind the
/// register row an intent was queued beside, and an intent sent after that
/// would have the relay refuse a device every replica shows as current (#257).
/// Such a row is skipped, not deleted, so it is sent after all if a later fold
/// revokes the device again; `crate::relay_intents` holds the one definition.
/// The check is repeated for each device immediately before its send, not
/// read once for the whole drain: the sends await the network, and a local
/// command can unwind a device while an earlier one is in flight. An unwind
/// that lands during a device's own send is the case the module doc names:
/// the relay has been told, and nothing takes that back.
///
/// Failures leave the row in place, deliberately. The device this is about is
/// typically lost or stolen; giving up after one refused call would mean the
/// revocation silently never reached the relay, which is the failure this whole
/// mechanism exists to prevent. `attempts` is what makes a relay that keeps
/// refusing visible in the log rather than retried in silence.
///
/// [`RevokeOutcome::Unknown`] clears the row and warns rather than reporting
/// success. It is terminal — the relay holds no active row with that vault id
/// and retrying cannot make one appear — but it is *not* the outcome the user
/// asked for: it is also the answer for a device that registered before
/// `vault_device_id` existed, which the relay is still accepting under a row
/// this vault cannot name. Reading it as success is the exact defect PR #86
/// withdrew.
///
/// A [`TransportError::Unsupported`] transport is not an attempt: the loopback
/// and in-process transports have no account API, and counting them would
/// inflate `attempts` on every convergence test that revokes anything.
async fn drain_relay_revocations<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(pending) = core.pending_relay_revocations() else {
        return;
    };
    for device_id in pending {
        // The snapshot above is taken once, and every send below awaits the
        // network, so a local command can unwind a later device's register row
        // while an earlier one is in flight. Ask again right before each send.
        match core.relay_revocation_pending(&device_id) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return,
        }
        match transport.revoke_device(device_id).await {
            Ok(RevokeOutcome::Revoked) => {
                let _ = core.clear_relay_revocation(device_id);
                tracing::info!(
                    ev = "sync.device.revoke_relayed",
                    subject_h = hex_short(&device_id),
                    "the relay has been told to stop accepting a revoked device"
                );
            }
            Ok(RevokeOutcome::Unknown) => {
                let _ = core.clear_relay_revocation(device_id);
                tracing::warn!(
                    ev = "sync.device.revoke_unknown_to_relay",
                    subject_h = hex_short(&device_id),
                    "the relay holds no device bound to this vault device id; if that device \
                     registered before it sent one, the relay is still accepting it"
                );
            }
            Err(TransportError::Unsupported) => return,
            Err(e) => {
                let attempts = core
                    .note_relay_revocation_attempt(device_id, core.now_ms())
                    .unwrap_or(0);
                tracing::warn!(
                    ev = "sync.device.revoke_not_relayed",
                    subject_h = hex_short(&device_id),
                    attempt = attempts,
                    cause = %e,
                    "the relay has not accepted a device revocation; it still accepts that device"
                );
            }
        }
    }
}

/// Move attachment ciphertext in both directions: queued blobs up, missing
/// blobs down.
///
/// This is the step that was absent, and its absence is issue #176: the
/// attachment metadata op reached every device and the bytes reached none, so a
/// Task's body was complete on the machine that made it and incomplete
/// everywhere else. [`crate::blob_sync`] carries the reasoning for every policy
/// decision below — where the upload is driven from, what a retry re-uses, why
/// the read side is a query rather than a queue, and what each bound is.
///
/// Like [`drain_relay_revocations`], a [`TransportError::Unsupported`]
/// transport ends the drain without counting an attempt: the in-process
/// loopbacks carry the op stream and nothing else, and charging them would
/// exhaust `MAX_UPLOAD_ATTEMPTS` on every convergence test that attaches a file.
async fn drain_blob_transfers<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    if upload_queued_blobs(core, transport).await.is_err() {
        return;
    }
    // Asked-for downloads before unasked ones. Both halves are sequential and
    // an attachment may be 100 MB, so the order decides how long somebody
    // watching a Download button waits — and nobody is watching the automatic
    // queue. See [`crate::blob_fetch`] for the rest of that route.
    if crate::blob_fetch::drain_requested(core, transport)
        .await
        .is_err()
    {
        return;
    }
    fetch_missing_blobs(core, transport).await;
}

/// `init` → `PUT` → `finalize`, for up to `MAX_UPLOADS_PER_DRAIN` queued blobs.
///
/// `Err(())` means this transport has no blob API, so the caller should stop
/// rather than try the fetch half against it too.
async fn upload_queued_blobs<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
) -> Result<(), ()> {
    let Ok(pending) = core.pending_blob_uploads() else {
        return Ok(());
    };
    for row in pending {
        match upload_one_blob(core, transport, &row).await {
            Ok(()) => {}
            Err(TransportError::Unsupported) => return Err(()),
            Err(e) => {
                // The row survives every failure. Whatever went wrong, the
                // bytes are still on this disk and still not on the relay, and
                // that is exactly what the row says.
                let attempts = core.note_blob_upload_attempt(&row.blob_id).unwrap_or(0);
                // A refusal about the upload *id* is the one thing re-using it
                // cannot survive, so this is where the id is dropped and a
                // fresh `init` is allowed. Anything else — a dropped
                // connection, a 5xx, a refused signature — keeps it, because
                // keeping it is what stops a retry leaving a second pending
                // directory on the relay.
                if matches!(&e, TransportError::Server { code, .. }
                    if *code == ErrorCode::SyncOpInvalid.as_str())
                {
                    let _ = core.forget_blob_upload_id(&row.blob_id);
                }
                tracing::warn!(
                    ev = "sync.blob.upload_failed",
                    err_code = "SYNC_NETWORK_UNAVAILABLE",
                    err_kind = "transient",
                    retryable = attempts < crate::blob_sync::MAX_UPLOAD_ATTEMPTS,
                    blob_h = hex_short(&row.blob_id),
                    attempt = attempts,
                    cause = %e,
                    "an attachment's bytes have not reached the relay"
                );
            }
        }
    }
    Ok(())
}

/// One blob, all the way to a committed relay-side copy.
async fn upload_one_blob<T: Transport + ?Sized>(
    core: &Core,
    transport: &mut T,
    row: &crate::blob_sync::PendingUpload,
) -> Result<(), TransportError> {
    let Ok(Some(chunks)) = core.sealed_chunks(&row.blob_id, row.chunk_count) else {
        // The chunks are not on this disk. Nothing can be uploaded and no
        // number of retries will change that, so the row goes rather than
        // consuming a slot in every drain for the life of the vault.
        let _ = core.clear_blob_upload(&row.blob_id);
        tracing::warn!(
            ev = "sync.blob.upload_abandoned",
            blob_h = hex_short(&row.blob_id),
            "a queued attachment's chunks are missing locally; nothing to upload"
        );
        return Ok(());
    };

    // The id from a previous attempt, or one reserved now. Persisted before the
    // first chunk leaves, so even a crash here retries under this id.
    let upload_id = if let Some(id) = &row.upload_id {
        id.clone()
    } else {
        let id = transport
            .blob_init(&row.stream_id, row.chunk_count, row.size_bytes)
            .await?;
        core.record_blob_upload_id(&row.blob_id, &id)
            .map_err(|e| TransportError::Protocol(e.to_string()))?;
        id
    };

    for (idx, chunk) in chunks.iter().enumerate() {
        let idx = u32::try_from(idx).unwrap_or(u32::MAX);
        transport.blob_put_chunk(&upload_id, idx, chunk).await?;
    }

    let hashes: Vec<[u8; 32]> = chunks
        .iter()
        .map(|c| sunrise_crypto::ciphertext_hash(std::iter::once(c.as_slice())))
        .collect();
    let commit = transport
        .blob_finalize(&upload_id, &row.ciphertext_hash, &hashes)
        .await?;

    // The relay content-addresses by the hash this device computed while
    // sealing, so the committed id is one this device already predicted — and
    // every other device predicts the same one from the same op. A
    // disagreement would mean the two sides hashed different bytes, which
    // `finalize` should have refused, so it is logged rather than trusted.
    let mut expected = [0u8; 16];
    expected.copy_from_slice(&row.ciphertext_hash[..16]);
    if commit.blob_id != expected {
        tracing::warn!(
            ev = "sync.blob.upload_id_unexpected",
            blob_h = hex_short(&row.blob_id),
            "the relay committed this blob under an address no reader will ask for"
        );
    }

    let _ = core.clear_blob_upload(&row.blob_id);
    tracing::info!(
        ev = "sync.blob.uploaded",
        blob_h = hex_short(&row.blob_id),
        n_chunks = row.chunk_count,
        "an attachment's bytes are on the relay and readable by this account's other devices"
    );
    Ok(())
}

/// Pull the ciphertext for attachments this device has metadata for and bytes
/// for.
async fn fetch_missing_blobs<T: Transport + ?Sized>(core: &Core, transport: &mut T) {
    let Ok(wanted) = core.attachments_awaiting_bytes() else {
        return;
    };
    for att in wanted {
        let Some(relay_id) = att.relay_blob_id() else {
            continue;
        };
        match transport.blob_fetch(&relay_id).await {
            // Not committed yet, or not this account's. Either way there is
            // nothing to hold on to: the attachment row is already the durable
            // record of what to ask for, so the next drain asks again.
            Ok(None) => {}
            Ok(Some(body)) => match core.store_fetched_blob(&att, &body) {
                Ok(true) => tracing::info!(
                    ev = "sync.blob.fetched",
                    blob_h = hex_short(&att.blob_id),
                    "an attachment created on another device is now readable here"
                ),
                Ok(false) => tracing::warn!(
                    ev = "sync.blob.fetch_rejected",
                    err_code = "SYNC_OP_INVALID",
                    err_kind = "user",
                    retryable = true,
                    blob_h = hex_short(&att.blob_id),
                    "the relay returned bytes that are not this attachment's; nothing was stored"
                ),
                Err(e) => tracing::warn!(
                    ev = "sync.blob.fetch_not_stored",
                    blob_h = hex_short(&att.blob_id),
                    cause = %e,
                    "an attachment's bytes arrived and could not be written locally"
                ),
            },
            Err(TransportError::Unsupported) => return,
            Err(e) => tracing::warn!(
                ev = "sync.blob.fetch_failed",
                err_code = "SYNC_NETWORK_UNAVAILABLE",
                err_kind = "transient",
                retryable = true,
                blob_h = hex_short(&att.blob_id),
                cause = %e,
                "an attachment's bytes could not be fetched; it stays unreadable here"
            ),
        }
    }
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

/// Queue a `Subscribe` for every Stream a delivery just taught this device
/// about, so that Stream's task ops start flowing.
///
/// Scanned across **every** event one delivery produced, not just the first: a
/// `key_envelope` op releases whatever was parked waiting for its key, and a
/// `stream.create` can be among them. A driver that read only the first event
/// would never subscribe, and the Stream's tasks would never arrive.
fn subscribe_to_new_streams(
    events: &[DomainEvent],
    subscribed: &mut HashSet<[u8; 16]>,
    pending_sends: &mut Vec<Vec<u8>>,
) {
    for ev in events {
        let DomainEvent::Created(r) = ev else {
            continue;
        };
        if r.kind() != EntityKind::Stream {
            continue;
        }
        let sid = *r.bytes();
        if subscribed.insert(sid) {
            if let Ok(frame) = encode_subscribe_one(sid) {
                pending_sends.push(frame);
            }
        }
    }
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
mod scheduling {
    //! The driver's pure scheduling arithmetic, driven with no sleeping at all.
    //!
    //! Separate from the harness tests below, which need a fake relay and a
    //! `multi_thread` runtime — the combination that made these deadlines
    //! untestable in the first place (`#207`). Nothing here starts a runtime,
    //! opens a vault or connects a transport: the backoff schedule is a pure
    //! function of an attempt counter and a jitter unit, and every instant
    //! comes from an injected [`MonotonicClock`] a test moves by hand.
    //!
    //! The delays asserted here are the ones
    //! `docs/10-cross-cutting/error-handling.md` §canonical-retry-policy and
    //! `docs/05-sync/offline-queue.md` §backoff state, so a silent change to
    //! either reddens this module rather than the docs drifting.

    use super::{
        next_backoff_delay, next_deadline, retransmit_due, Backoff, Deadlines, InflightBatch,
        LossEvidence, MonotonicClock, MIN_RESYNC_GAP,
    };
    use crate::config::Rng;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::Instant;

    /// A clock a test moves by hand. No timer, no runtime, no waiting.
    #[derive(Debug)]
    struct ManualClock(Mutex<Instant>);

    impl ManualClock {
        fn started() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Instant::now())))
        }

        fn advance(&self, by: Duration) {
            let mut at = self.0.lock();
            *at += by;
        }
    }

    impl MonotonicClock for ManualClock {
        fn now(&self) -> Instant {
            *self.0.lock()
        }
    }

    /// An RNG whose [`super::rng_unit`] is exactly 0.5, the midpoint of the
    /// jitter band — so a delay comes out at its base with no jitter applied
    /// and the *schedule* is what the assertion is about.
    ///
    /// `rng_unit` is `(u64::from_le_bytes(b) >> 11) / 2^53`, so the midpoint is
    /// the top bit alone: `2^63 >> 11 == 2^52`, and `2^52 / 2^53 == 0.5`.
    #[derive(Debug)]
    struct MidJitterRng;

    impl Rng for MidJitterRng {
        fn fill_bytes(&self, dest: &mut [u8]) {
            dest.fill(0);
            if let Some(last) = dest.last_mut() {
                *last = 0x80;
            }
        }
    }

    fn ms(d: Duration) -> u64 {
        u64::try_from(d.as_millis()).expect("a backoff delay fits in u64 ms")
    }

    /// The reconnect schedule end to end, including the arm that only a sixth
    /// consecutive failure reaches.
    ///
    /// Jitter is pinned to the midpoint, so these are the policy's bases
    /// exactly: 100 ms doubling five times, then the flat 30 s an exhausted
    /// policy waits, then back to 100 ms because a long-lived client cycles
    /// rather than giving up.
    #[test]
    fn reconnect_backoff_doubles_five_times_then_cycles_through_a_flat_thirty_seconds() {
        let mut backoff = Backoff::canonical();
        let schedule: Vec<u64> = (0..7)
            .map(|_| ms(next_backoff_delay(&mut backoff, 0.5)))
            .collect();
        assert_eq!(
            schedule,
            vec![100, 200, 400, 800, 1_600, 30_000, 100],
            "the canonical policy's delays, in order"
        );
        assert_eq!(
            backoff.attempt(),
            1,
            "the seventh call is the second cycle's first attempt"
        );
    }

    /// The exhaustion arm is flat *and* un-jittered — the one delay in the
    /// driver that is identical on every device, which is what
    /// `docs/05-sync/offline-queue.md` §backoff calls load-bearing.
    #[test]
    fn the_exhausted_arm_is_thirty_seconds_regardless_of_jitter() {
        for unit in [0.0_f64, 0.5, 1.0] {
            let mut backoff = Backoff::canonical();
            for _ in 0..5 {
                let _ = next_backoff_delay(&mut backoff, unit);
            }
            assert!(backoff.exhausted(), "five attempts exhaust the policy");
            assert_eq!(
                next_backoff_delay(&mut backoff, unit),
                Duration::from_millis(30_000),
                "jitter {unit} must not move the exhausted delay"
            );
            assert_eq!(backoff.attempt(), 0, "the exhausted arm resets the counter");
        }
    }

    /// The wrapper passes the jitter unit straight through to the policy
    /// rather than swallowing or re-deriving it.
    ///
    /// The ±20% band itself, the clamp on an out-of-range unit and the burst
    /// arithmetic belong to `Backoff` and are pinned next door, in
    /// `crates/sunrise-sync/tests/backoff_policy.rs`. Repeating them here
    /// would be two copies of one contract. What is *not* covered there is
    /// this function, because the cycling arm lives in the driver.
    #[test]
    fn the_jitter_unit_reaches_the_policy_unchanged() {
        let band: Vec<u64> = [0.0_f64, 0.5, 1.0]
            .into_iter()
            .map(|unit| ms(next_backoff_delay(&mut Backoff::canonical(), unit)))
            .collect();
        assert_eq!(band, vec![80, 100, 120], "100 ms base, jittered ±20%");
    }

    /// The resync deadline fires **at** its interval and not one millisecond
    /// before.
    #[test]
    fn the_resync_deadline_fires_at_the_interval_and_not_before() {
        let clock = ManualClock::started();
        let interval = Duration::from_secs(30);
        let deadlines = Deadlines::new(interval, Arc::clone(&clock) as Arc<dyn MonotonicClock>);

        assert!(!deadlines.resync_due(), "nothing is due at t=0");
        clock.advance(interval - Duration::from_millis(1));
        assert!(
            !deadlines.resync_due(),
            "one millisecond short of the interval is still short"
        );
        clock.advance(Duration::from_millis(1));
        assert!(deadlines.resync_due(), "the deadline fires at the interval");
    }

    /// A resync re-arms the next one a full interval out, and closes the
    /// minimum gap behind it.
    #[test]
    fn a_resync_rearms_the_interval_and_opens_the_minimum_gap() {
        let clock = ManualClock::started();
        let interval = Duration::from_secs(30);
        let mut deadlines = Deadlines::new(interval, Arc::clone(&clock) as Arc<dyn MonotonicClock>);

        clock.advance(interval);
        assert!(deadlines.resync_due());
        deadlines.resynced();
        assert!(
            !deadlines.resync_due(),
            "the next one is a full interval out"
        );

        clock.advance(interval - Duration::from_millis(1));
        assert!(!deadlines.resync_due());
        clock.advance(Duration::from_millis(1));
        assert!(deadlines.resync_due(), "and it fires on that interval too");
    }

    /// Loss evidence immediately after a resync cannot re-trigger one inside
    /// [`MIN_RESYNC_GAP`] — the floor that stops a burst of corrupt frames
    /// turning into a burst of resyncs.
    #[test]
    fn loss_evidence_cannot_pull_a_resync_inside_the_minimum_gap() {
        let clock = ManualClock::started();
        let mut deadlines = Deadlines::new(
            Duration::from_secs(30),
            Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        );
        deadlines.resynced();

        // Three pieces of evidence in the same breath, which is the case the
        // floor exists for.
        deadlines.note_loss(LossEvidence::UndecodableFrame);
        deadlines.note_loss(LossEvidence::CorruptOp);
        deadlines.note_loss(LossEvidence::Retransmit);
        assert!(
            !deadlines.resync_due(),
            "the floor holds the resync off at t=0"
        );

        clock.advance(MIN_RESYNC_GAP - Duration::from_millis(1));
        assert!(!deadlines.resync_due(), "still inside the gap");
        clock.advance(Duration::from_millis(1));
        assert!(
            deadlines.resync_due(),
            "the pulled-forward resync fires exactly at the floor"
        );
    }

    /// Past the floor, evidence pulls the resync to now rather than to the
    /// floor — waiting a further `MIN_RESYNC_GAP` after evidence of loss would
    /// be the opposite of the point.
    #[test]
    fn loss_evidence_past_the_floor_pulls_the_resync_to_now() {
        let clock = ManualClock::started();
        let mut deadlines = Deadlines::new(
            Duration::from_secs(30),
            Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        );
        deadlines.resynced();
        clock.advance(MIN_RESYNC_GAP * 4);

        assert!(!deadlines.resync_due(), "the interval has not elapsed");
        deadlines.note_loss(LossEvidence::CorruptOp);
        assert!(deadlines.resync_due(), "evidence makes it due at once");
    }

    /// Evidence only ever pulls a deadline **forward**. A resync already due
    /// stays due; it is not pushed out to the floor.
    #[test]
    fn loss_evidence_never_postpones_a_resync() {
        let clock = ManualClock::started();
        let interval = Duration::from_secs(30);
        let mut deadlines = Deadlines::new(interval, Arc::clone(&clock) as Arc<dyn MonotonicClock>);
        clock.advance(interval);
        assert!(deadlines.resync_due());

        deadlines.note_loss(LossEvidence::UndecodableFrame);
        assert!(deadlines.resync_due(), "an overdue resync stays overdue");
    }

    /// A batch is retransmitted **at** its deadline and not before, and the
    /// retry that follows is spaced by the batch's own policy.
    #[test]
    fn a_batch_is_retransmitted_at_its_deadline_and_respaced_by_its_policy() {
        let clock = ManualClock::started();
        let rng = MidJitterRng;
        let mut deadlines = Deadlines::new(
            Duration::from_secs(300),
            Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        );
        let start = clock.now();
        let mut inflight = HashMap::new();
        inflight.insert(
            1,
            InflightBatch {
                op_ids: vec![[7u8; 16]],
                frame: vec![0xde, 0xad],
                backoff: Backoff::canonical(),
                due_at: start + Duration::from_millis(100),
            },
        );

        let mut out = Vec::new();
        clock.advance(Duration::from_millis(99));
        assert!(retransmit_due(
            &mut inflight,
            &mut out,
            &mut deadlines,
            &rng
        ));
        assert!(out.is_empty(), "not due yet, nothing sent");

        clock.advance(Duration::from_millis(1));
        assert!(retransmit_due(
            &mut inflight,
            &mut out,
            &mut deadlines,
            &rng
        ));
        assert_eq!(out.len(), 1, "the frame goes out at the deadline");
        assert_eq!(out[0], vec![0xde, 0xad], "byte for byte, not re-read");
        assert!(
            deadlines.resync_due(),
            "an unacked batch is loss evidence, so a resync is pulled forward"
        );

        let batch = &inflight[&1];
        assert_eq!(batch.backoff.attempt(), 1, "one attempt consumed");
        assert_eq!(
            batch.due_at,
            clock.now() + Duration::from_millis(100),
            "respaced by the policy's first delay"
        );
    }

    /// Exhausting a batch's policy is reported to the caller, which tears the
    /// session down rather than retrying forever on a link that is not
    /// delivering.
    #[test]
    fn an_exhausted_batch_policy_ends_the_session() {
        let clock = ManualClock::started();
        let rng = MidJitterRng;
        let mut deadlines = Deadlines::new(
            Duration::from_secs(300),
            Arc::clone(&clock) as Arc<dyn MonotonicClock>,
        );
        let mut backoff = Backoff::canonical();
        for _ in 0..5 {
            backoff.record_attempt();
        }
        let mut inflight = HashMap::new();
        inflight.insert(
            1,
            InflightBatch {
                op_ids: vec![[7u8; 16]],
                frame: vec![0xde, 0xad],
                backoff,
                due_at: clock.now(),
            },
        );

        let mut out = Vec::new();
        assert!(
            !retransmit_due(&mut inflight, &mut out, &mut deadlines, &rng),
            "five unacked retries is the end of the session"
        );
    }

    /// The pump sleeps until the soonest of the two deadlines it owns.
    #[test]
    fn the_pump_wakes_for_whichever_deadline_comes_first() {
        let clock = ManualClock::started();
        let interval = Duration::from_secs(30);
        let deadlines = Deadlines::new(interval, Arc::clone(&clock) as Arc<dyn MonotonicClock>);
        let start = clock.now();

        let empty: HashMap<u64, InflightBatch> = HashMap::new();
        assert_eq!(
            next_deadline(&empty, &deadlines),
            start + interval,
            "with nothing in flight, the resync is the only deadline"
        );

        let mut inflight = HashMap::new();
        inflight.insert(
            1,
            InflightBatch {
                op_ids: Vec::new(),
                frame: Vec::new(),
                backoff: Backoff::canonical(),
                due_at: start + Duration::from_millis(100),
            },
        );
        inflight.insert(
            2,
            InflightBatch {
                op_ids: Vec::new(),
                frame: Vec::new(),
                backoff: Backoff::canonical(),
                due_at: start + Duration::from_millis(50),
            },
        );
        assert_eq!(
            next_deadline(&inflight, &deadlines),
            start + Duration::from_millis(50),
            "the soonest retransmit wins over the resync and its own sibling"
        );
    }
}

#[cfg(test)]
mod tests {
    //! Driver tests against an in-process fake relay over a channel transport
    //! (no sockets). Each `on_connect`/`inbound`/`duplicate`/`reconnect`/
    //! `submit_while_live` test drives the real driver task; the fake server
    //! scripts the protocol side.

    use super::{
        drain_relay_revocations, mark_renewals_current, note_marked_at_connect, run, BoxTransport,
        ClosePayload, ConnectFuture, SessionEnd, SyncConfig, SyncShared, TokenSource,
        TransportFactory,
    };
    use crate::config::{Clock, Rng};
    use crate::{Command, Core, CoreConfig, DomainEvent, Query, QueryResult, SystemRng, Unlock};
    use async_trait::async_trait;
    use std::collections::{HashSet, VecDeque};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::time::Duration;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::TaskDraft;
    use sunrise_error::ErrorCode;
    use sunrise_sync::{RevokeOutcome, SyncState, Transport, TransportError};
    use sunrise_wire_protocol::{
        decode_frame, encode_frame, AckPayload, Capability, CapabilityBits, CaughtUpPayload,
        ErrorPayload, FrameFlags, HelloAck, MsgKind, OpBatchPayload, SubscribePayload,
    };
    use tokio::sync::{broadcast, mpsc};
    use tokio::time::timeout;

    const ROOT: [u8; 32] = [0x5a; 32];
    const T0: u64 = 1_700_000_000_000;
    /// The event whose placement this change is about.
    const MARKED_AT_CONNECT: &str = "sync.credential.marked_at_connect";

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

    /// Records what it was asked to revoke, and answers as a relay would.
    struct Recorder {
        seen: Arc<std::sync::Mutex<Vec<[u8; 16]>>>,
        answer: Result<RevokeOutcome, ()>,
    }

    #[async_trait]
    impl Transport for Recorder {
        async fn send_frame(&mut self, _frame: Vec<u8>) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(None)
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn revoke_device(
            &mut self,
            device_id: [u8; 16],
        ) -> Result<RevokeOutcome, TransportError> {
            self.seen.lock().unwrap().push(device_id);
            self.answer
                .map_err(|()| TransportError::Unavailable("relay down".into()))
        }
    }

    /// Revoking a device queues the relay's half, and a session hands it to
    /// the transport.
    ///
    /// This is the write bound. A revocation that never reaches the relay
    /// leaves it accepting the revoked device's uploads and streaming it
    /// everyone else's ops, whatever the vault believes — and the relay cannot
    /// learn it from the op stream, because `device_revoke` is sealed under a
    /// key the relay does not hold.
    #[tokio::test]
    async fn a_revocation_is_handed_to_the_transport_and_then_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        let target = [0x9c; 16];

        // A revocation of a device this vault knows about. The command refuses
        // an unknown device, so the row is planted the way pairing would.
        core.queue_relay_revocation_for_test(target).unwrap();
        assert_eq!(core.pending_relay_revocations().unwrap(), vec![target]);

        // A relay that cannot be reached keeps the intent: the device this is
        // about is typically lost, and that is exactly when the network is also
        // the thing that just went away. Giving up would leave it accepted
        // forever, with nothing left that remembers otherwise.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut unreachable = Recorder {
            seen: seen.clone(),
            answer: Err(()),
        };
        drain_relay_revocations(&core, &mut unreachable).await;
        assert_eq!(*seen.lock().unwrap(), vec![target]);
        assert_eq!(
            core.pending_relay_revocations().unwrap(),
            vec![target],
            "a refused revocation must stay queued"
        );
        assert_eq!(
            core.relay_revocation_attempts_for_test(target).unwrap(),
            1,
            "and the refusal must be counted, so a relay that keeps saying no is visible"
        );

        // And an accepting one clears it, so it is not re-sent every session.
        let mut accepting = Recorder {
            seen: seen.clone(),
            answer: Ok(RevokeOutcome::Revoked),
        };
        drain_relay_revocations(&core, &mut accepting).await;
        assert!(
            core.pending_relay_revocations().unwrap().is_empty(),
            "an accepted revocation must not be re-sent forever"
        );
    }

    /// A relay that has never heard of the device clears the intent, because
    /// retrying cannot make a row appear — and it is *not* reported as a
    /// revocation.
    ///
    /// This is the case PR #86 got wrong: it read the `404` as success and
    /// logged that the relay had been told to stop accepting the device, about
    /// a request naming an id the relay had never held. The distinction is a
    /// separate `RevokeOutcome` arm rather than a comment, so the two cannot be
    /// collapsed again by accident.
    #[tokio::test]
    async fn a_relay_that_never_knew_the_device_ends_the_intent_without_claiming_a_revocation() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        let target = [0x3e; 16];
        core.queue_relay_revocation_for_test(target).unwrap();

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut unknown = Recorder {
            seen: seen.clone(),
            answer: Ok(RevokeOutcome::Unknown),
        };
        drain_relay_revocations(&core, &mut unknown).await;

        assert_eq!(*seen.lock().unwrap(), vec![target]);
        assert!(
            core.pending_relay_revocations().unwrap().is_empty(),
            "a 404 is terminal: no retry can make a row appear"
        );
    }

    /// A transport with no account API is not counted as an attempt.
    ///
    /// The loopback transports every convergence test runs on cannot revoke
    /// anything. Counting them would inflate `attempts` on a row that had never
    /// actually been offered to a relay, which is the one number an operator
    /// would read to decide whether a relay is refusing them.
    #[tokio::test]
    async fn an_unsupported_transport_does_not_burn_an_attempt() {
        struct NoAccountApi;
        #[async_trait]
        impl Transport for NoAccountApi {
            async fn send_frame(&mut self, _f: Vec<u8>) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
                Ok(None)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        let target = [0x7d; 16];
        core.queue_relay_revocation_for_test(target).unwrap();

        drain_relay_revocations(&core, &mut NoAccountApi).await;

        let attempts = core.relay_revocation_attempts_for_test(target).unwrap();
        assert_eq!(
            attempts, 0,
            "a transport with no account API is not a refusal"
        );
        assert_eq!(
            core.pending_relay_revocations().unwrap(),
            vec![target],
            "and the intent survives for a transport that can carry it"
        );
    }

    /// An intent whose register row a re-fold unwound is not sent, and is
    /// sent after all once the register revokes the device again (#257).
    ///
    /// The unwind is the fold rebuilding `device_revocations` without the
    /// row; it never touches `relay_revocation_intents`. Sending the intent
    /// anyway would have the relay refuse a device every replica shows as
    /// current. Deleting it instead would lose it for the later fold that
    /// restores the revocation, and the relay would then be behind the
    /// register — the #160 direction, which is the unsafe one.
    #[tokio::test]
    async fn an_intent_the_register_no_longer_agrees_with_is_held_not_sent() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        let target = [0x5a; 16];
        core.queue_relay_revocation_for_test(target).unwrap();
        core.unwind_register_row_for_test(target).unwrap();

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut accepting = Recorder {
            seen: seen.clone(),
            answer: Ok(RevokeOutcome::Revoked),
        };
        drain_relay_revocations(&core, &mut accepting).await;
        assert!(
            seen.lock().unwrap().is_empty(),
            "a device the register calls current must not be cut at the relay"
        );
        assert!(
            !core.relay_revocation_pending(&target).unwrap(),
            "and no surface may call it queued, since it will not be sent"
        );
        assert!(
            core.relay_intent_row_exists_for_test(target).unwrap(),
            "the intent is held, not dropped"
        );

        // The register revokes it again, as a later fold can.
        core.queue_relay_revocation_for_test(target).unwrap();
        assert!(core.relay_revocation_pending(&target).unwrap());
        drain_relay_revocations(&core, &mut accepting).await;
        assert_eq!(
            *seen.lock().unwrap(),
            vec![target],
            "once the register agrees again the relay is owed the revocation"
        );
        assert!(!core.relay_intent_row_exists_for_test(target).unwrap());
    }

    /// A device unwound while the drain is awaiting an earlier send is not
    /// sent (#257).
    ///
    /// The drain reads the owed set once, then awaits the network per device.
    /// This transport unwinds the second device's register row while the
    /// first send is in flight, as a local command on another task could.
    #[tokio::test]
    async fn a_device_unwound_mid_drain_is_not_sent() {
        struct UnwindsOnFirstSend {
            core: Arc<Core>,
            unwind: [u8; 16],
            seen: Vec<[u8; 16]>,
        }
        #[async_trait]
        impl Transport for UnwindsOnFirstSend {
            async fn send_frame(&mut self, _f: Vec<u8>) -> Result<(), TransportError> {
                Ok(())
            }
            async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
                Ok(None)
            }
            async fn close(&mut self) -> Result<(), TransportError> {
                Ok(())
            }
            async fn revoke_device(
                &mut self,
                device_id: [u8; 16],
            ) -> Result<RevokeOutcome, TransportError> {
                if self.seen.is_empty() {
                    self.core.unwind_register_row_for_test(self.unwind).unwrap();
                }
                self.seen.push(device_id);
                Ok(RevokeOutcome::Revoked)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        // Same `created_at_ms`, so the id breaks the tie: `first` is sent first.
        let first = [0x11; 16];
        let second = [0x22; 16];
        core.queue_relay_revocation_for_test(first).unwrap();
        core.queue_relay_revocation_for_test(second).unwrap();
        assert_eq!(
            core.pending_relay_revocations().unwrap(),
            vec![first, second]
        );

        let mut transport = UnwindsOnFirstSend {
            core: core.clone(),
            unwind: second,
            seen: Vec::new(),
        };
        drain_relay_revocations(&core, &mut transport).await;
        assert_eq!(
            transport.seen,
            vec![first],
            "a device the register stopped calling revoked mid-drain must not be cut"
        );
        assert!(
            core.relay_intent_row_exists_for_test(second).unwrap(),
            "and its intent is held, not dropped"
        );
    }

    /// A storage failure is an error, not "nothing queued".
    ///
    /// Reporting `false` here would tell someone the relay had been told about
    /// a device when this client could not even read whether it had.
    #[tokio::test]
    async fn a_storage_failure_is_not_read_as_nothing_pending() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        let target = [0x6b; 16];
        core.queue_relay_revocation_for_test(target).unwrap();
        core.break_register_for_test().unwrap();

        assert!(
            core.relay_revocation_pending(&target).is_err(),
            "an unreadable register must surface as an error"
        );
        assert!(core.pending_relay_revocations().is_err());
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
        /// Attempts whose **factory was called**, not attempts that dialled.
        /// The closure counts and pops its script on entry, so a future `run`
        /// drops un-polled — shutdown winning the connect select — has still
        /// consumed a script and moved this. Every assertion on it is relative
        /// or `>=` for that reason.
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
            let batch_tx = batch_tx.clone();
            let subs = factory_subs.clone();
            let refreshes = factory_refreshes.clone();
            // Counted in the closure's own body rather than in the future it
            // returns, because that is where the driver's own ordering is:
            // `run` marks the renewal handle the instant this closure returns,
            // so `connect_count` is an observable a test can order a
            // credential write against only if it moves first. Holding the
            // server lock across a `set` then keeps the next attempt out.
            let script = {
                let mut s = factory_server.lock();
                s.connect_count += 1;
                s.scripts.pop_front().unwrap_or_default()
            };
            let fut = async move {
                let (client_end, server_end) = duplex();
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
        let bundle = sunrise_pairing::encode_pairing_payload(
            &core
                .pair_device_in_process("joiner".into(), "test".into(), [0x71; 32], [0x72; 32])
                .unwrap(),
        )
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

    /// One captured event: the `ev` name it carried, and its `to_v` if it had
    /// one.
    ///
    /// `sunrise-core` captures no `tracing` output anywhere else, and
    /// `sync.credential.marked_at_connect` is this change's whole
    /// user-visible surface, so both branches of its trigger are asserted
    /// against a real subscriber rather than argued. Following
    /// `crates/sunrise-log/tests/redaction.rs`, the code under test runs
    /// *under* a subscriber instead of a formatted line being matched.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct LoggedEvent {
        ev: Option<String>,
        to_v: Option<u64>,
        /// `sync.backoff`'s two fields, which
        /// [`an_unanswered_reconnect_climbs_the_backoff_schedule`] reads.
        attempt: Option<u64>,
        delay_ms: Option<u64>,
        /// `sync.session.closed` and `sync.session.error`'s two fields, which
        /// the session-end tests below read.
        err_code: Option<String>,
        result: Option<String>,
        /// `sync.session.error`'s `err_kind` and `retryable`, which must agree
        /// with the catalogue entry for its `err_code`.
        err_kind: Option<String>,
        retryable: Option<bool>,
    }

    impl tracing::field::Visit for LoggedEvent {
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            if field.name() == "retryable" {
                self.retryable = Some(value);
            }
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            match field.name() {
                "to_v" => self.to_v = Some(value),
                "attempt" => self.attempt = Some(value),
                "delay_ms" => self.delay_ms = Some(value),
                _ => {}
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "ev" => self.ev = Some(value.to_string()),
                "err_code" => self.err_code = Some(value.to_string()),
                "result" => self.result = Some(value.to_string()),
                "err_kind" => self.err_kind = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
    }

    #[derive(Clone, Default)]
    struct EventLog(Arc<parking_lot::Mutex<Vec<LoggedEvent>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventLog {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut logged = LoggedEvent::default();
            event.record(&mut logged);
            self.0.lock().push(logged);
        }
    }

    /// Every event `f` emitted, in order.
    fn captured_events(f: impl FnOnce()) -> Vec<LoggedEvent> {
        use tracing_subscriber::layer::SubscriberExt as _;
        let log = EventLog::default();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(log.clone()), f);
        let events = log.0.lock().clone();
        events
    }

    /// A renewal that landed while the driver was disconnected is consumed by
    /// the connect that carried it, and that consumption is reported once.
    ///
    /// The driver's one handle only ever falls behind between sessions: while
    /// a session is up the pump consumes every write. The connect that ends
    /// that gap reads the credential itself, so the relay has already been
    /// given the new bearer in the `Authorization` header by the time the pump
    /// starts, and a lagging handle would spend a round trip re-presenting it.
    ///
    /// This drives the two seams the driver composes — the mark in `run` and
    /// the report in `session` — in the order and with the state it composes
    /// them in. Where each is *called* is pinned by the driver-level tests
    /// below: the mark after the factory, by
    /// [`a_renewal_landing_during_the_dial_still_reaches_the_live_session`],
    /// and the report after the handshake, by
    /// [`the_report_waits_for_the_handshake_not_for_the_connect`].
    #[tokio::test]
    async fn a_connect_that_consumed_a_renewal_logs_it_once() {
        let credential = TokenSource::new(Some("first-token".into()));
        let mut renewals = credential.watch();
        // The disconnected window: two renewals, no session to observe them.
        credential.set(Some("second-token".into()));
        credential.set(Some("third-token".into()));

        let mut marked = None;
        let events = captured_events(|| {
            marked = mark_renewals_current(&mut renewals, marked);
            note_marked_at_connect(marked.take());
        });
        assert_eq!(
            events,
            vec![LoggedEvent {
                ev: Some("sync.credential.marked_at_connect".into()),
                to_v: Some(2),
                ..LoggedEvent::default()
            }],
            "the connect carried both writes, and the event says which version it reached"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), renewals.changed())
                .await
                .is_err(),
            "the pump must not re-announce a bearer this connect already presented"
        );
    }

    /// The event's silent branch, in the shape that made the version
    /// comparison this replaced a false positive.
    ///
    /// A renewal announced **in band** by a live session has already been
    /// consumed by the pump, so the reconnect that follows brings nothing
    /// forward and must say nothing. The guard this replaced compared two
    /// *source* versions taken at two connects, and the source's counter does
    /// not go back down when the pump consumes a write: it fired here, saying
    /// "connect carried a renewal from the offline window" about a renewal the
    /// previous session had already announced. With a 45-minute renewal
    /// cadence against long-lived sessions that is the ordinary path, not an
    /// edge.
    #[tokio::test]
    async fn a_renewal_announced_in_band_is_not_claimed_by_the_next_connect() {
        let credential = TokenSource::new(Some("first-token".into()));
        let mut renewals = credential.watch();
        let mut marked = mark_renewals_current(&mut renewals, None);
        note_marked_at_connect(marked.take());

        // The session is live, and its pump consumes the renewal in band.
        credential.set(Some("second-token".into()));
        assert_eq!(renewals.changed().await, 1, "the pump sees the write");

        // The session drops; the driver reconnects with nothing outstanding.
        let events = captured_events(|| {
            marked = mark_renewals_current(&mut renewals, marked);
            note_marked_at_connect(marked.take());
        });
        assert!(
            events.is_empty(),
            "this connect brought nothing forward and must claim nothing, saw {events:?}"
        );
    }

    /// A renewal consumed by an attempt that never connected is reported
    /// against the attempt that did.
    ///
    /// The mark runs before the dial, so a renewal landing during backoff is
    /// consumed by an attempt that may then fail. That is sound for the handle
    /// — the next attempt re-reads the credential and carries at least as much
    /// — and it is the report that has to follow the bearer. Emitted before the
    /// relay has answered, the event lands between `sync.session.closed` and
    /// `sync.backoff` for an attempt whose header never reached it, and the
    /// attempt that did get through finds the handle already current and stays
    /// silent.
    #[tokio::test]
    async fn a_renewal_consumed_by_a_failed_attempt_is_reported_by_the_next_connect() {
        let credential = TokenSource::new(Some("first-token".into()));
        let mut renewals = credential.watch();
        // A renewal lands while the driver sits in backoff.
        credential.set(Some("second-token".into()));

        let mut marked = None;
        let during_the_failure = captured_events(|| {
            marked = mark_renewals_current(&mut renewals, marked);
        });
        assert!(
            during_the_failure.is_empty(),
            "an attempt that never dialled presented no bearer, saw {during_the_failure:?}"
        );

        // The next attempt re-reads the credential, so it carries the same
        // renewal — and finds the handle already current, with nothing of its
        // own to bring forward.
        let on_the_connect = captured_events(|| {
            marked = mark_renewals_current(&mut renewals, marked);
            note_marked_at_connect(marked.take());
        });
        assert_eq!(
            on_the_connect,
            vec![LoggedEvent {
                ev: Some("sync.credential.marked_at_connect".into()),
                to_v: Some(1),
                ..LoggedEvent::default()
            }],
            "the connect that actually presented the bearer is the one that reports it"
        );
    }

    /// Driver-level: the report follows the **handshake**, not the connect.
    ///
    /// `Ok` from a [`ConnectFuture`] is a transport object. No factory in this
    /// workspace can resolve one to `Err` — the shipped two build an
    /// `SseTransport`, which dials nothing — so a report made in `run`'s `Ok`
    /// arm would be made by every attempt, including the ones the relay never
    /// answers. Here attempt 1 consumes the renewal and then loses its
    /// handshake against a peer that is already gone; attempt 2 is the one the
    /// relay answers, and it is the one that speaks.
    ///
    /// Four mutations this fails that nothing else catches: moving
    /// `note_marked_at_connect` back into `run`'s `Ok` arm (the event lands
    /// *before* attempt 1's `sync.session.closed`), deleting the call (no
    /// event at all), dropping the carry-forward — passing `None` to
    /// `mark_renewals_current`, or `renewals.mark_current()` without the
    /// `.or(unreported)` — after which attempt 2 finds the handle current and
    /// nothing is ever reported, and **dropping the `.take()`**, after which
    /// the mark stays outstanding and session 3 reports the same version a
    /// second time.
    ///
    /// The third session is what makes that last one reachable. Session 2 is
    /// scripted to drop after its subscribe, because `run_fake_server` runs
    /// until a `Close` or a dead peer and would otherwise hold the driver in
    /// session 2 until shutdown, leaving nothing to re-report into.
    ///
    /// The driver is built here rather than through `Core::start_sync` so a
    /// subscriber can be attached to `run`'s own future. `captured_events`
    /// cannot reach it: `tracing::subscriber::with_default` is thread-local
    /// and `start_sync` spawns.
    // Three scripted sessions and the connect-count assertion put this one
    // six lines past the limit; `session` above carries the same allow.
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_report_waits_for_the_handshake_not_for_the_connect() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use tracing::instrument::WithSubscriber as _;
        use tracing_subscriber::layer::SubscriberExt as _;

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

        // One script, and attempt 1 never reaches `inner` — it returns its own
        // dead-peer transport — so the script is popped by attempt 2. Attempt 3
        // gets `Script::default()` and stays up until shutdown.
        let (inner, server, _batch_rx, subs, _refreshes) = harness_full(
            vec![Script {
                close_after_subscribe: true,
                ..Script::default()
            }],
            true,
        );
        let attempts = Arc::new(AtomicU64::new(0));
        let renewing = credential.clone();
        let factory: TransportFactory = Arc::new(move || {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // The offline window, placed by ordering rather than by a
                // clock: `run` has taken its watch handle (it does that before
                // the loop) and this attempt has not yet fixed its bearer, so
                // the mark that follows this closure consumes this write.
                renewing.set(Some("renewed-token".into()));
            }
            // Read per attempt, in the closure body, as `TransportFactory`
            // requires.
            let _bearer = renewing.get();
            if n == 0 {
                // A transport whose peer is already gone. The connect future
                // still resolves `Ok` — that is the whole point — and the
                // `Hello` is what fails.
                let (client, server) = duplex();
                drop(server);
                return Box::pin(async move { Ok(Box::new(client) as BoxTransport) })
                    as ConnectFuture;
            }
            inner()
        });

        let log = EventLog::default();
        let (status_tx, _status_rx) = broadcast::channel(64);
        let shared = SyncShared::new(status_tx, 0);
        shared.mark_active();
        let rng: Arc<dyn Rng> = Arc::new(SystemRng);
        let driver = tokio::spawn(
            run(Arc::downgrade(&core), Arc::clone(&shared), factory, rng)
                .with_subscriber(tracing_subscriber::registry().with(log.clone())),
        );

        // Wait for the report, and then for session 3 to be past the point at
        // which it could repeat it. `session` reports immediately after the
        // handshake and subscribes immediately after that, so the second
        // Subscribe frame the fake relay sees is session 3's, and by then
        // session 3 has already had its turn to speak.
        timeout(Duration::from_secs(10), async {
            loop {
                let seen = log
                    .0
                    .lock()
                    .iter()
                    .any(|e| e.ev.as_deref() == Some(MARKED_AT_CONNECT));
                if seen && subs.load(Ordering::SeqCst) >= 2 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no attempt reported the renewal it consumed, or no third session opened");

        shared.request_shutdown();
        timeout(Duration::from_secs(10), driver)
            .await
            .expect("the driver never stopped")
            .unwrap();

        // `subs >= 2` above is a proxy for "session 3 opened", and it is sound
        // only via three properties nothing here states. Say it in the
        // harness's own counter instead. **Two, not three**: `connect_count`
        // is bumped inside `harness_full`'s closure, and attempt 1 returns its
        // own dead-peer duplex without ever calling `inner()`, so the harness
        // sees attempts 2 and 3 and nothing else.
        assert_eq!(
            server.lock().connect_count,
            2,
            "sessions 2 and 3 must both have been built through the harness, or \
             `reports.len() == 1` below proves nothing about repetition"
        );

        let events = log.0.lock().clone();
        let reports: Vec<&LoggedEvent> = events
            .iter()
            .filter(|e| e.ev.as_deref() == Some(MARKED_AT_CONNECT))
            .collect();
        assert_eq!(
            reports.len(),
            1,
            "the carried mark is reported exactly once — session 3 handshook too \
             and must not repeat it, saw {events:?}"
        );
        assert_eq!(
            reports[0].to_v,
            Some(1),
            "and it names the version the failed attempt consumed"
        );
        let names: Vec<&str> = events.iter().filter_map(|e| e.ev.as_deref()).collect();
        let failed = names
            .iter()
            .position(|n| *n == "sync.session.closed")
            .expect("attempt 1's session must have closed");
        let reported = names
            .iter()
            .position(|n| *n == MARKED_AT_CONNECT)
            .expect("the report must be in the log");
        assert!(
            reported > failed,
            "the report belongs to the attempt the relay answered, not the one it did not: {names:?}"
        );
        core.shutdown().await;
    }

    /// Driver-level: an attempt the relay never answers climbs the reconnect
    /// schedule, and one it does answer starts the next reconnect from its
    /// first step (#283).
    ///
    /// `Backoff` and `next_backoff_delay` are tested in isolation elsewhere,
    /// and both were correct while `run` retried every ~100 ms forever: the
    /// defect was in the composition, where a `reset` in the connect-`Ok` arm
    /// — taken on every attempt, since no factory here resolves `Err` — zeroed
    /// the counter before it could advance. So this drives `run` itself and
    /// reads the `sync.backoff` events an operator would.
    ///
    /// Attempts 1-3 and 5-10 get a transport whose peer is already gone, so
    /// the connect resolves `Ok` and the `Hello` fails; attempt 4 handshakes
    /// with the fake relay and is dropped after its subscribe. The expected
    /// `attempt` sequence is therefore `1, 2, 3` (climbing), `1` (reset by
    /// attempt 4's handshake), `2, 3, 4, 5` (climbing), `0` (exhausted: the
    /// flat 30 s), `1` (the cycle again). Resetting on the connect reads
    /// `1, 1, 1, ...`; never resetting on the handshake reads `1, 2, 3, 4, ...`.
    ///
    /// Paused time, so the thirty-second exhaustion arm costs nothing. The
    /// core is opened first; nothing past that point does blocking I/O.
    #[tokio::test(start_paused = true)]
    async fn an_unanswered_reconnect_climbs_the_backoff_schedule() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use tracing::instrument::WithSubscriber as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        const ANSWERED_ATTEMPT: u64 = 3; // zero-based: the fourth connect
        const WANT: [u64; 10] = [1, 2, 3, 1, 2, 3, 4, 5, 0, 1];

        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;

        let (inner, server, _batch_rx, subs, _refreshes) = harness_full(
            vec![Script {
                close_after_subscribe: true,
                ..Script::default()
            }],
            true,
        );
        let attempts = Arc::new(AtomicU64::new(0));
        let factory: TransportFactory = Arc::new(move || {
            if attempts.fetch_add(1, Ordering::SeqCst) == ANSWERED_ATTEMPT {
                return inner();
            }
            // A relay that is unreachable, as the shipped factory presents
            // one: the connect still resolves `Ok`, and the `Hello` fails.
            let (client, server) = duplex();
            drop(server);
            Box::pin(async move { Ok(Box::new(client) as BoxTransport) }) as ConnectFuture
        });

        let log = EventLog::default();
        let (status_tx, _status_rx) = broadcast::channel(64);
        let shared = SyncShared::new(status_tx, 0);
        shared.mark_active();
        let rng: Arc<dyn Rng> = Arc::new(SystemRng);
        let driver = tokio::spawn(
            run(Arc::downgrade(&core), Arc::clone(&shared), factory, rng)
                .with_subscriber(tracing_subscriber::registry().with(log.clone())),
        );

        let backoffs = || -> Vec<LoggedEvent> {
            log.0
                .lock()
                .iter()
                .filter(|e| e.ev.as_deref() == Some("sync.backoff"))
                .cloned()
                .collect()
        };
        // Virtual time: the schedule to the tenth reconnect is ~35 s.
        timeout(Duration::from_secs(600), async {
            while backoffs().len() < WANT.len() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the driver stopped reconnecting");

        shared.request_shutdown();
        timeout(Duration::from_secs(60), driver)
            .await
            .expect("the driver never stopped")
            .unwrap();

        assert_eq!(
            server.lock().connect_count,
            1,
            "exactly one attempt must have reached the fake relay"
        );
        assert!(
            subs.load(Ordering::SeqCst) >= 1,
            "the answered attempt must have got past its handshake"
        );

        let seen = backoffs();
        let got: Vec<u64> = seen[..WANT.len()]
            .iter()
            .map(|e| e.attempt.expect("sync.backoff carries attempt"))
            .collect();
        assert_eq!(
            got, WANT,
            "sync.backoff attempts must climb across unanswered reconnects and \
             restart after a handshake"
        );
        for (e, attempt) in seen.iter().zip(WANT) {
            let delay = e.delay_ms.expect("sync.backoff carries delay_ms");
            if attempt == 0 {
                assert_eq!(
                    delay, 30_000,
                    "the exhausted arm is a flat, un-jittered 30 s"
                );
            } else {
                let base = 100u64 << (attempt - 1);
                assert!(
                    (base * 8 / 10..=base * 12 / 10).contains(&delay),
                    "attempt {attempt} waited {delay} ms, outside ±20% of {base} ms"
                );
            }
        }
        core.shutdown().await;
    }

    /// End to end: a renewal that lands while the driver is disconnected
    /// produces **no** `0x12 RefreshToken` in the next session.
    ///
    /// Driven through `run` — a real connect, a scripted drop, a real
    /// `Backoff` sleep, a reconnect, a handshake, a subscribe and the pump —
    /// rather than through the seam. The offline window is genuine:
    /// `Disconnected` is broadcast immediately before `backoff_sleep`, so the
    /// write below lands with no session up.
    ///
    /// # What this proves, and what it does not
    ///
    /// The absence of a `0x12`, and nothing about *why*. `harness_full` is an
    /// in-process duplex that reads no `TokenSource` and presents no bearer at
    /// all, so this cannot fail for a driver that swallows an offline renewal
    /// a non-conforming factory never carried — the exact failure
    /// [`TransportFactory`]'s precondition exists to prevent. The causal half
    /// — that the reconnect presented the renewed bearer, which is *why* no
    /// `0x12` follows — is
    /// [`the_reconnect_presents_the_renewed_bearer_it_then_does_not_re_announce`]
    /// below.
    ///
    /// It is what makes the placement falsifiable. Deleting the
    /// `mark_renewals_current` call in `run` fails it 30 times in 30 with
    /// `saw ["renewed-token"]`, which is precisely the wasted round trip
    /// #244 reported.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renewal_from_the_offline_window_produces_no_refresh_frame_on_reconnect() {
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
        let (factory, server, _batch_rx, _subs, refreshes) = harness_full(
            vec![
                Script {
                    close_after_subscribe: true,
                    ..Default::default()
                },
                Script::default(),
            ],
            true,
        );
        core.start_sync(factory).unwrap();

        // The scripted close ends session 1; `Disconnected` is set immediately
        // before `backoff_sleep`, so this wakes at the top of the offline
        // window.
        timeout(Duration::from_secs(10), async {
            loop {
                match status_rx.recv().await {
                    Ok(s) if s.state == SyncState::Disconnected => return,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        })
        .await
        .expect("session 1 never closed");
        // Ordered against the harness rather than against `Backoff::canonical`'s
        // jittered 80-120 ms first delay: attempt 2's factory closure takes
        // this same lock before it returns, and `run` marks the renewal handle
        // the instant it does return, so holding it here puts the write ahead
        // of the mark that has to consume it. Left to the jitter, a scheduling
        // stall on a loaded runner pushes the write past the reconnect, the
        // pump sends a `0x12`, and the assertion below fails for a reason this
        // test is not about.
        {
            let attempts = server.lock();
            assert_eq!(
                attempts.connect_count, 1,
                "attempt 2 dialled before the write; the offline window was missed"
            );
            credential.set(Some("renewed-token".into()));
            drop(attempts);
        }

        timeout(Duration::from_secs(10), async {
            while server.lock().connect_count < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the driver never reconnected");
        tokio::time::sleep(Duration::from_millis(400)).await;
        let seen = refreshes.lock().clone();
        assert!(
            seen.is_empty(),
            "a renewal the reconnect carried must not be re-announced, saw {seen:?}"
        );

        // And the handle was not marked *past* the source: a renewal arriving
        // now, with the session up, still reaches the relay in band. Without
        // this the test would also pass for a driver that swallowed every
        // renewal forever.
        credential.set(Some("later-token".into()));
        timeout(Duration::from_secs(10), async {
            while refreshes.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a renewal during the live session must still be announced");
        assert_eq!(
            refreshes.lock().as_slice(),
            ["later-token".to_string()],
            "and it is the later bearer, not a replay of the one the connect carried"
        );
        core.shutdown().await;
    }

    /// The causal half of the test above: the reconnect **presented** the
    /// renewed bearer, which is why no `0x12` follows it.
    ///
    /// The proposition #244 reports is "no refresh frame *because* the connect
    /// already carried the token", and `harness_full` alone cannot reach the
    /// second clause — it presents no bearer, so the absence of a frame is
    /// equally consistent with a driver that swallowed a renewal nothing
    /// carried. Here the harness is wrapped in a factory in the shape
    /// [`TransportFactory`] requires: the credential is read in the closure's
    /// own body, per attempt, and what it read is recorded.
    ///
    /// Deleting the `mark_renewals_current` call in `run` leaves the bearer
    /// assertion green and fails the frame assertion; freezing the read by
    /// hoisting `reading.get()` out of the closure leaves the frame assertion
    /// green and fails the bearer assertion. The two clauses are independent,
    /// which is why both are here.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_reconnect_presents_the_renewed_bearer_it_then_does_not_re_announce() {
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
        let (inner, _server, _batch_rx, _subs, refreshes) = harness_full(
            vec![
                Script {
                    close_after_subscribe: true,
                    ..Default::default()
                },
                Script::default(),
            ],
            true,
        );
        // The bearer each attempt fixed, in order.
        let presented: Arc<parking_lot::Mutex<Vec<Option<String>>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let reading = credential.clone();
        let recorded = presented.clone();
        let factory: TransportFactory = Arc::new(move || {
            // The lock is taken in its own statement, before the read, and the
            // test below holds it while it writes the renewal — so an attempt
            // that starts during the write waits here and reads afterwards.
            // Written as `recorded.lock().push(reading.get())` this works only
            // because Rust evaluates a method call's receiver before its
            // arguments, which is not something the next reader should have to
            // know to keep the ordering intact.
            let mut recorded = recorded.lock();
            let bearer = reading.get();
            recorded.push(bearer);
            drop(recorded);
            inner()
        });
        core.start_sync(factory).unwrap();

        timeout(Duration::from_secs(10), async {
            loop {
                match status_rx.recv().await {
                    Ok(s) if s.state == SyncState::Disconnected => return,
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        })
        .await
        .expect("session 1 never closed");
        // The same ordering the test above takes, against the observable that
        // matters here: attempt 2's factory takes this lock before it reads
        // the credential, so holding it here puts the write ahead of that read.
        {
            let reads = presented.lock();
            assert_eq!(
                reads.len(),
                1,
                "attempt 2 read the credential before the write; the window was missed"
            );
            credential.set(Some("renewed-token".into()));
            drop(reads);
        }

        timeout(Duration::from_secs(10), async {
            while presented.lock().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the driver never reconnected");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            presented.lock().as_slice(),
            [
                Some("first-token".to_string()),
                Some("renewed-token".to_string())
            ],
            "the reconnect must present the renewal, not the bearer it opened with"
        );
        let seen = refreshes.lock().clone();
        assert!(
            seen.is_empty(),
            "and having presented it, must not re-announce it, saw {seen:?}"
        );
        core.shutdown().await;
    }

    /// The placement, stated as the thing that distinguishes it from the site
    /// the dead guard occupied: a renewal landing **during the dial** is still
    /// announced in band.
    ///
    /// `run` marks the handle immediately after `factory()` returns, which is
    /// when the attempt's bearer is fixed. Everything after that moment — the
    /// dial, the handshake, the subscribe, the whole session — is still ahead
    /// of the relay, so a write landing there must reach it as a `0x12` rather
    /// than wait for the next reconnect.
    ///
    /// Moving the call back inside `session`, where the dead guard stood,
    /// leaves the test above green and fails this one: the mark would then
    /// happen *after* the dial and consume this write, and no frame would ever
    /// be sent. That is the difference between the two sites, and it is the
    /// whole of what the placement decision bought.
    ///
    /// The write is issued inside the returned `ConnectFuture` rather than on
    /// a timer, because "after the factory returned, before the session
    /// exists" is an ordering rather than a duration and there is no wall
    /// clock that names it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_renewal_landing_during_the_dial_still_reaches_the_live_session() {
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
        let (inner, _server, _batch_rx, _subs, refreshes) = harness_full(vec![], true);
        // A factory in the shape `TransportFactory` requires — it reads the
        // credential in its own body — whose connect future then performs the
        // renewal, putting the write after this attempt's bearer was fixed and
        // before the session that will carry it exists.
        let dialing = credential.clone();
        let factory: TransportFactory = Arc::new(move || {
            let _bearer = dialing.get();
            let dialing = dialing.clone();
            let fut = inner();
            Box::pin(async move {
                dialing.set(Some("dialed-token".into()));
                fut.await
            }) as ConnectFuture
        });
        core.start_sync(factory).unwrap();
        let _ = collect_until_live(&mut status_rx).await;

        timeout(Duration::from_secs(10), async {
            while refreshes.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a renewal landing after the factory read must not be consumed by the mark");
        assert_eq!(
            refreshes.lock().as_slice(),
            ["dialed-token".to_string()],
            "the relay is told the bearer this connect did not carry, exactly once"
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

    // ---- session ends the relay chooses (#278) ----

    /// One scripted answer to `recv_frame`.
    type Recv = Result<Option<Vec<u8>>, TransportError>;

    /// A transport that answers `recv_frame` from a script, then waits
    /// forever, and accepts every send.
    ///
    /// The fake relay above cannot stand in here: its duplex only ever yields
    /// `Ok`, and a refused stream is a [`TransportError::Server`] out of
    /// `recv_frame`, which is the value these tests are about.
    struct ScriptedTransport(VecDeque<Recv>);

    #[async_trait]
    impl Transport for ScriptedTransport {
        async fn send_frame(&mut self, _frame: Vec<u8>) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv_frame(&mut self) -> Recv {
            match self.0.pop_front() {
                Some(r) => r,
                None => std::future::pending().await,
            }
        }
        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    // Returns a script entry, so it is shaped like one: the `Ok` is what the
    // scripted transport hands `recv_frame`, not a fallibility this has.
    #[allow(clippy::unnecessary_wraps)]
    fn hello_ack() -> Recv {
        let ack = HelloAck {
            server_app_v: "scripted".into(),
            wire_proto: 1,
            crypto_suite: 1,
            doc_schema_floor: 1,
            capabilities: 0,
            server_time_ms: T0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&ack, &mut buf).unwrap();
        Ok(Some(
            encode_frame(MsgKind::HelloAck, FrameFlags::EMPTY, &buf).unwrap(),
        ))
    }

    #[allow(clippy::unnecessary_wraps)] // a script entry; see `hello_ack`
    fn close_frame(code: ErrorCode) -> Recv {
        let payload = ClosePayload {
            code,
            reason: format!("scripted {code}"),
        }
        .encode()
        .unwrap();
        Ok(Some(
            encode_frame(MsgKind::Close, FrameFlags::EMPTY, &payload).unwrap(),
        ))
    }

    fn refusal(code: ErrorCode) -> Recv {
        Err(TransportError::Server {
            code: code.as_str(),
            message: "scripted refusal".into(),
        })
    }

    /// A factory that hands each connect the next script (an empty one once
    /// they run out), and the number of connects it has served.
    fn scripted(scripts: Vec<Vec<Recv>>) -> (TransportFactory, Arc<AtomicU64>) {
        let scripts = Arc::new(parking_lot::Mutex::new(
            scripts.into_iter().collect::<VecDeque<_>>(),
        ));
        let connects = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&connects);
        let factory: TransportFactory = Arc::new(move || {
            counted.fetch_add(1, AtomicOrdering::SeqCst);
            let script = scripts.lock().pop_front().unwrap_or_default();
            Box::pin(async move {
                Ok(Box::new(ScriptedTransport(script.into_iter().collect())) as BoxTransport)
            }) as ConnectFuture
        });
        (factory, connects)
    }

    /// `run` against `factory`, under a subscriber the test can read, with a
    /// receiver that sees every state the driver broadcasts.
    fn drive(
        core: &Arc<Core>,
        factory: TransportFactory,
    ) -> (
        Arc<SyncShared>,
        EventLog,
        broadcast::Receiver<crate::SyncStatus>,
        tokio::task::JoinHandle<()>,
    ) {
        use tracing::instrument::WithSubscriber as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let log = EventLog::default();
        let (status_tx, status_rx) = broadcast::channel(256);
        let shared = SyncShared::new(status_tx, 0);
        shared.mark_active();
        let rng: Arc<dyn Rng> = Arc::new(SystemRng);
        let driver = tokio::spawn(
            run(Arc::downgrade(core), Arc::clone(&shared), factory, rng)
                .with_subscriber(tracing_subscriber::registry().with(log.clone())),
        );
        (shared, log, status_rx, driver)
    }

    async fn until(what: &str, cond: impl Fn() -> bool) {
        timeout(Duration::from_secs(600), async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never saw: {what}"));
    }

    /// Every state the driver broadcast so far.
    fn states_seen(rx: &mut broadcast::Receiver<crate::SyncStatus>) -> Vec<SyncState> {
        let mut out = Vec::new();
        while let Ok(s) = rx.try_recv() {
            out.push(s.state);
        }
        out
    }

    /// `(result, err_code)` of every `ev` event, in order.
    fn fields_of(log: &EventLog, ev: &str) -> Vec<(Option<String>, Option<String>)> {
        log.0
            .lock()
            .iter()
            .filter(|e| e.ev.as_deref() == Some(ev))
            .map(|e| (e.result.clone(), e.err_code.clone()))
            .collect()
    }

    fn named(result: &str, code: Option<ErrorCode>) -> (Option<String>, Option<String>) {
        (Some(result.into()), code.map(|c| c.as_str().to_owned()))
    }

    /// The mapping itself, against the codes the relay's stream closes with
    /// (`AUTH_TOKEN_EXPIRED`, `AUTH_DEVICE_REVOKED`,
    /// `RELAY_STORAGE_UNAVAILABLE`), `AUTH_TOKEN_INVALID`, and the one the
    /// transport substitutes for a code it cannot read. A close parks the
    /// driver exactly when the catalogue says retrying will not help.
    #[test]
    fn only_a_non_retryable_close_is_terminal() {
        let end_for = |code| {
            let Ok(Some(frame)) = close_frame(code) else {
                unreachable!()
            };
            let (_, payload) = decode_frame(&frame).unwrap();
            SessionEnd::from_close(&payload)
        };
        for code in [
            ErrorCode::AuthTokenExpired,
            ErrorCode::RelayStorageUnavailable,
        ] {
            assert!(
                !end_for(code).is_terminal(),
                "{code} is retryable and must reconnect"
            );
        }
        for code in [
            ErrorCode::AuthDeviceRevoked,
            ErrorCode::AuthTokenInvalid,
            ErrorCode::InternalUnknownCode,
        ] {
            assert!(end_for(code).is_terminal(), "{code} must stop the driver");
        }
        assert!(
            matches!(SessionEnd::from_close(&[0xff]), SessionEnd::Disconnected),
            "an undecodable close is a drop, not a verdict"
        );
        assert!(matches!(
            SessionEnd::from_recv_error(TransportError::Unavailable("gone".into())),
            SessionEnd::Disconnected
        ));
        assert!(matches!(
            SessionEnd::from_recv_error(TransportError::Server {
                code: ErrorCode::AuthDeviceSigInvalid.as_str(),
                message: String::new(),
            }),
            SessionEnd::Refused { code, .. } if code == "AUTH_DEVICE_SIG_INVALID"
        ));
    }

    /// Driver-level: a terminal close stops the reconnect loop, and a new
    /// credential — the user signing in again — is what restarts it.
    ///
    /// Before #278 the `Close` arm returned the same value as a dropped
    /// socket, so this driver would have reconnected on the backoff schedule
    /// against a relay that had revoked the device: hundreds of attempts in
    /// the virtual hour below instead of none. The second session closes
    /// before its handshake completes, which is the handshake's own `Close`
    /// arm, and must stop the driver too.
    ///
    /// The third session is the one that answers its handshake, and it is
    /// there for the renewal the park consumed. `park_until_renewed` moves the
    /// driver's watch handle to the new version, so the next connect's mark
    /// finds nothing outstanding; only `run` carrying that version forward as
    /// `marked_at_connect` makes the handshake report it. Drop the carry and
    /// `sync.credential.marked_at_connect` never appears. The second session
    /// never handshakes, so it must leave the mark outstanding rather than
    /// report it, and the third reports the version it actually presented.
    #[tokio::test(start_paused = true)]
    async fn a_terminal_close_parks_the_driver_until_the_credential_is_replaced() {
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
        let (factory, connects) = scripted(vec![
            vec![hello_ack(), close_frame(ErrorCode::AuthDeviceRevoked)],
            vec![close_frame(ErrorCode::AuthTokenInvalid)],
            vec![hello_ack(), close_frame(ErrorCode::AuthDeviceRevoked)],
        ]);
        let (shared, log, _rx, driver) = drive(&core, factory);
        let n = || connects.load(AtomicOrdering::SeqCst);

        until("the first terminal close", || {
            shared.current_state() == SyncState::Stopped
        })
        .await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(n(), 1, "a stopped driver must not reconnect on a timer");
        assert_eq!(shared.current_state(), SyncState::Stopped);

        credential.set(Some("second-token".into()));
        until("the reconnect a new credential buys", || n() == 2).await;
        until("the second terminal close", || {
            shared.current_state() == SyncState::Stopped
        })
        .await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
        assert_eq!(
            n(),
            2,
            "and it stops again when that session is refused too"
        );

        credential.set(Some("third-token".into()));
        until("the reconnect the third credential buys", || n() == 3).await;
        until("the third terminal close", || {
            shared.current_state() == SyncState::Stopped
        })
        .await;

        shared.request_shutdown();
        timeout(Duration::from_secs(60), driver)
            .await
            .expect("a stopped driver must still honour shutdown")
            .unwrap();

        assert_eq!(
            fields_of(&log, "sync.session.closed"),
            vec![
                named("stopped", Some(ErrorCode::AuthDeviceRevoked)),
                named("stopped", Some(ErrorCode::AuthTokenInvalid)),
                named("stopped", Some(ErrorCode::AuthDeviceRevoked)),
            ]
        );
        // The credential events in order, with the version each names.
        let credential_events: Vec<(String, Option<u64>)> = log
            .0
            .lock()
            .iter()
            .filter_map(|e| match e.ev.as_deref() {
                Some(ev @ ("sync.session.resumed" | MARKED_AT_CONNECT)) => {
                    Some((ev.to_owned(), e.to_v))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            credential_events,
            vec![
                ("sync.session.resumed".to_owned(), Some(1)),
                ("sync.session.resumed".to_owned(), Some(2)),
                (MARKED_AT_CONNECT.to_owned(), Some(2)),
            ],
            "each resume names its credential, the unanswered second session \
             reports nothing, and the answered third reports the renewal the \
             park consumed"
        );
        core.shutdown().await;
    }

    /// Driver-level: a retryable close reconnects, and so does a close
    /// whose payload does not decode. None may ever read as `Stopped`.
    ///
    /// `RELAY_STORAGE_UNAVAILABLE` is in the set because the relay sends it
    /// for a failed durable-log read and logs it `transient`/`retryable`
    /// itself: a driver that parked on it would strand every device that
    /// opened a stream during the fault until its user signed in again.
    #[tokio::test(start_paused = true)]
    async fn a_recoverable_close_reconnects_and_names_its_code() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        let garbage = Ok(Some(
            encode_frame(MsgKind::Close, FrameFlags::EMPTY, &[0xff]).unwrap(),
        ));
        let (factory, connects) = scripted(vec![
            vec![hello_ack(), close_frame(ErrorCode::AuthTokenExpired)],
            vec![hello_ack(), garbage],
            vec![hello_ack(), close_frame(ErrorCode::RelayStorageUnavailable)],
        ]);
        let (shared, log, mut rx, driver) = drive(&core, factory);

        until("three reconnects", || {
            connects.load(AtomicOrdering::SeqCst) >= 4
        })
        .await;
        shared.request_shutdown();
        timeout(Duration::from_secs(60), driver)
            .await
            .expect("the driver never stopped")
            .unwrap();

        let states = states_seen(&mut rx);
        assert!(
            !states.contains(&SyncState::Stopped),
            "no close here is terminal, saw {states:?}"
        );
        assert_eq!(
            fields_of(&log, "sync.session.closed")[..3],
            [
                named("failed", Some(ErrorCode::AuthTokenExpired)),
                named("failed", None),
                named("failed", Some(ErrorCode::RelayStorageUnavailable)),
            ]
        );
        assert_eq!(
            fields_of(&log, "sync.session.error")[..2],
            [
                named("failed", Some(ErrorCode::AuthTokenExpired)),
                named("failed", Some(ErrorCode::RelayStorageUnavailable)),
            ],
            "a retryable close is logged as one, never as `stopped`"
        );
        let storage_error = log
            .0
            .lock()
            .iter()
            .find(|e| {
                e.ev.as_deref() == Some("sync.session.error")
                    && e.err_code.as_deref() == Some(ErrorCode::RelayStorageUnavailable.as_str())
            })
            .cloned()
            .expect("the storage close is logged");
        assert_eq!(
            (storage_error.err_kind.as_deref(), storage_error.retryable),
            (Some("transient"), Some(true)),
            "the client's line agrees with codes.toml and with the relay's own log"
        );
        core.shutdown().await;
    }

    /// Driver-level: a refused stream reaches the log under the relay's own
    /// code, whether the refusal lands during the handshake or mid-session,
    /// and the driver reconnects as it does for a drop.
    ///
    /// Before #278 both `recv_frame` sites discarded the error unread, so
    /// these two sessions logged exactly what a dropped TCP connection logs.
    #[tokio::test(start_paused = true)]
    async fn a_refused_stream_is_logged_under_the_relays_code() {
        let dir = tempfile::tempdir().unwrap();
        let core = open_arc(dir.path()).await;
        let (factory, connects) = scripted(vec![
            vec![refusal(ErrorCode::AuthTokenInvalid)],
            vec![hello_ack(), refusal(ErrorCode::AuthDeviceSigInvalid)],
        ]);
        let (shared, log, mut rx, driver) = drive(&core, factory);

        until("two reconnects", || {
            connects.load(AtomicOrdering::SeqCst) >= 3
        })
        .await;
        shared.request_shutdown();
        timeout(Duration::from_secs(60), driver)
            .await
            .expect("the driver never stopped")
            .unwrap();

        assert!(!states_seen(&mut rx).contains(&SyncState::Stopped));
        assert_eq!(
            fields_of(&log, "sync.session.closed")[..2],
            [
                named("failed", Some(ErrorCode::AuthTokenInvalid)),
                named("failed", Some(ErrorCode::AuthDeviceSigInvalid)),
            ]
        );
        assert_eq!(
            fields_of(&log, "sync.session.error")[..2],
            [
                named("failed", Some(ErrorCode::AuthTokenInvalid)),
                named("failed", Some(ErrorCode::AuthDeviceSigInvalid)),
            ]
        );
        core.shutdown().await;
    }
}
