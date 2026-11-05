//! The contract between the two typed grammars: capture and annotate read the
//! same `~duration` sigil the same way.
//!
//! Each parser has its own tests, and those let the two drift apart: `~1h30`
//! was a typo to capture and an hour and a half to annotate. These tables run
//! one token through both parsers, so a change to either one fails here.

#![allow(clippy::unwrap_used)]

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::capture::{self, Unresolved};
use sunrise_domain::{parse_annotate, EditError};

fn now() -> Timestamp {
    "2026-08-21T12:00:00Z".parse().unwrap()
}

/// Capture's reading of `~body` on a line with a title.
fn captured(body: &str) -> capture::Capture {
    capture::parse(&format!("t ~{body}"), now(), &TimeZone::UTC, &[], &[])
}

/// Annotate's reading of `~body`.
fn annotated(body: &str) -> sunrise_domain::TaskEdit {
    parse_annotate(&format!("~{body}"), &[], &[], now(), &TimeZone::UTC)
}

#[test]
fn both_parsers_read_a_duration_as_the_same_seconds() {
    for (body, secs) in [
        ("30m", 30 * 60),
        ("2h", 2 * 3600),
        ("1d", 86_400),
        ("1h30m", 5400),
        ("1H30M", 5400),
        ("90", 90 * 60),
    ] {
        let c = captured(body);
        assert_eq!(c.draft.estimated_duration_s, Some(secs), "capture: {body}");
        assert!(c.unresolved.is_empty(), "capture: {body}");

        let e = annotated(body);
        assert_eq!(
            e.patch_for(&[]).estimated_duration_s,
            Some(Some(secs)),
            "annotate: {body}"
        );
        assert!(e.errors.is_empty(), "annotate: {body}");
    }
}

/// A trailing number with no unit is refused, not read as minutes: declining is
/// safer than inventing a unit the user did not type. The rest of the table is
/// what that refusal must not disturb: a token with no value in it, an unknown
/// unit, and a count too large to hold.
#[test]
fn both_parsers_refuse_what_is_not_a_duration() {
    for body in [
        "1h30",
        "1d2h3",
        "30m15",
        "0h",
        "h",
        "1w",
        "banana",
        "5124095576030431h5124095576030431h",
        "99999999999999999999999",
    ] {
        let c = captured(body);
        assert_eq!(c.draft.estimated_duration_s, None, "capture: {body}");
        assert_eq!(
            c.unresolved,
            vec![Unresolved::UnparseableDuration(body.into())],
            "capture: {body}"
        );
        assert!(
            c.draft.title.contains(&format!("~{body}")),
            "capture keeps the token in the title: {body}"
        );

        let e = annotated(body);
        assert!(e.is_empty(), "annotate: {body}");
        assert_eq!(
            e.errors,
            vec![EditError::BadDuration(body.into())],
            "annotate: {body}"
        );
    }
}
