//! DST-aware RRULE expansion and deterministic task materialization helpers.
//!
//! This module implements the expansion half of the recurrence engine
//! described in `docs/08-features/recurrence-engine.md` and
//! `docs/02-domain/routines-and-recurrence.md`. [`RRule`]
//! (parsed in [`crate::rrule`]) describes *which* wall-clock instants recur;
//! [`expand`] turns a rule + anchor + timezone + window into concrete
//! [`Occurrence`] instants.
//!
//! # Determinism
//!
//! [`expand`] is a **pure function**: identical inputs always produce identical
//! outputs. It never reads a wall clock, never allocates randomness, and never
//! consults ambient timezone state — the timezone is passed in explicitly.
//! This is load-bearing: every device runs the same expansion and must agree on
//! the resulting task ids (see [`occurrence_task_id`]) so the CRDT layer can
//! dedup materialized tasks.
//!
//! # DST policy (`Disambiguation::Compatible`)
//!
//! Civil (wall-clock) candidates are converted to absolute instants with jiff's
//! [`TimeZone::to_ambiguous_zoned`] + [`AmbiguousZoned::compatible`]. That is
//! the RFC 5545 / RFC 5545-compatible behaviour:
//!
//! * **Spring-forward gap** (e.g. `America/Los_Angeles` 2025-03-09, the wall
//!   clock jumps 02:00 → 03:00 so 02:30 does not exist): the instant shifts
//!   *forward* by the gap, so a 02:30 routine fires at 03:30 local that day.
//! * **Fall-back fold** (e.g. 2025-11-02, 01:30 happens twice): the *earlier*
//!   offset is taken (the first 01:30).
//!
//! The [`Occurrence::key`] is always the **intended civil datetime** (the rule's
//! wall clock, e.g. `2025-03-09T02:30`), never the DST-shifted instant. This
//! keeps the key stable across devices *and* immune to tzdb-version drift (the
//! shifted instant depends on the tzdb DST tables; the intended wall clock does
//! not).
//!
//! [`TimeZone::to_ambiguous_zoned`]: jiff::tz::TimeZone::to_ambiguous_zoned
//! [`AmbiguousZoned::compatible`]: jiff::tz::AmbiguousZoned::compatible

use crate::routine::Routine;
use crate::rrule::{Frequency, RRule, Weekday};
use jiff::civil::{Date, DateTime, Time, Weekday as JiffWeekday};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use sunrise_id::{EntityKind, EntityRef};
use thiserror::Error;

/// Hard cap on the number of interval periods [`expand`] will scan before
/// giving up. A daily rule over ~270 years still fits; anything larger is a
/// pathological window and returns [`ExpandError::WindowTooLarge`] rather than
/// spinning.
///
/// Public because [`ExpandError::WindowTooLarge`] is public and this is the
/// number that produced it. A caller that hits it has to decide whether to
/// narrow the window or reject the rule, and that decision needs the bound;
/// reading it out of this crate's source is not an answer for a caller in
/// another crate.
pub const MAX_PERIODS: usize = 100_000;

/// One materialized occurrence of a routine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// Stable, tzdb-drift-immune key: the intended civil datetime formatted as
    /// `YYYY-MM-DDTHH:MM` in the routine's timezone (minute precision).
    pub key: String,
    /// The absolute instant this occurrence fires at (DST-resolved).
    pub at: Timestamp,
}

/// Errors returned by [`expand`] / [`Routine::occurrences_in`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ExpandError {
    /// The scan exceeded [`MAX_PERIODS`] interval periods without terminating.
    #[error("recurrence expansion window too large")]
    WindowTooLarge,
    /// The routine's IANA timezone string did not resolve against the bundled
    /// tzdb.
    #[error("invalid timezone: {0}")]
    InvalidTimeZone(String),
}

/// Map a domain [`Weekday`] to a jiff civil weekday.
fn to_jiff_weekday(w: Weekday) -> JiffWeekday {
    match w {
        Weekday::Su => JiffWeekday::Sunday,
        Weekday::Mo => JiffWeekday::Monday,
        Weekday::Tu => JiffWeekday::Tuesday,
        Weekday::We => JiffWeekday::Wednesday,
        Weekday::Th => JiffWeekday::Thursday,
        Weekday::Fr => JiffWeekday::Friday,
        Weekday::Sa => JiffWeekday::Saturday,
    }
}

