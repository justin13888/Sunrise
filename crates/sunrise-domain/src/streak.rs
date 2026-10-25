//! Routine streak mechanics per `docs/08-features/recurrence-engine.md`
//! §Streak counter and `docs/02-domain/routines-and-recurrence.md`
//! §Streak counter.
//!
//! Three rules, all implemented here as one pure state transition on a
//! [`Routine`] so they unit-test without storage, clock, or sync:
//!
//! 1. **Grace window.** Completing an occurrence at or before
//!    `occurrence_at + grace_window` advances the streak by one. The window is
//!    per-Routine ([`Routine::grace_window_s`]), defaults to 24h, and is
//!    clamped to 7 days.
//! 2. **Idempotency keys.** Only the *first* completion of a given occurrence
//!    moves the counter. Every later `done → todo → done` on the same
//!    occurrence hits the key set and is a no-op, which is also what makes
//!    resurrection streak-neutral and what makes two devices completing the
//!    same occurrence concurrently produce one effective increment rather than
//!    two.
//! 3. **Forgiveness.** A completion *outside* the grace window is a missed
//!    occurrence and normally resets the streak to 0. The forgiveness rule
//!    (on by default, per-Routine toggle) absorbs one miss per rolling 30-day
//!    window instead, anchored at [`Routine::streak_started_at`].
//!
//! The caller feeds `now` from the injected `Clock`; nothing here reads a wall
//! clock.

use crate::routine::Routine;
use crate::time::SunriseTime;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Default completion grace window: 24 hours.
pub const DEFAULT_GRACE_WINDOW_S: u64 = 24 * 60 * 60;

/// Upper bound on a per-Routine grace window: 7 days.
pub const MAX_GRACE_WINDOW_S: u64 = 7 * 24 * 60 * 60;

/// Length of the rolling forgiveness window: 30 days, in seconds.
pub const FORGIVENESS_WINDOW_S: i64 = 30 * 24 * 60 * 60;

/// Forgivenesses allowed per rolling window.
pub const FORGIVENESS_ALLOWANCE: u32 = 1;

/// A Routine's streak, for step 3 and the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreakRow {
    /// Routine id.
    pub routine: EntityRef,
    /// Template title.
    pub title: String,
    /// Current streak counter.
    pub streak: i64,
    /// Last completion (ms since epoch).
    pub last_completed_at_ms: Option<u64>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted (ADR-0045 §6): a row crosses
    /// the wire inside a saved review snapshot.
    #[serde(flatten)]
    pub unknown: crate::unknown::Unknowns,
}

/// What a completion did to the streak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreakOutcome {
    /// The occurrence was already counted; nothing changed.
    Duplicate,
    /// Completed within grace: counter +1.
    Incremented,
    /// Completed late, but the forgiveness rule absorbed the miss: the counter
    /// is held, not advanced.
    Forgiven,
    /// Completed late with no forgiveness left: counter reset to 0.
    Reset,
}

impl Routine {
    /// Effective grace window in seconds (default applied, clamped to
    /// [`MAX_GRACE_WINDOW_S`]).
    #[must_use]
    pub fn grace_window_seconds(&self) -> u64 {
        self.grace_window_s
            .unwrap_or(DEFAULT_GRACE_WINDOW_S)
            .min(MAX_GRACE_WINDOW_S)
    }

    /// Has this occurrence already been counted toward the streak?
    #[must_use]
    pub fn streak_key_seen(&self, occurrence_key: &str) -> bool {
        self.streak_keys
            .binary_search_by(|k| (**k).cmp(occurrence_key))
            .is_ok()
    }

