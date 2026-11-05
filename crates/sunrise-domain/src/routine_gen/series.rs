//! A routine's series: the rule's expansion ([`super::expand`]) applied to one
//! [`Routine`], with its anchor read in civil space, the zone its occurrences
//! resolve in, its end, its pause and its skips; the intended key of a
//! materialized occurrence; and the Routines list's rows.
//!
//! Apart from the expansion because it changes with what a routine holds
//! (#506 made its times `SunriseTime`s), and the expansion with what a rule
//! means.

use super::{expand, format_key, occurrence_task_id, within_bound, ExpandError, Occurrence};
use crate::routine::Routine;
use crate::rrule::RRule;
use crate::time::SunriseTime;
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_id::EntityRef;

impl Routine {
    /// Every occurrence of this routine's rule within `window`, resolved for
    /// a reader in `reader`, before the end, pause and skip filters of
    /// [`Routine::occurrences_in`]: the series as the rule and the anchor
    /// define it. The routine's legacy instants are read as
    /// [`Routine::normalize_legacy_times`] reads them.
    ///
    /// # Errors
    ///
    /// [`ExpandError::WindowTooLarge`] from [`expand`].
    pub fn series_in(
        &self,
        window: (Timestamp, Timestamp),
        reader: &TimeZone,
    ) -> Result<Vec<Occurrence>, ExpandError> {
        let r = self.clone().normalized();
        let Some(anchor) = r.anchor_civil() else {
            return Ok(Vec::new());
        };
        let tz = r.occurrence_zone(reader);
        let mut occ = expand(&r.rrule, anchor, &tz, window)?;
        if let Some(end) = &r.ends_at {
            occ.retain(|o| within_bound(end, o.civil, o.at, &tz).unwrap_or(false));
        }
        Ok(occ)
    }

    /// Expand this routine's occurrences within `window` for a reader in
    /// `reader`: its anchor and rule, its `ends_at`, its pause, and its skip
    /// filters (both explicit skipped keys and `skip_dates`). An archived or
    /// deleted routine yields no occurrences, and neither does a paused one
    /// with no `paused_until`. A paused one with a `paused_until` yields the
    /// occurrences at or after it ([`Routine::is_paused_at`]).
    ///
    /// # Errors
    ///
    /// [`ExpandError::WindowTooLarge`] from [`expand`].
    pub fn occurrences_in(
        &self,
        window: (Timestamp, Timestamp),
        reader: &TimeZone,
    ) -> Result<Vec<Occurrence>, ExpandError> {
        if self.archived || self.deleted || (self.paused && self.paused_until.is_none()) {
            return Ok(Vec::new());
        }
        let mut occ = self.series_in(window, reader)?;
        let tz = self.occurrence_zone(reader);
        occ.retain(|o| !self.is_paused_at(o, reader) && !self.is_skipped(o, &tz));
        Ok(occ)
    }

    /// Whether the pause holds `occ` back, for a reader in `reader`.
    ///
    /// Only while [`Routine::paused`] is set. With no `paused_until` every
    /// occurrence is paused. With one, an occurrence before it is paused and
    /// one at or after it is generated, so a pause that has run out resumes
    /// generation without anyone unpausing it (`docs/10-cross-cutting/time.md`
    /// §6): a `floating` expiry is compared with the key's wall clock, an
    /// `all_day` one ends at the start of that day in the reader's zone, and
    /// an instant or `zoned` one on the timeline. An expiry of a kind this
    /// build cannot place pauses everything, the reading that generates
    /// nothing a newer build may not have meant.
    #[must_use]
    pub fn is_paused_at(&self, occ: &Occurrence, reader: &TimeZone) -> bool {
        if !self.paused {
            return false;
        }
        match &self.paused_until {
            None => true,
            Some(SunriseTime::Floating { civil }) => occ.civil < *civil,
            Some(until) => until.resolve_in(reader).is_none_or(|end| occ.at < end),
        }
    }

    /// Whether `occ` is skipped, matching either an explicit skipped key or a
    /// `skip_dates` timestamp by identical civil minute in the zone it
    /// resolves in.
    pub(crate) fn is_skipped(&self, occ: &Occurrence, tz: &TimeZone) -> bool {
        if self.skipped_keys.iter().any(|k| k == &occ.key) {
            return true;
        }
        self.skip_dates.iter().any(|ts| {
            let dt = ts.to_zoned(tz.clone()).datetime();
            format_key(dt.date(), dt.time()) == occ.key
        })
    }

