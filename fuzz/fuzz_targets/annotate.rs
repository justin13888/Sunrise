//! The annotate line parser, and the preview and patch built from its result,
//! which run on every keystroke of the annotate prompt.
//!
//! The whole input is one typed line, so a minimized crash is something a user
//! could have typed. Seeds are the token forms `annotate`'s own documentation
//! and tests use.
//!
//! Beyond not panicking, a line with a word in it either asks for something or
//! reports why it could not: annotate has no title to keep unread text in, so
//! a token neither applied nor reported would be lost.

#![no_main]

use jiff::{tz::TimeZone, Timestamp};
use libfuzzer_sys::fuzz_target;
use sunrise_domain::capture::NamedRef;
use sunrise_domain::parse_annotate;
use sunrise_id::{EntityKind, EntityRef};

/// 2024-03-10T02:25:00Z, the day New York's clocks spring forward.
const NOW_S: i64 = 1_710_037_500;

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
    let blank = line.split_whitespace().next().is_none();
    for zone in ZONES {
        let Ok(tz) = TimeZone::get(zone) else {
            continue;
        };
        let edit = parse_annotate(line, &streams, &contexts, now, &tz);
        assert_eq!(
            edit.is_empty() && edit.errors.is_empty(),
            blank,
            "a line was neither applied nor reported, or a blank one did something, in {zone}"
        );
        let _ = edit.preview(&streams, &contexts, &tz);
        let _ = edit.patch_for(&[contexts[0].id]);
        let _ = edit.error_note();
    }
});
