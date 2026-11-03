//! Snooze arithmetic: where a "not now" lands.

use jiff::tz::TimeZone;
use jiff::Timestamp;

/// How far a "not now" pushes something out.
///
/// The three answers `docs/08-features/notifications.md` §Action buttons puts
/// on every reminder. They are an enum
/// rather than a number of milliseconds because two of them are **civil**
/// decisions, not durations: "tomorrow" is a date, and a client adding
/// 86,400,000 ms gets it wrong twice a year — an hour early or an hour late
/// across a DST boundary, on exactly the reminders someone was relying on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnoozeSpan {
    /// One hour from now. A real duration, and the only one of the three
    /// that is.
    OneHour,
    /// The same wall-clock time tomorrow.
    Tomorrow,
    /// The same wall-clock time next week.
    NextWeek,
}

/// When a [`SnoozeSpan`] lands, read in `tz`.
///
/// `Tomorrow` and `NextWeek` keep the time of day and move the *date*, so a
/// 09:00 reminder snoozed on the night the clocks change is still at 09:00.
/// The civil arithmetic is `jiff`'s, which resolves a wall clock that does not
/// exist (the spring-forward gap) forward rather than failing — so a snooze
/// always lands somewhere, and the somewhere is the next real instant.
#[must_use]
pub fn snooze_target(from: Timestamp, span: SnoozeSpan, tz: &TimeZone) -> Timestamp {
    match span {
        SnoozeSpan::OneHour => from + jiff::SignedDuration::from_hours(1),
        SnoozeSpan::Tomorrow => civil_shift(from, 1, tz),
        SnoozeSpan::NextWeek => civil_shift(from, 7, tz),
    }
}

/// `from`, moved `days` civil days on in `tz`, keeping the time of day.
fn civil_shift(from: Timestamp, days: i32, tz: &TimeZone) -> Timestamp {
    let zoned = from.to_zoned(tz.clone());
    zoned.checked_add(jiff::Span::new().days(days)).map_or_else(
        |_| from + jiff::SignedDuration::from_hours(24 * i64::from(days)),
        |z| z.timestamp(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(iso: &str) -> Timestamp {
        iso.parse().expect("timestamp")
    }

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").expect("zone")
    }

    #[test]
    fn an_hour_is_an_hour() {
        assert_eq!(
            snooze_target(ts("2026-03-04T14:00:00Z"), SnoozeSpan::OneHour, &ny()),
            ts("2026-03-04T15:00:00Z")
        );
    }

    /// The reason these are civil and not durations. New York springs forward
    /// at 02:00 on 2026-03-08, so 09:00 Saturday to 09:00 Sunday is 23 hours,
    /// not 24 — and a client adding 86,400,000 ms would fire at 10:00.
    #[test]
    fn tomorrow_keeps_the_wall_clock_across_a_spring_forward() {
        let saturday_nine = ts("2026-03-07T14:00:00Z"); // 09:00 EST
        let tomorrow = snooze_target(saturday_nine, SnoozeSpan::Tomorrow, &ny());
        assert_eq!(tomorrow, ts("2026-03-08T13:00:00Z")); // 09:00 EDT
        assert_eq!(
            tomorrow.to_zoned(ny()).time(),
            saturday_nine.to_zoned(ny()).time(),
            "same wall clock, 23 real hours later"
        );
    }

    #[test]
    fn tomorrow_keeps_the_wall_clock_across_a_fall_back() {
        let saturday_nine = ts("2026-10-31T13:00:00Z"); // 09:00 EDT
        let tomorrow = snooze_target(saturday_nine, SnoozeSpan::Tomorrow, &ny());
        assert_eq!(tomorrow, ts("2026-11-01T14:00:00Z")); // 09:00 EST
    }

    #[test]
    fn next_week_is_the_same_weekday() {
        let now = ts("2026-03-04T14:00:00Z");
        let next = snooze_target(now, SnoozeSpan::NextWeek, &ny());
        assert_eq!(
            next.to_zoned(ny()).date().weekday(),
            now.to_zoned(ny()).date().weekday()
        );
        assert_eq!(next.to_zoned(ny()).time(), now.to_zoned(ny()).time());
    }
}
