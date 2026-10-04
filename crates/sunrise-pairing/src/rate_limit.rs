//! Pairing rate limits + email-hash helper.
//!
//! Per `docs/03-crypto/pairing-and-onboarding.md`.

const EMAIL_HASH_DOMAIN: &str = "sunrise.account_email_hash.v1";

/// Pair attempts per rolling hour per `account_email_hash`.
pub const RATE_LIMIT_HOURLY: u32 = 10;

/// Pair attempts per rolling day per `account_email_hash`.
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
        self.admitted_ms.push_back(now_ms);
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
    fn full_until(&self, cap: u32, span_ms: u64, now_ms: u64) -> Option<u64> {
        let horizon = now_ms.saturating_sub(span_ms);
        let inside: Vec<u64> = self
            .admitted_ms
            .iter()
            .copied()
            .filter(|&t| t > horizon)
            .collect();
        let cap = usize::try_from(cap).unwrap_or(usize::MAX);
        if inside.len() < cap {
            return None;
        }
        // The attempt whose expiry brings the count back under `cap`.
        let pivot = inside[inside.len() - cap];
        Some((pivot + span_ms).saturating_sub(now_ms).max(1))
    }
}

/// Compute the 4-byte (8 hex chars) `account_email_hash`. Email is normalized
/// (lowercase + trim) before hashing. The hash is non-reversible and used
/// only as the rate-limit bucket key — never to identify a user across
/// accounts.
#[must_use]
pub fn account_email_hash(email: &str) -> [u8; 4] {
    let normalized = email.trim().to_ascii_lowercase();
    let mut hasher = blake3::Hasher::new_derive_key(EMAIL_HASH_DOMAIN);
    hasher.update(normalized.as_bytes());
    let mut out = [0u8; 4];
    hasher.finalize_xof().fill(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_normalized() {
        let a = account_email_hash("Justin@Example.com");
        let b = account_email_hash(" justin@example.com  ");
        assert_eq!(a, b);
    }

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

    #[test]
    fn different_emails_diverge() {
        let a = account_email_hash("a@example.com");
        let b = account_email_hash("b@example.com");
        assert_ne!(a, b);
    }
}
