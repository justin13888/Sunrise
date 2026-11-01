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
//! `crates/sunrise-sync/src/sse/mod.rs`) and the in-process loopback used by the
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
//! the session sees evidence of loss: a retransmit, an undecodable frame, a
//! remote op that fails its integrity checks, or an applied op or peer digest
//! that names an op this vault does not hold (ADR-0043).
//!
//! **Inbound: digests.** The same timer publishes this vault's stream digests
//! when they are due (`Core::sync_publish_due_digests`). Resync asks the relay
//! for what lies past each cursor, which is only as good as the relay's view;
//! a peer's digest is how a replica learns it is missing ops the relay never
//! sent, or holds different ones.
//!
//! The timer is the guarantee and the evidence is the latency optimisation.
//! Evidence alone would miss the case where the *last* frame in each direction
//! is the one that vanished, leaving nothing to notice.

mod frames;
mod schedule;
mod shared;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::config::Rng;
use crate::core::Core;
use crate::snapshot_sync::{recover_gaps, GapRecovery};
use frames::{
    build_hello, decode_hello_ack, encode_hello, encode_subscribe_one, subscribe_to_new_streams,
};
use schedule::{
    backoff_sleep, next_deadline, retransmit_due, rng_unit, Deadlines, Due, InflightBatch,
    LossEvidence, SessionSchedule,
};
use sunrise_error::ErrorCode;
use sunrise_sync::{Backoff, SyncState, Transport, TransportError};

pub(crate) use frames::{encode_subscribe_all, Outgoing};
pub(crate) use schedule::{MonotonicClock, TokioClock};
pub(crate) use shared::SyncShared;
pub use sunrise_sync::{CredentialRead, TokenSource, TokenWatch};
use sunrise_wire_protocol::{
    decode_frame, encode_frame, AckPayload, BacklogPayload, Capability, CapabilityBits,
    CaughtUpPayload, ClosePayload, ErrorPayload, FrameFlags, MsgKind, OpBatchPayload,
    RefreshTokenAckPayload, RefreshTokenPayload, SubscribePayload,
};

/// Boxed transport produced by a [`TransportFactory`].
pub type BoxTransport = Box<dyn Transport>;

/// Future returned by a [`TransportFactory`]: yields a connected transport.
pub type ConnectFuture = Pin<Box<dyn Future<Output = Result<BoxTransport, TransportError>> + Send>>;

/// A factory that opens a fresh transport on every call. Called once per
/// connect attempt (initial connect and every reconnect after a drop), so it
/// must be able to produce a brand-new connection each time.
///
/// # The driver reads the credential, not the factory
///
/// Each call is handed the [`CredentialRead`] this attempt presents: the
/// bearer the [`SyncConfig::credential`] held when `run` read it, and the
/// version it was at. A factory presents [`CredentialRead::bearer`] and holds
/// no bearer of its own.
///
/// The read is the driver's because the driver has to know exactly what each
/// attempt carried. `run` brings its renewal handle forward to that version —
/// not to whatever the source holds by the time the factory returns — so a
/// renewal the attempt carried is not re-announced, and a renewal landing at
/// any moment after the read, including while the factory runs, is still
/// announced in band as a `0x12 RefreshToken`. When the factory did its own
/// read, the driver could only assume the factory read per attempt and mark
/// to whatever the source held afterwards: a factory that fixed its bearer
/// when it was built had every later renewal consumed by a connect that did
/// not carry it, and a write landing between the factory's read and the mark
/// was consumed the same way (#273).
///
/// A factory can still ignore the argument and present a bearer captured
/// when it was built. Nothing in this workspace does, and nothing has a reason
/// to: the bearer it would need is the one it is handed.
pub type TransportFactory = Arc<dyn Fn(CredentialRead) -> ConnectFuture + Send + Sync>;

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
// Driver task
// ---------------------------------------------------------------------------

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
    // seam exists for the scheduling unit tests in
    // `sync_driver/tests/scheduling.rs`, which build `Deadlines` directly.
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
        // Connect (cancellable by shutdown). The bearer this attempt presents
        // is read here, by the driver, and handed to the factory — so the
        // version it was read at is known rather than assumed.
        let read = credential.read();
        let carried = read.version();
        let connect_fut = factory(read);
        // This attempt carries every renewal up to `carried`, including those
        // that landed while the driver was disconnected. Bring the handle
        // forward to exactly that, and the pump's renewal arm below fires for
        // every write the attempt did not carry — one made during the factory
        // call, the dial, the handshake or the session alike. `TokenSource::set`
        // is called from other threads (the FFI seam in `sunrise-core-bindings`
        // and the CLI's login flow), and a write landing between the read above
        // and this mark is left pending rather than consumed (#273).
        marked_at_connect = mark_renewals_current(&mut renewals, carried, marked_at_connect);
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
            err_kind = close.code.default_kind().as_str(),
            retryable = true,
            result = "failed",
            cause = %close.reason,
            "relay closed the session; reconnecting"
        ),
        SessionEnd::Closed(close) => tracing::warn!(
            ev = "sync.session.error",
            err_code = close.code.as_str(),
            err_kind = close.code.default_kind().as_str(),
            retryable = false,
            result = "stopped",
            cause = %close.reason,
            "relay closed the session for a reason reconnecting cannot fix; waiting for a new credential"
        ),
    }
}

