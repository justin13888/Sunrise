//! The sync-propagation harness: how long an op takes from one device's
//! commit to another device's apply, through the real relay (#366).
//!
//! `docs/10-cross-cutting/performance-budgets.md` §Sync propagation is the
//! budget this measures: **p99 < 500 ms** for a peer that is online and
//! subscribed, over a relay at most 50 ms RTT from each device.
//!
//! # What one run does
//!
//! A relay and two paired [`Core`](sunrise_core::Core)s on loopback, each reaching the relay
//! through a [`Toxic`](crate::chaos::Toxic) link built from
//! [`ToxicConfig::round_trip`]: every frame is delayed by half the stated RTT,
//! ±20 %, in each direction, drawn from a seeded RNG. Device A commits
//! [`LatencyRun::ops`] tasks, one every [`LatencyRun::interval`]. Each sample
//! is the time from A's `submit` returning — the local commit is durable — to
//! B publishing that task's `Created` on its change feed, which is what a UI
//! repaints from. Both ends are read from one monotonic clock in this process,
//! so there is no cross-device clock skew to correct for.
//!
//! The op count is fixed and every op must arrive: a missing op fails the run
//! rather than shrinking the sample, because a latency distribution over the
//! ops that happened to arrive says nothing about the ones that did not.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sunrise_core::{Clock, Command, DomainEvent, SystemClock};
use sunrise_domain::TaskDraft;
use sunrise_id::EntityRef;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::chaos::ToxicConfig;
use crate::{
    open_core_with_factory, open_paired_core_with_factory, spawn_relay, toxic_ws_factory,
    wait_live, wait_pending_zero,
};

/// The end-to-end budget: p99 strictly below this.
pub const P99_BUDGET: Duration = Duration::from_millis(500);

/// The largest RTT the budget is stated for. A run above it is reported and
/// not held to [`P99_BUDGET`].
pub const BUDGET_MAX_RTT: Duration = Duration::from_millis(50);

/// One measurement's parameters.
#[derive(Debug, Clone, Copy)]
pub struct LatencyRun {
    /// The round trip each device's link to the relay averages.
    pub rtt: Duration,
    /// How many ops A commits. Every one must reach B.
    pub ops: usize,
    /// The spacing between A's commits.
    pub interval: Duration,
    /// Seeds both links' jitter.
    pub seed: u64,
}

/// What a run measured.
#[derive(Debug, Clone)]
pub struct LatencyReport {
    /// The parameters it ran with.
    pub run: LatencyRun,
    /// Every op's commit-to-apply latency, ascending.
    pub sorted: Vec<Duration>,
}

impl LatencyReport {
    /// Build a report from unordered samples.
    #[must_use]
    pub fn new(run: LatencyRun, mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self {
            run,
            sorted: samples,
        }
    }

    /// The nearest-rank `p`th percentile, `p` in `(0, 100]`.
    #[must_use]
    pub fn percentile(&self, p: f64) -> Duration {
        percentile(&self.sorted, p)
    }

    /// Whether this run is one the budget speaks for.
    #[must_use]
    pub fn in_budget_conditions(&self) -> bool {
        self.run.rtt <= BUDGET_MAX_RTT
    }

    /// One line for a CI log.
    #[must_use]
    pub fn summary(&self) -> String {
        let ms = |d: Duration| d.as_secs_f64() * 1_000.0;
        format!(
            "sync-latency: rtt={}ms ops={} interval={}ms seed={} p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
            self.run.rtt.as_millis(),
            self.sorted.len(),
            self.run.interval.as_millis(),
            self.run.seed,
            ms(self.percentile(50.0)),
            ms(self.percentile(95.0)),
            ms(self.percentile(99.0)),
            ms(self.sorted.last().copied().unwrap_or_default()),
        )
    }
}

/// Nearest-rank percentile of an ascending slice; zero for an empty one.
///
/// Nearest-rank rather than interpolated, so the value reported is a latency
/// some op actually had. The same definition `sunrise-bench`'s `baseline`
/// uses.
#[must_use]
pub fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Generous for a healthy run, which settles in a second or two: it bounds a
/// broken one rather than timing a working one.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long after A's last commit B has to apply the last op.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(60);

/// Shared vault root of the paired devices.
const ROOT: [u8; 32] = [0x66; 32];

