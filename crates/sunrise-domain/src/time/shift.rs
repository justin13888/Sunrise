//! Moving a time by whole civil days, keeping the kind the user meant.
//!
//! `docs/11-adr/0066-times-survive-travel.md` §4: deferring a floating
//! "tomorrow 09:00" by a day gives "the day after, 09:00", floating; an
//! all-day value moves its date; and a deferral counts civil days in the
//! reader's zone, never multiples of 86,400 seconds.

use super::{SunriseTime, Unresolvable};
use jiff::tz::TimeZone;
use jiff::Span;

/// `t` moved `days` civil days (negative moves it earlier), keeping its kind.
///
/// | Kind | Moves | Keeps |
/// |---|---|---|
/// | `floating` | its civil date | its time of day, and no zone |
/// | `zoned` | its civil date | its time of day and its zone |
/// | `all_day` | its date | — |
/// | `instant` | by `days` civil days in `reader` | its wall-clock time in `reader` |
///
/// A `floating` or `zoned` result keeps the wall-clock time it was given even
/// when that time does not exist on the new date. It is stored as meant and
/// resolves by `docs/10-cross-cutting/time.md` §3's one rule (jiff's
/// `Compatible`) whenever it is read, so 02:30 on a spring-forward night
/// fires at 03:30 that night and at 02:30 again the night after. An `instant`
/// has to be resolved now: its wall-clock time in `reader` moves by `days`
/// and lands by the same rule, forward across a gap and on the earlier pass
/// of a fold.
///
/// # Errors
///
/// [`Unresolvable::UnknownKind`] for a kind this build does not know, which it
/// never rewrites, and [`Unresolvable::OutOfRange`] when the result falls off
/// the calendar.
pub fn shift_civil_days(
    t: &SunriseTime,
    days: i32,
    reader: &TimeZone,
) -> Result<SunriseTime, Unresolvable> {
    let span = Span::new().try_days(days).map_err(|_| out_of_range(t))?;
    match t {
        SunriseTime::Floating { civil } => civil
            .checked_add(span)
            .map(SunriseTime::floating)
            .map_err(|_| out_of_range(t)),
        SunriseTime::Zoned { civil, tz } => civil
            .checked_add(span)
            .map(|c| SunriseTime::zoned(c, tz.clone()))
            .map_err(|_| out_of_range(t)),
        SunriseTime::AllDay { date } => date
            .checked_add(span)
            .map(SunriseTime::all_day)
            .map_err(|_| out_of_range(t)),
        SunriseTime::Instant { at } => at
            .to_zoned(reader.clone())
            .checked_add(span)
            .map(|z| SunriseTime::instant(z.timestamp()))
            .map_err(|_| out_of_range(t)),
        SunriseTime::Unknown { kind, .. } => Err(Unresolvable::UnknownKind { kind: kind.clone() }),
    }
}

/// The error for a shift that falls off the calendar, naming the value.
fn out_of_range(t: &SunriseTime) -> Unresolvable {
    Unresolvable::OutOfRange {
        value: t.to_string(),
    }
}
