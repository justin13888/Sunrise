//! Routine drift: how far a Routine's completions have fallen behind its own
//! cadence, the number the spec's tuning step keys on.

use crate::routine::Routine;
use crate::routine_gen::{occurrence_key_at, ExpandError};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// Drift window for routine tuning: the last 4 weeks.
pub const DRIFT_WINDOW_WEEKS: u32 = 4;

/// Default drift threshold: suggest tuning a Routine once more than 30% of its
/// occurrences in the window went uncompleted. Not user-facing (the spec puts
/// it behind the debug menu), hence a constant rather than a setting.
pub const DEFAULT_DRIFT_THRESHOLD: f64 = 0.30;

/// How far one Routine has drifted from its own cadence.
///
/// `missed` counts every occurrence in the window that was **not** completed,
/// whether it was explicitly skipped or simply never done — that is the "> 30%
/// skipped" the spec's tuning step keys on. `skipped` reports the explicit
/// subset separately, because "you skip this deliberately every Friday" and
/// "you keep forgetting this" want different suggestions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineDrift {
    /// Routine id.
    pub routine: EntityRef,
    /// Its template title, for display.
    pub title: String,
    /// Occurrences the rrule produced in the window.
    pub expected: u32,
    /// Of those, completed within the streak rules.
    pub completed: u32,
    /// Of those, explicitly skipped.
    pub skipped: u32,
    /// `expected - completed`.
    pub missed: u32,
    /// `missed / expected`, or `0.0` when the window held no occurrences.
    pub drift: f64,
    /// Whether `drift` exceeds the caller's threshold — the "suggest pausing /
    /// changing cadence" trigger.
    pub over_threshold: bool,
    /// Current streak, straight off the Routine (see [`crate::streak`]).
    pub streak: i64,
    /// Last completion, if any.
    pub last_completed_at_ms: Option<u64>,
    /// Whether the routine is already paused (a paused routine needs no pause
    /// suggestion).
    pub paused: bool,
}