/// Bring the driver's renewal handle forward to the credential version this
/// connect carried, and fold the result into what this connect still owes the
/// log.
///
/// `Some(v)` when a renewal out of the offline window has been consumed and
/// not yet reported — by this attempt, or by an earlier attempt the relay never
/// answered. `None` when nothing up to `carried` was pending and nothing is
/// outstanding.
///
/// `carried` is the version of the [`CredentialRead`] `run` handed the factory
/// for this attempt, so the mark consumes exactly what the attempt presents:
/// [`TokenWatch::mark_carried`] leaves any later write pending, and it reaches
/// the relay in band as a `0x12 RefreshToken` — whether it landed while the
/// factory ran, during the dial, the handshake or the session itself — instead
/// of waiting for the next reconnect.
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
fn mark_renewals_current(
    renewals: &mut TokenWatch,
    carried: u64,
    unreported: Option<u64>,
) -> Option<u64> {
    renewals.mark_carried(carried).or(unreported)
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
    shared.reset_replay();
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
    let mut pending_sends: Vec<Outgoing> = Vec::new();
    let mut deadlines = Deadlines::new(schedule.resync_interval, schedule.clock);
    // Ops this vault knows it lacks (ADR-0043). The session's own first
    // Subscribe already asks for everything past the cursors, so only growth
    // from here on is new evidence.
    let mut chain_wanted = core.sync_chain_wanted();
    // Streams the relay reported a cursor gap on, and what catching each up
    // from its snapshot record came to this session (ADR-0068).
    let mut gaps: Vec<[u8; 16]> = Vec::new();
    let mut recovery = GapRecovery::default();

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
    // A session is the first moment a revocation can reach the relay, so this
    // runs once per session rather than on a timer: the ordinary case is that
    // the user revoked a device while offline, or on a device that then closed.
    // Reconnect backoff is therefore the retry schedule.
    crate::relay_intents::drain(core, transport.as_mut()).await;
    // A session is also the first moment an attachment sealed offline can
    // reach the relay, and the first moment one created elsewhere can be
    // fetched. Both directions run here for the same reason the revocation
    // does: reconnect backoff is the retry schedule.
    crate::blob_sync::drain(core, transport.as_mut()).await;
    // A vault restored from a recovery code takes the account's keys before
    // the first frame is applied (ADR-0070 §4): what it would otherwise refuse
    // is an op under an epoch it minted a key of its own for.
    if core.wants_key_bundle_before_replay() {
        crate::key_bundle_sync::recover_if_pinned(core, transport.as_mut()).await;
    }
    maybe_live(
        core,
        shared,
        transport.as_mut(),
        &subscribed,
        &caught_up,
        &mut recovery.key_bundle_tried,
    )
    .await;

    // ---- Main pump ----
    //
    // Sends happen sequentially at the top of the loop (no recv future alive),
    // so the transport is never mutably borrowed by two branches at once.
    loop {
        if send_pending(
            transport.as_mut(),
            &mut pending_sends,
            &mut inflight,
            &deadlines,
        )
        .await
        .is_err()
        {
            return SessionEnd::Disconnected;
        }

        let wake_at = next_deadline(&inflight, &deadlines);
        // A frame that has already arrived is read before anything else but
        // shutdown (#475). A send can hold the pump for a round trip, and on
        // the SSE transport the ack is already queued by the time it returns.
        // Behind the timer, a deadline that passed during that wait resent a
        // batch whose ack sat unread; behind the submit wake, a commit rate
        // near the send time kept the wake ready on every turn and the acks
        // were never read at all, until the retry policy gave up. Reading one
        // costs no round trip and frees the batch it answers, and nothing
        // queued behind it is lost: submits coalesce into the next outbox
        // drain, and the timer is still due on the next turn.
        let ev = tokio::select! {
            biased;
            () = shared.shutdown_notified() => SessionEvent::Shutdown,
            r = transport.recv_frame() => SessionEvent::Recv(r),
            () = shared.submit_notified() => SessionEvent::Submit,
            _ = renewals.changed() => SessionEvent::CredentialRenewed,
            () = tokio::time::sleep_until(wake_at) => SessionEvent::Timer,
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
                crate::relay_intents::drain(core, transport.as_mut()).await;
                // `attach_file` writes its queue row and then submits the op,
                // which pokes this wake — so the upload starts in the same
                // breath as the metadata rather than waiting for a reconnect.
                crate::blob_sync::drain(core, transport.as_mut()).await;
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
                if deadlines.resync_due() {
                    // The digests that are due ride the same tick (ADR-0043
                    // §5): the cadence is a day or 256 ops, so checking it
                    // every interval is what makes either bound hold.
                    core.sync_publish_due_digests();
                    // The compactor's daily record rides the same tick as the
                    // compaction that wrote it (ADR-0068).
                    crate::snapshot_sync::publish(core, transport.as_mut()).await;
                    // And the account's key bundle, when this device holds
                    // keys the pinned one lacks (ADR-0069).
                    crate::key_bundle_sync::publish(core, transport.as_mut()).await;
                    // And the account's identity chain, when this device folds
                    // further than the relay's record (ADR-0070).
                    crate::identity_chain_sync::publish(core, transport.as_mut()).await;
                    match encode_subscribe_all(core) {
                        Ok(frame) => pending_sends.push(Outgoing::Frame(frame)),
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
                    crate::blob_sync::drain(core, transport.as_mut()).await;
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
                    &mut gaps,
                )
                .await
                {
                    return end;
                }
                // A gap the relay named a stream for is caught up from that
                // stream's snapshot record before the next frame is read, so
                // the replay behind the gap lands on the floors it raised.
                if !gaps.is_empty() {
                    recover_gaps(
                        core,
                        shared,
                        transport.as_mut(),
                        &mut gaps,
                        &mut recovery,
                        &mut pending_sends,
                        &mut subscribed,
                    )
                    .await;
                }
                // A frame that told this vault of an op it does not hold is
                // loss evidence that names the loss: re-subscribe now rather
                // than at the next tick. It is how a withheld last op, which
                // leaves no gap, gets asked for at all.
                let wanted = core.sync_chain_wanted();
                if wanted > chain_wanted {
                    deadlines.note_loss(LossEvidence::KnownMissing);
                }
                chain_wanted = wanted;
                // An inbound batch is the only way an attachment created on
                // another device becomes known here, so it is the trigger the
                // fetch half needs: without it a device that stays connected
                // would not look for new bytes until its next reconnect, and a
                // long-lived session is the ordinary case rather than the
                // exception.
                crate::blob_sync::drain(core, transport.as_mut()).await;
                maybe_live(
                    core,
                    shared,
                    transport.as_mut(),
                    &subscribed,
                    &caught_up,
                    &mut recovery.key_bundle_tried,
                )
                .await;
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
    pending_sends: &mut Vec<Outgoing>,
) {
    if !refresh_negotiated {
        tracing::debug!(
            ev = "sync.credential.renewed.deferred",
            "relay did not negotiate in-band refresh; the new bearer waits for the next connect"
        );
        return;
    }
    if let Some(frame) = encode_refresh_token(credential) {
        pending_sends.push(Outgoing::Frame(frame));
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

/// Hand every queued frame to the transport, in order.
///
/// A batch's retransmit deadline is armed here, from the clock reading after
/// its send returns, so the time a send spends blocked is not counted against
/// the ack it is waiting for. A batch that is no longer in flight has been
/// acked since it was queued, and there is nothing left to send for it.
async fn send_pending(
    transport: &mut dyn Transport,
    pending_sends: &mut Vec<Outgoing>,
    inflight: &mut HashMap<u64, InflightBatch>,
    deadlines: &Deadlines,
) -> Result<(), TransportError> {
    for out in pending_sends.drain(..) {
        match out {
            Outgoing::Frame(frame) => transport.send_frame(frame).await?,
            Outgoing::Batch(batch_id) => {
                let Some(frame) = inflight.get(&batch_id).map(|b| b.frame.clone()) else {
                    continue;
                };
                transport.send_frame(frame).await?;
                if let Some(batch) = inflight.get_mut(&batch_id) {
                    if let Due::AfterSend(delay) = batch.due_at {
                        batch.due_at = Due::At(deadlines.now() + delay);
                    }
                }
            }
        }
    }
    Ok(())
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
    pending_sends: &mut Vec<Outgoing>,
    deadlines: &mut Deadlines,
    gaps: &mut Vec<[u8; 16]>,
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
            shared.note_replayed(batch.stream_id, batch.ops.len() as u64);
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
                    // Idempotent re-receive, an op parked awaiting its key, or
                    // one parked because this build does not know its kind:
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
        // So does the backlog statement (#459), which is tried first: its bytes
        // would also pass for a `CaughtUpPayload`, whose reader ignores the
        // `ops` key it does not know.
        MsgKind::StreamUpdate => {
            if let Ok(backlog) = BacklogPayload::decode(&payload) {
                shared.note_backlog(backlog.stream_id, backlog.ops);
            } else if let Ok(cu) = CaughtUpPayload::decode(&payload) {
                shared.note_stream_caught_up(cu.stream_id);
                caught_up.insert(cu.stream_id);
            } else {
                deadlines.note_loss(LossEvidence::UndecodableFrame);
            }
        }
        MsgKind::Ping => {
            if let Ok(frame) = encode_frame(MsgKind::Pong, FrameFlags::EMPTY, &[]) {
                pending_sends.push(Outgoing::Frame(frame));
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
            on_error(shared, &err, gaps);
        }
        MsgKind::RefreshTokenAck => note_refresh_ack(&payload),
        // Nack and every other kind are non-fatal in the self-host build: they fall
        // through to a no-op.
        _ => {}
    }
    Ok(())
}

/// An `Error` frame from the relay: logged, and a cursor gap handed on.
fn on_error(shared: &SyncShared, err: &ErrorPayload, gaps: &mut Vec<[u8; 16]>) {
    // A cursor gap is the one error the session cannot recover from by
    // trying harder: the relay has said the ops between our cursor and
    // its watermark are gone, so neither a retransmit nor a resync can
    // produce them. Ignoring it — which is what the driver used to do —
    // meant accepting the `CaughtUp` that follows and reporting `Live`
    // while permanently missing data. Latch a degraded state instead;
    // it outlives the session because the loss does.
    //
    // Unless the stream's snapshot record covers the hole (ADR-0068):
    // a gap that names its stream is handed to `recover_gaps`, which
    // latches the same state only if the record cannot be applied.
    if err.code == ErrorCode::SyncCursorGap {
        if let Some(stream) = err.stream_id {
            gaps.push(stream);
            return;
        }
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
    out: &mut Vec<Outgoing>,
    rng: &dyn Rng,
) -> Result<(), ()> {
    let groups = core.sync_outbox_grouped(inflight_ops).map_err(|_| ())?;
    for (stream_id, ops) in groups {
        if ops.is_empty() {
            continue;
        }
        if subscribed.insert(stream_id) {
            let frame = encode_subscribe_one(stream_id)?;
            out.push(Outgoing::Frame(frame));
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
        // an attempt: attempt 0 is the original send. It starts when that send
        // returns, not now.
        let delay = backoff.next_delay(rng_unit(rng)).unwrap_or(MIN_RESYNC_GAP);
        inflight.insert(
            batch_id,
            InflightBatch {
                op_ids,
                frame,
                backoff,
                due_at: Due::AfterSend(delay),
            },
        );
        out.push(Outgoing::Batch(batch_id));
    }
    Ok(())
}

/// Transition to `Live` once every subscribed stream is caught up and the
/// outbox is empty (all local ops acked).
///
/// A session that is about to be `Live` first does its snapshot duty
/// (ADR-0068): compaction if it is due, and the publish of any snapshot record
/// this replica is the compactor for and the relay does not pin. Both are
/// nothing at all on almost every session; on the one that has a record to
/// upload, `Live` waits for it, so a sync that ends at `Live` — the CLI's,
/// an OS background window's — has left the record on the relay. The
/// account's key bundle is published the same way and for the same reason
/// (ADR-0069): a rotation's new keys are on the relay by the time the session
/// that sent them reports `Live`.
async fn maybe_live<T: Transport + ?Sized>(
    core: &Core,
    shared: &SyncShared,
    transport: &mut T,
    subscribed: &HashSet<[u8; 16]>,
    caught_up: &HashSet<[u8; 16]>,
    key_bundle_tried: &mut bool,
) {
    if shared.current_state() != SyncState::CatchingUp {
        return;
    }
    let pending = core.sync_pending().unwrap_or(0);
    if pending == 0 && subscribed.iter().all(|s| caught_up.contains(s)) {
        // Caught up with ops still waiting for a key, on a vault restored
        // from a recovery code in this process: the identity copies of the
        // epochs before a rotation are sealed to an identity it does not
        // hold, and the account's key bundle, sealed to the identity in
        // force, carries them (ADR-0070 §4). Once a session, as for a gap.
        if !*key_bundle_tried && core.wants_key_bundle_for_parked_ops() {
            *key_bundle_tried = true;
            crate::key_bundle_sync::recover_if_pinned(core, transport).await;
        }
        core.sync_compact_if_due();
        crate::snapshot_sync::publish(core, transport).await;
        crate::key_bundle_sync::publish(core, transport).await;
        crate::identity_chain_sync::publish(core, transport).await;
        if shared.current_state() != SyncState::CatchingUp {
            return;
        }
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
/// Whether an `apply_remote` failure means the bytes were damaged, as opposed
/// to the op being well-formed but from a device this vault does not trust.
///
/// An op of a kind this build does not know never reaches this: it verified
/// and opened, so the engine parks it and returns `Ok` (issue #320).
fn is_corruption(e: &crate::core::CoreError) -> bool {
    matches!(
        e,
        crate::core::CoreError::Engine(crate::engine::EngineError::RemoteOpInvalid(_))
    )
}
