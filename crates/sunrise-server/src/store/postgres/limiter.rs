//! Rate cells in one shared table (ADR-0062 §5), so a client's budget is one
//! budget however many relay nodes it reaches.
//!
//! The algorithm is `api::ratelimit::bucket`'s GCRA, unchanged: a cell is its
//! theoretical arrival time, and a request is admitted when that time, pushed
//! forward by the request's cost, lies no more than one bucket ahead of now.
//! A charge is **one statement**, an `INSERT … ON CONFLICT DO UPDATE …
//! RETURNING`, so the decision and the charge are one atomic step on the row
//! and two nodes charging one key take turns at the row lock rather than both
//! deciding from the same old value.
//!
//! The arithmetic runs in the database as `NUMERIC`, because a cell counts in
//! ticks of `milliseconds × limit`, which overflows a `BIGINT` for a byte
//! budget measured in the hundreds of millions. The parameters are sent as
//! text and cast, so no decimal type is needed on this side.
//!
//! What differs from [`MemoryStore`](crate::api::ratelimit::store::MemoryStore)
//! is bounded memory, which a table does not need: there is no key cap and no
//! fail-open, and idle cells are deleted by [`LimiterStore::sweep`] on the
//! maintenance pass instead of under pressure. A peek writes nothing.

use super::{signed, PgStore};
use crate::api::ratelimit::bucket::{Outcome, Quota};
use crate::api::ratelimit::store::LimiterStore;

/// The one statement a charge is. `$2` now, `$3` the bucket, `$4` what must
/// fit, `$5` what is charged and `$6` the limit, all in ticks. "Admitted" is
/// spelled out at each use because a `SET` list cannot name a shared term,
/// and every term reads the row as it was before this statement.
const CHARGE: &str = "
INSERT INTO rate_cells AS c (key, tat, idle_at_ms, refusing, first_refusal)
VALUES ($1, $2::text::numeric + $5::text::numeric,
        CEIL(($2::text::numeric + $5::text::numeric) / $6::text::numeric)::BIGINT, FALSE, FALSE)
ON CONFLICT (key) DO UPDATE SET
    tat = CASE
        WHEN GREATEST(c.tat, $2::text::numeric) + $4::text::numeric - $2::text::numeric
             <= $3::text::numeric
        THEN GREATEST(c.tat, $2::text::numeric) + $5::text::numeric
        ELSE c.tat END,
    idle_at_ms = CASE
        WHEN GREATEST(c.tat, $2::text::numeric) + $4::text::numeric - $2::text::numeric
             <= $3::text::numeric
        THEN CEIL((GREATEST(c.tat, $2::text::numeric) + $5::text::numeric)
                  / $6::text::numeric)::BIGINT
        ELSE c.idle_at_ms END,
    first_refusal = NOT c.refusing AND
        GREATEST(c.tat, $2::text::numeric) + $4::text::numeric - $2::text::numeric
        > $3::text::numeric,
    refusing =
        GREATEST(c.tat, $2::text::numeric) + $4::text::numeric - $2::text::numeric
        > $3::text::numeric
RETURNING tat::TEXT, refusing, first_refusal";

/// The Postgres [`LimiterStore`], on the same pool as the metadata.
#[derive(Debug, Clone)]
pub struct PgLimiter {
    store: PgStore,
}

impl PgLimiter {
    /// Rate cells in `store`'s database.
    #[must_use]
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }
}

/// The GCRA terms of one decision, in ticks.
struct Terms {
    limit: u128,
    interval: u128,
    burst: u128,
    now: u128,
}

impl Terms {
    fn new(quota: Quota, now_ms: u64) -> Self {
        let limit = u128::from(quota.limit.max(1));
        let interval = u128::from(quota.period_ms.max(1));
        Self {
            limit,
            interval,
            burst: limit * interval,
            now: u128::from(now_ms) * limit,
        }
    }

