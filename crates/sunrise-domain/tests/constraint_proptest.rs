//! Property tests for scheduling constraints: arbitrary *valid* constraint
//! lists round-trip through canonical CBOR encode/decode with value identity.

use proptest::prelude::*;
use sunrise_cbor::{decode_canonical, encode_canonical};
use sunrise_domain::constraint::{
    validate_list, ConstraintSeverity, DateRange, ScheduleConstraint, TimeOfDayRange, WeekdaySet,
    MAX_CONSTRAINTS,
};
use sunrise_domain::rrule::Weekday;

fn civil_time_strategy() -> impl Strategy<Value = jiff::civil::Time> {
    (0i8..24, 0i8..60, 0i8..60).prop_map(|(h, m, s)| jiff::civil::time(h, m, s, 0))
}

fn civil_date_strategy() -> impl Strategy<Value = jiff::civil::Date> {
    (2000i16..2100, 1i8..13, 1i8..29).prop_map(|(y, m, d)| jiff::civil::date(y, m, d))
}

fn severity_strategy() -> impl Strategy<Value = ConstraintSeverity> {
    prop_oneof![
        Just(ConstraintSeverity::Hard),
        Just(ConstraintSeverity::Soft)
    ]
}

const ALL_WEEKDAYS: [Weekday; 7] = [
    Weekday::Mo,
    Weekday::Tu,
    Weekday::We,
    Weekday::Th,
    Weekday::Fr,
    Weekday::Sa,
    Weekday::Su,
];

fn weekday_set_strategy() -> impl Strategy<Value = WeekdaySet> {
    proptest::collection::vec(0usize..7, 0..7)
        .prop_map(|idxs| WeekdaySet::from_days(idxs.into_iter().map(|i| ALL_WEEKDAYS[i])))
}

/// Build an always-*valid* constraint: at least one dimension populated,
/// ordered time and date ranges.
fn valid_constraint_strategy() -> impl Strategy<Value = ScheduleConstraint> {
    (
        proptest::option::of(civil_time_strategy()),
        weekday_set_strategy(),
        proptest::option::of((civil_date_strategy(), proptest::option::of(0i64..3650))),
        severity_strategy(),
    )
        .prop_map(|(t, days, dr, severity)| {
            // Time range: ensure start < end by expanding to a full day when
            // degenerate; if start is 23:59:59 shift start down.
            let time_of_day = t.map(|start| {
                let end = if start >= jiff::civil::time(23, 59, 59, 0) {
                    start
                } else {
                    jiff::civil::time(23, 59, 59, 0)
                };
                let start = if start >= end {
                    jiff::civil::time(0, 0, 0, 0)
                } else {
                    start
                };
                TimeOfDayRange { start, end }
            });
            let date_range = dr.map(|(start, add)| DateRange {
                start,
                end: add.map(|days| start.saturating_add(jiff::Span::new().days(days))),
            });
            ScheduleConstraint {
                time_of_day,
                days_of_week: days,
                date_range,
                severity,
            }
        })
        // Guarantee at least one dimension is populated.
        .prop_filter("must populate ≥1 dimension", |c| {
            c.time_of_day.is_some() || !c.days_of_week.is_empty() || c.date_range.is_some()
        })
}

fn valid_list_strategy() -> impl Strategy<Value = Vec<ScheduleConstraint>> {
    proptest::collection::vec(valid_constraint_strategy(), 0..=MAX_CONSTRAINTS)
}

proptest! {
    // `Direct`, not the `SourceParallel` default: nothing above a `tests/` file
    // holds a `lib.rs` or `main.rs`, so the default warns and drops the
    // counterexample beside this source instead. See
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/constraint_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    #[test]
    fn valid_constraints_pass_validation(c in valid_constraint_strategy()) {
        prop_assert!(c.validate().is_ok(), "generated constraint should be valid: {c:?}");
    }

    #[test]
    fn constraint_list_round_trips_canonical_cbor(list in valid_list_strategy()) {
        prop_assert!(validate_list(&list).is_ok());
        let bytes = encode_canonical(&list).expect("encode");
        let back: Vec<ScheduleConstraint> = decode_canonical(&bytes).expect("canonical decode");
        prop_assert_eq!(back, list);
    }

    #[test]
    fn single_constraint_round_trips(c in valid_constraint_strategy()) {
        let bytes = encode_canonical(&c).expect("encode");
        let back: ScheduleConstraint = decode_canonical(&bytes).expect("canonical decode");
        prop_assert_eq!(back, c);
    }
}
