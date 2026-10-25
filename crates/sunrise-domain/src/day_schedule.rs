//! The day schedule and the planner day (`docs/02-domain/day-schedule.md`,
//! ADR-0050 §3, #338).
//!
//! When the user usually wakes and sleeps, per weekday, per day of month and
//! per date. It is the vault-scoped `day_schedule` preference, stored one
//! register per entry (`day_schedule.weekday.MO`, `day_schedule.month_day.1`,
//! `day_schedule.date.2026-12-25`), so two devices editing two entries both
//! keep their edit. [`DaySchedule::from_values`] reads it out of the synced
//! `Preferences` map.
//!
//! Everything here is a pure function of its arguments. Wake and sleep are
//! civil times with no zone: they resolve in the **reader's** zone, which the
//! caller passes in, so the schedule follows a traveller. A civil time in a DST
//! gap or fold resolves by the one rule of `docs/10-cross-cutting/time.md` §3
//! (jiff's `Compatible`: a gap shifts forward by its length, a fold takes the
//! earlier instant).
//!
//! - [`DaySchedule::window_for`]: the window a date resolves to, first match
//!   of date override, day of month, weekday, unset.
//! - [`day_bounds`]: the **planner day**, `[boundary(d), boundary(d+1))`.
//!   Every instant belongs to exactly one.
//! - [`plannable`]: the wake-to-sleep window inside it.
//! - [`planner_date`]: the planner day an instant belongs to.
//! - [`wind_down_at`]: the sleep time less the lead, when there is one.
//! - [`brief_at`] and [`next_brief`]: when the morning and evening briefs
//!   fire, wake plus or sleep less their offset.
//! - [`starts_at`] and [`deadline_at`]: when a stored time starts, and when
//!   it is missed as a deadline, with an all-day value on its planner day
//!   (`boundary(d)` and `boundary(d + 1)`, ADR-0047 §Due instant).

use crate::preferences::PrefValue;
use crate::rrule::Weekday;
use crate::time::SunriseTime;
use crate::unknown::CborValue;
use jiff::civil::{self, Date};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The key prefix of every per-weekday entry: `day_schedule.weekday.MO`.
pub const WEEKDAY_PREFIX: &str = "day_schedule.weekday";
/// The key prefix of every per-day-of-month entry: `day_schedule.month_day.1`,
/// `day_schedule.month_day.-1` (the last day of the month).
pub const MONTH_DAY_PREFIX: &str = "day_schedule.month_day";
/// The key prefix of every per-date override: `day_schedule.date.2026-12-25`.
pub const DATE_PREFIX: &str = "day_schedule.date";

/// A wake and a sleep time. A sleep at or before the wake falls on the next
/// civil date; the two are never equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DayWindow {
    /// When the day's plannable window opens.
    pub wake: civil::Time,
    /// When it closes; on the next civil date when not after `wake`.
    pub sleep: civil::Time,
}

impl DayWindow {
    /// A window, or `None` when `wake == sleep` (`day-schedule.md`
    /// §Validation).
    #[must_use]
    pub fn new(wake: civil::Time, sleep: civil::Time) -> Option<Self> {
        (wake != sleep).then_some(Self { wake, sleep })
    }

    /// Whether the sleep time falls on the next civil date.
    #[must_use]
    pub fn crosses_midnight(self) -> bool {
        self.sleep <= self.wake
    }
}

/// What a per-date override says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DayOverride {
    /// This window, on this date.
    Window(DayWindow),
    /// No schedule this date. Stops the search: a holiday with no schedule
    /// does not fall through to its weekday.
    None,
}

/// One entry of the schedule, by the level it sits at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DayScheduleEntry {
    /// A weekday default. Never [`Weekday::Unknown`].
    Weekday(Weekday),
    /// A day-of-month default: `1..=31`, or `-1` for the last day.
    MonthDay(i8),
    /// A per-date override.
    Date(Date),
}