/// Start-of-week date for `date`, given the week-start weekday `wkst`.
fn week_start(date: Date, wkst: JiffWeekday) -> Date {
    let cur = i64::from(date.weekday().to_monday_zero_offset());
    let start = i64::from(wkst.to_monday_zero_offset());
    let back = ((cur - start) % 7 + 7) % 7;
    date.checked_sub(Span::new().days(back)).unwrap_or(date)
}

/// Format the stable occurrence key from an intended civil date + time.
fn format_key(date: Date, tod: Time) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}",
        date.year(),
        date.month(),
        date.day(),
        tod.hour(),
        tod.minute(),
    )
}

/// Does `date` satisfy the `BYMONTH` filter (empty list = no constraint)?
fn matches_bymonth(date: Date, rrule: &RRule) -> bool {
    let month = u32::try_from(date.month()).unwrap_or(0);
    rrule.by_month.is_empty() || rrule.by_month.contains(&month)
}

/// Does `date` satisfy the `BYDAY` filter (empty list = no constraint)?
/// `BYDAY` in the v1 subset carries no ordinal prefix; ordinals are expressed
/// via `BYSETPOS`.
fn matches_byday(date: Date, rrule: &RRule) -> bool {
    if rrule.by_day.is_empty() {
        return true;
    }
    rrule
        .by_day
        .iter()
        .any(|w| to_jiff_weekday(*w) == date.weekday())
}

/// Does `date` satisfy the `BYMONTHDAY` filter (empty list = no constraint)?
/// Negative values count from the end of the month (`-1` = last day).
fn matches_bymonthday(date: Date, rrule: &RRule) -> bool {
    if rrule.by_month_day.is_empty() {
        return true;
    }
    let dim = i32::from(date.days_in_month());
    let day = i32::from(date.day());
    rrule.by_month_day.iter().any(|&v| match v.cmp(&0) {
        std::cmp::Ordering::Greater => day == v,
        std::cmp::Ordering::Less => day == dim + 1 + v,
        std::cmp::Ordering::Equal => false,
    })
}

/// A `Span` of `n` days, weeks or months, or `None` when jiff cannot express
/// one that long.
///
/// `Span::new().days(n)` and its two siblings are infallible by signature and
/// **panic** when `n` leaves the unit's range (jiff caps days at ±7,304,484,
/// weeks at ±1,043,497, months at ±120,000). Every caller below already
/// treated an out-of-range *date* as "this period has no candidates" via
/// `checked_add(..).ok()` — but the panic fires while the span is being
/// built, before `checked_add` is ever reached, so that guard never saw it.
///
/// `interval` comes straight out of an `RRULE` that a user typed or an `.ics`
/// subscription served, so `p * interval` reaches those bounds from ordinary
/// input: `FREQ=DAILY;INTERVAL=700017975` is the case the `rrule` fuzz target
/// found, and it crashed the process rather than returning
/// [`ExpandError::WindowTooLarge`] as [`expand`]'s contract says it may. These
/// are the fallible halves of the same builders, and a period nobody can
/// express is a period with no candidates.
fn span_of(unit: Unit, n: i64) -> Option<Span> {
    let span = Span::new();
    match unit {
        Unit::Days => span.try_days(n),
        Unit::Weeks => span.try_weeks(n),
        Unit::Months => span.try_months(n),
    }
    .ok()
}

/// The three calendar units [`span_of`] builds.
#[derive(Debug, Clone, Copy)]
enum Unit {
    Days,
    Weeks,
    Months,
}

/// The earliest date of interval period `p` — used purely for the loop's
/// stop condition (a conservative lower bound on that period's occurrences).
fn period_start_date(
    rrule: &RRule,
    anchor_date: Date,
    wkst: JiffWeekday,
    p: i64,
    interval: i64,
) -> Option<Date> {
    match rrule.freq {
        Frequency::Daily => {
            span_of(Unit::Days, p * interval).and_then(|s| anchor_date.checked_add(s).ok())
        }
        Frequency::Weekly => span_of(Unit::Weeks, p * interval)
            .and_then(|s| week_start(anchor_date, wkst).checked_add(s).ok()),
        Frequency::Monthly => span_of(Unit::Months, p * interval)
            .and_then(|s| anchor_date.first_of_month().checked_add(s).ok()),
        Frequency::Yearly => {
            let y = i64::from(anchor_date.year()) + p * interval;
            i16::try_from(y).ok().and_then(|y| Date::new(y, 1, 1).ok())
        }
    }
}

