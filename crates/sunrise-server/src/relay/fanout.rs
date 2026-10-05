//! The live fan-out's latency clock, `sunrise_sync_fanout_latency_seconds`.
//!
//! Its own module because it changes for its own reason: what counts as a
//! timed fan-out is a metrics question, and the ring and broadcast in
//! [`super`] only carry the clock and call its two halves.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

/// Times one published frame from the moment the relay accepted it to the
/// moment its last live subscriber handed it to that subscriber's stream, and
/// records that in `sunrise_sync_fanout_latency_seconds`.
///
/// Neither side knows on its own which subscriber is last. The publisher
/// learns how many live receivers there are only when `broadcast::send`
/// returns, and by then a fast subscriber may already have delivered. So the
/// count is signed and both halves move it: each subscriber subtracts one when
/// it settles, and [`RelayHub::publish`](super::RelayHub::publish) adds the
/// receiver count once the send returns. Before the publisher's add the count
/// is at most zero, so no subscriber can see it reach zero early; whichever
/// operation does bring it to zero is the last one, and that one records the
/// observation. Exactly one observation per fanned-out frame, whatever the
/// interleaving.
///
/// A frame no live subscriber received is not observed: there was no fan-out
/// to time. Nor is one only its author's own stream received: that stream
/// closes its copy by [`skip`](Self::skip)ping it, which closes the count
/// without delivering anything, so a batch no peer received is not recorded
/// as a near-instant fan-out. A subscriber that never settles — its client
/// left mid-delivery, or it lagged past the broadcast buffer and skipped the
/// frame — leaves the count above zero, and that frame is not observed
/// either. The histogram therefore covers completed fan-outs only; a stream
/// that drops is counted by the stream metrics, not here.
#[derive(Debug)]
pub struct FanoutClock {
    accepted: tokio::time::Instant,
    outstanding: AtomicI64,
    /// Whether any receiver delivered its copy rather than skipping it.
    delivered: AtomicBool,
    metrics: crate::Metrics,
}

impl FanoutClock {
    /// Start timing a frame the relay accepted at `accepted`.
    #[must_use]
    pub fn start(accepted: tokio::time::Instant, metrics: crate::Metrics) -> Arc<Self> {
        Arc::new(Self {
            accepted,
            outstanding: AtomicI64::new(0),
            delivered: AtomicBool::new(false),
            metrics,
        })
    }

    /// The publisher's half: the send reached `receivers` live subscribers,
    /// each of which will [`settle`](Self::settle) or [`skip`](Self::skip)
    /// once.
    pub(super) fn expect(&self, receivers: usize) {
        if receivers == 0 {
            return;
        }
        let n = i64::try_from(receivers).unwrap_or(i64::MAX);
        if self.outstanding.fetch_add(n, Ordering::AcqRel) + n == 0 {
            self.observe();
        }
    }

    /// A subscriber's half: it has handed the frame to its stream. Call once
    /// per received copy, or call [`skip`](Self::skip) instead.
    pub fn settle(&self) {
        // Before the decrement, so whichever operation brings the count to
        // zero sees it: that one acquires every earlier decrement's release.
        self.delivered.store(true, Ordering::Release);
        self.close_one();
    }

    /// A subscriber's half for a copy it did not deliver, because the frame
    /// is its own. It closes the count like [`settle`](Self::settle), but a
    /// fan-out every receiver skipped delivered nothing and is not observed.
    pub fn skip(&self) {
        self.close_one();
    }

    fn close_one(&self) {
        if self.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.observe();
        }
    }

    fn observe(&self) {
        if !self.delivered.load(Ordering::Acquire) {
            return;
        }
        self.metrics.observe(
            "sunrise_sync_fanout_latency_seconds",
            &[],
            crate::metrics::LATENCY_BUCKETS,
            self.accepted.elapsed().as_secs_f64(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::FanoutClock;

    fn observed(metrics: &crate::Metrics) -> u64 {
        metrics.histogram_count("sunrise_sync_fanout_latency_seconds", &[])
    }

    fn clock(metrics: &crate::Metrics) -> std::sync::Arc<FanoutClock> {
        FanoutClock::start(tokio::time::Instant::now(), metrics.clone())
    }

    /// The interleaving the signed count exists for: every subscriber settles
    /// before the publisher learns how many there were.
    #[test]
    fn fanout_settled_before_the_publisher_counts_is_still_observed_once() {
        let metrics = crate::Metrics::new();
        let clock = clock(&metrics);
        clock.settle();
        clock.settle();
        assert_eq!(observed(&metrics), 0, "the count is not known yet");
        clock.expect(2);
        assert_eq!(observed(&metrics), 1);
    }

    #[test]
    fn fanout_settled_half_before_and_half_after_is_observed_once() {
        let metrics = crate::Metrics::new();
        let clock = clock(&metrics);
        clock.settle();
        clock.expect(3);
        clock.settle();
        assert_eq!(observed(&metrics), 0);
        clock.settle();
        assert_eq!(observed(&metrics), 1);
    }

    /// The author's own stream skipping its copy closes the count, and the
    /// peer's delivery is what is observed.
    #[test]
    fn fanout_with_a_skipped_author_copy_is_observed_once_on_delivery() {
        let metrics = crate::Metrics::new();
        let clock = clock(&metrics);
        clock.expect(2);
        clock.skip();
        assert_eq!(observed(&metrics), 0, "the peer has not delivered yet");
        clock.settle();
        assert_eq!(observed(&metrics), 1);
    }

    /// Only the author's own stream received the batch: nothing was delivered,
    /// so there was no fan-out to time.
    #[test]
    fn fanout_every_receiver_skipped_is_not_observed() {
        let metrics = crate::Metrics::new();
        let clock = clock(&metrics);
        clock.skip();
        clock.expect(1);
        assert_eq!(observed(&metrics), 0);
    }
}
