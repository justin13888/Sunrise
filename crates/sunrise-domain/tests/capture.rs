//! Capture-parser tests.
//!
//! The governing invariant, asserted throughout: **nothing the user typed is
//! ever silently discarded.** A token either applies to the draft or is
//! reported in `unresolved` *and* left in the title. A capture surface that
//! quietly drops input is worse than one that declines to interpret it.

#![allow(clippy::unwrap_used)]

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::capture::{normalize_title, parse, parse_when, Capture, NamedRef, Unresolved};
use sunrise_domain::SunriseTime;
use sunrise_id::{EntityKind, EntityRef};

/// 2026-08-21 is a Friday. Fixed so weekday arithmetic is checkable by hand.
const NOW: &str = "2026-08-21T12:00:00Z";

fn now() -> Timestamp {
    NOW.parse().unwrap()
}

fn utc() -> TimeZone {
    TimeZone::UTC
}

fn stream_ref(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Stream, [n; 16])
}

fn ctx_ref(n: u8) -> EntityRef {
    EntityRef::new(EntityKind::Context, [n; 16])
}

fn streams() -> Vec<(EntityRef, &'static str)> {
    vec![
        (stream_ref(1), "travel"),
        (stream_ref(2), "work-acme"),
        (stream_ref(3), "work-beta"),
    ]
}

fn contexts() -> Vec<(EntityRef, &'static str)> {
    vec![(ctx_ref(1), "errands"), (ctx_ref(2), "home")]
}

fn run(input: &str) -> Capture {
    let s = streams();
    let c = contexts();
    let sr: Vec<NamedRef<'_>> = s
        .iter()
        .map(|(id, n)| NamedRef { id: *id, name: n })
        .collect();
    let cr: Vec<NamedRef<'_>> = c
        .iter()
        .map(|(id, n)| NamedRef { id: *id, name: n })
        .collect();
    parse(input, now(), &utc(), &sr, &cr)
}

/// Local civil time of a [`SunriseTime`] in UTC, as `YYYY-MM-DD HH:MM`.
fn civil(t: &SunriseTime) -> String {
    let z = t.to_instant(&utc()).to_zoned(utc());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        z.year(),
        z.month(),
        z.day(),
        z.hour(),
        z.minute()
    )
}

// ---------------------------------------------------------------------------
// The spec's own examples.
// ---------------------------------------------------------------------------

#[test]
fn spec_example_full_annotation() {
    let c = run("Renew passport #travel @errands ^next saturday !1 ~1h");
    assert_eq!(c.draft.title, "Renew passport");
    assert_eq!(c.draft.stream_id, Some(stream_ref(1)));
    assert_eq!(c.draft.contexts, vec![ctx_ref(1)]);
    assert_eq!(c.draft.priority, Some(1));
    assert_eq!(c.draft.estimated_duration_s, Some(3600));
    // Friday 2026-08-21; "next saturday" is the Saturday of the following
    // week, not tomorrow.
    assert_eq!(civil(&c.draft.scheduled_at.unwrap()), "2026-08-29 00:00");
    assert!(c.unresolved.is_empty(), "{:?}", c.unresolved);
}

#[test]
fn spec_example_plain_text_goes_to_inbox() {
    let c = run("Email Sara about Q3");
    assert_eq!(c.draft.title, "Email Sara about Q3");
    assert_eq!(
        c.draft.stream_id, None,
        "no #stream means Inbox, which the core resolves"
    );
    assert!(c.draft.priority.is_none());
    assert!(c.draft.scheduled_at.is_none());
    assert!(c.unresolved.is_empty());
}

/// The spec lists `Standup ^weekday 9:30 #work-acme`. "weekday" is a
/// recurrence rule, not a date — it belongs in a Routine's RRULE. Assert we
/// decline to guess rather than silently scheduling something wrong.
#[test]
fn spec_example_weekday_recurrence_is_declined_not_guessed() {
    let c = run("Standup ^weekday 9:30 #work-acme");
    assert_eq!(c.draft.stream_id, Some(stream_ref(2)));
    assert!(
        c.draft.scheduled_at.is_none(),
        "a recurrence rule must not be silently collapsed to one datetime"
    );
    assert!(c
        .unresolved
        .iter()
        .any(|u| matches!(u, Unresolved::UnparseableDate(_))));
    assert!(
        c.draft.title.contains("weekday"),
        "declined input must survive in the title, got {:?}",
        c.draft.title
    );
}

// ---------------------------------------------------------------------------
// Nothing is silently lost.
// ---------------------------------------------------------------------------

#[test]
fn unknown_stream_is_reported_and_kept_in_title() {
    let c = run("Book flights #nosuchstream");
    assert_eq!(c.draft.stream_id, None);
    assert_eq!(
        c.unresolved,
        vec![Unresolved::UnknownStream("nosuchstream".into())]
    );
    assert_eq!(c.draft.title, "Book flights #nosuchstream");
}

