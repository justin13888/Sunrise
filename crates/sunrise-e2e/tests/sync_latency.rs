//! End-to-end sync propagation against its budget (#366):
//! `docs/10-cross-cutting/performance-budgets.md` §Sync propagation.
//!
//! [`sync_propagation_meets_the_p99_budget`] is the measurement. It is
//! `#[ignore]`d because it commits ten thousand ops at each of three RTTs and
//! takes minutes; the nightly `Sync latency` job in `ci.yml` runs it in a
//! release build:
//!
//! ```text
//! cargo test --release -p sunrise-e2e --test sync_latency -- --ignored --nocapture
//! ```
//!
//! `SUNRISE_SYNC_LATENCY_OPS` overrides the op count, and `SUNRISE_FUZZ_SEED`
//! the links' jitter seed, as it does for every chaos scenario.
//!
//! [`the_harness_measures_every_op`] runs on every pull request with a few
//! dozen ops, and asserts only that the harness itself works: a latency bound
//! on a shared runner, inside a parallel test binary, would fail on noise.

// The percentile line is this test's report, read off the CI log.
#![allow(clippy::doc_markdown, clippy::print_stdout)]

use std::time::Duration;

use sunrise_e2e::chaos::{seed_from_env, DEFAULT_FUZZ_SEED};
use sunrise_e2e::latency::{measure, LatencyRun, P99_BUDGET};

/// The RTTs the issue names. 0 and 20 ms are inside the budget's stated
/// conditions (at most 50 ms to the relay) and are held to it; 80 ms is
/// reported, to show where the tail goes past them.
const RTTS_MS: [u64; 3] = [0, 20, 80];

/// The budget asks for a p99 over at least this many ops.
const DEFAULT_OPS: usize = 10_000;

/// A hundred commits a second: a burst of typing, sustained.
const INTERVAL: Duration = Duration::from_millis(10);

fn ops_from_env() -> usize {
    std::env::var("SUNRISE_SYNC_LATENCY_OPS").map_or(DEFAULT_OPS, |raw| {
        raw.trim()
            .parse()
            .expect("SUNRISE_SYNC_LATENCY_OPS is a positive integer")
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "minutes long; the nightly Sync latency job runs it in release"]
async fn sync_propagation_meets_the_p99_budget() {
    let seed = seed_from_env().unwrap_or(DEFAULT_FUZZ_SEED);
    sunrise_test_seed::announce("sync-latency", seed);
    let ops = ops_from_env();

    let mut breaches = Vec::new();
    for rtt in RTTS_MS {
        let report = measure(LatencyRun {
            rtt: Duration::from_millis(rtt),
            ops,
            interval: INTERVAL,
            seed,
        })
        .await;
        println!("{}", report.summary());
        let p99 = report.percentile(99.0);
        if report.in_budget_conditions() && p99 >= P99_BUDGET {
            breaches.push(format!("rtt={rtt}ms p99={p99:?}"));
        }
    }
    assert!(
        breaches.is_empty(),
        "p99 at or above the {P99_BUDGET:?} budget: {}",
        breaches.join(", ")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_harness_measures_every_op() {
    let report = measure(LatencyRun {
        rtt: Duration::from_millis(20),
        ops: 40,
        interval: INTERVAL,
        seed: DEFAULT_FUZZ_SEED,
    })
    .await;
    println!("{}", report.summary());
    assert_eq!(report.sorted.len(), 40, "one sample per op");
    let (p50, p95, p99) = (
        report.percentile(50.0),
        report.percentile(95.0),
        report.percentile(99.0),
    );
    assert!(p50 <= p95 && p95 <= p99, "{}", report.summary());
    // Two links, each delaying by at least 40 % of the RTT one way: no op can
    // arrive faster than that, so a smaller p50 means the delay was not
    // injected at all.
    assert!(
        p50 >= Duration::from_millis(16),
        "the injected RTT is missing from the measurement: {}",
        report.summary()
    );
}
