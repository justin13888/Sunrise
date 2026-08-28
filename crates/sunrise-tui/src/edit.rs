//! Annotate-prompt glue: run a typed line through the **domain** parser.
//!
//! The grammar itself lives in [`sunrise_domain::annotate`], beside the
//! capture parser, because every client must read `#stream @ctx !N %energy
//! ~30m ^when due:when` identically. All that is left here is the same two
//! lines of row→[`NamedRef`] mapping [`crate::capture`] does, with archived
//! rows filtered out.

use jiff::tz::TimeZone;
use sunrise_core::queries::{ContextRow, StreamRow};
use sunrise_domain::annotate;
use sunrise_domain::capture::{now_ts, NamedRef};

pub use sunrise_domain::annotate::{EditError, TaskEdit};

/// Live (non-archived) stream rows as parser candidates.
#[must_use]
pub fn stream_refs(streams: &[StreamRow]) -> Vec<NamedRef<'_>> {
    streams
        .iter()
        .filter(|s| !s.archived)
        .map(|s| NamedRef {
            id: s.id,
            name: s.name.as_str(),
        })
        .collect()
}

/// Live (non-archived) context rows as parser candidates.
#[must_use]
pub fn context_refs(contexts: &[ContextRow]) -> Vec<NamedRef<'_>> {
    contexts
        .iter()
        .filter(|c| !c.archived)
        .map(|c| NamedRef {
            id: c.id,
            name: c.name.as_str(),
        })
        .collect()
}

/// Parse an annotate line against the vault's live streams and contexts.
#[must_use]
pub fn parse_edit(
    input: &str,
    streams: &[StreamRow],
    contexts: &[ContextRow],
    now_ms: u64,
    tz: &TimeZone,
) -> TaskEdit {
    annotate::parse(
        input,
        &stream_refs(streams),
        &context_refs(contexts),
        now_ts(now_ms),
        tz,
    )
}

/// One-line description of what an edit will do, for the live preview.
#[must_use]
pub fn preview(
    edit: &TaskEdit,
    streams: &[StreamRow],
    contexts: &[ContextRow],
    tz: &TimeZone,
) -> String {
    edit.preview(&stream_refs(streams), &context_refs(contexts), tz)
}
