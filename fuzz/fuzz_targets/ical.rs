//! Inbound iCalendar parsing and the writer that mirrors it.
//!
//! Scope from `docs/10-cross-cutting/testing.md` §Continuous fuzz targets:
//! "inbound iCalendar feed parser (`sunrise-integrations`)".
//!
//! This is the only parser in the tree whose input arrives from a third party
//! over the network as a matter of course — an `.ics` subscription URL — so
//! arbitrary bytes are not a hypothetical here, they are the feature.
//!
//! Seeds are the four vendor fixtures under
//! `crates/sunrise-integrations/testdata/`: Apple, Google, Fastmail and
//! Outlook each fold, escape and time-zone their output differently, and
//! starting from all four gives the fuzzer four different shapes of line
//! folding to mutate rather than one.
//!
//! # The assertion
//!
//! Rendering is a **fixed point** over parsing: whatever `write` emits for the
//! events a parse produced, re-parsing and re-writing must emit byte for byte
//! the same thing. Stating it that way rather than as `parse(write(x)) == x`
//! is deliberate — `write` is lossy by design (it carries what a `Block` can
//! hold and drops the rest, which is what `ICalNotice` records), so equality of
//! the events is not a property this codec claims. Idempotence of the render
//! *is*, and it is what a round trip through the vault has to preserve.

#![no_main]

use libfuzzer_sys::fuzz_target;
use sunrise_integrations::ical;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(calendar) = ical::parse(text) else {
        return;
    };

    let rendered = ical::write(&calendar.events);
    let reparsed = ical::parse(&rendered).expect("what ical::write emits must parse as iCalendar");
    assert_eq!(
        rendered,
        ical::write(&reparsed.events),
        "ical::write is not a fixed point over ical::parse"
    );

    // A second pass over the writer's own output must not start inventing
    // notices: `write` only emits what a `Block` carried, so nothing in it can
    // be unreadable to the parser that produced it.
    assert!(
        reparsed.notices.is_empty(),
        "re-parsing ical::write output raised notices: {:?}",
        reparsed.notices
    );
});
