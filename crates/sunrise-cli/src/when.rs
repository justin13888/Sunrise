//! What a typed date phrase means to `defer` and `schedule`.
//!
//! Both verbs read the capture grammar's `^when` phrases
//! ([`sunrise_domain::capture::parse_when_time`]), and both write through the
//! client layer ([`sunrise_client_core::schedule`]) rather than as an instant,
//! because an instant does not follow the user
//! (`docs/11-adr/0066-times-survive-travel.md` §4). What is decided here is
//! which of the client layer's two shapes a phrase is:
//!
//! * **A day** — `tomorrow`, `next friday`, `2026-11-02`, `+3d`, `+2w`. A
//!   deferral counts civil days in the reader's zone, and the task's plan
//!   keeps its kind and its time of day: a floating 09:00 deferred to
//!   tomorrow is a floating 09:00 tomorrow.
//! * **A moment the user named** — `tomorrow 3pm`, `17:30`, `+2h`. That is
//!   the user choosing the time as well as the day, so it is written as typed:
//!   a wall-clock time as floating, a relative span as an instant.

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::SunriseTime;

/// The shape of a defer or schedule phrase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// Move to this many civil days after today, keeping the plan's kind.
    Days(i32),
    /// Exactly this value.
    At(SunriseTime),
}

/// Read `phrase` at `now` in `reader`; `None` when it is not a date.
#[must_use]
pub fn read(phrase: &str, now: Timestamp, reader: &TimeZone) -> Option<When> {
    let p = phrase.trim().to_lowercase();
    if let Some(days) = relative_days(&p) {
        return Some(When::Days(days));
    }
    let value = sunrise_domain::capture::parse_when_time(&p, now, reader)?;
    match value {
        SunriseTime::AllDay { date } => {
            let today = now.to_zoned(reader.clone()).date();
            let days = date.since(today).ok()?.get_days();
            Some(When::Days(days))
        }
        other => Some(When::At(other)),
    }
}

/// `+Nd` and `+Nw`: civil days, not 24-hour spans. The unit is the last
/// character, not the last byte, so `+3日` is not a date rather than a panic.
fn relative_days(p: &str) -> Option<i32> {
    let rest = p.strip_prefix('+')?;
    let (unit_at, _) = rest.char_indices().next_back()?;
    let (num, unit) = rest.split_at(unit_at);
    let n: i32 = num.parse().ok()?;
    match unit {
        "d" => Some(n),
        "w" => n.checked_mul(7),
        _ => None,
    }
}

/// `value` as the CLI prints a plan: `YYYY-MM-DD` for a date,
/// `YYYY-MM-DD HH:MM` for a time, read in `reader`, then the kind in brackets
/// so a floating time and a pinned one never look alike.
#[must_use]
pub fn describe(value: &SunriseTime, reader: &TimeZone) -> String {
    match value {
        SunriseTime::AllDay { date } => format!("{date} [all_day]"),
        SunriseTime::Floating { civil } => {
            format!("{} [floating]", civil.strftime("%Y-%m-%d %H:%M"))
        }
        SunriseTime::Zoned { civil, tz } => {
            format!("{} [{tz}]", civil.strftime("%Y-%m-%d %H:%M"))
        }
        SunriseTime::Instant { at } => format!(
            "{} [instant]",
            at.to_zoned(reader.clone()).strftime("%Y-%m-%d %H:%M")
        ),
        SunriseTime::Unknown { .. } => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").unwrap()
    }

    #[test]
    fn a_day_phrase_is_a_count_of_civil_days_in_the_readers_zone() {
        // 03:00Z on the 24th is still the 23rd in New York.
        let now = at("2026-10-24T03:00:00Z");
        assert_eq!(read("tomorrow", now, &ny()), Some(When::Days(1)));
        assert_eq!(read("today", now, &ny()), Some(When::Days(0)));
        assert_eq!(read("2026-10-30", now, &ny()), Some(When::Days(7)));
        assert_eq!(read("+3d", now, &ny()), Some(When::Days(3)));
        assert_eq!(read("+2w", now, &ny()), Some(When::Days(14)));
    }

    #[test]
    fn a_named_time_is_written_as_typed() {
        let now = at("2026-10-24T03:00:00Z");
        let Some(When::At(SunriseTime::Floating { civil })) = read("tomorrow 3pm", now, &ny())
        else {
            panic!("a wall-clock time");
        };
        assert_eq!(civil.to_string(), "2026-10-24T15:00:00");
        assert!(matches!(
            read("+2h", now, &ny()),
            Some(When::At(SunriseTime::Instant { .. }))
        ));
    }

    #[test]
    fn something_that_is_not_a_date_is_none() {
        assert_eq!(read("whenever", at("2026-10-24T03:00:00Z"), &ny()), None);
        assert_eq!(read("+3x", at("2026-10-24T03:00:00Z"), &ny()), None);
    }

    /// A unit wider than one byte is not a date, rather than a panic on a
    /// byte slice inside the character.
    #[test]
    fn a_relative_phrase_with_a_non_ascii_unit_is_none() {
        let now = at("2026-10-24T03:00:00Z");
        for bad in ["+3\u{65e5}", "+3\u{e9}", "+\u{1f600}"] {
            assert_eq!(read(bad, now, &ny()), None, "{bad:?}");
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            rng_seed: sunrise_test_seed::proptest_rng_seed(),
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Any phrase is read as a date or as none, never a panic.
        #[test]
        fn any_phrase_is_read_without_panicking(s in proptest::prelude::any::<String>()) {
            let _ = read(&s, at("2026-10-24T03:00:00Z"), &ny());
        }

        /// The relative shape, with any character as its unit.
        #[test]
        fn a_relative_phrase_with_any_unit_is_read_without_panicking(
            n in 0u32..1000,
            unit in proptest::prelude::any::<char>(),
        ) {
            let _ = read(&format!("+{n}{unit}"), at("2026-10-24T03:00:00Z"), &ny());
        }
    }

    #[test]
    fn a_plan_is_printed_with_its_kind() {
        let date = jiff::civil::date(2026, 10, 25);
        assert_eq!(
            describe(&SunriseTime::all_day(date), &ny()),
            "2026-10-25 [all_day]"
        );
        assert_eq!(
            describe(&SunriseTime::floating(date.at(9, 0, 0, 0)), &ny()),
            "2026-10-25 09:00 [floating]"
        );
    }
}