impl DayScheduleEntry {
    /// Its preference key, e.g. `day_schedule.date.2026-12-25`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Weekday(d) => format!("{WEEKDAY_PREFIX}.{}", d.as_str()),
            Self::MonthDay(n) => format!("{MONTH_DAY_PREFIX}.{n}"),
            Self::Date(d) => format!("{DATE_PREFIX}.{d}"),
        }
    }

    /// The entry a preference key names, or `None` when it names none: a
    /// key outside the three prefixes, an unknown weekday, a day of month
    /// outside `-1, 1..=31`, or a date that does not parse.
    #[must_use]
    pub fn parse_key(key: &str) -> Option<Self> {
        let (prefix, tail) = key.rsplit_once('.')?;
        match prefix {
            WEEKDAY_PREFIX => Weekday::parse(tail).map(Self::Weekday),
            MONTH_DAY_PREFIX => {
                // Exactly the spelling `key` writes, so one entry has one key.
                let n: i8 = tail.parse().ok()?;
                (is_month_day(n) && n.to_string() == tail).then_some(Self::MonthDay(n))
            }
            DATE_PREFIX => {
                let d: Date = tail.parse().ok()?;
                (d.to_string() == tail).then_some(Self::Date(d))
            }
            _ => None,
        }
    }
}

/// Whether `n` is a day-of-month key: `-1` or `1..=31`.
#[must_use]
pub fn is_month_day(n: i8) -> bool {
    n == -1 || (1..=31).contains(&n)
}

/// The whole schedule. Empty is unset: every planner day is the civil day.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DaySchedule {
    /// Per weekday, Monday first (`jiff`'s Monday-zero offset).
    pub weekday: [Option<DayWindow>; 7],
    /// Per day of month, `-1` the last day.
    pub month_day: BTreeMap<i8, DayWindow>,
    /// Per date.
    pub date: BTreeMap<Date, DayOverride>,
}

fn weekday_index(day: &Weekday) -> Option<usize> {
    Some(match day {
        Weekday::Mo => 0,
        Weekday::Tu => 1,
        Weekday::We => 2,
        Weekday::Th => 3,
        Weekday::Fr => 4,
        Weekday::Sa => 5,
        Weekday::Su => 6,
        Weekday::Unknown(_) => return None,
    })
}

/// The seven weekdays, Monday first: the order [`DaySchedule::weekday`] holds
/// them in.
pub const WEEKDAYS: [Weekday; 7] = [
    Weekday::Mo,
    Weekday::Tu,
    Weekday::We,
    Weekday::Th,
    Weekday::Fr,
    Weekday::Sa,
    Weekday::Su,
];

impl DaySchedule {
    /// Whether nothing is set at any level.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.weekday.iter().all(Option::is_none)
            && self.month_day.is_empty()
            && self.date.is_empty()
    }

    /// The schedule held in a `Preferences` values map. An entry whose value
    /// does not read as its level's type (a newer build's shape) is left out,
    /// as the key table's resolver leaves out a value it cannot read; it stays
    /// stored, untouched.
    #[must_use]
    pub fn from_values(values: &BTreeMap<String, CborValue>) -> Self {
        let mut out = Self::default();
        for (key, raw) in values.range(String::from("day_schedule.")..) {
            if !key.starts_with("day_schedule.") {
                break;
            }
            let Some(entry) = DayScheduleEntry::parse_key(key) else {
                continue;
            };
            let Some(spec) = crate::preferences::pref_spec(key) else {
                continue;
            };
            let value = spec.ty.decode(raw);
            out.set(&entry, value.as_ref());
        }
        out
    }

    /// Set (or, with `None`, clear) one entry from its preference value. A
    /// value of the wrong shape for the level clears it.
    pub fn set(&mut self, entry: &DayScheduleEntry, value: Option<&PrefValue>) {
        let window = match value {
            Some(PrefValue::DayWindow { wake, sleep }) => DayWindow::new(*wake, *sleep),
            _ => None,
        };
        match entry {
            DayScheduleEntry::Weekday(d) => {
                if let Some(i) = weekday_index(d) {
                    self.weekday[i] = window;
                }
            }
            DayScheduleEntry::MonthDay(n) => {
                match window {
                    Some(w) => self.month_day.insert(*n, w),
                    None => self.month_day.remove(n),
                };
            }
            DayScheduleEntry::Date(d) => {
                let over = match value {
                    Some(PrefValue::NoSchedule) => Some(DayOverride::None),
                    _ => window.map(DayOverride::Window),
                };
                match over {
                    Some(o) => self.date.insert(*d, o),
                    None => self.date.remove(d),
                };
            }
        }
    }

    /// The weekday default for `day`.
    #[must_use]
    pub fn weekday_window(&self, day: &Weekday) -> Option<DayWindow> {
        weekday_index(day).and_then(|i| self.weekday[i])
    }

    /// The window `date` resolves to (`day-schedule.md` §Resolution). First
    /// match wins: the date's override (`None` there stops the search), the
    /// day of month, `-1` on the month's last day, the weekday, unset. Reads
    /// no zone and no clock.
    #[must_use]
    pub fn window_for(&self, date: Date) -> Option<DayWindow> {
        if let Some(over) = self.date.get(&date) {
            return match over {
                DayOverride::Window(w) => Some(*w),
                DayOverride::None => None,
            };
        }
        let day = date.day();
        if let Some(w) = self.month_day.get(&day) {
            return Some(*w);
        }
        if date == date.last_of_month() {
            if let Some(w) = self.month_day.get(&-1) {
                return Some(*w);
            }
        }
        let i = usize::try_from(date.weekday().to_monday_zero_offset()).unwrap_or(0);
        self.weekday[i]
    }
}

