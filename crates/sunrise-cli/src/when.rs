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

    /// Phrases at the edges of `read`'s input space: every byte width of
    /// character, and every integer and calendar bound the phrase crosses on
    /// its way to a value. Each row pins the answer, so a row that stopped
    /// panicking by returning something else would still fail.
    const EDGE_PHRASES: &[(&str, Option<i32>)] = &[
        // Nothing to read.
        ("", None),
        ("\u{3000}", None),
        ("+", None),
        ("+d", None),
        // Multibyte characters in the unit, the number and the phrase.
        ("+3\u{e9}", None),
        ("+3\u{65e5}", None),
        ("+3\u{1f600}", None),
        ("+\u{e9}", None),
        ("+\u{ff13}d", None),
        ("+\u{663}d", None),
        ("\u{130}", None),
        ("\u{660}\u{661}:\u{662}\u{663}", None),
        ("\u{660}pm", None),
        ("next \u{65e5}\u{66dc}\u{65e5}", None),
        ("2026-10-3\u{ff10}", None),
        // Multibyte whitespace is trimmed, and a phrase inside it still reads.
        ("\u{3000}tomorrow\u{3000}", Some(1)),
        ("\u{3000}+3d\u{3000}", Some(3)),
        // The day count fits `i32` up to its last value and not past it.
        ("+2147483647d", Some(i32::MAX)),
        ("+2147483648d", None),
        ("+306783378w", Some(306_783_378 * 7)),
        ("+306783379w", None),
        // Past every integer `read` parses into, in every unit.
        ("+9223372036854775807d", None),
        ("+9223372036854775808d", None),
        ("+9223372036854775807w", None),
        ("+9223372036854775807h", None),
        ("+9223372036854775807m", None),
        ("+9223372036854775808h", None),
        ("+340282366920938463463374607431768211456m", None),
        // A span the type holds, whose sum falls off the end of time.
        ("+175307616h", None),
        // The ends of the calendar are dates; one step past them is not.
        ("9999-12-31", Some(2_912_147)),
        ("10000-01-01", None),
        ("-009999-01-01", Some(-4_392_336)),
        ("-010000-01-01", None),
        ("2026-02-30", None),
        // The `i8` a clock time parses into, and its pm sum.
        ("128:00", None),
        ("0:128", None),
        ("24:00", None),
        ("128pm", None),
        ("127pm", None),
        ("tomorrow 128:00", None),
    ];

    /// The instant the table's day counts are from: 2026-10-23 in New York.
    const EDGE_NOW: &str = "2026-10-24T03:00:00Z";

    #[test]
    fn every_edge_phrase_has_its_pinned_answer() {
        for &(phrase, days) in EDGE_PHRASES {
            assert_eq!(
                read(phrase, at(EDGE_NOW), &ny()),
                days.map(When::Days),
                "{phrase:?}"
            );
        }
    }

    /// The ends of time are where "today" has no tomorrow and a day count no
    /// room, so the table's phrases must still return there.
    #[test]
    fn every_edge_phrase_returns_at_the_ends_of_time() {
        let phrases = EDGE_PHRASES.iter().map(|&(phrase, _)| phrase);
        for phrase in phrases.chain(["today", "tomorrow", "tonight", "next friday", "+1m"]) {
            for now in [Timestamp::MIN, Timestamp::MAX] {
                for reader in [ny(), TimeZone::UTC] {
                    let _ = read(phrase, now, &reader);
                }
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            rng_seed: sunrise_test_seed::proptest_rng_seed(),
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Any integer the phrase can carry, in any unit, is a value or none.
        #[test]
        fn a_relative_phrase_with_any_count_is_read_without_panicking(
            n in proptest::prelude::any::<i128>(),
            unit in proptest::sample::select(vec!['d', 'w', 'h', 'm']),
        ) {
            let _ = read(&format!("+{n}{unit}"), at("2026-10-24T03:00:00Z"), &ny());
        }

        /// Every day count that fits is that many days.
        #[test]
        fn a_relative_day_count_that_fits_is_that_many_days(n in 0..=i32::MAX) {
            proptest::prop_assert_eq!(
                read(&format!("+{n}d"), at("2026-10-24T03:00:00Z"), &ny()),
                Some(When::Days(n))
            );
        }

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