    /// The intended key of the occurrence whose task is `task`, given the
    /// instant it was due at (`Task.routine_occurrence`), or `None` when no
    /// occurrence of the rule near that instant has that task id.
    ///
    /// The task id is a function of the key ([`occurrence_task_id`]), so the
    /// key is recovered exactly: by expanding the series around `near` and
    /// finding the key whose id is `task`. This is what the streak counts and
    /// what a skip names, so the task id, the skip and the streak share one
    /// key even on a day the DST rule moved the occurrence (#331), and
    /// whichever zone the occurrence was due in. Every key resolves in UTC
    /// within 26 hours of where any real zone resolves it, so three days each
    /// side of `near` hold it.
    #[must_use]
    pub fn occurrence_key_of(&self, task: &EntityRef, near: Timestamp) -> Option<String> {
        let span = jiff::SignedDuration::from_hours(72);
        let window = (
            near.checked_sub(span).unwrap_or(Timestamp::MIN),
            near.checked_add(span).unwrap_or(Timestamp::MAX),
        );
        let utc = TimeZone::UTC;
        let r = self.clone().normalized();
        let anchor = r.anchor_civil()?;
        expand(&r.rrule, anchor, &utc, window)
            .ok()?
            .into_iter()
            .find(|o| occurrence_task_id(&self.series_root(), &o.key) == *task)
            .map(|o| o.key)
    }

    /// The instant the occurrence keyed at wall-clock `civil` is due at, for a
    /// reader in `reader`: its planned time ([`Routine::occurrence_time`])
    /// resolved there.
    #[must_use]
    pub fn occurrence_instant(&self, civil: DateTime, reader: &TimeZone) -> Option<Timestamp> {
        self.clone()
            .normalized()
            .occurrence_time(civil)
            .resolve_in(reader)
    }
}

/// Parse an occurrence key (`YYYY-MM-DDTHH:MM`) back into the wall-clock value
/// it names; `None` for text that is not one.
#[must_use]
pub fn parse_occurrence_key(key: &str) -> Option<DateTime> {
    let (date, time) = key.split_once('T')?;
    let date: Date = date.parse().ok()?;
    let (h, m) = time.split_once(':')?;
    let tod = Time::new(h.parse().ok()?, m.parse().ok()?, 0, 0).ok()?;
    Some(DateTime::from_parts(date, tod))
}

/// One projected row of the Routines list: the routine's template title, a
/// human-readable RRULE summary, and its next occurrence.
///
/// The parsed rule and the whole template are carried alongside the prose
/// because both are lossy in the other direction: `RoutinePatch.template`
/// replaces the *whole* template, so an edit that only changes a title must
/// still send back the stream, priority and contexts it did not touch, and
/// [`crate::rrule::rrule_summary`] cannot be parsed back into the rule it
/// described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineRow {
    /// Routine id.
    pub id: EntityRef,
    /// Template title.
    pub title: String,
    /// Human-readable recurrence summary (e.g. `every 2 weeks on Mo, We`).
    pub rrule: String,
    /// Next occurrence at or after "now", if one exists inside the routine's
    /// materialization horizon.
    pub next: Option<Timestamp>,
    /// Whether the routine is paused (no occurrences are generated).
    pub paused: bool,
    /// The routine's task template, kept so an edit patches the fields the
    /// user changed and leaves the rest exactly as they were.
    pub template: crate::routine::TaskTemplate,
    /// The parsed recurrence, kept because the summary string is lossy and
    /// cannot be patched back.
    pub rule: RRule,
    /// Current streak counter, so a Routines list can answer "am I keeping
    /// this up?" without a second query per row.
    pub streak: i64,
}

/// Project a routine list into [`RoutineRow`]s, resolving each routine's next
/// occurrence at or after `now` for a reader in `reader` (a floating
/// routine's next occurrence is at its wall-clock time there).
///
/// The lookahead window is the routine's own per-FREQ materialization horizon
/// (`docs/02-domain/routines-and-recurrence.md`), so a yearly routine still
/// resolves while a daily one stays cheap. Pure over `now` — no wall clock is
/// read here, which keeps the projection deterministic and unit-testable, and
/// is why every client can share it.
#[must_use]
pub fn routine_rows(routines: &[Routine], now: Timestamp, reader: &TimeZone) -> Vec<RoutineRow> {
    routines
        .iter()
        .map(|r| {
            let horizon_h =
                i64::from(crate::routine::materialization_horizon_days(&r.rrule.freq)) * 24;
            let next = now
                .checked_add(jiff::SignedDuration::from_hours(horizon_h))
                .ok()
                .and_then(|end| r.occurrences_in((now, end), reader).ok())
                .and_then(|occ| occ.first().map(|o| o.at));
            RoutineRow {
                id: r.id,
                title: r.template.title.clone(),
                rrule: crate::rrule::rrule_summary(&r.rrule),
                next,
                paused: r.paused,
                template: r.template.clone(),
                rule: r.rrule.clone(),
                streak: r.streak_counter,
            }
        })
        .collect()
}