#[test]
fn ambiguous_stream_offers_candidates_and_picks_nothing() {
    let c = run("Sprint planning #work");
    assert_eq!(
        c.draft.stream_id, None,
        "an ambiguous prefix must not pick arbitrarily"
    );
    match &c.unresolved[..] {
        [Unresolved::AmbiguousStream { typed, candidates }] => {
            assert_eq!(typed, "work");
            let mut got = candidates.clone();
            got.sort();
            assert_eq!(got, vec!["work-acme".to_string(), "work-beta".to_string()]);
        }
        other => panic!("expected AmbiguousStream, got {other:?}"),
    }
    assert!(c.draft.title.contains("#work"));
}

#[test]
fn unique_prefix_resolves_but_exact_match_wins() {
    assert_eq!(run("x #trav").draft.stream_id, Some(stream_ref(1)));
    // "work-acme" is both an exact match and a prefix of nothing else.
    assert_eq!(run("x #work-acme").draft.stream_id, Some(stream_ref(2)));
}

#[test]
fn priority_out_of_range_is_reported_not_clamped() {
    for bad in ["!0", "!6", "!99", "!x"] {
        let c = run(&format!("Thing {bad}"));
        assert_eq!(c.draft.priority, None, "{bad} must not set a priority");
        assert!(
            c.draft.title.contains(bad),
            "{bad} must survive in the title"
        );
        assert!(matches!(
            c.unresolved.as_slice(),
            [Unresolved::PriorityOutOfRange(_)]
        ));
    }
}

#[test]
fn every_priority_in_range_is_accepted() {
    for p in 1..=5u8 {
        assert_eq!(run(&format!("t !{p}")).draft.priority, Some(p));
    }
}

// ---------------------------------------------------------------------------
// Durations.
// ---------------------------------------------------------------------------

#[test]
fn duration_forms() {
    let cases = [
        ("~30m", 30 * 60),
        ("~2h", 2 * 3600),
        ("~90", 90 * 60), // bare number means minutes
        ("~1h30m", 3600 + 30 * 60),
        ("~1d", 86_400),
    ];
    for (tok, want) in cases {
        let c = run(&format!("t {tok}"));
        assert_eq!(c.draft.estimated_duration_s, Some(want), "for {tok}");
        assert_eq!(c.draft.title, "t");
    }
}

/// `1h30` is a typo for `1h30m`, not "1h and 30 seconds". Declining is safer
/// than inventing a unit the user did not type.
#[test]
fn duration_trailing_number_without_unit_is_rejected() {
    let c = run("t ~1h30");
    assert_eq!(c.draft.estimated_duration_s, None);
    assert!(matches!(
        c.unresolved.as_slice(),
        [Unresolved::UnparseableDuration(_)]
    ));
    assert!(c.draft.title.contains("~1h30"));
}

// ---------------------------------------------------------------------------
// Dates.
// ---------------------------------------------------------------------------

#[test]
fn date_keywords() {
    // now = Friday 2026-08-21 12:00 UTC
    let cases = [
        ("today", "2026-08-21 00:00"),
        ("tonight", "2026-08-21 20:00"),
        ("tomorrow", "2026-08-22 00:00"),
        ("2026-12-25", "2026-12-25 00:00"),
    ];
    for (input, want) in cases {
        let ts = parse_when(input, now(), &utc()).unwrap_or_else(|| panic!("{input} should parse"));
        assert_eq!(civil(&ts.into()), want, "for {input}");
    }
}

/// A bare weekday is the next such day strictly after today; `next <weekday>`
/// is a further week out. Today is Friday, so "friday" must mean *next*
/// Friday, never today — scheduling something for a moment that has already
/// passed is the failure mode here.
#[test]
fn weekday_arithmetic() {
    let cases = [
        ("saturday", "2026-08-22 00:00"),
        ("sat", "2026-08-22 00:00"),
        ("friday", "2026-08-28 00:00"),
        ("thursday", "2026-08-27 00:00"),
        ("next saturday", "2026-08-29 00:00"),
        ("next friday", "2026-09-04 00:00"),
    ];
    for (input, want) in cases {
        let ts = parse_when(input, now(), &utc()).unwrap_or_else(|| panic!("{input} should parse"));
        assert_eq!(civil(&ts.into()), want, "for {input}");
    }
}

#[test]
fn times_and_meridiem() {
    let cases = [
        ("9am", "2026-08-21 09:00"),
        ("9:30", "2026-08-21 09:30"),
        ("14:00", "2026-08-21 14:00"),
        ("9pm", "2026-08-21 21:00"),
        // The two everyone gets wrong.
        ("12am", "2026-08-21 00:00"),
        ("12pm", "2026-08-21 12:00"),
        ("tomorrow 9am", "2026-08-22 09:00"),
        ("saturday 14:30", "2026-08-22 14:30"),
    ];
    for (input, want) in cases {
        let ts = parse_when(input, now(), &utc()).unwrap_or_else(|| panic!("{input} should parse"));
        assert_eq!(civil(&ts.into()), want, "for {input}");
    }
}

