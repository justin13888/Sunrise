//! The driver's pure scheduling arithmetic, driven with no sleeping at all.
//!
//! Separate from the harness suites beside it, which need a fake relay and a
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

use crate::config::Rng;
use crate::sync_driver::frames::Outgoing;
use crate::sync_driver::schedule::{
    backoff_sleep, next_backoff_delay, next_deadline, retransmit_due, Deadlines, Due,
    InflightBatch, LossEvidence, MonotonicClock,
};
use crate::sync_driver::{send_pending, SyncShared, MIN_RESYNC_GAP};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use sunrise_sync::Backoff;
use sunrise_sync::{Transport, TransportError};
use tokio::sync::broadcast;
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

/// A wake ends the flat thirty-second wait at once, and the backoff still
/// advances: the wake skips one delay, not the schedule.
#[tokio::test]
async fn a_reconnect_wake_cuts_the_backoff_short() {
    let (status_tx, _status_rx) = broadcast::channel(4);
    let shared = SyncShared::new(status_tx, 0);
    let mut backoff = Backoff::canonical();
    for _ in 0..5 {
        let _ = next_backoff_delay(&mut backoff, 0.5);
    }
    assert!(
        backoff.exhausted(),
        "the next wait is the flat thirty seconds"
    );
    shared.wake_reconnect();
    let woke = tokio::time::timeout(
        Duration::from_secs(5),
        backoff_sleep(&mut backoff, &MidJitterRng, &shared),
    )
    .await
    .expect("a woken backoff returns at once");
    assert!(woke, "a wake is not a shutdown");
    assert_eq!(
        backoff.attempt(),
        0,
        "the delay was still taken from the schedule"
    );
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
            due_at: Due::At(start + Duration::from_millis(100)),
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
    assert_eq!(
        out,
        vec![Outgoing::Batch(1)],
        "the batch goes out at the deadline, as its own frame and not a re-read"
    );
    assert!(
        deadlines.resync_due(),
        "an unacked batch is loss evidence, so a resync is pulled forward"
    );

    let batch = &inflight[&1];
    assert_eq!(batch.backoff.attempt(), 1, "one attempt consumed");
    assert_eq!(
        batch.due_at,
        Due::AfterSend(Duration::from_millis(100)),
        "respaced by the policy's first delay, counted from the resend"
    );
}

/// A transport whose send holds the caller for a round trip, on a clock
/// the test owns, and records what it was handed.
struct BlockingSend {
    clock: Arc<ManualClock>,
    hold: Duration,
    sent: Vec<Vec<u8>>,
}

#[async_trait::async_trait]
impl Transport for BlockingSend {
    async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.clock.advance(self.hold);
        self.sent.push(frame);
        Ok(())
    }
    async fn recv_frame(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(None)
    }
    async fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}

/// A batch's deadline starts when its send returns, so a send that blocks
/// for longer than the first delay does not make the batch overdue the
/// moment it lands (#475). Two batches queued together each get their own
/// send's return as the start.
#[tokio::test]
async fn a_batch_deadline_starts_when_its_send_returns() {
    let clock = ManualClock::started();
    let rng = MidJitterRng;
    let mut deadlines = Deadlines::new(
        Duration::from_secs(300),
        Arc::clone(&clock) as Arc<dyn MonotonicClock>,
    );
    let start = clock.now();
    let mut inflight = HashMap::new();
    for (id, byte) in [(1, 0xa1), (2, 0xa2)] {
        inflight.insert(
            id,
            InflightBatch {
                op_ids: vec![[byte; 16]],
                frame: vec![byte],
                backoff: Backoff::canonical(),
                due_at: Due::AfterSend(Duration::from_millis(100)),
            },
        );
    }
    assert_eq!(
        next_deadline(&inflight, &deadlines),
        start + Duration::from_secs(300),
        "an unsent batch has no deadline to wake for"
    );
    clock.advance(Duration::from_secs(1));
    let mut out = Vec::new();
    assert!(retransmit_due(
        &mut inflight,
        &mut out,
        &mut deadlines,
        &rng
    ));
    assert!(out.is_empty(), "an unsent batch is never overdue");

    let mut transport = BlockingSend {
        clock: Arc::clone(&clock),
        hold: Duration::from_millis(300),
        sent: Vec::new(),
    };
    let mut pending = vec![
        Outgoing::Batch(1),
        Outgoing::Frame(vec![0xff]),
        Outgoing::Batch(2),
        // Acked between being queued and being sent: nothing to send.
        Outgoing::Batch(3),
    ];
    let before = clock.now();
    send_pending(&mut transport, &mut pending, &mut inflight, &deadlines)
        .await
        .unwrap();
    assert!(pending.is_empty(), "everything queued was handed over");
    assert_eq!(
        transport.sent,
        vec![vec![0xa1], vec![0xff], vec![0xa2]],
        "in the order queued, each batch byte for byte"
    );
    let hold = Duration::from_millis(300);
    let delay = Duration::from_millis(100);
    assert_eq!(inflight[&1].due_at, Due::At(before + hold + delay));
    assert_eq!(inflight[&2].due_at, Due::At(before + hold * 3 + delay));

    // Batch 1 is overdue now, because the two sends after it took longer
    // than its delay; the wait on its own send was not counted.
    let mut out = Vec::new();
    assert!(retransmit_due(
        &mut inflight,
        &mut out,
        &mut deadlines,
        &rng
    ));
    assert_eq!(out, vec![Outgoing::Batch(1)]);
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
            due_at: Due::At(clock.now()),
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
            due_at: Due::At(start + Duration::from_millis(100)),
        },
    );
    inflight.insert(
        2,
        InflightBatch {
            op_ids: Vec::new(),
            frame: Vec::new(),
            backoff: Backoff::canonical(),
            due_at: Due::At(start + Duration::from_millis(50)),
        },
    );
    inflight.insert(
        3,
        InflightBatch {
            op_ids: Vec::new(),
            frame: Vec::new(),
            backoff: Backoff::canonical(),
            // Still queued: shorter than either, and not a deadline yet.
            due_at: Due::AfterSend(Duration::from_millis(10)),
        },
    );
    assert_eq!(
        next_deadline(&inflight, &deadlines),
        start + Duration::from_millis(50),
        "the soonest retransmit wins over the resync and its own sibling"
    );
}