#[cfg(test)]
mod row_tests {
    use super::*;
    use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
    use crate::unknown::Unknowns;
    use sunrise_id::EntityKind;

    /// 2026-01-01T00:00:00Z — the fixed "now" for routine projection tests.
    fn now() -> Timestamp {
        "2026-01-01T00:00:00Z".parse().expect("valid timestamp")
    }

    fn fake_routine(idx: u8, title: &str, rrule: &str, starts_at: &str) -> Routine {
        Routine {
            id: EntityRef::new(EntityKind::Routine, [idx; 16]),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            template: TaskTemplate {
                title: title.into(),
                stream_id: None,
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
                unknown: crate::Unknowns::new(),
            },
            rrule: RRule::parse(rrule).expect("valid rrule"),
            timezone: "UTC".into(),
            starts_at: crate::SunriseTime::zoned(
                starts_at
                    .parse::<Timestamp>()
                    .expect("valid timestamp")
                    .to_zoned(TimeZone::UTC)
                    .datetime(),
                "UTC",
            ),
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

    #[test]
    fn rrule_summary_reads_as_english() {
        let daily = fake_routine(1, "t", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        assert_eq!(crate::rrule::rrule_summary(&daily.rrule), "every day");

        let biweekly = fake_routine(
            2,
            "t",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE",
            "2026-01-05T09:00:00Z",
        );
        assert_eq!(
            crate::rrule::rrule_summary(&biweekly.rrule),
            "every 2 weeks on Mo, We"
        );

        let monthly = fake_routine(
            3,
            "t",
            "FREQ=MONTHLY;BYMONTHDAY=1;COUNT=6",
            "2026-01-01T09:00:00Z",
        );
        assert_eq!(
            crate::rrule::rrule_summary(&monthly.rrule),
            "every month day 1 ×6"
        );
    }

    #[test]
    fn every_phrase_the_parser_accepts_summarizes_back() {
        // The two halves are inverses in prose, not in bytes: this pins that
        // the summary of a parsed phrase is at least *readable*, and that the
        // round-trippable form still survives the domain parser.
        for text in ["every day", "every 3 days", "weekdays", "monthly on day 1"] {
            let rule = crate::recur::parse_recurrence(text).expect("parses");
            let summary = crate::rrule::rrule_summary(&rule);
            assert!(summary.starts_with("every"), "{text} -> {summary}");
        }
    }

    #[test]
    fn routine_rows_resolve_the_next_occurrence() {
        let daily = fake_routine(1, "Water plants", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        let rows = routine_rows(&[daily], now(), &TimeZone::UTC);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Water plants");
        assert_eq!(rows[0].rrule, "every day");
        assert_eq!(
            rows[0].next,
            Some("2026-01-01T09:00:00Z".parse().expect("valid timestamp"))
        );
    }

    #[test]
    fn routine_rows_project_a_paused_routine_with_no_next() {
        let mut r = fake_routine(1, "Water plants", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        r.paused = true;
        let rows = routine_rows(&[r], now(), &TimeZone::UTC);
        assert!(rows[0].paused);
        assert_eq!(rows[0].next, None);
    }

    #[test]
    fn routine_rows_report_no_next_past_the_series_end() {
        // A daily series that stopped in 2025 has nothing left to schedule.
        let r = fake_routine(
            1,
            "Old habit",
            "FREQ=DAILY;UNTIL=20250601T000000Z",
            "2025-01-01T09:00:00Z",
        );
        let rows = routine_rows(&[r], now(), &TimeZone::UTC);
        assert_eq!(rows[0].next, None);
    }

    #[test]
    fn a_row_carries_the_template_and_the_parsed_rule_back() {
        // Both are load-bearing for editing: patching a title must not drop
        // the rest of the template, and the prose summary cannot be reparsed.
        let r = fake_routine(1, "Water plants", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        let rows = routine_rows(std::slice::from_ref(&r), now(), &TimeZone::UTC);
        assert_eq!(rows[0].template.title, r.template.title);
        assert_eq!(rows[0].rule, r.rrule);
    }
}