    /// What must fit in the bucket for `cost`: a cost above the limit is
    /// checked as the limit and charged in full.
    fn need(&self, cost: u64) -> u128 {
        u128::from(cost).clamp(1, self.limit) * self.interval
    }

    fn ms_ceil(&self, ticks: u128) -> u64 {
        u64::try_from(ticks.div_ceil(self.limit)).unwrap_or(u64::MAX)
    }

    /// The outcome for a cell now at `tat`, whose request needed `need`.
    fn outcome(&self, tat: u128, need: u128, allowed: bool, first_refusal: bool) -> Outcome {
        if allowed {
            let ahead = tat.saturating_sub(self.now);
            let remaining = self.burst.saturating_sub(ahead) / self.interval;
            Outcome {
                allowed: true,
                retry_after_ms: 0,
                remaining: u64::try_from(remaining).unwrap_or(u64::MAX),
                first_refusal: false,
            }
        } else {
            let base = tat.max(self.now);
            let wait = (base + need).saturating_sub(self.now + self.burst);
            Outcome {
                allowed: false,
                retry_after_ms: self.ms_ceil(wait),
                remaining: 0,
                first_refusal,
            }
        }
    }
}

/// A `NUMERIC` read back as text, to the integer it holds.
fn ticks(text: &str) -> u128 {
    text.split('.').next().unwrap_or("0").parse().unwrap_or(0)
}

/// What a store failure answers: admitted, untracked. A rate store that cannot
/// be reached must not take every client down with it; the failure is logged.
fn failed_open(quota: Quota, e: &impl std::fmt::Display) -> Outcome {
    tracing::error!(
        ev = "srv.store.failed",
        err_code = %sunrise_error::ErrorCode::RelayStorageUnavailable,
        err_kind = "transient",
        retryable = true,
        cause = %e,
        "the shared rate store did not answer; admitting untracked"
    );
    Outcome::untracked(quota.limit)
}

#[async_trait::async_trait]
impl LimiterStore for PgLimiter {
    async fn take(&self, key: &str, quota: Quota, cost: u64, now_ms: u64) -> Outcome {
        let cost = cost.max(1);
        let t = Terms::new(quota, now_ms);
        let need = t.need(cost);
        let charge = u128::from(cost) * t.interval;
        let row = match self.store.client().await {
            Ok(c) => c
                .query_one(
                    CHARGE,
                    &[
                        &key,
                        &t.now.to_string(),
                        &t.burst.to_string(),
                        &need.to_string(),
                        &charge.to_string(),
                        &t.limit.to_string(),
                    ],
                )
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match row {
            Ok(r) => {
                let refusing: bool = r.get(1);
                t.outcome(ticks(r.get(0)), need, !refusing, r.get(2))
            }
            Err(e) => failed_open(quota, &e),
        }
    }

    async fn peek(&self, key: &str, quota: Quota, now_ms: u64) -> Outcome {
        let t = Terms::new(quota, now_ms);
        let row = match self.store.client().await {
            Ok(c) => c
                .query_opt(
                    "SELECT tat::TEXT, refusing FROM rate_cells WHERE key = $1",
                    &[&key],
                )
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match row {
            Ok(None) => Outcome::untracked(quota.limit),
            Ok(Some(r)) => {
                let tat = ticks(r.get(0));
                let need = t.need(0);
                let base = tat.max(t.now);
                let allowed = base + need - t.now <= t.burst;
                let refusing: bool = r.get(1);
                t.outcome(base, need, allowed, !refusing)
            }
            Err(e) => failed_open(quota, &e),
        }
    }

    async fn sweep(&self, now_ms: u64) -> Result<u64, String> {
        let c = self.store.client().await.map_err(|e| e.to_string())?;
        c.execute(
            "DELETE FROM rate_cells WHERE idle_at_ms <= $1",
            &[&signed(now_ms)],
        )
        .await
        .map_err(|e| e.to_string())
    }
}