    /// Record the first `pending → done` transition of one occurrence and move
    /// the streak accordingly.
    ///
    /// `occurrence_key` is the routine-relative idempotency key (the
    /// `YYYY-MM-DDTHH:MM` occurrence key from
    /// [`crate::routine_gen::Occurrence::key`]); `occurrence_at` is when the
    /// occurrence was due; `now` comes from the injected clock.
    ///
    /// Returns [`StreakOutcome::Duplicate`] and leaves the Routine untouched
    /// when the key has been seen before — the caller can then skip emitting an
    /// op entirely.
    pub fn record_occurrence_completion(
        &mut self,
        occurrence_key: &str,
        occurrence_at: Timestamp,
        now: Timestamp,
    ) -> StreakOutcome {
        let Err(insert_at) = self
            .streak_keys
            .binary_search_by(|k| (**k).cmp(occurrence_key))
        else {
            return StreakOutcome::Duplicate;
        };
        self.streak_keys
            .insert(insert_at, occurrence_key.to_string());

        self.slide_forgiveness_window(now);
        self.last_completed_at = Some(now.into());

        let deadline = occurrence_at
            .as_second()
            .saturating_add(i64::try_from(self.grace_window_seconds()).unwrap_or(i64::MAX));
        if now.as_second() <= deadline {
            self.streak_counter = self.streak_counter.saturating_add(1);
            if self.streak_started_at.is_none() {
                self.streak_started_at = Some(now);
            }
            return StreakOutcome::Incremented;
        }

        // Past grace: a missed occurrence. Re-completing it later never
        // retroactively repairs the streak — it either burns forgiveness or
        // resets.
        if self.forgiveness_enabled && self.forgivenesses_in_window < FORGIVENESS_ALLOWANCE {
            self.forgivenesses_in_window += 1;
            return StreakOutcome::Forgiven;
        }
        self.streak_counter = 0;
        self.streak_started_at = None;
        self.forgivenesses_in_window = 0;
        StreakOutcome::Reset
    }

    /// Advance the forgiveness anchor while `now` sits more than 30 days past
    /// it, resetting the consumed count on each advance.
    ///
    /// The spec states one advance (`streak_started_at += 30 days`); a device
    /// that was offline for months would otherwise need several. We advance by
    /// whole windows in one step, which is the same fixed point.
    fn slide_forgiveness_window(&mut self, now: Timestamp) {
        let Some(anchor) = self.streak_started_at else {
            return;
        };
        let elapsed = now.as_second().saturating_sub(anchor.as_second());
        if elapsed <= FORGIVENESS_WINDOW_S {
            return;
        }
        let windows = elapsed / FORGIVENESS_WINDOW_S;
        let advanced = anchor
            .as_second()
            .saturating_add(windows.saturating_mul(FORGIVENESS_WINDOW_S));
        if let Ok(ts) = Timestamp::from_second(advanced) {
            self.streak_started_at = Some(ts);
        }
        self.forgivenesses_in_window = 0;
    }
}

// ---- derived from the keys (ADR-0044 §3, #331) ----

/// A routine's streak fields as its keys add up to.
///
/// [`Routine::derived_streak`] computes it from `streak_keys`, `skipped_keys`
/// (and the deprecated `skip_dates`), the rule and the forgiveness setting:
/// the data the merge keeps every write of, so two devices that complete
/// different occurrences concurrently both count. The merge projects these
/// four fields from it once a field-level write has written the keys
/// (ADR-0065 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedStreak {
    /// [`Routine::streak_counter`].
    pub streak_counter: i64,
    /// [`Routine::last_completed_at`]: the instant the newest counted
    /// occurrence was due at, not the moment it was completed, which no key
    /// records.
    pub last_completed_at: Option<SunriseTime>,
    /// [`Routine::streak_started_at`]: when the current run's first
    /// occurrence was due.
    pub streak_started_at: Option<Timestamp>,
    /// [`Routine::forgivenesses_in_window`].
    pub forgivenesses_in_window: u32,
}

impl DerivedStreak {
    /// The four fields as `routine` holds them.
    #[must_use]
    pub fn of(routine: &Routine) -> Self {
        Self {
            streak_counter: routine.streak_counter,
            last_completed_at: routine.last_completed_at.clone(),
            streak_started_at: routine.streak_started_at,
            forgivenesses_in_window: routine.forgivenesses_in_window,
        }
    }

    /// Write the four fields onto `routine`.
    pub fn apply_to(self, routine: &mut Routine) {
        routine.streak_counter = self.streak_counter;
        routine.last_completed_at = self.last_completed_at;
        routine.streak_started_at = self.streak_started_at;
        routine.forgivenesses_in_window = self.forgivenesses_in_window;
    }
}

