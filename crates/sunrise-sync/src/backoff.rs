//! Exponential backoff with jitter, per `spec/10-cross-cutting/error-handling.md`
//! §canonical-retry-policy:
//!
//! ```text
//! initial_delay_ms = 100
//! max_delay_ms     = 30000
//! jitter_pct       = ±20%
//! max_retries      = 5
//! ```
//!
//! Pure stateful struct; takes a `now` and an external RNG source as inputs
//! so the determinism rules in `spec/01-architecture/shared-core.md` apply
//! when consumed inside `sunrise-core`.

use std::time::Duration;

/// Backoff state machine.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    attempt: u32,
    max_retries: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::canonical()
    }
}

impl Backoff {
    /// The canonical v1 policy: 5 retries, base 100 ms, max 30 s.
    #[must_use]
    pub const fn canonical() -> Self {
        Self {
            attempt: 0,
            max_retries: 5,
        }
    }

    /// Next sleep duration without consuming an attempt. Returns `None` if
    /// the policy is exhausted.
    ///
    /// `jitter_unit` is a value in [0, 1] that selects the jitter (e.g., a
    /// `rand::random::<f64>()`). The output is base × (0.8 + 0.4 × jitter)
    /// for the configured ±20%.
    #[must_use]
    pub fn next_delay(&self, jitter_unit: f64) -> Option<Duration> {
        if self.attempt >= self.max_retries {
            return None;
        }
        let base_ms = 100u64.saturating_mul(1u64 << self.attempt.min(15));
        let capped = base_ms.min(30_000);
        let j = jitter_unit.clamp(0.0, 1.0);
        // 0.8 + 0.4 * j ∈ [0.8, 1.2]
        let scale = 0.8 + 0.4 * j;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let scaled = (capped as f64 * scale) as u64;
        Some(Duration::from_millis(scaled))
    }

    /// Consume one attempt; call after the prior `next_delay` actually slept.
    pub fn record_attempt(&mut self) {
        self.attempt = self.attempt.saturating_add(1);
    }

    /// Reset the attempt counter (call after a successful operation).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Whether the policy is exhausted.
    #[must_use]
    pub const fn exhausted(&self) -> bool {
        self.attempt >= self.max_retries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_grow_then_cap() {
        let mut b = Backoff::canonical();
        let mut last_ms = 0u64;
        for _ in 0..5 {
            let d = b.next_delay(0.5).unwrap();
            let ms = u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
            assert!(
                ms >= last_ms.saturating_sub(10),
                "delay shrank: {ms} after {last_ms}"
            );
            assert!(ms <= 30_000 + 6_000); // ±20% of cap
            last_ms = ms;
            b.record_attempt();
        }
        assert!(b.exhausted());
        assert!(b.next_delay(0.5).is_none());
    }

    #[test]
    fn jitter_bounds() {
        let b = Backoff::canonical();
        let lo = b.next_delay(0.0).unwrap().as_millis();
        let hi = b.next_delay(1.0).unwrap().as_millis();
        // First-attempt base=100ms; ±20% → [80, 120].
        assert!((80..=120).contains(&lo));
        assert!((80..=120).contains(&hi));
        assert!(hi >= lo);
    }

    #[test]
    fn reset_zeros_attempts() {
        let mut b = Backoff::canonical();
        b.record_attempt();
        b.record_attempt();
        b.reset();
        let _ = b.next_delay(0.5).unwrap();
    }
}