#[test]
fn relative_spans() {
    let cases = [
        ("+3d", "2026-08-24 12:00"),
        ("+2w", "2026-09-04 12:00"),
        ("+6h", "2026-08-21 18:00"),
        ("+90m", "2026-08-21 13:30"),
    ];
    for (input, want) in cases {
        let ts = parse_when(input, now(), &utc()).unwrap_or_else(|| panic!("{input} should parse"));
        assert_eq!(civil(&ts.into()), want, "for {input}");
    }
}

#[test]
fn unparseable_dates_return_none_rather_than_guessing() {
    for bad in [
        "sometime",
        "the day after the thing",
        "2026-13-45",
        "next",
        "25:00",
        "",
    ] {
        assert!(
            parse_when(bad, now(), &utc()).is_none(),
            "{bad:?} must not parse"
        );
    }
}

/// Scheduling across a DST spring-forward must land on a real instant. In
/// `America/New_York`, 2026-03-08 02:30 does not exist; the compatible reading
/// moves forward rather than failing or silently producing 01:30.
#[test]
fn dst_gap_is_resolved_compatibly() {
    let tz = TimeZone::get("America/New_York").unwrap();
    let base: Timestamp = "2026-03-07T12:00:00Z".parse().unwrap();
    let ts = parse_when("2026-03-08 02:30", base, &tz).expect("must resolve inside a DST gap");
    let z = ts.to_zoned(tz);
    assert_eq!(z.day(), 8);
    assert_eq!(
        (z.hour(), z.minute()),
        (3, 30),
        "a nonexistent civil time must shift forward, not silently back"
    );
}

#[test]
fn due_token_sets_due_not_scheduled() {
    let c = run("File taxes *due:2026-04-15*");
    assert_eq!(civil(&c.draft.due_at.unwrap()), "2026-04-15 00:00");
    assert!(c.draft.scheduled_at.is_none());
    assert_eq!(c.draft.title, "File taxes");
}

// ---------------------------------------------------------------------------
// Structure and invariants.
// ---------------------------------------------------------------------------

#[test]
fn multiple_contexts_accumulate_but_last_stream_wins() {
    let c = run("t @errands @home #travel #work-acme");
    assert_eq!(c.draft.contexts, vec![ctx_ref(1), ctx_ref(2)]);
    assert_eq!(
        c.draft.stream_id,
        Some(stream_ref(2)),
        "a task has exactly one stream, so the last one typed wins"
    );
}

#[test]
fn annotations_anywhere_in_the_line() {
    let a = run("#travel !2 Book flights ~30m");
    assert_eq!(a.draft.title, "Book flights");
    assert_eq!(a.draft.stream_id, Some(stream_ref(1)));
    assert_eq!(a.draft.priority, Some(2));
    assert_eq!(a.draft.estimated_duration_s, Some(30 * 60));
}

#[test]
fn bare_sigils_are_title_text() {
    // "#", "@" alone, and an email address, must not be eaten as annotations.
    let c = run("email me @ # sara@example.com");
    assert!(c.draft.title.contains("sara@example.com"));
    assert!(c.draft.title.contains('#'));
    assert_eq!(c.draft.contexts, vec![], "{:?}", c.draft.contexts);
}

#[test]
fn whitespace_is_collapsed_in_the_title() {
    assert_eq!(run("  a   b \t c  ").draft.title, "a b c");
}

#[test]
fn empty_input_yields_empty_title_for_live_preview() {
    // Deliberately not an error: the parser runs on every keystroke, and
    // validation is the caller's job at commit time.
    let c = run("   ");
    assert_eq!(c.draft.title, "");
    assert!(c.unresolved.is_empty());
    assert!(
        c.draft.validate().is_err(),
        "an empty title must not commit"
    );
}

#[test]
fn parse_is_deterministic() {
    let input = "Renew passport #travel @errands ^next saturday !1 ~1h";
    let a = run(input);
    let b = run(input);
    assert_eq!(a.draft.title, b.draft.title);
    assert_eq!(a.draft.stream_id, b.draft.stream_id);
    assert_eq!(a.draft.scheduled_at, b.draft.scheduled_at);
    assert_eq!(a.draft.priority, b.draft.priority);
    assert_eq!(a.unresolved, b.unresolved);
}

#[test]
fn parsed_drafts_pass_domain_validation() {
    let c = run("Renew passport #travel @errands ^next saturday !1 ~1h");
    c.draft
        .validate()
        .expect("a fully-annotated parse must produce a valid draft");
}

// ---------------------------------------------------------------------------
// Dedup key.
// ---------------------------------------------------------------------------

#[test]
fn normalize_title_matches_the_spec() {
    assert_eq!(normalize_title("  Buy   MILK \t now "), "buy milk now");
    assert_eq!(normalize_title("Buy milk now"), "buy milk now");
    assert_eq!(normalize_title(""), "");
    assert_eq!(
        normalize_title("Buy Milk"),
        normalize_title("buy    milk"),
        "the dedup key must collapse case and spacing"
    );
}
