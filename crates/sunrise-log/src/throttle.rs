//! Token-bucket throttle per `(ev, lv)`.
//!
//! Per `docs/10-cross-cutting/logging.md` §9: 100 tokens, refill 10/sec.
//! When the bucket is empty, records are dropped and a single
//! `log.throttled` record is emitted at most once per minute carrying
//! `n_dropped`.
//!
//! `std::time::Instant::now()` is sanctioned inside this module: token
//! buckets need a monotonic wall-clock independent of the core's injected
//! `Clock`. See the parallel allowance in `init.rs`.

#![allow(clippy::disallowed_methods)]

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::level::Level;

const CAPACITY: u32 = 100;
const REFILL_PER_SEC: u32 = 10;
const THROTTLE_NOTICE_PERIOD: Duration = Duration::from_secs(60);

/// Result of asking the throttle whether a record may be emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Record may be emitted.
    Allow,
    /// Record was dropped. If `notify_with` is `Some(n)` the caller should
    /// emit a single `log.throttled` record with `n_dropped = n`.
    Drop {
        /// Whether the caller should now emit a `log.throttled` notice.
        notify_with: Option<u64>,
    },
}

/// Thread-safe throttle keyed by `(ev, lv)`.
#[derive(Debug, Default)]
pub struct Throttle {
    inner: Mutex<HashMap<(&'static str, Level), Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens available, integer arithmetic only (we refill per millisecond
    /// in fractional-token form via `tokens_milli` to avoid fp drift).
    tokens_milli: u64,
    /// Last refill snapshot.
    last: Instant,
    /// Number of records dropped since the last `log.throttled` notice.
    dropped_since_notice: u64,
    /// Time the last notice was emitted (for the once-per-minute limiter).
    last_notice: Option<Instant>,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens_milli: u64::from(CAPACITY) * 1000,
            last: now,
            dropped_since_notice: 0,
            last_notice: None,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last);
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if elapsed_ms == 0 {
            return;
        }
        // tokens per ms = REFILL_PER_SEC / 1000 → in milli-tokens that's
        // REFILL_PER_SEC per ms.
        let new_milli = elapsed_ms.saturating_mul(u64::from(REFILL_PER_SEC));
        self.tokens_milli = self
            .tokens_milli
            .saturating_add(new_milli)
            .min(u64::from(CAPACITY) * 1000);
        self.last = now;
    }
}

impl Throttle {
    /// Construct an empty throttle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask whether a record at `(ev, lv)` may be emitted.
    ///
    /// `Instant::now()` is sanctioned inside `sunrise-log` (only) — token
    /// buckets need a monotonic clock independent of the core's injected
    /// `Clock`. See `init.rs::rfc3339_now_ms` for the parallel rationale.
    #[allow(clippy::disallowed_methods)]
    pub fn check(&self, ev: &'static str, lv: Level) -> Verdict {
        self.check_at(ev, lv, Instant::now())
    }

    /// Test seam: same as [`check`] but with an injected `now`.
    pub fn check_at(&self, ev: &'static str, lv: Level, now: Instant) -> Verdict {
        let mut map = self.inner.lock();
        let bucket = map.entry((ev, lv)).or_insert_with(|| Bucket::new(now));
        bucket.refill(now);
        if bucket.tokens_milli >= 1000 {
            bucket.tokens_milli -= 1000;
            return Verdict::Allow;
        }
        // Out of tokens: drop and consider notifying.
        bucket.dropped_since_notice = bucket.dropped_since_notice.saturating_add(1);
        let should_notify = bucket
            .last_notice
            .is_none_or(|t| now.saturating_duration_since(t) >= THROTTLE_NOTICE_PERIOD);
        if should_notify {
            bucket.last_notice = Some(now);
            let n = bucket.dropped_since_notice;
            bucket.dropped_since_notice = 0;
            Verdict::Drop {
                notify_with: Some(n),
            }
        } else {
            Verdict::Drop { notify_with: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_burst_allows_capacity() {
        let t = Throttle::new();
        let now = Instant::now();
        for _ in 0..CAPACITY {
            assert_eq!(t.check_at("x.y", Level::Info, now), Verdict::Allow);
        }
        // 101st in the same instant must drop.
        let v = t.check_at("x.y", Level::Info, now);
        assert!(matches!(
            v,
            Verdict::Drop {
                notify_with: Some(_)
            }
        ));
    }

    #[test]
    fn throttle_notice_at_most_once_per_minute() {
        let t = Throttle::new();
        let now = Instant::now();
        for _ in 0..CAPACITY {
            assert_eq!(t.check_at("x.y", Level::Info, now), Verdict::Allow);
        }
        // First drop produces a notice.
        let v1 = t.check_at("x.y", Level::Info, now);
        assert!(matches!(
            v1,
            Verdict::Drop {
                notify_with: Some(_)
            }
        ));
        // Subsequent drops at the exact same instant: no notice (same minute,
        // and bucket has not refilled because dt = 0).
        for _ in 0..50 {
            let v = t.check_at("x.y", Level::Info, now);
            assert!(
                matches!(v, Verdict::Drop { notify_with: None }),
                "got {v:?}"
            );
        }
        // After 60s: bucket has fully refilled, so this checks the *notice*
        // boundary by jumping 60s+ but we burn the new tokens first.
        let later = now + Duration::from_secs(61);
        for _ in 0..CAPACITY {
            assert_eq!(t.check_at("x.y", Level::Info, later), Verdict::Allow);
        }
        let v = t.check_at("x.y", Level::Info, later);
        assert!(
            matches!(
                v,
                Verdict::Drop {
                    notify_with: Some(_)
                }
            ),
            "expected notice after 61s, got {v:?}"
        );
    }

    #[test]
    fn refill_works_over_time() {
        let t = Throttle::new();
        let mut now = Instant::now();
        for _ in 0..CAPACITY {
            assert_eq!(t.check_at("x.y", Level::Info, now), Verdict::Allow);
        }
        // After 1s the bucket has refilled REFILL_PER_SEC tokens.
        now += Duration::from_secs(1);
        for _ in 0..REFILL_PER_SEC {
            assert_eq!(t.check_at("x.y", Level::Info, now), Verdict::Allow);
        }
        // 11th in the same instant: drop.
        assert!(matches!(
            t.check_at("x.y", Level::Info, now),
            Verdict::Drop { .. }
        ));
    }

    #[test]
    fn keys_are_independent() {
        let t = Throttle::new();
        let now = Instant::now();
        for _ in 0..CAPACITY {
            assert_eq!(t.check_at("a.b", Level::Info, now), Verdict::Allow);
        }
        // Other key still has full bucket.
        for _ in 0..CAPACITY {
            assert_eq!(t.check_at("c.d", Level::Info, now), Verdict::Allow);
        }
    }
}
