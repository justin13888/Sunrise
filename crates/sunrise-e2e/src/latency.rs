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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sunrise_core::{Clock, Command, DomainEvent, SystemClock};
use sunrise_domain::TaskDraft;
use sunrise_id::EntityRef;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::chaos::ToxicConfig;
use crate::{
    canonical_tasks, open_core_with_factory, open_paired_core_with_factory, spawn_relay_with,
    toxic_ws_factory, wait_live, wait_pending_zero,
};
use sunrise_server::ServerConfig;

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
    /// How many of those are upper bounds rather than exact: ops whose
    /// `Created` B's change feed dropped under a burst, stamped instead by the
    /// snapshot that found them applied.
    pub bounded: usize,
}

impl LatencyReport {
    /// Build a report from unordered samples, all exact.
    #[must_use]
    pub fn new(run: LatencyRun, mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self {
            run,
            sorted: samples,
            bounded: 0,
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
            "sync-latency: rtt={}ms ops={} bounded={} interval={}ms seed={} p50={:.1}ms p95={:.1}ms p99={:.1}ms max={:.1}ms",
            self.run.rtt.as_millis(),
            self.sorted.len(),
            self.bounded,
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

/// The relay the harness measures: the default configuration, with one limit
/// raised.
///
/// `sync_per_min` counts the sync routes per client *address*, and both
/// devices here share `127.0.0.1`. Two phones on a real network do not, so
/// the default of 600 a minute would throttle a pair that, deployed, never
/// shares one bucket. Every per-device limit — the 50 ops a second
/// `ops_per_sec` allows each device among them — stays as shipped, and
/// [`LatencyRun::interval`] has to keep A under it.
fn relay_config() -> ServerConfig {
    let mut config = ServerConfig::default();
    config.limits.sync_per_min = 1_000_000;
    config
}

/// Run one measurement. Panics if setup fails or any op does not reach B.
pub async fn measure(run: LatencyRun) -> LatencyReport {
    let (addr, relay) = spawn_relay_with(relay_config(), |s| s).await;
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
    //
    // The feed is a bounded broadcast, and a burst — a reconnect's replay
    // applying hundreds of ops at once — can overrun it. The events it drops
    // were for ops already applied, so a snapshot of B's tasks taken right
    // after the drop holds every one of them, and the snapshot's time is an
    // upper bound on when each was applied. Those samples are kept, counted
    // in `bounded`, and can only make the reported tail worse, never better.
    let watched = Arc::new(Mutex::new(Watched::default()));
    let mut changes = b.changes();
    let watcher = tokio::spawn({
        let watched = Arc::clone(&watched);
        let b = Arc::clone(&b);
        async move {
            loop {
                match changes.recv().await {
                    Ok(DomainEvent::Created(r)) => {
                        let now = Instant::now();
                        lock(&watched).seen.entry(r).or_insert(now);
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => {
                        let present: HashSet<String> = canonical_tasks(&b)
                            .await
                            .into_iter()
                            .map(|t| t.id)
                            .collect();
                        let now = Instant::now();
                        lock(&watched).snapshots.push((now, present));
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

    // Wait until B has applied every op A committed, or the deadline.
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    let missing = loop {
        let missing = {
            let w = lock(&watched);
            committed
                .iter()
                .filter(|(r, _)| w.applied(*r).is_none())
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

    assert_eq!(
        missing,
        0,
        "{missing} of {} ops had not reached B {DRAIN_TIMEOUT:?} after A's last commit at rtt={:?}",
        committed.len(),
        run.rtt
    );
    let w = lock(&watched);
    let mut bounded = 0;
    let samples = committed
        .iter()
        .map(|(r, at)| {
            let (applied, exact) = w.applied(*r).expect("every op was applied");
            bounded += usize::from(!exact);
            applied.saturating_duration_since(*at)
        })
        .collect();
    LatencyReport {
        bounded,
        ..LatencyReport::new(run, samples)
    }
}

/// What B's watcher has recorded.
#[derive(Debug, Default)]
struct Watched {
    /// Each task's `Created`, stamped when the feed delivered it.
    seen: HashMap<EntityRef, Instant>,
    /// After each overrun of the feed: when, and every task id B held then.
    snapshots: Vec<(Instant, HashSet<String>)>,
}

impl Watched {
    /// When `r` was applied on B, and whether that time is exact. A task the
    /// feed delivered is exact; one only a snapshot found is bounded above by
    /// the first snapshot that held it.
    fn applied(&self, r: EntityRef) -> Option<(Instant, bool)> {
        if let Some(at) = self.seen.get(&r) {
            return Some((*at, true));
        }
        let id = r.to_str();
        self.snapshots
            .iter()
            .find(|(_, present)| present.contains(id.as_str()))
            .map(|(at, _)| (*at, false))
    }
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

    /// The feed's stamp wins; a dropped event falls back to the first
    /// snapshot that held the task, marked inexact; neither means unapplied.
    #[test]
    fn an_op_is_stamped_by_the_feed_or_bounded_by_a_snapshot() {
        let r = |b: u8| EntityRef::new(sunrise_id::EntityKind::Task, [b; 16]);
        let t0 = Instant::now();
        let mut w = Watched::default();
        w.seen.insert(r(1), t0);
        w.snapshots
            .push((t0 + ms(5), [r(1).to_str(), r(2).to_str()].into()));
        w.snapshots.push((t0 + ms(9), [r(2).to_str()].into()));

        assert_eq!(w.applied(r(1)), Some((t0, true)));
        assert_eq!(w.applied(r(2)), Some((t0 + ms(5), false)));
        assert_eq!(w.applied(r(3)), None);
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
