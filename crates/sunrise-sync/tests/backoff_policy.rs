//! The reconnect policy, pinned to the numbers the documentation publishes.
//!
//! `Backoff` is the piece of `sunrise-sync` a driver most depends on and least
//! controls: `sunrise-core::sync_driver` asks it for a delay, sleeps that long,
//! and asks again, and the only thing standing between a flaky link and a hot
//! reconnect loop against someone's relay is the schedule this type produces.
//!
//! The unit tests in `src/backoff.rs` assert the *shape* of that schedule —
//! delays do not shrink, they stay under the cap, jitter lands in a band. None
//! of them asserts a value. A `next_delay` that returned a flat 100 ms forever
//! satisfies every one of them, which is the class of miss mutation testing
//! already found here (#35 cites it). Everything below asserts the schedule
//! itself:
//!
//! ```text
//! initial_delay_ms = 100      max_retries = 5
//! max_delay_ms     = 30000    jitter       = x[0.8, 1.2]
//! ```
//!
//! `docs/05-sync/transports.md` §Reconnect on failure and
//! `docs/05-sync/offline-queue.md` §Backoff — which owns the numbers — spell
//! out what that yields: "five jittered delays of 100, 200, 400, 800 and
//! 1600 ms, after which `next_delay` returns `None`".

use std::time::Duration;

use sunrise_sync::Backoff;

/// The un-jittered schedule, from the documents that own it.
const BASE_MS: [u64; 5] = [100, 200, 400, 800, 1600];

/// A jitter unit that lands exactly on the base delay: 0.8 + 0.4 x 0.5 = 1.0.
const NO_JITTER: f64 = 0.5;

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).expect("a backoff delay fits in u64 ms")
}

/// Walk the whole policy, recording what a driver would actually sleep.
fn schedule(mut b: Backoff, jitter_unit: f64) -> Vec<u64> {
    let mut out = Vec::new();
    while let Some(d) = b.next_delay(jitter_unit) {
        out.push(ms(d));
        b.record_attempt();
        assert!(out.len() <= 16, "the policy must terminate");
    }
    out
}

/// The schedule, exactly. Five delays, doubling from 100 ms, and then nothing.
#[test]
fn the_canonical_policy_is_five_delays_doubling_from_one_hundred_milliseconds() {
    assert_eq!(schedule(Backoff::canonical(), NO_JITTER), BASE_MS.to_vec());
}

/// `Default` is the canonical policy, not a second one that happens to agree
/// today. `sync_driver` constructs both ways.
#[test]
fn the_default_policy_is_the_canonical_one() {
    assert_eq!(
        schedule(Backoff::default(), NO_JITTER),
        schedule(Backoff::canonical(), NO_JITTER)
    );
}

/// Jitter is +/-20% of each base delay, at both ends of the unit interval.
///
/// The bound is exact rather than a range, because the two endpoints are the
/// whole of what "+/-20%" means and a test that accepted a range would accept a
/// jitter that had quietly become +/-2%.
#[test]
fn every_delay_is_jittered_by_exactly_twenty_percent_at_the_ends_of_the_interval() {
    let lowest = schedule(Backoff::canonical(), 0.0);
    let highest = schedule(Backoff::canonical(), 1.0);

    let expected_low: Vec<u64> = BASE_MS.iter().map(|b| b * 8 / 10).collect();
    let expected_high: Vec<u64> = BASE_MS.iter().map(|b| b * 12 / 10).collect();

    assert_eq!(lowest, expected_low, "jitter_unit 0.0 must give base x 0.8");
    assert_eq!(
        highest, expected_high,
        "jitter_unit 1.0 must give base x 1.2"
    );
}