/// A date at the edge of the calendar, whose bounds have no instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the planner day {0} has no instant in the timestamp range")]
pub struct OutOfCalendar(pub Date);

/// `time` on `date` in `tz`, by the one DST rule (`time.md` §3).
fn at(tz: &TimeZone, date: Date, time: civil::Time) -> Result<Timestamp, OutOfCalendar> {
    tz.to_zoned(date.to_datetime(time))
        .map(|z| z.timestamp())
        .map_err(|_| OutOfCalendar(date))
}

fn next(date: Date) -> Result<Date, OutOfCalendar> {
    date.tomorrow().map_err(|_| OutOfCalendar(date))
}

fn prev(date: Date) -> Result<Date, OutOfCalendar> {
    date.yesterday().map_err(|_| OutOfCalendar(date))
}

/// Whether `date` happens at all in `tz`: `Pacific/Apia` skipped 2011-12-30.
fn exists(tz: &TimeZone, date: Date) -> Result<bool, OutOfCalendar> {
    let start = at(tz, date, civil::Time::midnight())?;
    Ok(start.to_zoned(tz.clone()).date() == date)
}

/// When `date`'s plannable window opens: its wake time, else its midnight.
fn nominal_start(s: &DaySchedule, tz: &TimeZone, date: Date) -> Result<Timestamp, OutOfCalendar> {
    match s.window_for(date) {
        Some(w) => at(tz, date, w.wake),
        None => at(tz, date, civil::Time::midnight()),
    }
}

/// When `date`'s plannable window closes: its sleep time (on the next date
/// when it crosses midnight), else the next midnight.
fn end(s: &DaySchedule, tz: &TimeZone, date: Date) -> Result<Timestamp, OutOfCalendar> {
    match s.window_for(date) {
        Some(w) if w.crosses_midnight() => at(tz, next(date)?, w.sleep),
        Some(w) => at(tz, date, w.sleep),
        None => at(tz, next(date)?, civil::Time::midnight()),
    }
}

/// The nearest date before `date` that exists in `tz`. A zone skips at most
/// one date at a time, so two steps back always land on one.
fn prev_existing(tz: &TimeZone, date: Date) -> Result<Date, OutOfCalendar> {
    let mut d = prev(date)?;
    for _ in 0..2 {
        if exists(tz, d)? {
            return Ok(d);
        }
        d = prev(d)?;
    }
    Ok(d)
}

/// The instant planner day `date` starts at: the later of its nominal start
/// and the end of the day before it. A date the zone skips starts where the
/// next one does, so its planner day is empty.
fn boundary(s: &DaySchedule, tz: &TimeZone, date: Date) -> Result<Timestamp, OutOfCalendar> {
    let mut d = date;
    for _ in 0..2 {
        if exists(tz, d)? {
            break;
        }
        d = next(d)?;
    }
    let before = end(s, tz, prev_existing(tz, d)?)?;
    Ok(nominal_start(s, tz, d)?.max(before))
}

/// The planner day `date`, `[boundary(date), boundary(date + 1))`, in `tz`
/// (`day-schedule.md` §Planner day). With an empty schedule it is the civil
/// day; a date the zone skips is empty.
///
/// # Errors
/// [`OutOfCalendar`] for a date at the edge of the calendar.
pub fn day_bounds(
    schedule: &DaySchedule,
    date: Date,
    tz: &TimeZone,
) -> Result<(Timestamp, Timestamp), OutOfCalendar> {
    Ok((
        boundary(schedule, tz, date)?,
        boundary(schedule, tz, next(date)?)?,
    ))
}

