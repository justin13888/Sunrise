//! Properties of the typed-input parsers (`capture::parse`, the annotate
//! grammar and `parse_when_time`): they are total. Whatever a user types, the
//! answer is a value or a refusal, never a panic.
//!
//! The grammar strategy mixes the parsers' own sigils and units with
//! multi-byte characters, because a parser that slices by byte offset only
//! fails when a character wider than one byte sits next to a boundary it
//! computed.

use jiff::tz::TimeZone;
use jiff::Timestamp;
use proptest::prelude::*;
use sunrise_domain::capture::{parse, parse_when_time, NamedRef};
use sunrise_domain::parse_annotate;
use sunrise_id::{EntityKind, EntityRef};

/// Pieces the grammars give meaning to, and characters wider than a byte.
const FRAGMENTS: &[&str] = &[
    "^",
    "+",
    "-",
    "#",
    "@",
    "!",
    "~",
    "*",
    "due:",
    "target:",
    "3",
    "12",
    "d",
    "w",
    "h",
    "m",
    "am",
    "pm",
    ":",
    "next",
    "friday",
    "tomorrow",
    "2026-11-02",
    " ",
    "\u{e9}",
    "\u{65e5}",
    "\u{1f600}",
    "\u{301}",
    "travel",
    "home",
];

fn grammar_input() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            4 => proptest::sample::select(FRAGMENTS).prop_map(str::to_owned),
            1 => any::<char>().prop_map(String::from),
        ],
        0..24,
    )
    .prop_map(|parts| parts.concat())
}

fn now() -> Timestamp {
    "2026-08-21T12:00:00Z".parse().expect("valid timestamp")
}

fn named() -> Vec<NamedRef<'static>> {
    vec![
        NamedRef {
            id: EntityRef::new(EntityKind::Stream, [1; 16]),
            name: "travel",
        },
        NamedRef {
            id: EntityRef::new(EntityKind::Context, [2; 16]),
            name: "home",
        },
    ]
}

fn exercise(s: &str) {
    let tz = TimeZone::UTC;
    let refs = named();
    let _ = parse(s, now(), &tz, &refs, &refs);
    let _ = parse_annotate(s, &refs, &refs, now(), &tz);
    let _ = parse_when_time(s, now(), &tz);
}

proptest! {
    // `Direct`, not the `SourceParallel` default: see
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        cases: 1024,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/capture_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Every string is read without a panic.
    #[test]
    fn any_string_is_read_without_panicking(s in any::<String>()) {
        exercise(&s);
    }

    /// Every string built from the grammars' own pieces is read without a
    /// panic.
    #[test]
    fn grammar_input_is_read_without_panicking(s in grammar_input()) {
        exercise(&s);
    }
}
