//! Review statistics per `docs/08-features/reviews-and-stats.md` §Stats:
//! completed-per-week and deferred-per-week trends, routine streaks, and
//! routine drift. Time-in-focus is *not* re-derived here — it is already
//! [`crate::focus::fold_focus_stats`], and a second implementation of the same
//! number is a second answer waiting to disagree.
//!
//! This module holds the [`WeekGrid`] a trend is reported in; the folds are in
//! `trends` and `routine_drift`.
//!
//! Everything in this module is pure. Time enters only as explicit parameters
//! ( `now_ms`, a [`WeekGrid`], a window) and a [`TimeZone`] the caller resolved
//! from the injected clock.

mod routine_drift;
mod trends;

pub use routine_drift::{routine_drift, RoutineDrift, DEFAULT_DRIFT_THRESHOLD, DRIFT_WINDOW_WEEKS};
pub use trends::{fold_trends, StreamTrend, Trends, WeekBucket, TREND_WEEKS};

use crate::rrule::Weekday;
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};

/// Errors from building a [`WeekGrid`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StatsError {
    /// `now_ms` is not a representable instant.
    #[error("timestamp out of range: {0}")]
    TimestampRange(u64),
    /// Calendar arithmetic ran off the end of the supported range.
    #[error("week grid arithmetic overflowed")]
    Overflow,
}

/// The week buckets a trend is reported in.
///
/// Weeks are **civil** weeks in a real timezone, not fixed 604800-second
/// slices: a DST transition makes one week 23 or 25 hours longer, and a chart
/// whose Monday drifts an hour twice a year is a bug the user sees. The grid is
/// built once from a zone and a week-start weekday, then used as a pure
/// `instant → bucket` function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeekGrid {
    /// Ascending week-start instants (ms since epoch), one per bucket.
    starts: Vec<u64>,
    /// Exclusive end of the last bucket.
    end_ms: u64,
    /// The reader's zone the grid was built in, which a time with no zone of
    /// its own (an all-day completion) resolves in before it is bucketed
    /// (`docs/10-cross-cutting/time.md` §2).
    zone: TimeZone,
}

impl WeekGrid {
    /// Build directly from ascending week starts and an exclusive end, in
    /// UTC.
    ///
    /// Used by tests and by callers that already know their boundaries; the
    /// normal constructor is [`WeekGrid::trailing`].
    #[must_use]
    pub fn from_starts(starts: Vec<u64>, end_ms: u64) -> Self {
        Self {
            starts,
            end_ms,
            zone: TimeZone::UTC,
        }
    }

    /// The zone the grid's weeks are civil weeks in.
    #[must_use]
    pub const fn zone(&self) -> &TimeZone {
        &self.zone
    }

    /// The `weeks` civil weeks ending with the one containing `now_ms`.
    ///
    /// `week_start` is the weekday a week begins on, read by
    /// [`crate::time::first_day_of_week`]: a token this build does not know
    /// starts the week on the preference's default, Sunday.
    /// The last bucket is the *current, partial* week — a review run on Friday
    /// must show the week it is in.
    pub fn trailing(
        now_ms: u64,
        weeks: u32,
        tz: &TimeZone,
        week_start: &Weekday,
    ) -> Result<Self, StatsError> {
        let weeks = weeks.max(1);
        let now = to_timestamp(now_ms)?;
        let date = now.to_zoned(tz.clone()).date();
        let current = crate::time::first_day_of_week(date, week_start);

        let mut starts = Vec::with_capacity(weeks as usize);
        for i in (0..weeks).rev() {
            let d = current
                .checked_sub(Span::new().weeks(i64::from(i)))
                .map_err(|_| StatsError::Overflow)?;
            starts.push(zone_start_ms(d, tz)?);
        }
        let next = current
            .checked_add(Span::new().weeks(1))
            .map_err(|_| StatsError::Overflow)?;
        Ok(Self {
            starts,
            end_ms: zone_start_ms(next, tz)?,
            zone: tz.clone(),
        })
    }

    /// The bucket start instants, ascending.
    #[must_use]
    pub fn starts(&self) -> &[u64] {
        &self.starts
    }

    /// Number of buckets.
    #[must_use]
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    /// Whether the grid has no buckets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Exclusive end of the grid.
    #[must_use]
    pub const fn end_ms(&self) -> u64 {
        self.end_ms
    }

    /// Start of the *last* (current) bucket — the window a weekly review
    /// covers by default.
    #[must_use]
    pub fn last_start_ms(&self) -> u64 {
        self.starts.last().copied().unwrap_or(0)
    }