/// The plannable window of `date`, wake to sleep, in `tz`, or `None` when
/// the date has no schedule (the planner then uses the whole planner day).
///
/// # Errors
/// [`OutOfCalendar`] for a date at the edge of the calendar.
pub fn plannable(
    schedule: &DaySchedule,
    date: Date,
    tz: &TimeZone,
) -> Result<Option<(Timestamp, Timestamp)>, OutOfCalendar> {
    if schedule.window_for(date).is_none() || !exists(tz, date)? {
        return Ok(None);
    }
    Ok(Some((
        nominal_start(schedule, tz, date)?,
        end(schedule, tz, date)?,
    )))
}

/// The planner day `instant` belongs to in `tz`: the inverse of
/// [`day_bounds`]. Time after midnight belongs to the day before until the
/// later of that day's sleep and the next day's wake, so a task done at 00:30
/// by someone who sleeps at 01:00 lands on the day they were living.
///
/// # Errors
/// [`OutOfCalendar`] for an instant at the edge of the calendar.
pub fn planner_date(
    schedule: &DaySchedule,
    instant: Timestamp,
    tz: &TimeZone,
) -> Result<Date, OutOfCalendar> {
    // `boundary(c + 1)` is never before civil midnight of `c + 1`, so the
    // answer is the civil date or one of the (at most two, with a skipped
    // date between) before it.
    let mut d = instant.to_zoned(tz.clone()).date();
    for _ in 0..3 {
        if instant >= boundary(schedule, tz, d)? {
            return Ok(d);
        }
        d = prev(d)?;
    }
    Ok(d)
}

/// The planner day a stored time falls on for a reader in `tz`: an all-day
/// value's own date, otherwise the planner day of its resolved instant.
/// `None` for a value with no instant (`docs/11-adr/0066-times-survive-travel.md`
/// §3), which is on no day. With an empty schedule this is
/// [`SunriseTime::day_in`].
#[must_use]
pub fn planner_day_of(schedule: &DaySchedule, value: &SunriseTime, tz: &TimeZone) -> Option<Date> {
    match value {
        SunriseTime::AllDay { date } => Some(*date),
        _ => planner_date(schedule, value.resolve_in(tz)?, tz).ok(),
    }
}

/// The instant a stored time **starts** at for a reader in `tz` whose days
/// are `schedule`'s planner days: the start of an all-day value's planner
/// day, `boundary(d)`, and the resolved instant of every other kind
/// (`docs/10-cross-cutting/time.md` §2 rule 1, §4).
///
/// With an empty schedule an all-day value starts at civil midnight, which
/// is [`SunriseTime::resolve_in`]. `None` for a value with no instant
/// (`docs/11-adr/0066-times-survive-travel.md` §3), which is compared with
/// nothing.
#[must_use]
pub fn starts_at(schedule: &DaySchedule, value: &SunriseTime, tz: &TimeZone) -> Option<Timestamp> {
    match value {
        SunriseTime::AllDay { date } => {
            day_bounds(schedule, *date, tz).ok().map(|(start, _)| start)
        }
        _ => value.resolve_in(tz),
    }
}

/// The instant a stored time is **missed** at, read as a deadline by a
/// reader in `tz` whose days are `schedule`'s planner days: `due()` in
/// ADR-0047 §Due instant.
///
/// | Kind | Missed at |
/// |---|---|
/// | `instant` | the instant |
/// | `zoned` | the civil time in its own zone |
/// | `floating` | the civil time in the reader's zone |
/// | `all_day(d)` | `boundary(d + 1)`, the end of planner day `d` |
///
/// So an all-day deadline of Friday is not missed at 00:30 on Saturday for
/// someone whose Friday runs until 01:00. With an empty schedule it is the
/// next civil midnight, which is [`SunriseTime::due_in`]. `None` for a value
/// with no instant, which is never missed.
#[must_use]
pub fn deadline_at(
    schedule: &DaySchedule,
    value: &SunriseTime,
    tz: &TimeZone,
) -> Option<Timestamp> {
    match value {
        SunriseTime::AllDay { date } => day_bounds(schedule, *date, tz).ok().map(|(_, end)| end),
        _ => value.resolve_in(tz),
    }
}

