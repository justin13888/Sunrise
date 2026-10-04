//! End-to-end sync propagation against its budget (#366):
//! `docs/10-cross-cutting/performance-budgets.md` §Sync propagation.
//!
//! [`sync_propagation_meets_the_p99_budget`] is the measurement. It is
//! `#[ignore]`d because it commits ten thousand ops at each of three RTTs, ten
//! a second, which takes about seventeen minutes; the nightly `Sync latency`
//! job in `ci.yml` runs it in a release build:
//!
//! ```text
//! cargo test --release -p sunrise-e2e --test sync_latency -- --ignored --nocapture
//! ```
//!
//! `SUNRISE_SYNC_LATENCY_OPS`, `SUNRISE_SYNC_LATENCY_INTERVAL_MS` and
//! `SUNRISE_SYNC_LATENCY_RTTS_MS` narrow a local run; `SUNRISE_FUZZ_SEED` sets
//! the links' jitter seed, as it does for every chaos scenario; `RUST_LOG`
//! turns on the logs.
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

/// Ten commits a second, sustained: faster than anyone types, and the rate a
/// bulk edit reaches.
///
/// Not faster, because faster does not measure propagation. At 40 a second
/// and 20 ms RTT, or 10 a second and 80 ms, the author's sync driver stops
/// getting its acks: its `select!` is biased with the inbound frame last, the
/// sends block for the RTT, and the 100 ms retransmit deadline fires before
/// the ack is read, which sends again and blocks again, until the retry
/// policy gives up and the session is torn down. That collapse is the
/// driver's, tracked on its own; here it is what an 80 ms leg reports as
/// `missing`.
const INTERVAL: Duration = Duration::from_millis(100);

/// `SUNRISE_SYNC_LATENCY_INTERVAL_MS` overrides the commit spacing.
fn interval_from_env() -> Duration {
    std::env::var("SUNRISE_SYNC_LATENCY_INTERVAL_MS").map_or(INTERVAL, |raw| {
        Duration::from_millis(
            raw.trim()
                .parse()
                .expect("SUNRISE_SYNC_LATENCY_INTERVAL_MS is milliseconds"),
        )
    })
}

fn ops_from_env() -> usize {
    std::env::var("SUNRISE_SYNC_LATENCY_OPS").map_or(DEFAULT_OPS, |raw| {
        raw.trim()
            .parse()
            .expect("SUNRISE_SYNC_LATENCY_OPS is a positive integer")
    })
}

/// `SUNRISE_SYNC_LATENCY_RTTS_MS`, comma-separated, narrows a local run to
/// the RTTs it is about; CI leaves it unset.
fn rtts_from_env() -> Vec<u64> {
    std::env::var("SUNRISE_SYNC_LATENCY_RTTS_MS").map_or_else(
        |_| RTTS_MS.to_vec(),
        |raw| {
            raw.split(',')
                .map(|s| {
                    s.trim()
                        .parse()
                        .expect("SUNRISE_SYNC_LATENCY_RTTS_MS is a list of milliseconds")
                })
                .collect()
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "minutes long; the nightly Sync latency job runs it in release"]
async fn sync_propagation_meets_the_p99_budget() {
    // `RUST_LOG` turns on the cores' and the relay's logs, which is where a
    // stalled run says why.
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    }
    let seed = seed_from_env().unwrap_or(DEFAULT_FUZZ_SEED);
    sunrise_test_seed::announce("sync-latency", seed);
    let (ops, interval) = (ops_from_env(), interval_from_env());

    // The legs run side by side, each with its own relay and its own pair of
    // devices: at ten commits a second a leg is mostly waiting, and in
    // sequence the three would take the better part of an hour.
    let legs: Vec<_> = rtts_from_env()
        .into_iter()
        .map(|rtt| {
            tokio::spawn(measure(LatencyRun {
                rtt: Duration::from_millis(rtt),
                ops,
                interval,
                seed,
            }))
        })
        .collect();

    let mut failures = Vec::new();
    for leg in legs {
        let report = leg.await.expect("a latency leg panicked");
        println!("{}", report.summary());
        if report.missing > 0 {
            println!("  stalled: {}", report.diagnosis);
        }
        if !report.in_budget_conditions() {
            continue;
        }
        let rtt = report.run.rtt.as_millis();
        let p99 = report.percentile(99.0);
        if report.missing > 0 {
            failures.push(format!(
                "rtt={rtt}ms: {} ops never reached B",
                report.missing
            ));
        } else if p99 >= P99_BUDGET {
            failures.push(format!("rtt={rtt}ms: p99={p99:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "outside the {P99_BUDGET:?} p99 budget: {}",
        failures.join("; ")
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
    assert_eq!(report.missing, 0, "{}", report.diagnosis);
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