impl Routine {
    /// The streak the keys add up to, as of the newest key: a function of the
    /// data alone, read the same on every replica, with no clock.
    ///
    /// The occurrences of the rule up to the newest key in `streak_keys` are
    /// walked in civil order, keyed in the zone the routine resolves in
    /// (UTC for a floating one, so the walk does not depend on the reader):
    ///
    /// - a **keyed** occurrence counts: the counter moves up one, and the
    ///   run's start and the last completion move with it;
    /// - a **skipped** one (`skipped_keys`, `skip_dates`) is neutral;
    /// - any other is a **miss**: it spends the forgiveness allowance of the
    ///   rolling 30-day window, or ends the run.
    ///
    /// A key the rule does not produce (the rule changed after it was
    /// counted) still counts, at its own place in the order. A miss before
    /// any run started changes nothing.
    ///
    /// No key records when its completion happened, so the grace window does
    /// not apply here: a late completion counts, and an occurrence completed
    /// after a later one first reads as a miss and stops being one when its
    /// key arrives. The grace window applies at read time, to the
    /// occurrences after the newest key ([`Routine::streak_as_of`]).
    #[must_use]
    pub fn derived_streak(&self) -> DerivedStreak {
        let utc = jiff::tz::TimeZone::UTC;
        let zone = self.occurrence_zone(&utc);
        // Keys are minute-precision civil times, `YYYY-MM-DDTHH:MM`, so their
        // text sorts in time order and names an occurrence whatever seconds
        // its anchor has.
        let keyed: std::collections::BTreeMap<&str, jiff::civil::DateTime> = self
            .streak_keys
            .iter()
            .filter_map(|k| Some((k.as_str(), crate::routine_gen::parse_occurrence_key(k)?)))
            .collect();
        let mut fresh = self.clone();
        DerivedStreak {
            streak_counter: 0,
            last_completed_at: None,
            streak_started_at: None,
            forgivenesses_in_window: 0,
        }
        .apply_to(&mut fresh);
        let Some((newest, newest_civil)) = keyed.last_key_value().map(|(k, c)| (*k, *c)) else {
            return DerivedStreak::of(&fresh);
        };
        let place = |civil: jiff::civil::DateTime| -> Option<Timestamp> {
            zone.to_ambiguous_zoned(civil)
                .compatible()
                .ok()
                .map(|z| z.timestamp())
        };
        // Every occurrence up to the newest key, keyed or not, by key.
        let mut walk: std::collections::BTreeMap<String, (Timestamp, bool)> =
            std::collections::BTreeMap::new();
        let end = place(newest_civil)
            .and_then(|t| t.checked_add(jiff::SignedDuration::from_mins(1)).ok())
            .unwrap_or(Timestamp::MAX);
        for o in self
            .series_in((Timestamp::MIN, end), &utc)
            .unwrap_or_default()
        {
            let counted = keyed.contains_key(o.key.as_str());
            if o.key.as_str() > newest || (!counted && self.is_skipped(&o, &zone)) {
                continue;
            }
            walk.insert(o.key.clone(), (o.at, counted));
        }
        for (key, civil) in &keyed {
            if let Some(entry) = walk.get_mut(*key) {
                entry.1 = true;
            } else if let Some(at) = place(*civil) {
                walk.insert((*key).to_owned(), (at, true));
            }
        }
        for (at, counted) in walk.into_values() {
            if counted {
                fresh.count_occurrence(at);
            } else {
                fresh.miss_occurrence(at);
            }
        }
        DerivedStreak::of(&fresh)
    }

    /// This routine's streak as of `now` for a reader in `reader`: its
    /// streak fields, with every occurrence after the newest key whose grace
    /// window closed before `now`, and that is neither counted nor skipped
    /// nor paused, applied as a miss.
    ///
    /// The stored fields hold the streak as of the newest key, whichever way
    /// they were written ([`Routine::derived_streak`], or the completion that
    /// wrote them); this is what a read shows, so a run that lapsed reads as
    /// lapsed before the next completion says so.
    #[must_use]
    pub fn streak_as_of(&self, now: Timestamp, reader: &jiff::tz::TimeZone) -> DerivedStreak {
        let held = DerivedStreak::of(self);
        if self.streak_counter == 0 {
            return held;
        }
        let zone = self.occurrence_zone(reader);
        let after = self
            .streak_keys
            .iter()
            .filter_map(|k| crate::routine_gen::parse_occurrence_key(k))
            .max()
            .and_then(|c| zone.to_ambiguous_zoned(c).compatible().ok())
            .map(|z| z.timestamp());
        let Some(after) = after else {
            return held;
        };
        let grace = jiff::SignedDuration::from_secs(
            i64::try_from(self.grace_window_seconds()).unwrap_or(i64::MAX),
        );
        let Ok(closed_before) = now.checked_sub(grace) else {
            return held;
        };
        if closed_before <= after {
            return held;
        }
        let Ok(start) = after.checked_add(jiff::SignedDuration::from_secs(1)) else {
            return held;
        };
        let mut out = self.clone();
        for o in self
            .occurrences_in((start, closed_before), reader)
            .unwrap_or_default()
        {
            if !self.streak_key_seen(&o.key) {
                out.miss_occurrence(o.at);
            }
        }
        DerivedStreak::of(&out)
    }