/// When the wind-down notification for `date` fires: its resolved sleep time
/// less `lead_s` (`day-schedule.md` §Wind-down). `None` when the date has no
/// schedule, so no wind-down fires that night.
///
/// # Errors
/// [`OutOfCalendar`] for a date at the edge of the calendar.
pub fn wind_down_at(
    schedule: &DaySchedule,
    date: Date,
    tz: &TimeZone,
    lead_s: u32,
) -> Result<Option<WindDown>, OutOfCalendar> {
    if schedule.window_for(date).is_none() || !exists(tz, date)? {
        return Ok(None);
    }
    let sleep_at = end(schedule, tz, date)?;
    let fire_at = sleep_at
        .checked_sub(SignedDuration::from_secs(i64::from(lead_s)))
        .map_err(|_| OutOfCalendar(date))?;
    Ok(Some(WindDown {
        date,
        sleep_at,
        fire_at,
    }))
}

/// One night's wind-down trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WindDown {
    /// The planner day whose sleep time it counts back from.
    pub date: Date,
    /// That sleep time, resolved in the reader's zone.
    pub sleep_at: Timestamp,
    /// When it fires: `sleep_at` less the lead.
    pub fire_at: Timestamp,
}

/// The first wind-down that fires at or after `now`, looking at the planner
/// day `now` is in and the next one: the current day's sleep may already be
/// behind the reader.
///
/// # Errors
/// [`OutOfCalendar`] at the edge of the calendar.
pub fn next_wind_down(
    schedule: &DaySchedule,
    now: Timestamp,
    tz: &TimeZone,
    lead_s: u32,
) -> Result<Option<WindDown>, OutOfCalendar> {
    let mut d = planner_date(schedule, now, tz)?;
    for _ in 0..3 {
        if let Some(w) = wind_down_at(schedule, d, tz, lead_s)? {
            if w.fire_at >= now {
                return Ok(Some(w));
            }
        }
        d = next(d)?;
    }
    Ok(None)
}

/// Which of the two daily briefs (`docs/08-features/notifications.md` #5, #7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Brief {
    /// The morning brief: wake plus its offset.
    Morning,
    /// The evening plan: sleep less its offset.
    Evening,
}

impl Brief {
    /// The wall clock it fires at on a date with no schedule: 08:00 and
    /// 20:00. The offset does not move it: an offset counts from a wake or a
    /// sleep time, and the date has neither.
    #[must_use]
    pub const fn unscheduled_time(self) -> civil::Time {
        match self {
            Self::Morning => civil::Time::constant(8, 0, 0, 0),
            Self::Evening => civil::Time::constant(20, 0, 0, 0),
        }
    }
}

/// When `which` brief fires for planner day `date` in `tz`: the morning one
/// `offset_s` after the date's wake time, the evening one `offset_s` before
/// its resolved sleep time (on the next date when it crosses midnight), and
/// [`Brief::unscheduled_time`] on a date with no schedule. `None` for a date
/// the zone skips.
///
/// A negative offset is allowed: `notifications.morning_brief.offset_s` runs
/// −86400..86400, so a brief may fire before waking.
///
/// # Errors
/// [`OutOfCalendar`] for a date at the edge of the calendar.
pub fn brief_at(
    schedule: &DaySchedule,
    date: Date,
    tz: &TimeZone,
    which: Brief,
    offset_s: i32,
) -> Result<Option<Timestamp>, OutOfCalendar> {
    if !exists(tz, date)? {
        return Ok(None);
    }
    let offset = SignedDuration::from_secs(i64::from(offset_s));
    let at = match (schedule.window_for(date), which) {
        (None, _) => return at(tz, date, which.unscheduled_time()).map(Some),
        (Some(w), Brief::Morning) => at(tz, date, w.wake)?.checked_add(offset),
        (Some(_), Brief::Evening) => end(schedule, tz, date)?.checked_sub(offset),
    };
    at.map(Some).map_err(|_| OutOfCalendar(date))
}

/// The first `which` brief that fires at or after `now`: the planner day
/// `now` is in, then the next ones, as `(date, fire_at)`. Looks three days
/// ahead, the most a skipped date and a past brief can push it.
///
/// # Errors
/// [`OutOfCalendar`] at the edge of the calendar.
pub fn next_brief(
    schedule: &DaySchedule,
    now: Timestamp,
    tz: &TimeZone,
    which: Brief,
    offset_s: i32,
) -> Result<Option<(Date, Timestamp)>, OutOfCalendar> {
    let mut d = prev(planner_date(schedule, now, tz)?)?;
    for _ in 0..4 {
        if let Some(at) = brief_at(schedule, d, tz, which, offset_s)? {
            if at >= now {
                return Ok(Some((d, at)));
            }
        }
        d = next(d)?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