    /// Which bucket `at_ms` falls in, or `None` when it is outside the grid.
    ///
    /// `None` is a normal answer, not an error: a re-open that decrements a
    /// completion from before the window simply has nowhere to land, which is
    /// exactly the behaviour a 12-week chart wants.
    #[must_use]
    pub fn index_of(&self, at_ms: u64) -> Option<usize> {
        if at_ms >= self.end_ms {
            return None;
        }
        // `partition_point` gives the count of starts <= at_ms; the bucket is
        // one below that. Zero means `at_ms` precedes the grid.
        let n = self.starts.partition_point(|s| *s <= at_ms);
        n.checked_sub(1)
    }
}

/// Convert epoch milliseconds into a jiff [`Timestamp`].
fn to_timestamp(ms: u64) -> Result<Timestamp, StatsError> {
    let signed = i64::try_from(ms).map_err(|_| StatsError::TimestampRange(ms))?;
    Timestamp::from_millisecond(signed).map_err(|_| StatsError::TimestampRange(ms))
}

/// Start-of-day instant for a civil date in `tz`, in epoch milliseconds.
fn zone_start_ms(date: jiff::civil::Date, tz: &TimeZone) -> Result<u64, StatsError> {
    let zoned = date
        .to_zoned(tz.clone())
        .map_err(|_| StatsError::Overflow)?;
    u64::try_from(zoned.timestamp().as_millisecond()).map_err(|_| StatsError::Overflow)
}

/// What the grid's, the trends' and the drift tests all read.
#[cfg(test)]
mod fixtures {
    use super::*;
    use sunrise_id::{EntityKind, EntityRef};

    /// 2026-01-05T00:00:00Z is a **Monday**, which makes every week boundary in
    /// these tests checkable by hand.
    pub(super) const MON: u64 = 1_767_571_200_000;
    pub(super) const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

    pub(super) fn eref(kind: EntityKind, b: u8) -> EntityRef {
        EntityRef::new(kind, [b; 16])
    }

    pub(super) fn ts(ms: u64) -> Timestamp {
        Timestamp::from_millisecond(i64::try_from(ms).unwrap()).unwrap()
    }

    pub(super) fn utc() -> TimeZone {
        TimeZone::UTC
    }