    /// One counted occurrence, due at `at`, in the walk of
    /// [`Routine::derived_streak`].
    fn count_occurrence(&mut self, at: Timestamp) {
        self.slide_forgiveness_window(at);
        self.streak_counter = self.streak_counter.saturating_add(1);
        if self.streak_started_at.is_none() {
            self.streak_started_at = Some(at);
        }
        self.last_completed_at = Some(at.into());
    }

    /// One missed occurrence, due at `at`: forgiven while the window has
    /// allowance left, the end of the run otherwise. Nothing while no run
    /// has started.
    fn miss_occurrence(&mut self, at: Timestamp) {
        if self.streak_counter == 0 {
            return;
        }
        self.slide_forgiveness_window(at);
        if self.forgiveness_enabled && self.forgivenesses_in_window < FORGIVENESS_ALLOWANCE {
            self.forgivenesses_in_window += 1;
            return;
        }
        self.streak_counter = 0;
        self.streak_started_at = None;
        self.forgivenesses_in_window = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
    use crate::rrule::RRule;
    use crate::unknown::Unknowns;
    use sunrise_id::{EntityKind, EntityRef};

    const T0: i64 = 1_700_000_000;
    const DAY: i64 = 24 * 60 * 60;

    fn ts(secs: i64) -> Timestamp {
        Timestamp::from_second(secs).unwrap()
    }

    /// The default grace window as signed seconds, for arithmetic in tests.
    fn grace() -> i64 {
        i64::try_from(DEFAULT_GRACE_WINDOW_S).unwrap()
    }

    fn routine() -> Routine {
        Routine {
            id: EntityRef::new(EntityKind::Routine, [1u8; 16]),
            created_at: ts(T0),
            updated_at: ts(T0),
            template: TaskTemplate {
                title: "Stretch".into(),
                stream_id: Some(EntityRef::new(EntityKind::Stream, [2u8; 16])),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: crate::Unknowns::new(),
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: ts(T0).into(),
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
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    #[test]
    fn grace_window_defaults_to_24h_and_clamps_at_7_days() {
        let mut r = routine();
        assert_eq!(r.grace_window_seconds(), DEFAULT_GRACE_WINDOW_S);
        r.grace_window_s = Some(u64::try_from(365 * DAY).unwrap());
        assert_eq!(r.grace_window_seconds(), MAX_GRACE_WINDOW_S);
        r.grace_window_s = Some(0);
        assert_eq!(r.grace_window_seconds(), 0);
    }

    #[test]
    fn on_time_completion_increments_and_anchors() {
        let mut r = routine();
        let out = r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0 + 60));
        assert_eq!(out, StreakOutcome::Incremented);
        assert_eq!(r.streak_counter, 1);
        assert_eq!(r.streak_started_at, Some(ts(T0 + 60)));
        assert_eq!(r.last_completed_at, Some(ts(T0 + 60).into()));
    }

    #[test]
    fn streak_survives_a_gap_inside_the_grace_window() {
        let mut r = routine();
        r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0));
        // Next occurrence a day later, completed 23h59m late — still inside the
        // 24h grace window.
        let out = r.record_occurrence_completion(
            "2026-08-22T09:00",
            ts(T0 + DAY),
            ts(T0 + DAY + grace() - 60),
        );
        assert_eq!(out, StreakOutcome::Incremented);
        assert_eq!(r.streak_counter, 2, "streak keeps climbing inside grace");
    }