/// Measure one Routine's drift over `window`, for a reader in `reader` (where
/// a floating routine's occurrences fall).
///
/// Pure: the occurrence set comes from [`Routine::series_in`], completion comes from the
/// Routine's own `streak_keys` (the idempotency-key set the streak mechanics
/// already maintain), and explicit skips from `skipped_keys` / `skip_dates`.
/// No database, no clock.
pub fn routine_drift(
    routine: &Routine,
    window: (Timestamp, Timestamp),
    threshold: f64,
    reader: &TimeZone,
) -> Result<RoutineDrift, ExpandError> {
    // `series_in` (not `Routine::occurrences_in`) on purpose: the drift
    // measure needs the *unfiltered* occurrence set, because an explicitly
    // skipped occurrence is a data point, and `occurrences_in` drops exactly
    // those.
    let occurrences = routine.series_in(window, reader)?;
    // `skip_dates` is matched by civil minute in the routine's zone, the same
    // way `Routine::is_skipped` does it — a stored instant that a tzdb update
    // moved must still match the occurrence it was meant to skip.
    let skip_keys: BTreeSet<String> = routine
        .skip_dates
        .iter()
        .filter_map(|d| occurrence_key_at(&routine.timezone, *d).ok())
        .collect();

    let mut completed = 0u32;
    let mut skipped = 0u32;
    for occ in &occurrences {
        if routine.streak_key_seen(&occ.key) {
            completed = completed.saturating_add(1);
        } else if routine.skipped_keys.contains(&occ.key) || skip_keys.contains(&occ.key) {
            skipped = skipped.saturating_add(1);
        }
    }
    let expected = u32::try_from(occurrences.len()).unwrap_or(u32::MAX);
    let missed = expected.saturating_sub(completed);
    let drift = if expected == 0 {
        0.0
    } else {
        f64::from(missed) / f64::from(expected)
    };
    Ok(RoutineDrift {
        routine: routine.id,
        title: routine.template.title.clone(),
        expected,
        completed,
        skipped,
        missed,
        drift,
        over_threshold: expected > 0 && drift > threshold,
        streak: routine.streak_counter,
        last_completed_at_ms: routine
            .last_completed_at
            .as_ref()
            .and_then(|t| t.resolve_in(reader))
            .and_then(|t| u64::try_from(t.as_millisecond()).ok()),
        paused: routine.paused,
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{eref, ts, utc, MON, WEEK_MS};
    use super::*;
    use crate::common::Energy;
    use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
    use crate::rrule::RRule;
    use crate::unknown::Unknowns;
    use sunrise_id::EntityKind;

    fn daily_routine() -> Routine {
        Routine {
            id: eref(EntityKind::Routine, 5),
            created_at: ts(MON),
            updated_at: ts(MON),
            template: TaskTemplate {
                title: "Stretch".into(),
                stream_id: Some(eref(EntityKind::Stream, 9)),
                contexts: Vec::new(),
                energy: Some(Energy::Low),
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: crate::Unknowns::new(),
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: ts(MON).into(),
            ends_at: None,
            scheduling_constraints: Vec::new(),
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: RoutineCatchupPolicy::Skip,
            streak_counter: 0,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            split_from: None,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    /// The 7 occurrence keys a daily routine anchored at `MON` produces in the
    /// week that starts there.
    fn week_keys() -> Vec<String> {
        (0..7)
            .map(|d| {
                let z = ts(MON + d * 86_400_000).to_zoned(utc());
                format!("{:04}-{:02}-{:02}T00:00", z.year(), z.month(), z.day())
            })
            .collect()
    }

    #[test]
    fn a_routine_completed_every_day_has_zero_drift() {
        let mut r = daily_routine();
        r.streak_keys = week_keys();
        r.streak_keys.sort();
        r.streak_counter = 7;
        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(d.expected, 7);
        assert_eq!(d.completed, 7);
        assert_eq!(d.missed, 0);
        assert!((d.drift - 0.0).abs() < f64::EPSILON);
        assert!(!d.over_threshold);
        assert_eq!(d.streak, 7);
    }

    #[test]
    fn missing_more_than_thirty_percent_trips_the_tuning_threshold() {
        let mut r = daily_routine();
        // 4 of 7 done → 3 missed → drift 3/7 ≈ 0.4286 > 0.30.
        let mut keys = week_keys();
        keys.truncate(4);
        keys.sort();
        r.streak_keys = keys;

        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!((d.expected, d.completed, d.missed), (7, 4, 3));
        assert!((d.drift - 3.0 / 7.0).abs() < 1e-12);
        assert!(d.over_threshold);
    }

    #[test]
    fn exactly_at_the_threshold_does_not_trip_it() {
        let mut r = daily_routine();
        // 7 of 10 done over 10 days → drift exactly 0.30, which is not "> 30%".
        let keys: Vec<String> = (0..10)
            .map(|d| {
                let z = ts(MON + d * 86_400_000).to_zoned(utc());
                format!("{:04}-{:02}-{:02}T00:00", z.year(), z.month(), z.day())
            })
            .take(7)
            .collect();
        r.streak_keys = keys;
        r.streak_keys.sort();
        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + 10 * 86_400_000)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!((d.expected, d.completed, d.missed), (10, 7, 3));
        assert!(!d.over_threshold, "drift was {}", d.drift);
    }

    #[test]
    fn explicit_skips_count_as_missed_but_are_reported_separately() {
        let mut r = daily_routine();
        let keys = week_keys();
        r.streak_keys = keys[..5].to_vec();
        r.streak_keys.sort();
        r.skipped_keys = keys[5..].to_vec();

        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!((d.completed, d.skipped, d.missed), (5, 2, 2));
    }

    #[test]
    fn an_empty_window_yields_zero_drift_and_no_suggestion() {
        let r = daily_routine();
        // A window entirely before the routine's anchor produces nothing.
        let d = routine_drift(
            &r,
            (ts(MON - 10 * WEEK_MS), ts(MON - 9 * WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(d.expected, 0);
        assert!(!d.over_threshold, "no occurrences is not drift");
    }

    /// `title`, `last_completed_at_ms` and `paused` are carried straight
    /// through from the Routine, and nothing asserted any of the three: a
    /// drift row could have reported an empty title, no last completion, and
    /// the wrong paused flag with the suite green. The fixture sets all three
    /// away from their defaults so the plumbing is what is being read.
    #[test]
    fn drift_carries_the_routines_title_last_completion_and_paused_flag() {
        let mut r = daily_routine();
        r.template.title = "Water the ferns".into();
        r.last_completed_at = Some(ts(MON + 3 * 86_400_000).into());
        r.paused = true;
        r.streak_counter = 4;

        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(d.routine, r.id);
        assert_eq!(
            d.title, "Water the ferns",
            "the row is what the History view labels the routine with"
        );
        assert_eq!(
            d.last_completed_at_ms,
            Some(MON + 3 * 86_400_000),
            "epoch milliseconds, straight off the Routine"
        );
        assert!(
            d.paused,
            "a paused routine needs no pause suggestion, so the caller must be told"
        );
        assert_eq!(d.streak, 4);
        // And pausing does **not** suppress the occurrence set: `routine_drift`
        // calls `expand` rather than `Routine::occurrences_in` on purpose, so a
        // paused routine still reports the drift that led to the pause.
        assert_eq!(d.expected, 7, "a paused routine is still measured");
    }

    /// A completion recorded before the epoch cannot be expressed in the
    /// unsigned field, and is reported as "no completion" rather than as a
    /// wrapped instant. Only reachable from a corrupt or hostile row.
    #[test]
    fn a_pre_epoch_last_completion_is_reported_as_none_not_wrapped() {
        let mut r = daily_routine();
        r.last_completed_at = Some(Timestamp::from_millisecond(-1).unwrap().into());
        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(d.last_completed_at_ms, None);
    }

    /// `ends_at` truncates the occurrence set, so a routine that finished
    /// mid-window is not scored against the days it was never meant to run.
    /// Without this, a finished routine reads as 100% drift forever.
    #[test]
    fn an_ends_at_inside_the_window_truncates_the_expected_occurrences() {
        let mut r = daily_routine();
        // Four days of a seven-day window: MON, +1, +2, +3. The bound is
        // inclusive (`o.at <= end`), so the occurrence landing exactly on
        // `ends_at` counts.
        r.ends_at = Some(ts(MON + 3 * 86_400_000).into());
        r.streak_keys = week_keys()[..4].to_vec();
        r.streak_keys.sort();

        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(
            d.expected, 4,
            "the three days after `ends_at` are not occurrences"
        );
        assert_eq!(d.completed, 4);
        assert_eq!(d.missed, 0, "a routine that ended has not drifted");
        assert!(!d.over_threshold);

        // The same routine without the bound is scored over the whole window,
        // which is what makes the truncation load-bearing rather than a no-op.
        let mut unbounded = r.clone();
        unbounded.ends_at = None;
        let d2 = routine_drift(
            &unbounded,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!((d2.expected, d2.missed), (7, 3));
    }

    /// `skip_dates` holds *instants*, and they are matched by rendering each
    /// one to a civil-minute key in the routine's own zone — the same way
    /// `Routine::is_skipped` does it. The comment in `routine_drift` says this
    /// is so a stored instant a tzdb update moved still matches the occurrence
    /// it was meant to skip; nothing asserted that the path runs at all.
    #[test]
    fn a_skip_date_instant_is_matched_by_civil_key_not_by_equality() {
        let mut r = daily_routine();
        // Wednesday of the anchored week, as an instant rather than a key.
        r.skip_dates = vec![ts(MON + 2 * 86_400_000)];

        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(d.expected, 7);
        assert_eq!(
            d.skipped, 1,
            "the instant resolved to an occurrence key and matched it"
        );
        assert_eq!(
            d.missed, 7,
            "an explicit skip is still a missed occurrence — `skipped` only says why"
        );
        // The key it must have produced, spelled out: the same civil minute
        // the expansion names that occurrence by.
        assert_eq!(
            occurrence_key_at("UTC", ts(MON + 2 * 86_400_000)).unwrap(),
            week_keys()[2]
        );
    }

    /// The key is a civil **minute**, which decides how close a stored instant
    /// has to be. A second's drift still names the same occurrence; a minute's
    /// names none, and is ignored rather than skipping something adjacent.
    #[test]
    fn a_skip_dates_precision_is_the_civil_minute() {
        let mut within = daily_routine();
        within.skip_dates = vec![ts(MON + 2 * 86_400_000 + 1_000)];
        let d = routine_drift(
            &within,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(d.skipped, 1, "a second's drift is the same civil minute");

        let mut outside = daily_routine();
        outside.skip_dates = vec![ts(MON + 2 * 86_400_000 + 60_000)];
        let d = routine_drift(
            &outside,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &TimeZone::UTC,
        )
        .unwrap();
        assert_eq!(
            d.skipped, 0,
            "a minute's drift names no occurrence, and skips nothing rather than \
             skipping the nearest one"
        );
    }

    /// A routine whose zone this build does not know is not read in UTC: its
    /// legacy instant anchor has no zone to pin to, so it reads as floating
    /// (`crate::routine::read_legacy_instant`) and its occurrences fall where
    /// the reader is.
    #[test]
    fn an_unresolvable_timezone_reads_in_the_readers_zone_not_utc() {
        let mut r = daily_routine();
        r.timezone = "Mars/Olympus".into();
        let kolkata = TimeZone::get("Asia/Kolkata").unwrap();
        let d = routine_drift(
            &r,
            (ts(MON), ts(MON + WEEK_MS)),
            DEFAULT_DRIFT_THRESHOLD,
            &kolkata,
        )
        .unwrap();
        assert_eq!(d.expected, 7);
        let occ = r.series_in((ts(MON), ts(MON + WEEK_MS)), &kolkata).unwrap();
        let first = occ.first().unwrap();
        assert_eq!(
            first.at.to_zoned(kolkata).datetime().time(),
            first.civil.time(),
            "at its wall clock in the reader's zone"
        );
    }
}
