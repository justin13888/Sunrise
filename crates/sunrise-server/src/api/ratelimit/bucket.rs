//! The rate algorithm: GCRA over one key, as a pure function of time.
//!
//! The generic cell rate algorithm is a token bucket that stores one number
//! instead of two. A key's state is its *theoretical arrival time* — the
//! instant its bucket would be full again if nothing else arrived — and a
//! request is admitted when that instant, pushed forward by the request's
//! cost, lies no further ahead of now than one full bucket. Nothing refills on
//! a timer, so a key nobody touches costs nothing and an idle one is
//! indistinguishable from a fresh one.
//!
//! Everything here is integer arithmetic on an injected `now_ms`, so a test
//! states a burst, a refill and a `Retry-After` as values and never sleeps.
//!
//! # Costs larger than the bucket
//!
//! A weighted request — a 40-op batch, a 1 MiB chunk, a 100 MB fetch — can
//! cost more than the bucket holds. Refusing it would refuse it forever, and a
//! client cannot make a batch smaller after the fact. Such a request is
//! admitted only from a *full* bucket and then leaves the key in debt: the
//! whole cost is charged, so the next request waits for the debt to refill and
//! the average rate still holds. A request is therefore checked against
//! `min(cost, limit)` and charged `cost`.

/// A rate: `limit` units per `period_ms`, with a burst of `limit`.
///
/// One quota is both the sustained rate and the burst. A separate burst knob
/// would be a second number for an operator to get wrong, and "the whole
/// window's allowance at once" is the burst every row of the policy wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// Units admitted per period, and the most admitted at once.
    pub limit: u64,
    /// The period, in milliseconds.
    pub period_ms: u64,
}

impl Quota {
    /// `limit` per minute.
    #[must_use]
    pub const fn per_minute(limit: u64) -> Self {
        Self {
            limit,
            period_ms: 60_000,
        }
    }

    /// `limit` per second.
    #[must_use]
    pub const fn per_second(limit: u64) -> Self {
        Self {
            limit,
            period_ms: 1_000,
        }
    }

    /// `limit` per `secs` seconds.
    #[must_use]
    pub const fn per_secs(limit: u64, secs: u64) -> Self {
        Self {
            limit,
            period_ms: secs * 1_000,
        }
    }

    /// The scaled clock: one tick is `1 / limit` milliseconds, so one unit of
    /// cost is exactly `period_ms` ticks and no division is ever inexact.
    fn ticks(self, ms: u64) -> u128 {
        u128::from(ms) * u128::from(self.limit.max(1))
    }

    /// One unit of cost, in ticks.
    fn interval(self) -> u128 {
        u128::from(self.period_ms.max(1))
    }

    /// A whole bucket, in ticks.
    fn burst(self) -> u128 {
        u128::from(self.limit.max(1)) * self.interval()
    }

    /// Ticks back to whole milliseconds, rounding up so a `Retry-After` is
    /// never early.
    fn ms_ceil(self, ticks: u128) -> u64 {
        let limit = u128::from(self.limit.max(1));
        u64::try_from(ticks.div_ceil(limit)).unwrap_or(u64::MAX)
    }
}

/// One key's state.
///
/// `Copy`, so a store can read it, compute, and write it back under one lock
/// without the algorithm ever seeing the store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cell {
    /// Theoretical arrival time, in the quota's scaled ticks.
    tat: u128,
    /// When the bucket is full again, in milliseconds. A cell past this is the
    /// same as no cell, which is what lets a store forget it.
    idle_at_ms: u64,
    /// Whether the last decision on this key was a refusal. Read to report
    /// only the first refusal of a run, so a flood is one log line rather than
    /// one per request.
    refusing: bool,
}

impl Cell {
    /// Whether this cell carries no state worth keeping at `now_ms`.
    #[must_use]
    pub const fn is_idle(&self, now_ms: u64) -> bool {
        self.idle_at_ms <= now_ms
    }
}

/// What a decision reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Whether the request may continue.
    pub allowed: bool,
    /// How long until a refused request would be admitted, in milliseconds.
    /// Zero when allowed.
    pub retry_after_ms: u64,
    /// Whole units left in the bucket after this decision.
    pub remaining: u64,
    /// A refusal that follows an admission: the start of a run of refusals on
    /// this key. Always false when allowed.
    pub first_refusal: bool,
}