/// Build the candidate date set for interval period `p`, applying the
/// `BYMONTH` / `BYMONTHDAY` / `BYDAY` filters. When no relevant `BY*` rule
/// constrains a level, the anchor's own value fills in (RFC 5545's implicit
/// `BY*` defaults from `DTSTART`). The returned dates are unsorted; the caller
/// sorts + dedups before applying `BYSETPOS`.
fn period_candidates(
    rrule: &RRule,
    anchor_date: Date,
    wkst: JiffWeekday,
    p: i64,
    interval: i64,
) -> Vec<Date> {
    match rrule.freq {
        Frequency::Daily => {
            let Some(d) =
                span_of(Unit::Days, p * interval).and_then(|s| anchor_date.checked_add(s).ok())
            else {
                return Vec::new();
            };
            if matches_bymonth(d, rrule) && matches_byday(d, rrule) && matches_bymonthday(d, rrule)
            {
                vec![d]
            } else {
                Vec::new()
            }
        }
        Frequency::Weekly => {
            let Some(ws) = span_of(Unit::Weeks, p * interval)
                .and_then(|s| week_start(anchor_date, wkst).checked_add(s).ok())
            else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for i in 0..7 {
                let Ok(d) = ws.checked_add(Span::new().days(i)) else {
                    continue;
                };
                let day_ok = if rrule.by_day.is_empty() {
                    d.weekday() == anchor_date.weekday()
                } else {
                    matches_byday(d, rrule)
                };
                if day_ok && matches_bymonth(d, rrule) && matches_bymonthday(d, rrule) {
                    out.push(d);
                }
            }
            out
        }
        Frequency::Monthly => {
            let Some(first) = span_of(Unit::Months, p * interval)
                .and_then(|s| anchor_date.first_of_month().checked_add(s).ok())
            else {
                return Vec::new();
            };
            month_candidates(rrule, anchor_date, first)
        }
        Frequency::Yearly => {
            let y = i64::from(anchor_date.year()) + p * interval;
            let Some(year) = i16::try_from(y).ok() else {
                return Vec::new();
            };
            let months: Vec<i8> = if rrule.by_month.is_empty() {
                vec![anchor_date.month()]
            } else {
                rrule
                    .by_month
                    .iter()
                    .filter_map(|m| i8::try_from(*m).ok())
                    .collect()
            };
            let mut out = Vec::new();
            for m in months {
                let Ok(first) = Date::new(year, m, 1) else {
                    continue;
                };
                out.extend(month_candidates(rrule, anchor_date, first));
            }
            out
        }
    }
}

/// Days-in-month candidate selection shared by MONTHLY and (per-month) YEARLY.
/// `first` is the first day of the target month.
fn month_candidates(rrule: &RRule, anchor_date: Date, first: Date) -> Vec<Date> {
    let dim = first.days_in_month();
    let mut out = Vec::new();
    if rrule.by_month_day.is_empty() && rrule.by_day.is_empty() {
        // No day-level BY rule: fall back to the anchor's day-of-month. If the
        // month is too short (e.g. day 31 in February) the occurrence is
        // skipped per RFC 5545.
        let day = anchor_date.day();
        if day <= dim {
            if let Ok(d) = Date::new(first.year(), first.month(), day) {
                if matches_bymonth(d, rrule) {
                    out.push(d);
                }
            }
        }
        return out;
    }
    for day in 1..=dim {
        let Ok(d) = Date::new(first.year(), first.month(), day) else {
            continue;
        };
        let md_ok = matches_bymonthday(d, rrule);
        let wd_ok = matches_byday(d, rrule);
        // If both BYMONTHDAY and BYDAY are present they intersect; if only one
        // is present, that one selects.
        let ok = match (rrule.by_month_day.is_empty(), rrule.by_day.is_empty()) {
            (false, false) => md_ok && wd_ok,
            (false, true) => md_ok,
            (true, false) => wd_ok,
            (true, true) => true,
        };
        if ok && matches_bymonth(d, rrule) {
            out.push(d);
        }
    }
    out
}

