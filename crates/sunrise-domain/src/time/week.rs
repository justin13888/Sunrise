//! Where a week begins, for the `week_start` preference.
//!
//! One rule, read by the core's `Query::WeekBlocks` and by every client grid
//! that draws the week it returned. Two copies of it would draw the right
//! blocks in the wrong columns the first time they disagreed.

use crate::rrule::Weekday;
use jiff::civil::{self, Weekday as JiffWeekday};
use jiff::Span;

/// The first day of the week containing `day`, for a week that begins on
/// `week_start`.
///
/// An unknown weekday token starts the week on the preference's default,
/// Sunday (`docs/02-domain/preferences.md`). A date that cannot step back
/// (the first days of the calendar) is its own week's first day.
#[must_use]
pub fn first_day_of_week(day: civil::Date, week_start: &Weekday) -> civil::Date {
    let first = match week_start {
        Weekday::Mo => JiffWeekday::Monday,
        Weekday::Tu => JiffWeekday::Tuesday,
        Weekday::We => JiffWeekday::Wednesday,
        Weekday::Th => JiffWeekday::Thursday,
        Weekday::Fr => JiffWeekday::Friday,
        Weekday::Sa => JiffWeekday::Saturday,
        Weekday::Su | Weekday::Unknown(_) => JiffWeekday::Sunday,
    };
    let back = i64::from(day.weekday().since(first).rem_euclid(7));
    day.checked_sub(Span::new().days(back)).unwrap_or(day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    /// Wednesday 2026-03-04 sits in a week that starts on each of the seven
    /// days in turn, and an unknown token reads as Sunday.
    #[test]
    fn every_week_start_finds_its_own_first_day() {
        let wed = date(2026, 3, 4);
        let cases = [
            (Weekday::Su, date(2026, 3, 1)),
            (Weekday::Mo, date(2026, 3, 2)),
            (Weekday::Tu, date(2026, 3, 3)),
            (Weekday::We, date(2026, 3, 4)),
            (Weekday::Th, date(2026, 2, 26)),
            (Weekday::Fr, date(2026, 2, 27)),
            (Weekday::Sa, date(2026, 2, 28)),
            (Weekday::from_raw("XX"), date(2026, 3, 1)),
        ];
        for (start, want) in cases {
            assert_eq!(first_day_of_week(wed, &start), want, "{start:?}");
        }
    }
}
