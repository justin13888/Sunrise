//! When a session acts on its own: the resync deadline and its floor, each
//! batch's retransmit deadline, and the reconnect backoff.
//!
//! Every instant here comes from an injected [`MonotonicClock`] and every delay
//! from the [`Backoff`] policy, so `tests/scheduling.rs` drives all of it to
//! exact instants without a runtime or a relay.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::frames::Outgoing;
use super::shared::SyncShared;
use super::MIN_RESYNC_GAP;
use crate::config::Rng;
use sunrise_sync::Backoff;

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
pub(super) struct InflightBatch {
    pub(super) op_ids: Vec<[u8; 16]>,
    pub(super) frame: Vec<u8>,
    /// Retry policy for this batch alone. Per batch, not per session, so one
    /// unlucky batch does not consume another's attempts.
    pub(super) backoff: Backoff,
    /// When this batch is considered lost and worth sending again.
    pub(super) due_at: Due,
}

/// A batch's retransmit deadline, which starts when its send returns.
///
/// Not when the batch is built: a send can hold the pump for a whole round
/// trip (the SSE transport's is a `POST` whose reply is the ack), and a
/// deadline counted from before that wait is spent on the wait itself. At a
/// round trip near the first delay that collapses the session (#475): the
/// deadline passes while the send is still blocking, the batch is resent, the
/// resend blocks again, and the retry policy runs out on a link that
/// delivered every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Due {
    /// Queued and not sent yet; the deadline is this long after the send
    /// returns.
    AfterSend(Duration),
    /// Sent; considered lost at this instant.
    At(Instant),
}

/// Why a session decided the link is losing data.
///
/// Not an error in itself — each of these is survivable on its own — but each
/// one is evidence that something *inbound* may have been lost too, which
/// nothing else in the protocol would reveal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LossEvidence {
    /// An op batch went unacked long enough to be sent again.
    Retransmit,
    /// An inbound frame could not be decoded at the frame or payload level.
    UndecodableFrame,
    /// A remote op failed signature, AEAD, or CBOR checks. Distinct from an op
    /// from an untrusted device, which is a policy outcome, not corruption.
    CorruptOp,
    /// An applied op told this replica of an op it does not hold: a later op's
    /// field 14 or 15, or a peer's stream digest (ADR-0043). Unlike the three
    /// above this one names what is missing, and it is the only evidence a
    /// relay that withholds the last op of a sequence leaves.
    KnownMissing,
}