    /// A 3-week grid whose last week starts on `MON`.
    pub(super) fn grid3() -> WeekGrid {
        WeekGrid::trailing(MON + 3_600_000, 3, &utc(), &Weekday::Mo).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{grid3, ts, utc, MON, WEEK_MS};
    use super::*;
    use jiff::civil::Weekday as JiffWeekday;

    #[test]
    fn trailing_grid_lands_on_week_starts_and_includes_the_current_week() {
        let g = grid3();
        assert_eq!(
            g.starts(),
            [MON - 2 * WEEK_MS, MON - WEEK_MS, MON],
            "three Mondays ending with the one we are in"
        );
        assert_eq!(g.end_ms(), MON + WEEK_MS);
        assert_eq!(g.last_start_ms(), MON);
    }

    #[test]
    fn grid_buckets_instants_and_rejects_everything_outside() {
        let g = grid3();
        assert_eq!(g.index_of(MON - 2 * WEEK_MS), Some(0), "inclusive start");
        assert_eq!(g.index_of(MON - WEEK_MS - 1), Some(0), "last ms of week 0");
        assert_eq!(g.index_of(MON), Some(2));
        assert_eq!(
            g.index_of(MON + WEEK_MS - 1),
            Some(2),
            "last ms of the grid"
        );
        assert_eq!(g.index_of(MON + WEEK_MS), None, "exclusive end");
        assert_eq!(g.index_of(MON - 2 * WEEK_MS - 1), None, "before the grid");
    }

    #[test]
    fn a_week_that_crosses_a_dst_transition_is_25_hours_long() {
        // US DST ends 2025-11-02; the week starting Monday 2025-10-27 in
        // America/New_York therefore spans 7*24+1 hours. A fixed-604800s grid
        // would put the following Monday an hour early.
        let tz = TimeZone::get("America/New_York").expect("tzdb bundled");
        let now = u64::try_from(
            "2025-11-03T12:00:00-05:00"
                .parse::<Timestamp>()
                .unwrap()
                .as_millisecond(),
        )
        .unwrap();
        let g = WeekGrid::trailing(now, 2, &tz, &Weekday::Mo).unwrap();
        let [a, b] = [g.starts()[0], g.starts()[1]];
        assert_eq!(
            b - a,
            WEEK_MS + 3_600_000,
            "the DST week really is an hour longer"
        );
        // And every start is local midnight, not 23:00 the day before.
        for s in g.starts() {
            let z = ts(*s).to_zoned(tz.clone());
            assert_eq!(z.hour(), 0, "week starts at local midnight: {z}");
            assert_eq!(z.weekday(), JiffWeekday::Monday);
        }
    }

    /// A Sunday-start grid, so the `week_start` parameter is something other
    /// than the `Weekday::Mo` every caller passes.
    ///
    /// **Unobserved today.** No product surface is known to set a non-Monday
    /// week start: every call site in the workspace passes `Weekday::Mo`, and
    /// there is no setting that would produce anything else. This pins the
    /// parameter's arithmetic so a later reader can trust it, and says plainly
    /// that trusting it is not the same as the feature having shipped.
    #[test]
    fn a_sunday_start_grid_begins_its_weeks_on_the_sunday_before() {
        // `MON` is a Monday, so a Sunday-start week containing it began the
        // day before — the one boundary a Monday-only test can never catch.
        let g = WeekGrid::trailing(MON + 3_600_000, 3, &utc(), &Weekday::Su).unwrap();
        let sun = MON - 86_400_000;
        assert_eq!(
            g.starts(),
            [sun - 2 * WEEK_MS, sun - WEEK_MS, sun],
            "three Sundays ending with the one we are in"
        );
        assert_eq!(g.end_ms(), sun + WEEK_MS);
        for start in g.starts() {
            assert_eq!(
                ts(*start).to_zoned(utc()).weekday(),
                JiffWeekday::Sunday,
                "every bucket starts on the requested weekday"
            );
        }
        // And `MON` itself falls in the *last* bucket, not the first day of a
        // new one: this is the assertion a Monday-start grid gets wrong.
        assert_eq!(g.index_of(MON), Some(2));
        assert_eq!(
            g.index_of(sun - 2 * WEEK_MS - 1),
            None,
            "the instant before the grid"
        );
        assert_eq!(g.index_of(sun - 1), Some(1), "the last ms of week 1");
    }

    /// A `week_start` token this build does not know starts the grid's weeks
    /// where `first_day_of_week` starts them, on the preference's default
    /// (Sunday), so trends and the weekly review bucket the same weeks as
    /// `Query::WeekBlocks`.
    #[test]
    fn an_unknown_week_start_buckets_the_weeks_first_day_of_week_does() {
        let unknown = Weekday::from_raw("XX");
        assert!(matches!(unknown, Weekday::Unknown(_)));
        for day in 0..7 {
            let now = MON + day * 86_400_000 + 3_600_000;
            let g = WeekGrid::trailing(now, 2, &utc(), &unknown).unwrap();
            let today = ts(now).to_zoned(utc()).date();
            let first = crate::time::first_day_of_week(today, &unknown);
            assert_eq!(
                g.starts()[1],
                zone_start_ms(first, &utc()).unwrap(),
                "day {day}: the current bucket begins on {first}"
            );
            assert_eq!(
                g.starts(),
                WeekGrid::trailing(now, 2, &utc(), &Weekday::Su)
                    .unwrap()
                    .starts()
            );
        }
    }

    /// Asking for zero weeks is clamped to one rather than yielding a grid
    /// that buckets nothing. `index_of` on an empty grid would answer `None`
    /// for every instant, which reads as "no activity" instead of "bad call".
    #[test]
    fn a_grid_of_zero_weeks_is_clamped_to_one() {
        let g = WeekGrid::trailing(MON + 3_600_000, 0, &utc(), &Weekday::Mo).unwrap();
        assert_eq!(g.len(), 1, "clamped, not empty");
        assert!(!g.is_empty());
        assert_eq!(g.starts(), [MON]);
        assert_eq!(g.end_ms(), MON + WEEK_MS);
        assert_eq!(g.last_start_ms(), MON);
        assert_eq!(g.index_of(MON + 3_600_000), Some(0));
    }

    /// The two lengths the spec fixes. They are read by callers that size a
    /// review window, so a change to either is a change to what a user sees.
    #[test]
    fn the_trend_and_drift_windows_are_the_lengths_the_spec_states() {
        assert_eq!(TREND_WEEKS, 12, "the spec asks for the last 12 weeks");
        assert_eq!(DRIFT_WINDOW_WEEKS, 4, "drift is measured over 4 weeks");
    }
}
