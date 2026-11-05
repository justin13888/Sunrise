//! Date values in queries, and what they cover once a reader's day is known.
//!
//! A value names a civil day or period relative to the reader's today
//! (`docs/08-features/search.md` §Dates). Nothing here reads a clock: the
//! caller passes today, in the reader's zone, and the week start.

use crate::first_day_of_week;
use crate::rrule::Weekday;
use jiff::civil::{Date, Weekday as JiffWeekday};
use jiff::ToSpan;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The most days, weeks or months an offset like `+3d` may move.
pub const MAX_OFFSET: i16 = 999;

/// A unit of a relative offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OffsetUnit {
    /// `d`.
    Days,
    /// `w`.
    Weeks,
    /// `m`.
    Months,
}

/// One date value, as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DateValue {
    /// `today`.
    Today,
    /// `tomorrow`.
    Tomorrow,
    /// `yesterday`.
    Yesterday,
    /// `mon` … `sun`: the next such day, today included.
    Weekday(DayOfWeek),
    /// `this-week`, the week containing today.
    ThisWeek,
    /// `next-week`.
    NextWeek,
    /// `last-week`.
    LastWeek,
    /// `this-month`.
    ThisMonth,
    /// `next-month`.
    NextMonth,
    /// `last-month`.
    LastMonth,
    /// `+3d`, `-2w`, `+1m`: the day that far from today.
    Offset {
        /// How far, signed, at most [`MAX_OFFSET`] either way.
        n: i16,
        /// In what.
        unit: OffsetUnit,
    },
    /// `2026-10-01`.
    Day(Date),
    /// `2026-10`: the month.
    Month {
        /// The year.
        year: i16,
        /// The month, 1 to 12.
        month: i8,
    },
}

/// A day of the week, as a date value names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DayOfWeek {
    /// `mon`.
    Mon,
    /// `tue`.
    Tue,
    /// `wed`.
    Wed,
    /// `thu`.
    Thu,
    /// `fri`.
    Fri,
    /// `sat`.
    Sat,
    /// `sun`.
    Sun,
}

impl DayOfWeek {
    const ALL: [(Self, &'static str, &'static str); 7] = [
        (Self::Mon, "mon", "monday"),
        (Self::Tue, "tue", "tuesday"),
        (Self::Wed, "wed", "wednesday"),
        (Self::Thu, "thu", "thursday"),
        (Self::Fri, "fri", "friday"),
        (Self::Sat, "sat", "saturday"),
        (Self::Sun, "sun", "sunday"),
    ];

    fn jiff(self) -> JiffWeekday {
        match self {
            Self::Mon => JiffWeekday::Monday,
            Self::Tue => JiffWeekday::Tuesday,
            Self::Wed => JiffWeekday::Wednesday,
            Self::Thu => JiffWeekday::Thursday,
            Self::Fri => JiffWeekday::Friday,
            Self::Sat => JiffWeekday::Saturday,
            Self::Sun => JiffWeekday::Sunday,
        }
    }
}

/// A date operator's value: one day or period, or `a..b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DateSpan {
    /// One value.
    One(DateValue),
    /// `a..b`: from the start of `a` to the end of `b`.
    Range(DateValue, DateValue),
}

/// The civil days a value covers: `start` included, `end` excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Days {
    /// The first day.
    pub start: Date,
    /// The day after the last.
    pub end: Date,
}

impl DateValue {
    /// Read one value, case-insensitively. `None` when it is no date value.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let lower = s.to_ascii_lowercase();
        let fixed = match lower.as_str() {
            "today" => Some(Self::Today),
            "tomorrow" => Some(Self::Tomorrow),
            "yesterday" => Some(Self::Yesterday),
            "this-week" => Some(Self::ThisWeek),
            "next-week" => Some(Self::NextWeek),
            "last-week" => Some(Self::LastWeek),
            "this-month" => Some(Self::ThisMonth),
            "next-month" => Some(Self::NextMonth),
            "last-month" => Some(Self::LastMonth),
            _ => None,
        };
        if fixed.is_some() {
            return fixed;
        }
        if let Some((day, _, _)) = DayOfWeek::ALL
            .iter()
            .find(|(_, short, long)| lower == *short || lower == *long)
        {
            return Some(Self::Weekday(*day));
        }
        parse_offset(&lower).or_else(|| parse_absolute(&lower))
    }

    /// The days this value covers for a reader whose today is `today` and
    /// whose week starts on `week_start`. `None` only past the calendar's
    /// end.
    #[must_use]
    pub fn days(self, today: Date, week_start: &Weekday) -> Option<Days> {
        let one = |d: Date| d.tomorrow().ok().map(|end| Days { start: d, end });
        let week = |start: Date| {
            start
                .checked_add(7.days())
                .ok()
                .map(|end| Days { start, end })
        };
        let month = |first: Date| {
            first
                .checked_add(1.month())
                .ok()
                .map(|end| Days { start: first, end })
        };
        match self {
            Self::Today => one(today),
            Self::Tomorrow => one(today.tomorrow().ok()?),
            Self::Yesterday => one(today.yesterday().ok()?),
            Self::Weekday(d) => {
                let ahead = (i16::from(d.jiff().to_monday_zero_offset())
                    - i16::from(today.weekday().to_monday_zero_offset()))
                .rem_euclid(7);
                one(today.checked_add(ahead.days()).ok()?)
            }
            Self::ThisWeek => week(first_day_of_week(today, week_start)),
            Self::NextWeek => week(
                first_day_of_week(today, week_start)
                    .checked_add(7.days())
                    .ok()?,
            ),
            Self::LastWeek => week(
                first_day_of_week(today, week_start)
                    .checked_sub(7.days())
                    .ok()?,
            ),
            Self::ThisMonth => month(today.first_of_month()),
            Self::NextMonth => month(today.first_of_month().checked_add(1.month()).ok()?),
            Self::LastMonth => month(today.first_of_month().checked_sub(1.month()).ok()?),
            Self::Offset { n, unit } => {
                let span = match unit {
                    OffsetUnit::Days => i64::from(n).days(),
                    OffsetUnit::Weeks => i64::from(n).weeks(),
                    OffsetUnit::Months => i64::from(n).months(),
                };
                one(today.checked_add(span).ok()?)
            }
            Self::Day(d) => one(d),
            Self::Month { year, month: m } => month(Date::new(year, m, 1).ok()?),
        }
    }
}