impl LossEvidence {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Retransmit => "retransmit",
            Self::UndecodableFrame => "undecodable_frame",
            Self::CorruptOp => "corrupt_op",
            Self::KnownMissing => "known_missing",
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
pub(super) struct SessionSchedule {
    pub(super) resync_interval: Duration,
    pub(super) clock: Arc<dyn MonotonicClock>,
}

/// Deadlines a live session is waiting on, and the loss evidence that pulls
/// the resync deadline forward.
pub(super) struct Deadlines {
    /// Next anti-entropy resync.
    pub(super) resync_at: Instant,
    /// Earliest a resync may happen at all, from [`MIN_RESYNC_GAP`].
    pub(super) resync_floor: Instant,
    pub(super) interval: Duration,
    /// Where every instant below comes from. Held rather than passed per call
    /// because `note_loss` is reached from deep inside frame handling, and
    /// threading a `now` through five frame kinds to reach it would put the
    /// clock in signatures that have nothing to do with time.
    clock: Arc<dyn MonotonicClock>,
}

impl Deadlines {
    pub(super) fn new(interval: Duration, clock: Arc<dyn MonotonicClock>) -> Self {
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
    pub(super) fn now(&self) -> Instant {
        self.clock.now()
    }

    /// Whether the anti-entropy resync is due.
    ///
    /// `<=`, not `<`: a deadline at exactly `resync_at` has arrived. The pump
    /// sleeps *until* this instant, so a strict comparison would wake, decide
    /// nothing was due, and sleep again on a zero-length timer.
    pub(super) fn resync_due(&self) -> bool {
        self.resync_at <= self.clock.now()
    }

    /// Record that a resync just happened.
    pub(super) fn resynced(&mut self) {
        let now = self.clock.now();
        self.resync_floor = now + MIN_RESYNC_GAP;
        self.resync_at = now + self.interval;
    }

    /// Bring the next resync forward to the earliest the floor allows.
    pub(super) fn note_loss(&mut self, evidence: LossEvidence) {
        tracing::debug!(
            ev = "sync.loss_evidence",
            cause = evidence.as_str(),
            "scheduling a resync"
        );
        let now = self.clock.now();
        self.resync_at = self.resync_at.min(self.resync_floor.max(now));
    }
}

/// Sleep for the next backoff delay, interruptible by shutdown and cut short
/// by [`SyncShared::wake_reconnect`]. Returns `false` if shutdown was
/// requested (caller should stop).
pub(super) async fn backoff_sleep(
    backoff: &mut Backoff,
    rng: &dyn Rng,
    shared: &SyncShared,
) -> bool {
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
        () = shared.reconnect.notified() => true,
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
pub(super) fn next_backoff_delay(backoff: &mut Backoff, jitter_unit: f64) -> Duration {
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

/// When the pump next has something to do on its own: the soonest retransmit
/// deadline, or the next resync, whichever comes first. A batch still waiting
/// for its send has no deadline yet.
pub(super) fn next_deadline(
    inflight: &HashMap<u64, InflightBatch>,
    deadlines: &Deadlines,
) -> Instant {
    inflight
        .values()
        .filter_map(|b| match b.due_at {
            Due::At(at) => Some(at),
            Due::AfterSend(_) => None,
        })
        .min()
        .map_or(deadlines.resync_at, |d| d.min(deadlines.resync_at))
}

/// Queue a retransmit for every batch whose ack never arrived.
///
/// Returns false when a batch has exhausted the retry policy — the caller ends
/// the session rather than retrying forever on a link that is clearly not
/// delivering.
pub(super) fn retransmit_due(
    inflight: &mut HashMap<u64, InflightBatch>,
    out: &mut Vec<Outgoing>,
    deadlines: &mut Deadlines,
    rng: &dyn Rng,
) -> bool {
    // The same clock the deadlines were computed from. Reading `Instant::now`
    // here instead would compare two different timelines the moment either one
    // is faked.
    let now = deadlines.now();
    for (batch_id, batch) in inflight.iter_mut() {
        // A batch queued and not yet sent is not overdue, whatever the clock
        // says: its deadline has not started.
        let Due::At(due_at) = batch.due_at else {
            continue;
        };
        if due_at > now {
            continue;
        }
        let Some(delay) = batch.backoff.next_delay(rng_unit(rng)) else {
            return false;
        };
        batch.backoff.record_attempt();
        batch.due_at = Due::AfterSend(delay);
        tracing::debug!(
            ev = "sync.op.retransmit",
            batch_id = *batch_id,
            attempt = u64::from(batch.backoff.attempt()),
            n_ops = batch.op_ids.len() as u64,
            "op batch unacked; sending again"
        );
        out.push(Outgoing::Batch(*batch_id));
        // An ack that never came is the clearest evidence this link drops
        // frames, and the inbound direction has no ack of its own to miss.
        deadlines.note_loss(LossEvidence::Retransmit);
    }
    true
}

/// Map RNG output to a jitter unit in `[0, 1]` for [`Backoff::next_delay`].
pub(super) fn rng_unit(rng: &dyn Rng) -> f64 {
    let mut b = [0u8; 8];
    rng.fill_bytes(&mut b);
    // 53-bit mantissa → uniform [0, 1).
    let v = u64::from_le_bytes(b) >> 11;
    #[allow(clippy::cast_precision_loss)]
    {
        v as f64 / (1u64 << 53) as f64
    }
}
