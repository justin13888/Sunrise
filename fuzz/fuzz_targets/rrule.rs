//! RRULE parsing, canonical rendering, and DST-aware expansion.
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "RRULE parser and DST-aware expansion".
//!
//! The whole input is an RRULE body — the text after `RRULE:` — so the corpus
//! stays human-readable and a minimized crash is a rule someone can paste into
//! a calendar. Seeds are the rules the domain crate's own tests and the shipped
//! `.ics` fixtures use.
//!
//! # The two assertions
//!
//! 1. `to_rfc5545` round-trips. The doc comment on it says so ("Round-trips
//!    losslessly through `RRule::parse`"), and the storage layer persists a
//!    rule as that text, so a rule that renders to something that parses back
//!    differently is silent data loss on the next read. A generator of valid
//!    `RRule` values checks the same sentence over structures the generator
//!    thought of; this checks it over every structure the *parser* admits,
//!    which is a wider set — `BYMONTHDAY=99`, `COUNT=0`, a `BYSETPOS` list
//!    longer than the candidate set it indexes.
//!
//! 2. Expansion terminates and stays inside its window, in a zone with a real
//!    DST discontinuity. `expand` walks up to `MAX_PERIODS` civil dates and
//!    resolves each into an instant; the resolution is where a nonexistent or
//!    ambiguous local time lands, and a rule that selects one is reachable
//!    from text.

#![no_main]

use jiff::{tz::TimeZone, Timestamp};
use libfuzzer_sys::fuzz_target;
use sunrise_domain::routine_gen::expand;
use sunrise_domain::rrule::RRule;

/// Fixed anchors and zones, rather than fuzzer-chosen ones.
///
/// Keeping the whole input as text is what makes the corpus seedable and a
/// crash readable, so the expansion knobs are a small fixed matrix instead of
/// bytes carved off the input. Each zone is here for a discontinuity a
/// half-hour-offset-free zone does not have:
///
/// - `America/New_York` — the ordinary spring-forward gap and autumn overlap.
/// - `Australia/Lord_Howe` — a **30-minute** DST shift, so a gap that is not a
///   whole hour wide.
/// - `Pacific/Chatham` — a 12:45 base offset, where civil minute arithmetic
///   and instant arithmetic disagree about what "the same time" means.
/// - `UTC` — the control.
const ZONES: [&str; 4] = [
    "America/New_York",
    "Australia/Lord_Howe",
    "Pacific/Chatham",
    "UTC",
];

/// 2024-03-10T02:30:00 US/Eastern falls inside that year's spring-forward gap;
/// as a UTC instant this is the minute the gap opens.
const ANCHOR_MS: i64 = 1_710_038_700_000;

/// One year past the anchor. Wide enough to cross both transitions in every
/// zone above, narrow enough that a `FREQ=MINUTELY`-shaped rule cannot turn one
/// iteration into a minute of work.
const WINDOW_MS: i64 = 366 * 24 * 60 * 60 * 1000;

fuzz_target!(|data: &[u8]| {
    let Ok(body) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(rule) = RRule::parse(body) else {
        return;
    };

    // 1. Canonical rendering round-trips, and is a fixed point.
    let rendered = rule.to_rfc5545();
    let reparsed =
        RRule::parse(&rendered).expect("to_rfc5545 must emit a body RRule::parse accepts");
    assert_eq!(
        rule, reparsed,
        "to_rfc5545 lost or changed a part: {rendered:?}"
    );
    assert_eq!(
        rendered,
        reparsed.to_rfc5545(),
        "to_rfc5545 is not idempotent"
    );

    // 2. Expansion. Every occurrence must sit inside the window it was asked
    // for; `ExpandError` is a legitimate answer and is not asserted against.
    let Ok(anchor) = Timestamp::from_millisecond(ANCHOR_MS) else {
        return;
    };
    let Ok(window_end) = Timestamp::from_millisecond(ANCHOR_MS + WINDOW_MS) else {
        return;
    };
    for zone in ZONES {
        let Ok(tz) = TimeZone::get(zone) else {
            continue;
        };
        let Ok(occurrences) = expand(&rule, anchor, &tz, (anchor, window_end)) else {
            continue;
        };
        for occ in &occurrences {
            assert!(
                occ.at >= anchor && occ.at < window_end,
                "expand returned an occurrence outside its window in {zone}: {occ:?}"
            );
        }
        // `COUNT=n` bounds the series from the anchor, and the window starts at
        // the anchor, so the window can never hold more than `n`.
        if let Some(count) = rule.count {
            assert!(
                occurrences.len() <= count as usize,
                "expand returned {} occurrences for COUNT={count} in {zone}",
                occurrences.len()
            );
        }
    }
});