/// A jitter unit outside `[0, 1]` is clamped rather than extrapolated.
///
/// The doc comment asks for a value in `[0, 1]`; the caller supplies it, and a
/// caller that supplies -1 or 2 must not be able to produce a negative delay
/// or one 60% over base. This is the guard on the boundary of the contract,
/// and nothing tested it.
#[test]
fn a_jitter_unit_outside_the_unit_interval_is_clamped() {
    let b = Backoff::canonical();
    assert_eq!(ms(b.next_delay(-5.0).expect("attempt 0")), 80);
    assert_eq!(ms(b.next_delay(5.0).expect("attempt 0")), 120);
}

/// `next_delay` reports; `record_attempt` advances. A driver reads the delay
/// before deciding to sleep, and reading it must not cost an attempt — a
/// `next_delay` that advanced the counter would burn the policy in a single
/// pass through a `select!` arm.
#[test]
fn next_delay_does_not_consume_an_attempt() {
    let mut b = Backoff::canonical();
    for _ in 0..8 {
        assert_eq!(ms(b.next_delay(NO_JITTER).expect("attempt 0")), 100);
        assert_eq!(b.attempt(), 0);
        assert!(!b.exhausted());
    }
    b.record_attempt();
    assert_eq!(b.attempt(), 1);
    assert_eq!(ms(b.next_delay(NO_JITTER).expect("attempt 1")), 200);
}

/// Exhaustion is a state a driver has to be able to see and leave.
///
/// `sync_driver` cycles: on the sixth call it gets `None`, sleeps a flat 30 s
/// and calls `reset`, and the sequence must then start again at 100 ms rather
/// than resuming where it stopped. That is what makes a client that cannot
/// reach its relay retry forever in bursts of five instead of giving up.
#[test]
fn an_exhausted_policy_stays_exhausted_until_reset_and_then_restarts() {
    let mut b = Backoff::canonical();
    for _ in 0..5 {
        assert!(b.next_delay(NO_JITTER).is_some());
        b.record_attempt();
    }
    assert!(b.exhausted());
    assert!(b.next_delay(NO_JITTER).is_none());
    // Still none however many times it is asked, and asking does not somehow
    // wrap the counter back into range.
    assert!(b.next_delay(0.0).is_none());
    assert!(b.next_delay(1.0).is_none());
    assert!(b.exhausted());

    b.reset();
    assert_eq!(b.attempt(), 0);
    assert!(!b.exhausted());
    assert_eq!(ms(b.next_delay(NO_JITTER).expect("reset restarts")), 100);
    assert_eq!(schedule(b, NO_JITTER), BASE_MS.to_vec());
}

/// The 30 s cap in `next_delay` never binds on the canonical policy, and this
/// says so rather than leaving a reader to work it out.
///
/// `offline-queue.md` §Backoff calls this out because it looks like a bug and
/// is not: with `max_retries = 5` the largest base is 1600 ms, so the only
/// 30 s that ever elapses is the flat sleep the driver does on the exhausted
/// branch. Pinned here so that raising `max_retries` without also thinking
/// about the cap is a test failure and not a silent change to how long a
/// client waits — at 9 retries the cap starts binding and the burst runs to
/// nearly a minute.
#[test]
fn the_thirty_second_cap_is_unreachable_under_the_canonical_policy() {
    let longest = schedule(Backoff::canonical(), 1.0)
        .into_iter()
        .max()
        .expect("the policy yields at least one delay");
    assert_eq!(longest, 1_920, "1600 ms base, jittered to its maximum");
    assert!(
        longest < 30_000,
        "the documented cap is unreachable; see offline-queue.md §Backoff"
    );
}

/// A whole burst costs about three seconds, whatever the jitter does.
///
/// The number a support answer is made of: "it retries five times over roughly
/// three seconds, then waits thirty and does it again". Bounded on both sides
/// so neither a longer nor a shorter schedule slips through.
#[test]
fn a_full_burst_costs_between_two_and_four_seconds() {
    let shortest: u64 = schedule(Backoff::canonical(), 0.0).iter().sum();
    let longest: u64 = schedule(Backoff::canonical(), 1.0).iter().sum();
    assert_eq!(shortest, 2_480);
    assert_eq!(longest, 3_720);
}