/// Apply `BYSETPOS` selection to an ascending, deduped candidate list.
/// Positions are 1-based; negatives count from the end. Empty = take all.
fn apply_setpos(dates: &[Date], setpos: &[i32]) -> Vec<Date> {
    if setpos.is_empty() {
        return dates.to_vec();
    }
    let n = i32::try_from(dates.len()).unwrap_or(i32::MAX);
    let mut sel = Vec::new();
    for &pos in setpos {
        let idx = match pos.cmp(&0) {
            std::cmp::Ordering::Greater => pos - 1,
            std::cmp::Ordering::Less => n + pos,
            std::cmp::Ordering::Equal => continue,
        };
        if idx >= 0 && idx < n {
            if let Ok(u) = usize::try_from(idx) {
                sel.push(dates[u]);
            }
        }
    }
    sel.sort_unstable();
    sel.dedup();
    sel
}

/// Expand `rrule` anchored at `anchor` (interpreted in `tz`) into the
/// occurrences whose instants fall in `[window.0, window.1)`.
///
/// See the module docs for the DST disambiguation policy and determinism
/// guarantees. `COUNT` is counted from the anchor across the whole series (not
/// just the window); `UNTIL` bounds instants inclusively. Invalid civil dates
/// (e.g. Feb 30 from `BYMONTHDAY=30`) are silently skipped per RFC 5545.
///
/// # Errors
///
/// Returns [`ExpandError::WindowTooLarge`] if the scan would exceed
/// [`MAX_PERIODS`] interval periods.
pub fn expand(
    rrule: &RRule,
    anchor: Timestamp,
    tz: &TimeZone,
    window: (Timestamp, Timestamp),
) -> Result<Vec<Occurrence>, ExpandError> {
    let (win_start, win_end) = window;
    let anchor_zoned = anchor.to_zoned(tz.clone());
    let anchor_dt = anchor_zoned.datetime();
    let anchor_date = anchor_dt.date();
    let tod = anchor_dt.time();
    let wkst = to_jiff_weekday(rrule.wkst.unwrap_or(Weekday::Mo));
    let interval = i64::from(rrule.interval.max(1));
    let win_end_date = win_end.to_zoned(tz.clone()).date();

    let mut out = Vec::new();
    let mut count_seen: u32 = 0;
    let mut done = false;

    for p in 0..MAX_PERIODS {
        let p_i64 = i64::try_from(p).unwrap_or(i64::MAX);
        let Some(ps) = period_start_date(rrule, anchor_date, wkst, p_i64, interval) else {
            done = true;
            break;
        };
        // Every occurrence in this and later periods is on or after `ps`; if
        // `ps` already sits past the window end, nothing further can qualify.
        if ps > win_end_date {
            done = true;
            break;
        }

        let mut cand = period_candidates(rrule, anchor_date, wkst, p_i64, interval);
        cand.sort_unstable();
        cand.dedup();
        let selected = apply_setpos(&cand, &rrule.by_set_pos);

        for d in selected {
            if d < anchor_date {
                continue;
            }
            let dt = DateTime::from_parts(d, tod);
            let inst = match tz.to_ambiguous_zoned(dt).compatible() {
                Ok(z) => z.timestamp(),
                Err(_) => continue,
            };
            if let Some(until) = rrule.until {
                if inst > until {
                    done = true;
                    break;
                }
            }
            if let Some(c) = rrule.count {
                if count_seen >= c {
                    done = true;
                    break;
                }
            }
            count_seen += 1;
            if inst >= win_start && inst < win_end {
                out.push(Occurrence {
                    key: format_key(d, tod),
                    at: inst,
                });
            }
        }
        if done {
            break;
        }
    }

    if !done {
        return Err(ExpandError::WindowTooLarge);
    }
    Ok(out)
}

/// Deterministically derive a materialized task's id from its routine and the
/// occurrence key. Every device computes the same id for the same
/// `(routine, key)` pair, so the CRDT layer dedups concurrent materializations.
#[must_use]
pub fn occurrence_task_id(routine_id: &EntityRef, key: &str) -> EntityRef {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"sunrise.routine_task.v1");
    hasher.update(routine_id.bytes());
    hasher.update(key.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityRef::new(EntityKind::Task, bytes)
}