    #[test]
    fn a_miss_outside_grace_burns_forgiveness_then_resets() {
        let mut r = routine();
        r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0));
        assert_eq!(r.streak_counter, 1);

        // 25h late: outside grace. Forgiveness absorbs it — counter held.
        let out = r.record_occurrence_completion(
            "2026-08-22T09:00",
            ts(T0 + DAY),
            ts(T0 + DAY + grace() + 3600),
        );
        assert_eq!(out, StreakOutcome::Forgiven);
        assert_eq!(r.streak_counter, 1, "forgiven miss does not reset");
        assert_eq!(r.forgivenesses_in_window, 1);

        // A second miss in the same 30-day window has no forgiveness left.
        let out = r.record_occurrence_completion(
            "2026-08-23T09:00",
            ts(T0 + 2 * DAY),
            ts(T0 + 2 * DAY + grace() + 3600),
        );
        assert_eq!(out, StreakOutcome::Reset);
        assert_eq!(r.streak_counter, 0);
        assert_eq!(r.streak_started_at, None);
    }

    #[test]
    fn forgiveness_disabled_resets_on_the_first_miss() {
        let mut r = routine();
        r.forgiveness_enabled = false;
        r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0));
        let out = r.record_occurrence_completion(
            "2026-08-22T09:00",
            ts(T0 + DAY),
            ts(T0 + DAY + 10 * DAY),
        );
        assert_eq!(out, StreakOutcome::Reset);
        assert_eq!(r.streak_counter, 0);
    }

    #[test]
    fn forgiveness_allowance_refreshes_when_the_window_slides() {
        let mut r = routine();
        r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0));
        r.record_occurrence_completion(
            "2026-08-22T09:00",
            ts(T0 + DAY),
            ts(T0 + DAY + grace() + 3600),
        );
        assert_eq!(r.forgivenesses_in_window, 1);

        // 40 days on, the anchor advances one whole window and the allowance
        // comes back, so this late completion is forgiven rather than a reset.
        let late = T0 + 40 * DAY;
        let out = r.record_occurrence_completion("2026-09-30T09:00", ts(late - 5 * DAY), ts(late));
        assert_eq!(out, StreakOutcome::Forgiven);
        assert_eq!(r.streak_counter, 1);
        assert_eq!(r.streak_started_at, Some(ts(T0 + FORGIVENESS_WINDOW_S)));
        assert_eq!(r.forgivenesses_in_window, 1);
    }

    #[test]
    fn re_completing_the_same_occurrence_is_a_no_op() {
        let mut r = routine();
        r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0));
        let before = r.clone();
        // done → todo → done on the same occurrence.
        let out = r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0 + 5 * DAY));
        assert_eq!(out, StreakOutcome::Duplicate);
        assert_eq!(r, before, "resurrection leaves the streak untouched");
        assert!(r.streak_key_seen("2026-08-21T09:00"));
        assert!(!r.streak_key_seen("2026-08-22T09:00"));
    }

    #[test]
    fn keys_stay_sorted_for_canonical_encoding() {
        let mut r = routine();
        r.record_occurrence_completion("2026-08-23T09:00", ts(T0), ts(T0));
        r.record_occurrence_completion("2026-08-21T09:00", ts(T0), ts(T0));
        r.record_occurrence_completion("2026-08-22T09:00", ts(T0), ts(T0));
        assert_eq!(
            r.streak_keys,
            vec![
                "2026-08-21T09:00".to_string(),
                "2026-08-22T09:00".to_string(),
                "2026-08-23T09:00".to_string()
            ],
            "insertion order must not leak into the encoded state"
        );
    }

    // ---- derived from the keys ----

    /// A daily 09:00 UTC routine from 2026-08-01, with `days` of August
    /// counted.
    fn daily(days: &[i8]) -> Routine {
        let mut r = routine();
        r.starts_at = SunriseTime::zoned(jiff::civil::date(2026, 8, 1).at(9, 0, 0, 0), "UTC");
        r.streak_keys = days
            .iter()
            .map(|d| format!("2026-08-{d:02}T09:00"))
            .collect();
        r.streak_keys.sort();
        r
    }

    fn at(day: i8, hour: i8) -> Timestamp {
        jiff::civil::date(2026, 8, day)
            .at(hour, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    #[test]
    fn consecutive_keys_count_one_each() {
        let s = daily(&[1, 2, 3, 4, 5]).derived_streak();
        assert_eq!(s.streak_counter, 5);
        assert_eq!(s.streak_started_at, Some(at(1, 9)));
        assert_eq!(s.last_completed_at, Some(at(5, 9).into()));
        assert_eq!(s.forgivenesses_in_window, 0);
        assert_eq!(
            daily(&[]).derived_streak(),
            DerivedStreak {
                streak_counter: 0,
                last_completed_at: None,
                streak_started_at: None,
                forgivenesses_in_window: 0,
            }
        );
    }

    #[test]
    fn a_gap_between_keys_is_a_miss_that_forgiveness_absorbs_once() {
        let s = daily(&[1, 2, 4]).derived_streak();
        assert_eq!((s.streak_counter, s.forgivenesses_in_window), (3, 1));
        // A second miss in the window ends the run; it starts again after.
        let s = daily(&[1, 2, 4, 6, 7]).derived_streak();
        assert_eq!((s.streak_counter, s.forgivenesses_in_window), (2, 0));
        assert_eq!(s.streak_started_at, Some(at(6, 9)));
        let mut strict = daily(&[1, 2, 4]);
        strict.forgiveness_enabled = false;
        let s = strict.derived_streak();
        assert_eq!(s.streak_counter, 1);
        assert_eq!(s.streak_started_at, Some(at(4, 9)));
    }

    #[test]
    fn keys_name_occurrences_whatever_seconds_the_anchor_has() {
        let mut r = daily(&[1, 2, 3]);
        r.starts_at = SunriseTime::zoned(jiff::civil::date(2026, 8, 1).at(9, 0, 20, 0), "UTC");
        let s = r.derived_streak();
        assert_eq!((s.streak_counter, s.forgivenesses_in_window), (3, 0));
    }

    #[test]
    fn a_miss_before_any_run_spends_nothing() {
        // The 1st was never completed; the run starts on the 2nd.
        let s = daily(&[2, 3]).derived_streak();
        assert_eq!((s.streak_counter, s.forgivenesses_in_window), (2, 0));
        assert_eq!(s.streak_started_at, Some(at(2, 9)));
    }

    #[test]
    fn a_skipped_occurrence_is_neither_a_count_nor_a_miss() {
        let mut r = daily(&[1, 2, 4]);
        r.skipped_keys = vec!["2026-08-03T09:00".into()];
        let s = r.derived_streak();
        assert_eq!((s.streak_counter, s.forgivenesses_in_window), (3, 0));
    }

    #[test]
    fn a_key_the_rule_does_not_produce_still_counts() {
        let mut r = daily(&[1, 2]);
        r.streak_keys.push("2026-08-02T15:30".into());
        r.streak_keys.sort();
        assert_eq!(r.derived_streak().streak_counter, 3);
    }

    #[test]
    fn the_derivation_reads_only_the_data() {
        // The stored fields are not inputs: whatever they say, the keys win.
        let mut r = daily(&[1, 2, 3]);
        r.streak_counter = 40;
        r.forgivenesses_in_window = 1;
        r.streak_started_at = Some(at(2, 0));
        assert_eq!(r.derived_streak(), daily(&[1, 2, 3]).derived_streak());
    }

    #[test]
    fn as_of_now_a_lapsed_run_reads_as_lapsed() {
        let utc = jiff::tz::TimeZone::UTC;
        let mut r = daily(&[1, 2, 3]);
        r.derived_streak().apply_to(&mut r);
        assert_eq!(r.streak_counter, 3);
        // The 4th's grace window has not closed.
        assert_eq!(r.streak_as_of(at(4, 12), &utc), DerivedStreak::of(&r));
        assert_eq!(r.streak_as_of(at(5, 9), &utc), DerivedStreak::of(&r));
        // Closed: forgiven.
        let s = r.streak_as_of(at(5, 10), &utc);
        assert_eq!((s.streak_counter, s.forgivenesses_in_window), (3, 1));
        // Two more closed: the run is over.
        let s = r.streak_as_of(at(7, 10), &utc);
        assert_eq!((s.streak_counter, s.streak_started_at), (0, None));
        // A skipped or paused occurrence is no miss.
        let mut skip = r.clone();
        skip.skipped_keys = vec!["2026-08-04T09:00".into()];
        assert_eq!(skip.streak_as_of(at(5, 10), &utc), DerivedStreak::of(&r));
        let mut paused = r.clone();
        paused.paused = true;
        assert_eq!(paused.streak_as_of(at(9, 10), &utc), DerivedStreak::of(&r));
    }
}
