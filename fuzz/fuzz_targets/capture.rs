//! The capture line parser, which runs on every keystroke of the capture bar.
//!
//! The whole input is one typed line, so a minimized crash is something a user
//! could have typed. Seeds are the line forms `capture`'s own documentation and
//! tests use.
//!
//! Beyond not panicking, the title holds only the line's own words in the
//! line's order. Every line is read in a zone with a real DST discontinuity as
//! well as in UTC, on the day of that zone's transition.

#![no_main]

use jiff::{tz::TimeZone, Timestamp};
use libfuzzer_sys::fuzz_target;
use sunrise_domain::capture::{parse, NamedRef};
use sunrise_id::{EntityKind, EntityRef};

/// 2024-03-10T06:25:00Z, 01:25 in New York, 35 minutes before its clocks spring
/// forward, so "today" is the transition's day in both zones.
const NOW_S: i64 = 1_710_051_900;

const ZONES: [&str; 2] = ["America/New_York", "UTC"];

fuzz_target!(|data: &[u8]| {
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(now) = Timestamp::from_second(NOW_S) else {
        return;
    };
    let streams = [NamedRef {
        id: EntityRef::new(EntityKind::Stream, [1; 16]),
        name: "travel",
    }];
    let contexts = [NamedRef {
        id: EntityRef::new(EntityKind::Context, [2; 16]),
        name: "errands",
    }];
    for zone in ZONES {
        let Ok(tz) = TimeZone::get(zone) else {
            continue;
        };
        let capture = parse(line, now, &tz, &streams, &contexts);
        let mut words = line.split_whitespace();
        assert!(
            capture
                .draft
                .title
                .split_whitespace()
                .all(|t| words.any(|w| w == t)),
            "the title holds a word the line does not, in {zone}: {:?}",
            capture.draft.title
        );
    }
});
