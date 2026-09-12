//! Property test for the recurrence round trip.
//!
//! `src/recur.rs` states the invariant in prose — *"the proof that this front
//! end produces rules the engine accepts: everything it emits must survive the
//! domain's own parser"* — and then checks seven hardcoded phrases against the
//! thirteen input forms it documents. A rule shape nobody wrote a literal for
//! is unproven, and the parts most likely to break the trip (`BYSETPOS`, a
//! `BYMONTHDAY` list, an `UNTIL` with a fractional second, an `INTERVAL` at
//! the edge of `u32`) are exactly the ones no literal covers.
//!
//! So the property is stated over the rules themselves: for any `RRule` this
//! build can hold, writing it and reading it back is the identity. Both
//! directions of the front end depend on it — the wire form is what the
//! storage layer persists, so a rule that does not survive the trip is a
//! routine that fires on a different cadence after a restart.
//!
//! # Scope
//!
//! **Round-trip fidelity only.** Panic-freedom on arbitrary *strings* is not
//! this file's job and is not asserted here: `fuzz/fuzz_targets/rrule.rs`
//! already owns it, and `docs/10-cross-cutting/testing.md` §Fuzzing records
//! that division of labour. Adding a second, weaker version of it here would
//! put two answers in the repository for one question.

#![allow(clippy::unwrap_used)]

use jiff::Timestamp;
use proptest::prelude::*;
use sunrise_domain::recur::parse_recurrence;
use sunrise_domain::rrule::{Frequency, RRule, Weekday};

fn frequency() -> impl Strategy<Value = Frequency> {
    prop_oneof![
        Just(Frequency::Daily),
        Just(Frequency::Weekly),
        Just(Frequency::Monthly),
        Just(Frequency::Yearly),
    ]
}

fn weekday() -> impl Strategy<Value = Weekday> {
    prop_oneof![
        Just(Weekday::Su),
        Just(Weekday::Mo),
        Just(Weekday::Tu),
        Just(Weekday::We),
        Just(Weekday::Th),
        Just(Weekday::Fr),
        Just(Weekday::Sa),
    ]
}

/// `INTERVAL` must be at least 1 — zero is rejected on parse, so a rule
/// holding it could not have come from one. The top of the range is included
/// because `to_rfc5545` formats the integer and `parse` reads it back through
/// `u32::from_str`, and the widest value is where that pairing would show a
/// seam.
fn interval() -> impl Strategy<Value = u32> {
    prop_oneof![
        9 => 1u32..=520,
        1 => Just(u32::MAX),
    ]
}

/// `UNTIL` is written with jiff's `Display` and read with its `FromStr`.
/// Sub-second instants are generated deliberately: they print a fractional
/// part that a whole-second literal never exercises.
fn until() -> impl Strategy<Value = Timestamp> {
    // 1970-01-01 .. 2100-01-01, in milliseconds.
    (0i64..4_102_444_800_000).prop_map(|ms| Timestamp::from_millisecond(ms).unwrap())
}

fn rrule_strategy() -> impl Strategy<Value = RRule> {
    (
        frequency(),
        interval(),
        proptest::collection::vec(weekday(), 0..5),
        // Negative month-days are 1-based-from-end; zero is not meaningful but
        // parses, so it is generated rather than assumed away.
        proptest::collection::vec(-31i32..=31, 0..4),
        proptest::collection::vec(1u32..=12, 0..4),
        proptest::collection::vec(-366i32..=366, 0..3),
        proptest::option::of(0u32..=520),
        proptest::option::of(until()),
        proptest::option::of(weekday()),
    )
        .prop_map(
            |(freq, interval, by_day, by_month_day, by_month, by_set_pos, count, until, wkst)| {
                RRule {
                    freq,
                    interval,
                    by_day,
                    by_month_day,
                    by_month,
                    by_set_pos,
                    count,
                    until,
                    wkst,
                }
            },
        )
}

/// The phrase forms `recur`'s table documents, with their variable parts
/// generated rather than fixed. The rule each one produces is the parser's
/// business; what is asserted is only that whatever it produces survives the
/// wire.
fn phrase_strategy() -> impl Strategy<Value = String> {
    let day = prop_oneof![
        Just("monday"),
        Just("tue"),
        Just("wed"),
        Just("thurs"),
        Just("fri"),
        Just("sat"),
        Just("sun"),
    ];
    prop_oneof![
        Just("daily".to_string()),
        Just("every day".to_string()),
        Just("weekly".to_string()),
        Just("weekdays".to_string()),
        Just("weekends".to_string()),
        Just("monthly".to_string()),
        Just("yearly".to_string()),
        Just("annually".to_string()),
        Just("monthly on the last day".to_string()),
        (1u32..500).prop_map(|n| format!("every {n} days")),
        (1u32..500).prop_map(|n| format!("every {n} weeks")),
        (1u32..500).prop_map(|n| format!("every {n} months")),
        (1i32..=31).prop_map(|d| format!("monthly on day {d}")),
        day.clone().prop_map(|d| format!("every {d}")),
        (1u32..60, day.clone()).prop_map(|(n, d)| format!("every {n} weeks on {d}")),
        (day.clone(), day).prop_map(|(a, b)| format!("every {a}, {b}")),
        (1u32..500).prop_map(|n| format!("every week x{n}")),
    ]
}

proptest! {
    // `Direct`, not the `SourceParallel` default: nothing above a `tests/` file
    // holds a `lib.rs` or `main.rs`, so the default warns and drops the
    // counterexample beside this source instead. See
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/recur_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Writing a rule and reading it back is the identity. This is what lets
    /// the storage layer keep a routine as text.
    #[test]
    fn a_rule_survives_the_wire_form_unchanged(rule in rrule_strategy()) {
        let body = rule.to_rfc5545();
        prop_assert!(
            !body.starts_with("RRULE:"),
            "the body is emitted without the wire prefix, which the caller adds"
        );
        let parsed = RRule::parse(&body)
            .map_err(|e| TestCaseError::fail(format!("{body}: {e}")))?;
        prop_assert_eq!(&parsed, &rule);
        // And the text is stable, not merely equivalent: two devices that hold
        // the same rule must persist the same bytes for it.
        prop_assert_eq!(parsed.to_rfc5545(), body);
    }

    /// Every phrase the front end accepts produces a rule that survives the
    /// same trip — the invariant `recur`'s own round-trip test states, with
    /// the variable parts of each documented form generated instead of fixed.
    #[test]
    fn a_phrase_produces_a_rule_that_survives_the_wire_form(phrase in phrase_strategy()) {
        let rule = parse_recurrence(&phrase)
            .map_err(|e| TestCaseError::fail(format!("{phrase:?}: {e}")))?;
        let body = rule.to_rfc5545();
        let reparsed = RRule::parse(&body)
            .map_err(|e| TestCaseError::fail(format!("{phrase:?} -> {body}: {e}")))?;
        prop_assert_eq!(reparsed, rule, "{:?} -> {}", phrase, body);
    }
}
