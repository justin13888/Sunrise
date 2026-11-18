//! Pairing rate limits.
//!
//! Per `docs/03-crypto/pairing-and-onboarding.md`.

/// Pair attempts per rolling hour per account; the relay buckets them by
/// `account_id`.
pub const RATE_LIMIT_HOURLY: u32 = 10;

/// Pair attempts per rolling day per account; the relay buckets them by
/// `account_id`.
pub const RATE_LIMIT_DAILY: u32 = 30;

/// Pair attempts per rolling hour per relay client address.
///
/// Defence in depth for shared-NAT users, per spec §Rate limits: it is looser
/// than the per-account limit so one household's two accounts do not starve
/// each other, and it still bounds a client that rotates accounts.
pub const RATE_LIMIT_PER_ADDRESS_HOURLY: u32 = 60;

const HOUR_MS: u64 = 60 * 60 * 1000;
const DAY_MS: u64 = 24 * HOUR_MS;

/// The two rolling windows one key is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptLimit {
    /// Attempts admitted in any rolling hour.
    pub per_hour: u32,
    /// Attempts admitted in any rolling day, where a day bound applies.
    pub per_day: Option<u32>,
}

impl AttemptLimit {
    /// The per-account limit: 10 an hour, 30 a day.
    pub const PER_ACCOUNT: Self = Self {
        per_hour: RATE_LIMIT_HOURLY,
        per_day: Some(RATE_LIMIT_DAILY),
    };

    /// The per-address limit: 60 an hour, no daily bound.
    pub const PER_ADDRESS: Self = Self {
        per_hour: RATE_LIMIT_PER_ADDRESS_HOURLY,
        per_day: None,
    };
}

/// One key's pair attempts over the last day, oldest first.
///
/// The log is kept in time order even when `now_ms` steps back between two
/// attempts: an attempt is inserted where its timestamp sorts, not appended,
/// so the oldest attempt in a window is always the first one in it and the
/// `Retry-After` a refusal promises is exactly when a retry is admitted.
///
/// A sliding log rather than a token bucket because the spec's limits are
/// stated as *rolling* windows, and `Retry-After` has to be "the seconds until
/// the next slot opens" — the moment the oldest attempt inside a full window
/// ages out of it, which a log knows exactly and a bucket only approximates.
/// It holds at most a day's admitted attempts, which the limits themselves
/// bound to `per_day` (or `24 * per_hour` with no daily bound).
///
/// Time is a parameter, so the whole policy is a pure function of the
/// timestamps it is fed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttemptWindow {
    admitted_ms: std::collections::VecDeque<u64>,
}

impl AttemptWindow {
    /// Admit one attempt at `now_ms` and record it, or refuse it with the
    /// milliseconds until the earliest moment a retry would be admitted.
    ///
    /// A refused attempt is not recorded: a client that keeps knocking during
    /// a refusal does not push its own window further out.
    ///
    /// # Errors
    ///
    /// `Err(wait_ms)`, at least 1, when either window is full.
    pub fn try_admit(&mut self, limit: AttemptLimit, now_ms: u64) -> Result<(), u64> {
        self.forget_before(now_ms.saturating_sub(DAY_MS));
        let mut wait = 0u64;
        if let Some(opens) = self.full_until(limit.per_hour, HOUR_MS, now_ms) {
            wait = wait.max(opens);
        }
        if let Some(per_day) = limit.per_day {
            if let Some(opens) = self.full_until(per_day, DAY_MS, now_ms) {
                wait = wait.max(opens);
            }
        }
        if wait > 0 {
            return Err(wait);
        }
        // After every attempt already logged, unless the clock stepped back.
        let at = self.admitted_ms.partition_point(|&t| t <= now_ms);
        self.admitted_ms.insert(at, now_ms);
        Ok(())
    }

    /// Whether nothing inside the last day is recorded, so the key can be
    /// dropped by a caller that holds one window per key.
    #[must_use]
    pub fn is_idle(&self, now_ms: u64) -> bool {
        let horizon = now_ms.saturating_sub(DAY_MS);
        self.admitted_ms.iter().all(|&t| t <= horizon)
    }

    fn forget_before(&mut self, horizon_ms: u64) {
        while self.admitted_ms.front().is_some_and(|&t| t <= horizon_ms) {
            self.admitted_ms.pop_front();
        }
    }