/// Run one measurement. Panics if setup fails or any op does not reach B.
pub async fn measure(run: LatencyRun) -> LatencyReport {
    let (addr, relay) = spawn_relay().await;
    let dir_a = tempfile::tempdir().expect("tempdir");
    let dir_b = tempfile::tempdir().expect("tempdir");
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let link = ToxicConfig::round_trip(run.rtt);
    // Distinct streams per link, both derived from the run's seed.
    let (factory_a, _) = toxic_ws_factory(addr, link, run.seed);
    let (factory_b, _) = toxic_ws_factory(addr, link, run.seed ^ 0x5eed_b00b);

    let a = open_core_with_factory(dir_a.path(), ROOT, addr, clock.clone(), factory_a).await;
    let b = open_paired_core_with_factory(dir_b.path(), &a, addr, clock, factory_b).await;
    wait_live(&a, SETUP_TIMEOUT).await;
    wait_live(&b, SETUP_TIMEOUT).await;
    // Whatever pairing queued is gone before the clock starts.
    wait_pending_zero(&a, SETUP_TIMEOUT).await;
    wait_pending_zero(&b, SETUP_TIMEOUT).await;

    // B's side: every `Created`, stamped as it is read. Subscribed before A
    // commits anything, so no op can be applied before it is watched.
    let seen: Arc<Mutex<HashMap<EntityRef, Instant>>> = Arc::default();
    let lagged = Arc::new(AtomicU64::new(0));
    let mut changes = b.changes();
    let watcher = tokio::spawn({
        let seen = Arc::clone(&seen);
        let lagged = Arc::clone(&lagged);
        async move {
            loop {
                match changes.recv().await {
                    Ok(DomainEvent::Created(r)) => {
                        let now = Instant::now();
                        lock(&seen).entry(r).or_insert(now);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        lagged.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        }
    });

    // A's side: a steady rate, each commit stamped when `submit` returns.
    let mut committed: Vec<(EntityRef, Instant)> = Vec::with_capacity(run.ops);
    let mut tick = tokio::time::interval(run.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    for i in 0..run.ops {
        tick.tick().await;
        let res = a
            .submit(Command::CreateTask(TaskDraft {
                title: format!("latency {i}"),
                ..Default::default()
            }))
            .await
            .expect("commit on A");
        committed.push((res.entity, Instant::now()));
    }

    // Wait until B has published every op A committed, or the deadline.
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    let missing = loop {
        let missing = {
            let seen = lock(&seen);
            committed
                .iter()
                .filter(|(r, _)| !seen.contains_key(r))
                .count()
        };
        if missing == 0 || Instant::now() >= deadline {
            break missing;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    watcher.abort();
    a.shutdown().await;
    b.shutdown().await;
    relay.abort();

    // A lagged feed lost events, so some op's stamp may be a later event's,
    // or absent. Neither is a measurement.
    assert_eq!(
        lagged.load(Ordering::Relaxed),
        0,
        "B's change feed lagged; samples were lost"
    );
    assert_eq!(
        missing,
        0,
        "{missing} of {} ops had not reached B {DRAIN_TIMEOUT:?} after A's last commit at rtt={:?}",
        committed.len(),
        run.rtt
    );
    let seen = lock(&seen);
    let samples = committed
        .iter()
        .map(|(r, at)| seen[r].saturating_duration_since(*at))
        .collect();
    LatencyReport::new(run, samples)
}

/// The watcher never panics while holding the lock, so a poisoned lock still
/// holds a consistent map.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn nearest_rank_percentiles() {
        let sorted: Vec<Duration> = (1..=100).map(ms).collect();
        assert_eq!(percentile(&sorted, 50.0), ms(50));
        assert_eq!(percentile(&sorted, 95.0), ms(95));
        assert_eq!(percentile(&sorted, 99.0), ms(99));
        assert_eq!(percentile(&sorted, 100.0), ms(100));
        assert_eq!(percentile(&[], 99.0), Duration::ZERO);
        assert_eq!(percentile(&[ms(7)], 99.0), ms(7));
    }

    #[test]
    fn a_report_sorts_its_samples() {
        let run = LatencyRun {
            rtt: ms(20),
            ops: 3,
            interval: ms(10),
            seed: 1,
        };
        let r = LatencyReport::new(run, vec![ms(30), ms(10), ms(20)]);
        assert_eq!(r.sorted, vec![ms(10), ms(20), ms(30)]);
        assert_eq!(r.percentile(99.0), ms(30));
        assert!(r.in_budget_conditions());
        assert!(r.summary().contains("p99=30.0ms"), "{}", r.summary());
    }

    #[test]
    fn eighty_ms_is_outside_the_budget_conditions() {
        let run = LatencyRun {
            rtt: ms(80),
            ops: 0,
            interval: ms(10),
            seed: 1,
        };
        assert!(!LatencyReport::new(run, vec![]).in_budget_conditions());
    }
}
