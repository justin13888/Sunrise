//! Properties of the search query parser (`docs/08-features/search.md`
//! §Query language, ADR-0052 §3): it is total, and its canonical text reads
//! back as the same query.
//!
//! That no query reaches FTS5 as a syntax error is a property of the core,
//! which quotes what it sends, and is tested there against a real index.

use proptest::prelude::*;
use sunrise_domain::search::{parse, parse_with_notices, MAX_INPUT};

/// Fragments the grammar gives meaning to, so generated input exercises
/// operators, quoting and nesting rather than only plain words.
const FRAGMENTS: &[&str] = &[
    "a",
    "pass",
    "東京",
    "café",
    "OR",
    "or",
    "-",
    "--",
    "(",
    ")",
    "\"",
    "#",
    "@",
    "!",
    "!1",
    "!9",
    "#work",
    "@home",
    "stream:",
    "context:",
    "state:",
    "kind:",
    "is:",
    "has:",
    "priority",
    "planned",
    "due",
    "hard",
    "target",
    "created",
    "done",
    "place:",
    ":",
    "<",
    "<=",
    ">",
    ">=",
    ",",
    "..",
    "today",
    "mon",
    "+3d",
    "-2w",
    "2026-10-01",
    "2026-10",
    "open",
    "task",
    "late",
    "attachment",
    "colour:",
    " ",
    "  ",
    "\t",
    "\n",
    "\u{0}",
    "\u{301}",
    "Side project",
];

fn grammar_input() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            4 => proptest::sample::select(FRAGMENTS).prop_map(str::to_owned),
            1 => any::<char>().prop_map(String::from),
        ],
        0..48,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    // `Direct`, not the `SourceParallel` default: see
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        cases: 2048,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/search_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// Every string parses, whatever it holds.
    #[test]
    fn any_string_parses(s in any::<String>()) {
        let parsed = parse_with_notices(&s);
        let len = u32::try_from(s.chars().count()).unwrap_or(u32::MAX);
        for n in &parsed.notices {
            prop_assert!(n.start <= n.end && n.end <= len, "{n:?} in {s:?}");
        }
    }

    /// `parse(display(q)) == q`, for arbitrary text.
    #[test]
    fn the_canonical_text_reads_back_from_any_string(s in any::<String>()) {
        let q = parse(&s);
        let canonical = q.to_string();
        prop_assert_eq!(parse(&canonical), q, "canonical {:?}", canonical);
    }

    /// `parse(display(q)) == q`, for text built from the grammar's own
    /// pieces, and the canonical text is a fixed point.
    #[test]
    fn the_canonical_text_reads_back_from_grammar_input(s in grammar_input()) {
        let q = parse(&s);
        let canonical = q.to_string();
        let again = parse(&canonical);
        prop_assert_eq!(&again, &q, "canonical {:?}", canonical);
        prop_assert_eq!(again.to_string(), canonical);
    }
}

#[test]
fn a_huge_input_parses_quickly_and_reads_back() {
    let huge = "(#a OR -\"b c\" due<fri) ".repeat(MAX_INPUT / 8);
    let q = parse(&huge);
    assert_eq!(parse(&q.to_string()), q);
    let spaces = " ".repeat(1 << 20);
    assert!(parse(&spaces).is_empty());
}
