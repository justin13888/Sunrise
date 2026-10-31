//! Properties of the typed-input parsers (`capture::parse`, the annotate
//! grammar and `parse_when_time`): they are total. Whatever a user types, the
//! answer is a value or a refusal, never a panic.
//!
//! The grammar strategy mixes the parsers' own sigils and units with
//! multi-byte characters, because a parser that slices by byte offset only
//! fails when a character wider than one byte sits next to a boundary it
//! computed.
//!
//! Capture also promises that input is never silently discarded (the module
//! documentation of `capture`), and annotate that every token is applied or
//! reported, so both are asserted here over whole lines.

use jiff::tz::TimeZone;
use jiff::Timestamp;
use proptest::prelude::*;
use sunrise_domain::capture::{parse, parse_when_time, Capture, NamedRef, Unresolved};
use sunrise_domain::parse_annotate;
use sunrise_id::{EntityKind, EntityRef};

/// How many words a `^` date may span, as `capture` documents it.
const MAX_DATE_WORDS: usize = 3;

/// Pieces the grammars give meaning to, and characters wider than a byte.
const FRAGMENTS: &[&str] = &[
    "^",
    "+",
    "-",
    "#",
    "@",
    "!",
    "%",
    "~",
    "*",
    "low",
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

/// Whole words a capture line is made of, each landing on a different branch
/// of the parser: applied, reported, or plain text.
const LINE_WORDS: &[&str] = &[
    "buy",
    "milk",
    "friday",
    "next",
    "9am",
    "\u{e9}",
    "#travel",
    "#nope",
    "#",
    "@home",
    "@nope",
    "!1",
    "!9",
    "~30m",
    "~bad",
    "^",
    "^tomorrow",
    "^next",
    "^nonsense",
    "*due:tomorrow*",
    "*due:nonsense*",
    "*target:friday*",
    "*target:nonsense*",
];

/// A space-separated line, so the parser's multi-word and per-word branches
/// are reached far more often than concatenated fragments reach them.
fn capture_line() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            3 => proptest::sample::select(LINE_WORDS).prop_map(str::to_owned),
            1 => grammar_input(),
        ],
        0..12,
    )
    .prop_map(|words| words.join(" "))
}

/// Every string a conservation property reads: arbitrary, or line-shaped.
fn any_line() -> impl Strategy<Value = String> {
    prop_oneof![any::<String>(), grammar_input(), capture_line()]
}

/// Whether `needle` appears in `haystack` in order, repeats counted.
fn is_subsequence(needle: &[&str], haystack: &[&str]) -> bool {
    let mut rest = haystack.iter();
    needle.iter().all(|n| rest.any(|h| h == n))
}

/// Whether a `^` word at most `MAX_DATE_WORDS - 1` before `at` opens a phrase
/// that covers `at` and reads as a date: the one way a plain word may leave the
/// title, and only because it became `planned_at`.
fn read_as_date(words: &[&str], at: usize) -> bool {
    let tz = TimeZone::UTC;
    (at.saturating_sub(MAX_DATE_WORDS - 1)..at).any(|open| {
        let Some(head) = words[open].strip_prefix('^') else {
            return false;
        };
        (at..words.len().min(open + MAX_DATE_WORDS)).any(|last| {
            let tail = words[open + 1..=last].join(" ");
            parse_when_time(&format!("{head} {tail}"), now(), &tz).is_some()
        })
    })
}

/// Whether the draft holds what the annotation `word` would have set.
fn applied(word: &str, capture: &Capture) -> bool {
    let draft = &capture.draft;
    match word.chars().next() {
        Some('#') => draft.stream_id.is_some(),
        Some('@') => !draft.contexts.is_empty(),
        Some('!') => draft.priority.is_some(),
        Some('~') => draft.estimated_duration_s.is_some(),
        Some('^') => draft.planned_at.is_some(),
        _ if word.starts_with("*due:") => draft.hard_due_at.is_some(),
        _ if word.starts_with("*target:") => draft.target_at.is_some(),
        _ => false,
    }
}

/// The text of a reported token as the title must still hold it.
fn reported_text(unresolved: &Unresolved) -> String {
    match unresolved {
        Unresolved::UnknownStream(typed) | Unresolved::AmbiguousStream { typed, .. } => {
            format!("#{typed}")
        }
        Unresolved::UnknownContext(typed) | Unresolved::AmbiguousContext { typed, .. } => {
            format!("@{typed}")
        }
        Unresolved::UnparseableDate(text) => text.clone(),
        Unresolved::PriorityOutOfRange(rest) => format!("!{rest}"),
        Unresolved::UnparseableDuration(rest) => format!("~{rest}"),
    }
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

    /// The title is only ever the line's own words, in the line's order.
    #[test]
    fn the_title_invents_nothing(s in any_line()) {
        let refs = named();
        let capture = parse(&s, now(), &TimeZone::UTC, &refs, &refs);
        let words: Vec<&str> = s.split_whitespace().collect();
        let title: Vec<&str> = capture.draft.title.split_whitespace().collect();
        prop_assert!(is_subsequence(&title, &words), "{:?} -> {:?}", s, title);
    }

    /// A word with no annotation sigil stays in the title, unless a `^` phrase
    /// read it as part of a date.
    #[test]
    fn a_plain_word_is_never_dropped(s in any_line()) {
        let refs = named();
        let capture = parse(&s, now(), &TimeZone::UTC, &refs, &refs);
        let words: Vec<&str> = s.split_whitespace().collect();
        let title: Vec<&str> = capture.draft.title.split_whitespace().collect();
        let kept: Vec<&str> = words
            .iter()
            .enumerate()
            .filter(|(at, w)| {
                !w.starts_with(['#', '@', '!', '~', '^'])
                    && !w.starts_with("*due:")
                    && !w.starts_with("*target:")
                    && !read_as_date(&words, *at)
            })
            .map(|(_, w)| *w)
            .collect();
        prop_assert!(is_subsequence(&kept, &title), "{:?} -> {:?}", s, title);
    }

    /// An annotation either took effect or is still in the title.
    #[test]
    fn an_annotation_is_applied_or_kept(s in any_line()) {
        let refs = named();
        let capture = parse(&s, now(), &TimeZone::UTC, &refs, &refs);
        let title: Vec<&str> = capture.draft.title.split_whitespace().collect();
        for word in s.split_whitespace() {
            if word.starts_with(['#', '@', '!', '~', '^'])
                || word.starts_with("*due:")
                || word.starts_with("*target:")
            {
                prop_assert!(
                    title.contains(&word) || applied(word, &capture),
                    "{:?}: {:?} was dropped",
                    s,
                    word
                );
            }
        }
    }

    /// Whatever is reported as unresolved is still there to read in the title.
    #[test]
    fn a_reported_token_stays_in_the_title(s in any_line()) {
        let refs = named();
        let capture = parse(&s, now(), &TimeZone::UTC, &refs, &refs);
        for unresolved in &capture.unresolved {
            let text = reported_text(unresolved);
            prop_assert!(
                capture.draft.title.contains(&text),
                "{:?}: {:?} is not in {:?}",
                s,
                text,
                capture.draft.title
            );
        }
    }

    /// A line with any word in it either asks for something or reports why it
    /// could not: annotate has no title to fall back on.
    #[test]
    fn annotate_applies_or_reports_every_line(s in any_line()) {
        let refs = named();
        let edit = parse_annotate(&s, &refs, &refs, now(), &TimeZone::UTC);
        let blank = s.split_whitespace().next().is_none();
        prop_assert_eq!(edit.is_empty() && edit.errors.is_empty(), blank, "{:?}", s);
    }
}