/// The occurrence key for an already-materialized instant, resolved in `tz`.
///
/// Used by the streak counter, which starts from a Task's stored
/// `routine_occurrence` rather than from a fresh expansion. Every replica holds
/// the same `routine_occurrence` and the same routine timezone, so every
/// replica derives the same key — which is what makes the streak's idempotency
/// set converge.
///
/// Note this resolves the *actual* instant's civil datetime. For an occurrence
/// that DST shifted (see the module docs), that differs from the intended
/// wall-clock key [`expand`] emitted. The key only has to be stable and unique
/// per occurrence, and it is both.
///
/// # Errors
///
/// [`ExpandError::InvalidTimeZone`] if `tz` does not resolve.
pub fn occurrence_key_at(tz: &str, at: Timestamp) -> Result<String, ExpandError> {
    let zone = TimeZone::get(tz).map_err(|_| ExpandError::InvalidTimeZone(tz.to_string()))?;
    let dt = at.to_zoned(zone).datetime();
    Ok(format_key(dt.date(), dt.time()))
}

impl Routine {
    /// Expand this routine's occurrences within `window`, applying its
    /// timezone, `starts_at` anchor, `ends_at` upper bound, and skip filters
    /// (both explicit skipped keys and `skip_dates`). A paused, archived, or
    /// deleted routine yields no occurrences.
    ///
    /// # Errors
    ///
    /// Returns [`ExpandError::InvalidTimeZone`] if the IANA timezone string
    /// does not resolve, or [`ExpandError::WindowTooLarge`] from [`expand`].
    pub fn occurrences_in(
        &self,
        window: (Timestamp, Timestamp),
    ) -> Result<Vec<Occurrence>, ExpandError> {
        if self.paused || self.archived || self.deleted {
            return Ok(Vec::new());
        }
        let tz = TimeZone::get(&self.timezone)
            .map_err(|_| ExpandError::InvalidTimeZone(self.timezone.clone()))?;
        let mut occ = expand(&self.rrule, self.starts_at, &tz, window)?;
        if let Some(end) = self.ends_at {
            occ.retain(|o| o.at <= end);
        }
        occ.retain(|o| !self.is_skipped(o, &tz));
        Ok(occ)
    }

    /// Whether `occ` is skipped, matching either an explicit skipped key or a
    /// `skip_dates` timestamp by identical civil minute in the routine's tz.
    fn is_skipped(&self, occ: &Occurrence, tz: &TimeZone) -> bool {
        if self.skipped_keys.iter().any(|k| k == &occ.key) {
            return true;
        }
        self.skip_dates.iter().any(|ts| {
            let dt = ts.to_zoned(tz.clone()).datetime();
            format_key(dt.date(), dt.time()) == occ.key
        })
    }
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
/// occurrence at or after `now`.
///
/// The lookahead window is the routine's own per-FREQ materialization horizon
/// (`docs/02-domain/routines-and-recurrence.md`), so a yearly routine still
/// resolves while a daily one stays cheap. Pure over `now` — no wall clock is
/// read here, which keeps the projection deterministic and unit-testable, and
/// is why every client can share it.
#[must_use]
pub fn routine_rows(routines: &[Routine], now: Timestamp) -> Vec<RoutineRow> {
    routines
        .iter()
        .map(|r| {
            let horizon_h =
                i64::from(crate::routine::materialization_horizon_days(r.rrule.freq)) * 24;
            let next = now
                .checked_add(jiff::SignedDuration::from_hours(horizon_h))
                .ok()
                .and_then(|end| r.occurrences_in((now, end)).ok())
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
    use crate::inbox::inbox_stream_ref;
    use crate::routine::{RoutineCatchupPolicy, TaskTemplate};
    use crate::unknown::Unknowns;

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
                stream_id: inbox_stream_ref(),
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse(rrule).expect("valid rrule"),
            timezone: "UTC".into(),
            starts_at: starts_at.parse().expect("valid timestamp"),
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
        let rows = routine_rows(&[daily], now());
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
        let rows = routine_rows(&[r], now());
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
        let rows = routine_rows(&[r], now());
        assert_eq!(rows[0].next, None);
    }

    #[test]
    fn a_row_carries_the_template_and_the_parsed_rule_back() {
        // Both are load-bearing for editing: patching a title must not drop
        // the rest of the template, and the prose summary cannot be reparsed.
        let r = fake_routine(1, "Water plants", "FREQ=DAILY", "2026-01-01T09:00:00Z");
        let rows = routine_rows(std::slice::from_ref(&r), now());
        assert_eq!(rows[0].template.title, r.template.title);
        assert_eq!(rows[0].rule, r.rrule);
    }
}