impl DateSpan {
    /// The days the span covers, from the start of its first value to the end
    /// of its last. `None` past the calendar's end, and for a range that ends
    /// before it starts.
    #[must_use]
    pub fn days(self, today: Date, week_start: &Weekday) -> Option<Days> {
        match self {
            Self::One(v) => v.days(today, week_start),
            Self::Range(a, b) => {
                let start = a.days(today, week_start)?.start;
                let end = b.days(today, week_start)?.end;
                (start < end).then_some(Days { start, end })
            }
        }
    }
}

/// `+3d`, `-2w`, `+1m`. The sign is required, so `2026` is not an offset.
fn parse_offset(s: &str) -> Option<DateValue> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1i16, &s[1..]),
        b'-' => (-1i16, &s[1..]),
        _ => return None,
    };
    let unit = match rest.as_bytes().last()? {
        b'd' => OffsetUnit::Days,
        b'w' => OffsetUnit::Weeks,
        b'm' => OffsetUnit::Months,
        _ => return None,
    };
    let digits = &rest[..rest.len() - 1];
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i16 = digits.parse().ok()?;
    Some(DateValue::Offset { n: sign * n, unit })
}

/// `2026-10-01` or `2026-10`, with exactly those digit counts.
fn parse_absolute(s: &str) -> Option<DateValue> {
    let parts: Vec<&str> = s.split('-').collect();
    let num = |p: &str, len: usize| {
        (p.len() == len && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<i16>().ok())
            .flatten()
    };
    match parts.as_slice() {
        [y, m] => {
            let (year, month) = (num(y, 4)?, num(m, 2)?);
            let month = i8::try_from(month).ok()?;
            Date::new(year, month, 1).ok()?;
            Some(DateValue::Month { year, month })
        }
        [y, m, d] => {
            let (year, month, day) = (num(y, 4)?, num(m, 2)?, num(d, 2)?);
            let date = Date::new(year, i8::try_from(month).ok()?, i8::try_from(day).ok()?).ok()?;
            Some(DateValue::Day(date))
        }
        _ => None,
    }
}

impl fmt::Display for DateValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Today => f.write_str("today"),
            Self::Tomorrow => f.write_str("tomorrow"),
            Self::Yesterday => f.write_str("yesterday"),
            Self::Weekday(d) => {
                let name = DayOfWeek::ALL
                    .iter()
                    .find(|(x, _, _)| x == d)
                    .map_or("mon", |(_, short, _)| short);
                f.write_str(name)
            }
            Self::ThisWeek => f.write_str("this-week"),
            Self::NextWeek => f.write_str("next-week"),
            Self::LastWeek => f.write_str("last-week"),
            Self::ThisMonth => f.write_str("this-month"),
            Self::NextMonth => f.write_str("next-month"),
            Self::LastMonth => f.write_str("last-month"),
            Self::Offset { n, unit } => {
                let u = match unit {
                    OffsetUnit::Days => 'd',
                    OffsetUnit::Weeks => 'w',
                    OffsetUnit::Months => 'm',
                };
                let sign = if *n < 0 { '-' } else { '+' };
                write!(f, "{sign}{}{u}", n.unsigned_abs())
            }
            Self::Day(d) => write!(f, "{d}"),
            Self::Month { year, month } => write!(f, "{year:04}-{month:02}"),
        }
    }
}

impl fmt::Display for DateSpan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One(v) => write!(f, "{v}"),
            Self::Range(a, b) => write!(f, "{a}..{b}"),
        }
    }
}