    /// With `cap` attempts admitted inside the `span_ms` ending now, the
    /// milliseconds until the oldest of them leaves the window.
    ///
    /// The log is sorted, so the attempts inside the window are a suffix of
    /// it, oldest first. Attempts logged later than `now_ms` (before a clock
    /// step-back) are inside it too.
    fn full_until(&self, cap: u32, span_ms: u64, now_ms: u64) -> Option<u64> {
        let horizon = now_ms.saturating_sub(span_ms);
        let first_inside = self.admitted_ms.partition_point(|&t| t <= horizon);
        let inside = self.admitted_ms.len() - first_inside;
        let cap = usize::try_from(cap).unwrap_or(usize::MAX);
        if inside < cap {
            return None;
        }
        // The attempt whose expiry brings the count back under `cap`.
        let pivot = self.admitted_ms[first_inside + inside - cap];
        Some((pivot + span_ms).saturating_sub(now_ms).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000_000;

    #[test]
    fn the_eleventh_attempt_in_an_hour_waits_for_the_first_to_age_out() {
        let mut w = AttemptWindow::default();
        for i in 0..10 {
            assert_eq!(
                w.try_admit(AttemptLimit::PER_ACCOUNT, T0 + i * 1000),
                Ok(())
            );
        }
        let now = T0 + 10_000;
        assert_eq!(
            w.try_admit(AttemptLimit::PER_ACCOUNT, now),
            Err(T0 + HOUR_MS - now)
        );
        // A refusal is not recorded, so the slot opens exactly when promised.
        assert_eq!(w.try_admit(AttemptLimit::PER_ACCOUNT, T0 + HOUR_MS), Ok(()));
    }

    #[test]
    fn the_daily_window_binds_after_the_hourly_one_has_reset() {
        let mut w = AttemptWindow::default();
        // Thirty attempts, ten an hour for three hours.
        for h in 0..3 {
            for i in 0..10 {
                let t = T0 + h * HOUR_MS + i;
                assert_eq!(w.try_admit(AttemptLimit::PER_ACCOUNT, t), Ok(()), "{h}/{i}");
            }
        }
        let now = T0 + 4 * HOUR_MS;
        assert_eq!(
            w.try_admit(AttemptLimit::PER_ACCOUNT, now),
            Err(T0 + DAY_MS - now),
            "the hour is clear; the day is not"
        );
        assert_eq!(w.try_admit(AttemptLimit::PER_ACCOUNT, T0 + DAY_MS), Ok(()));
    }

    /// An attempt admitted after a step-back (`T0 - 5_000`, logged after nine
    /// attempts from `T0`) is the oldest in the hour, so the `Retry-After` a
    /// refusal promises is its expiry, and the retry is admitted exactly then
    /// and not a millisecond before.
    #[test]
    fn a_clock_step_back_promises_the_expiry_of_the_oldest_attempt() {
        let mut w = AttemptWindow::default();
        for i in 0..9 {
            assert_eq!(
                w.try_admit(AttemptLimit::PER_ACCOUNT, T0 + i * 1000),
                Ok(())
            );
        }
        let stepped_back = T0 - 5_000;
        assert_eq!(w.try_admit(AttemptLimit::PER_ACCOUNT, stepped_back), Ok(()));
        assert!(
            w.admitted_ms.iter().is_sorted(),
            "the log stays in time order"
        );
        let now = T0 + 10_000;
        assert_eq!(
            w.try_admit(AttemptLimit::PER_ACCOUNT, now),
            Err(stepped_back + HOUR_MS - now)
        );
        assert!(w
            .clone()
            .try_admit(AttemptLimit::PER_ACCOUNT, stepped_back + HOUR_MS - 1)
            .is_err());
        assert_eq!(
            w.try_admit(AttemptLimit::PER_ACCOUNT, stepped_back + HOUR_MS),
            Ok(())
        );
    }

    #[test]
    fn the_address_limit_has_no_daily_bound() {
        let mut w = AttemptWindow::default();
        for h in 0..5 {
            for i in 0..60 {
                let t = T0 + h * HOUR_MS + i;
                assert_eq!(w.try_admit(AttemptLimit::PER_ADDRESS, t), Ok(()));
            }
        }
        assert!(w
            .try_admit(AttemptLimit::PER_ADDRESS, T0 + 4 * HOUR_MS + 60)
            .is_err());
    }

    #[test]
    fn a_window_is_idle_once_its_last_attempt_is_a_day_old() {
        let mut w = AttemptWindow::default();
        assert!(w.is_idle(T0));
        w.try_admit(AttemptLimit::PER_ACCOUNT, T0).unwrap();
        assert!(!w.is_idle(T0 + DAY_MS - 1));
        assert!(w.is_idle(T0 + DAY_MS));
    }

    /// Gaps between consecutive attempts: mostly bursts, so the hourly cap
    /// fills, with hour- and day-scale pauses so windows also drain.
    fn gap_ms() -> impl proptest::strategy::Strategy<Value = u64> {
        use proptest::prelude::*;
        prop_oneof![
            6 => 0..60_000u64,
            3 => 0..HOUR_MS,
            1 => 0..=DAY_MS,
        ]
    }

    /// The two shipped limits, and small arbitrary ones so a short run
    /// reaches every cap, with or without a daily bound.
    fn limit() -> impl proptest::strategy::Strategy<Value = AttemptLimit> {
        use proptest::prelude::*;
        prop_oneof![
            Just(AttemptLimit::PER_ACCOUNT),
            Just(AttemptLimit::PER_ADDRESS),
            (1..6u32, proptest::option::of(1..16u32))
                .prop_map(|(per_hour, per_day)| AttemptLimit { per_hour, per_day }),
        ]
    }

    /// Admitted attempts in the half-open window `(now - span, now]`.
    fn count_inside(admitted: &[u64], span_ms: u64, now_ms: u64) -> usize {
        let horizon = now_ms.saturating_sub(span_ms);
        admitted.iter().filter(|&&t| t > horizon).count()
    }

    /// Moves between consecutive attempts that may step the clock back:
    /// mostly forward as `gap_ms`, sometimes back by up to two days, so a
    /// step-back lands inside the hour, inside the day, and past attempts the
    /// window has already forgotten.
    fn step_ms() -> impl proptest::strategy::Strategy<Value = i64> {
        use proptest::prelude::*;
        let back = i64::try_from(2 * DAY_MS).unwrap();
        prop_oneof![
            8 => gap_ms().prop_map(|g| i64::try_from(g).unwrap()),
            2 => (1..=back).prop_map(|b| -b),
        ]
    }

    /// Feeds `arrivals` to one window and checks, at every attempt, against a
    /// model that counts the attempts the window still holds: an attempt is
    /// admitted exactly when both rolling windows ending at it are under their
    /// caps; a refusal records nothing; its wait is the earliest moment a
    /// retry is admitted; and the log stays sorted and never holds more than
    /// one day's admissible attempts.
    ///
    /// The model forgets what aged out of the day at each attempt, as the
    /// window does: over a non-decreasing sequence that changes nothing, and
    /// after a step-back an attempt the window has forgotten stays forgotten.
    fn check_arrivals(
        limit: AttemptLimit,
        arrivals: impl IntoIterator<Item = u64>,
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        let day_bound = usize::try_from(
            limit
                .per_day
                .map_or(24 * limit.per_hour, |d| d.min(24 * limit.per_hour)),
        )
        .unwrap();
        let per_hour = usize::try_from(limit.per_hour).unwrap();
        let per_day = limit.per_day.map(|d| usize::try_from(d).unwrap());
        let mut w = AttemptWindow::default();
        let mut admitted: Vec<u64> = Vec::new();
        for now in arrivals {
            admitted.retain(|&t| t > now.saturating_sub(DAY_MS));
            // Checking every window that ends at an attempt covers every
            // rolling window: the count only rises at an attempt.
            let hour_has_room = count_inside(&admitted, HOUR_MS, now) < per_hour;
            let day_has_room = per_day.is_none_or(|cap| count_inside(&admitted, DAY_MS, now) < cap);
            let before = w.clone();
            match w.try_admit(limit, now) {
                Ok(()) => {
                    proptest::prop_assert!(
                        hour_has_room && day_has_room,
                        "admitted at {now} past a full window"
                    );
                    admitted.push(now);
                }
                Err(wait) => {
                    proptest::prop_assert!(
                        !(hour_has_room && day_has_room),
                        "refused at {now} with room in both windows"
                    );
                    proptest::prop_assert!(wait >= 1);
                    // A refusal records nothing: what is left is what was
                    // there, less what aged out of the day.
                    let kept = w.admitted_ms.len();
                    proptest::prop_assert!(kept <= before.admitted_ms.len());
                    proptest::prop_assert!(before
                        .admitted_ms
                        .iter()
                        .skip(before.admitted_ms.len() - kept)
                        .eq(w.admitted_ms.iter()));
                    let mut retry = w.clone();
                    proptest::prop_assert_eq!(retry.try_admit(limit, now + wait), Ok(()));
                    let mut early = w.clone();
                    proptest::prop_assert!(
                        early.try_admit(limit, now + wait - 1).is_err(),
                        "a retry before the promised wait of {wait} was admitted"
                    );
                }
            }
            proptest::prop_assert!(w.admitted_ms.iter().is_sorted(), "log out of time order");
            proptest::prop_assert_eq!(w.admitted_ms.len(), admitted.len());
            proptest::prop_assert!(
                w.admitted_ms.len() <= day_bound,
                "log holds {} attempts, over a day's {day_bound}",
                w.admitted_ms.len()
            );
            proptest::prop_assert!(w
                .admitted_ms
                .iter()
                .all(|&t| t > now.saturating_sub(DAY_MS)));
        }
        Ok(())
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: sunrise_test_seed::proptest_cases(512),
            rng_seed: sunrise_test_seed::proptest_rng_seed(),
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Over any non-decreasing arrival sequence the window holds its caps
        /// and keeps its promises, as `check_arrivals` states them.
        #[test]
        fn the_window_holds_its_caps_over_any_arrival_sequence(
            limit in limit(),
            gaps in proptest::collection::vec(gap_ms(), 1..200),
        ) {
            let arrivals = gaps.into_iter().scan(T0, |now, gap| {
                *now += gap;
                Some(*now)
            });
            check_arrivals(limit, arrivals)?;
        }

        /// The same holds when the clock steps back between attempts: a
        /// refusal's wait is never longer than the moment a retry is in fact
        /// admitted, and never shorter.
        #[test]
        fn the_window_holds_its_caps_when_the_clock_steps_back(
            limit in limit(),
            steps in proptest::collection::vec(step_ms(), 1..200),
        ) {
            let arrivals = steps.into_iter().scan(T0, |now, step| {
                *now = now.saturating_add_signed(step);
                Some(*now)
            });
            check_arrivals(limit, arrivals)?;
        }
    }
}
