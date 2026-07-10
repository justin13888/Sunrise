//! DST-aware RRULE expansion and deterministic task materialization helpers.
//!
//! This module implements the expansion half of the recurrence engine
//! described in `docs/08-features/recurrence-engine.md` and
//! `docs/02-domain/routines-and-recurrence.md`. [`RRule`](crate::rrule::RRule)
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
const MAX_PERIODS: usize = 100_000;

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
        Frequency::Daily => anchor_date.checked_add(Span::new().days(p * interval)).ok(),
        Frequency::Weekly => week_start(anchor_date, wkst)
            .checked_add(Span::new().weeks(p * interval))
            .ok(),
        Frequency::Monthly => anchor_date
            .first_of_month()
            .checked_add(Span::new().months(p * interval))
            .ok(),
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
            let Ok(d) = anchor_date.checked_add(Span::new().days(p * interval)) else {
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
            let Ok(ws) = week_start(anchor_date, wkst).checked_add(Span::new().weeks(p * interval))
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
            let Ok(first) = anchor_date
                .first_of_month()
                .checked_add(Span::new().months(p * interval))
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