impl Outcome {
    /// An admission that consulted nothing — a disabled limit, or a key the
    /// store could not track.
    #[must_use]
    pub const fn untracked(limit: u64) -> Self {
        Self {
            allowed: true,
            retry_after_ms: 0,
            remaining: limit,
            first_refusal: false,
        }
    }

    /// `Retry-After` in whole seconds: rounded up, and at least one, because
    /// `Retry-After: 0` invites the immediate retry the refusal exists to stop.
    #[must_use]
    pub fn retry_after_secs(&self) -> u64 {
        self.retry_after_ms.div_ceil(1_000).max(1)
    }
}

/// Decide a request of `cost` units against `quota`, and the key's next state.
///
/// `cost == 0` is a peek: it asks whether one unit would be admitted and
/// changes nothing but the refusal flag.
#[must_use]
pub fn decide(quota: Quota, cell: Option<Cell>, now_ms: u64, cost: u64) -> (Outcome, Cell) {
    let cell = cell.unwrap_or_default();
    let now = quota.ticks(now_ms);
    let interval = quota.interval();
    let burst = quota.burst();
    let base = cell.tat.max(now);

    let need = u128::from(cost.clamp(1, quota.limit.max(1))) * interval;
    let charge = u128::from(cost) * interval;

    if base + need - now <= burst {
        let tat = base + charge;
        let ahead = tat - now;
        let remaining = burst.saturating_sub(ahead) / interval;
        let next = Cell {
            tat,
            idle_at_ms: quota.ms_ceil(tat),
            refusing: false,
        };
        (
            Outcome {
                allowed: true,
                retry_after_ms: 0,
                remaining: u64::try_from(remaining).unwrap_or(u64::MAX),
                first_refusal: false,
            },
            next,
        )
    } else {
        let wait = base + need - now - burst;
        let next = Cell {
            refusing: true,
            ..cell
        };
        (
            Outcome {
                allowed: false,
                retry_after_ms: quota.ms_ceil(wait),
                remaining: 0,
                first_refusal: !cell.refusing,
            },
            next,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_704_067_200_000;

    /// Drive `n` unit requests at one instant and count the admissions.
    fn burst_at(quota: Quota, mut cell: Option<Cell>, now: u64, n: u32) -> (u32, Option<Cell>) {
        let mut admitted = 0;
        for _ in 0..n {
            let (out, next) = decide(quota, cell, now, 1);
            cell = Some(next);
            if out.allowed {
                admitted += 1;
            }
        }
        (admitted, cell)
    }

    /// A fresh key gets exactly one bucket at once, and not one more.
    #[test]
    fn a_fresh_key_bursts_to_the_limit_and_no_further() {
        let q = Quota::per_minute(5);
        let (admitted, _) = burst_at(q, None, T0, 9);
        assert_eq!(admitted, 5);
    }

    /// The bucket refills at the sustained rate: one unit per `period / limit`.
    #[test]
    fn a_drained_key_refills_one_unit_per_interval() {
        let q = Quota::per_minute(5); // one unit every 12 s
        let (_, cell) = burst_at(q, None, T0, 5);

        let (early, _) = decide(q, cell, T0 + 11_999, 1);
        assert!(!early.allowed, "a unit is not back before its interval");

        let (on_time, cell) = decide(q, cell, T0 + 12_000, 1);
        assert!(on_time.allowed, "and is back exactly on it");
        let (again, _) = decide(q, Some(cell), T0 + 12_000, 1);
        assert!(!again.allowed, "only one unit came back");
    }

    /// A full window of quiet restores the whole burst, and no more than it:
    /// an idle key does not bank allowance.
    #[test]
    fn idleness_restores_one_bucket_and_never_more() {
        let q = Quota::per_minute(5);
        let (_, cell) = burst_at(q, None, T0, 5);
        let (admitted, _) = burst_at(q, cell, T0 + 10 * 60_000, 9);
        assert_eq!(admitted, 5);
    }

    /// `Retry-After` names the instant the next unit is back: waiting exactly
    /// that long is admitted, and a millisecond less is not.
    #[test]
    fn retry_after_is_exactly_when_the_next_request_is_admitted() {
        let q = Quota::per_minute(5);
        let (_, cell) = burst_at(q, None, T0, 5);
        let (refused, cell) = decide(q, cell, T0 + 3_000, 1);
        assert!(!refused.allowed);
        assert_eq!(refused.retry_after_ms, 9_000);
        assert_eq!(refused.retry_after_secs(), 9);

        let (too_early, _) = decide(q, Some(cell), T0 + 3_000 + 8_999, 1);
        assert!(!too_early.allowed);
        let (on_time, _) = decide(q, Some(cell), T0 + 3_000 + 9_000, 1);
        assert!(on_time.allowed);
    }

    /// A sub-second wait still says one second: `Retry-After: 0` would invite
    /// the retry storm the refusal exists to stop.
    #[test]
    fn retry_after_rounds_up_to_at_least_a_second() {
        let q = Quota::per_second(50);
        let (_, cell) = burst_at(q, None, T0, 50);
        let (refused, _) = decide(q, cell, T0, 1);
        assert_eq!(refused.retry_after_ms, 20);
        assert_eq!(refused.retry_after_secs(), 1);
    }

    /// A weighted request that costs more than the bucket holds is admitted
    /// from a full bucket and leaves the key in debt, so the average still
    /// holds — refusing it would refuse it forever.
    #[test]
    fn a_cost_larger_than_the_bucket_is_admitted_once_and_then_repaid() {
        let q = Quota::per_second(50);
        let (big, cell) = decide(q, None, T0, 200);
        assert!(big.allowed, "a full bucket admits an oversized batch");
        assert_eq!(big.remaining, 0);

        // 200 units at 50/s is 4 s of allowance; the debt beyond one bucket is
        // 150 units, 3 s, and then one unit's 20 ms interval before even one
        // unit is back.
        let (next, _) = decide(q, Some(cell), T0 + 2_999, 1);
        assert!(!next.allowed);
        assert_eq!(next.retry_after_ms, 21);
        let (not_yet, _) = decide(q, Some(cell), T0 + 3_019, 1);
        assert!(!not_yet.allowed);
        let (repaid, _) = decide(q, Some(cell), T0 + 3_020, 1);
        assert!(repaid.allowed);
    }

    /// An oversized request is not admitted from a partly drained bucket.
    #[test]
    fn a_cost_larger_than_the_bucket_waits_for_a_full_one() {
        let q = Quota::per_second(50);
        let (_, cell) = decide(q, None, T0, 1);
        let (big, _) = decide(q, Some(cell), T0, 200);
        assert!(!big.allowed);
        assert_eq!(big.retry_after_ms, 20, "one unit's interval refills it");
    }

    /// A peek reports what one unit would get and charges nothing.
    #[test]
    fn a_peek_charges_nothing() {
        let q = Quota::per_minute(2);
        let (peek, cell) = decide(q, None, T0, 0);
        assert!(peek.allowed);
        let (admitted, _) = burst_at(q, Some(cell), T0, 5);
        assert_eq!(admitted, 2, "the peek took nothing from the bucket");
    }

    /// Only the first refusal of a run says so, and an admission resets it.
    #[test]
    fn only_the_first_refusal_of_a_run_is_reported_as_first() {
        let q = Quota::per_minute(1);
        let (_, cell) = decide(q, None, T0, 1);
        let (first, cell) = decide(q, Some(cell), T0, 1);
        let (second, cell) = decide(q, Some(cell), T0, 1);
        assert!(first.first_refusal && !second.first_refusal);

        let (_, cell) = decide(q, Some(cell), T0 + 60_000, 1);
        let (again, _) = decide(q, Some(cell), T0 + 60_000, 1);
        assert!(again.first_refusal, "a new run starts after an admission");
    }

    /// A cell past its idle instant carries nothing, which is what lets a store
    /// forget it without changing any decision.
    #[test]
    fn a_cell_is_idle_exactly_when_its_bucket_is_full_again() {
        let q = Quota::per_minute(5);
        let (_, cell) = burst_at(q, None, T0, 2);
        let cell = cell.unwrap();
        assert!(!cell.is_idle(T0 + 23_999));
        assert!(cell.is_idle(T0 + 24_000));

        let (forgotten, _) = burst_at(q, None, T0 + 24_000, 9);
        let (remembered, _) = burst_at(q, Some(cell), T0 + 24_000, 9);
        assert_eq!(forgotten, remembered);
    }

    /// Byte-sized quotas do not overflow at real clock values.
    #[test]
    fn byte_quotas_do_not_overflow() {
        let q = Quota::per_minute(256 * 1024 * 1024);
        let (out, _) = decide(q, None, u64::MAX / 2, 100_000_000);
        assert!(out.allowed);
    }
}
