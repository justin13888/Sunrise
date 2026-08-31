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
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Default completion grace window: 24 hours.
pub const DEFAULT_GRACE_WINDOW_S: u64 = 24 * 60 * 60;

/// Upper bound on a per-Routine grace window: 7 days.
pub const MAX_GRACE_WINDOW_S: u64 = 7 * 24 * 60 * 60;

/// Length of the rolling forgiveness window: 30 days, in seconds.
pub const FORGIVENESS_WINDOW_S: i64 = 30 * 24 * 60 * 60;

/// Forgivenesses allowed per rolling window.
pub const FORGIVENESS_ALLOWANCE: u32 = 1;

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
        self.last_completed_at = Some(now);

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
                stream_id: EntityRef::new(EntityKind::Stream, [2u8; 16]),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse("FREQ=DAILY").unwrap(),
            timezone: "UTC".into(),
            starts_at: ts(T0),
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
        assert_eq!(r.last_completed_at, Some(ts(T0 + 60)));
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
}
