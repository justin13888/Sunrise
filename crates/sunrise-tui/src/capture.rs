//! Capture glue for the TUI: run a typed line through the **domain** parser
//! and describe the result to the user.
//!
//! `docs/08-features/inbox-and-capture.md` requires that every surface parse
//! capture identically and that nothing typed is silently discarded. Both
//! follow from calling [`sunrise_domain::capture::parse`] — the one
//! implementation — and from surfacing [`Unresolved`] rather than dropping it.
//!
//! # Why not `Core::capture`
//!
//! [`sunrise_core::Core::capture`] does the same job for the non-interactive
//! `sunrise-tui capture` subcommand, and would be the obvious call here too.
//! It cannot be: it is `async` (it runs `Query::StreamList` itself), and the
//! TUI's reducer — [`crate::runtime::apply_action`] — is a synchronous pure
//! function so that every keybinding is testable without a terminal or a
//! `Core`. A live preview that re-parses on each keystroke also cannot afford
//! a DB round trip per character.
//!
//! So the reducer supplies the rows itself, from the `Query::StreamList` and
//! `Query::Contexts` the event loop already refreshes into
//! [`crate::ViewState`], and calls the same domain parser with the same
//! `NamedRef` mapping `Core::capture` uses (archived entries excluded). The
//! *parser* is shared; only the two-line glue is restated, and it is restated
//! in one place — here.
//!
//! One deliberate difference: `Core::capture` still passes an empty context
//! list (its doc-comment says contexts are unresolvable "until Context CRUD
//! lands"), which now leaves it reporting every real `@name` as unknown.
//! `Query::Contexts` exists, so this path resolves them.

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_core::queries::{ContextRow, StreamRow};
use sunrise_domain::capture::{parse, Capture, NamedRef, Unresolved};

/// Convert the runtime's `now_ms` into the `Timestamp` the domain parser wants.
/// Saturates rather than failing: an out-of-range clock is not a reason to stop
/// accepting capture.
#[must_use]
pub fn now_ts(now_ms: u64) -> Timestamp {
    i64::try_from(now_ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .unwrap_or(Timestamp::UNIX_EPOCH)
}

/// Parse one capture line against the vault's live streams and contexts.
///
/// Archived streams and contexts are not candidates: they stay on the tasks
/// that already carry them but must not be resolution targets for new input.
#[must_use]
pub fn parse_line(
    input: &str,
    streams: &[StreamRow],
    contexts: &[ContextRow],
    now_ms: u64,
    tz: &TimeZone,
) -> Capture {
    let stream_refs: Vec<NamedRef<'_>> = streams
        .iter()
        .filter(|s| !s.archived)
        .map(|s| NamedRef {
            id: s.id,
            name: s.name.as_str(),
        })
        .collect();
    let context_refs: Vec<NamedRef<'_>> = contexts
        .iter()
        .filter(|c| !c.archived)
        .map(|c| NamedRef {
            id: c.id,
            name: c.name.as_str(),
        })
        .collect();
    parse(input, now_ts(now_ms), tz, &stream_refs, &context_refs)
}

/// One-line structured reading of a parsed capture, rendered under the input
/// line as the user types (`docs/08-features/inbox-and-capture.md`: "an inline
/// preview shows the structured interpretation").
///
/// Only the facets the parser actually resolved appear, so an unannotated line
/// stays quiet instead of showing a row of dashes.
#[must_use]
pub fn preview_line(
    cap: &Capture,
    streams: &[StreamRow],
    contexts: &[ContextRow],
    tz: &TimeZone,
) -> String {
    let d = &cap.draft;
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!("title \"{}\"", d.title));
    if let Some(id) = d.stream_id {
        let name = streams
            .iter()
            .find(|s| s.id == id)
            .map_or_else(|| id.to_str(), |s| s.name.clone());
        parts.push(format!("#{name}"));
    }
    for id in &d.contexts {
        let name = contexts
            .iter()
            .find(|c| c.id == *id)
            .map_or_else(|| id.to_str(), |c| c.name.clone());
        parts.push(format!("@{name}"));
    }
    if let Some(p) = d.priority {
        parts.push(format!("!{p}"));
    }
    if let Some(ts) = d.scheduled_at {
        parts.push(format!("^{}", local_stamp(ts, tz)));
    }
    if let Some(ts) = d.due_at {
        parts.push(format!("due {}", local_stamp(ts, tz)));
    }
    if let Some(secs) = d.estimated_duration_s {
        parts.push(format!("~{}", duration_label(secs)));
    }
    parts.join(" · ")
}

/// `2026-01-02 09:00` in the user's zone — a capture preview is about "did it
/// understand me", which a UTC instant answers badly.
fn local_stamp(ts: Timestamp, tz: &TimeZone) -> String {
    let dt = ts.to_zoned(tz.clone()).datetime();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

/// `5400` → `1h30m`.
fn duration_label(secs: u64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    }
}

/// Status-line note for tokens the parser could not apply, or `None` when
/// everything resolved.
///
/// The parser's governing invariant is that unresolved text stays in the title
/// rather than being dropped; this is the other half of that promise — the user
/// is told which token was not understood, instead of discovering it later in a
/// task with a stray `#travl` in its name.
#[must_use]
pub fn unresolved_note(unresolved: &[Unresolved]) -> Option<String> {
    if unresolved.is_empty() {
        return None;
    }
    let parts: Vec<String> = unresolved.iter().map(describe).collect();
    Some(format!("note: {}", parts.join("; ")))
}

/// Human phrasing for one unresolved token.
fn describe(u: &Unresolved) -> String {
    match u {
        Unresolved::UnknownStream(name) => format!("unknown stream \"{name}\""),
        Unresolved::AmbiguousStream { typed, candidates } => {
            format!("ambiguous stream \"{typed}\" — {}", candidates.join(", "))
        }
        Unresolved::UnknownContext(name) => format!("unknown context \"{name}\""),
        Unresolved::AmbiguousContext { typed, candidates } => {
            format!("ambiguous context \"{typed}\" — {}", candidates.join(", "))
        }
        Unresolved::UnparseableDate(text) => format!("could not read the date \"{text}\""),
        Unresolved::PriorityOutOfRange(text) => format!("priority \"{text}\" is not 1-5"),
        Unresolved::UnparseableDuration(text) => format!("could not read the duration \"{text}\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::fixtures::{context_row, inbox_row, stream_row};

    /// 2026-01-01T00:00:00Z.
    const NOW_MS: u64 = 1_767_225_600_000;

    fn streams() -> Vec<StreamRow> {
        vec![inbox_row(0), stream_row(7, "Travel", 0)]
    }

    fn contexts() -> Vec<ContextRow> {
        vec![context_row(11, "errands"), context_row(12, "deep-work")]
    }

    #[test]
    fn annotations_leave_the_title() {
        let c = parse_line(
            "Buy milk #travel ^tomorrow !2 ~1h",
            &streams(),
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        assert_eq!(c.draft.title, "Buy milk");
        assert_eq!(c.draft.priority, Some(2));
        assert_eq!(c.draft.stream_id, Some(stream_row(7, "Travel", 0).id));
        assert_eq!(c.draft.estimated_duration_s, Some(3600));
        assert!(c.draft.scheduled_at.is_some());
        assert!(c.unresolved.is_empty(), "got {:?}", c.unresolved);
    }

    #[test]
    fn an_archived_stream_is_not_a_candidate() {
        let mut rows = streams();
        rows[1].archived = true;
        let c = parse_line(
            "Pack bags #travel",
            &rows,
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        assert_eq!(c.draft.stream_id, None);
        // Nothing is lost: the token is still in the title *and* reported.
        assert_eq!(c.draft.title, "Pack bags #travel");
        assert_eq!(
            unresolved_note(&c.unresolved).as_deref(),
            Some("note: unknown stream \"travel\"")
        );
    }

    #[test]
    fn unknown_stream_becomes_a_status_note() {
        let c = parse_line(
            "Buy milk #travl",
            &streams(),
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        assert_eq!(
            unresolved_note(&c.unresolved).as_deref(),
            Some("note: unknown stream \"travl\"")
        );
        assert_eq!(c.draft.title, "Buy milk #travl", "the text is not dropped");
    }

    #[test]
    fn several_unresolved_tokens_are_all_reported() {
        let c = parse_line(
            "Ship it #nope !9 ~soon",
            &streams(),
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        let note = unresolved_note(&c.unresolved).expect("a note");
        assert!(note.contains("unknown stream \"nope\""), "got {note}");
        assert!(note.contains("priority \"9\" is not 1-5"), "got {note}");
        assert!(note.contains("duration \"soon\""), "got {note}");
    }

    #[test]
    fn a_clean_line_has_no_note() {
        let c = parse_line(
            "Just a task",
            &streams(),
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        assert_eq!(unresolved_note(&c.unresolved), None);
    }

    #[test]
    fn preview_shows_the_structured_reading() {
        let rows = streams();
        let c = parse_line(
            "Buy milk #travel @errands ^tomorrow 9am !2 ~90m",
            &rows,
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        let p = preview_line(&c, &rows, &contexts(), &TimeZone::UTC);
        assert!(p.contains("title \"Buy milk\""), "got {p}");
        assert!(p.contains("#Travel"), "got {p}");
        assert!(p.contains("@errands"), "got {p}");
        assert!(p.contains("!2"), "got {p}");
        assert!(p.contains("^2026-01-02 09:00"), "got {p}");
        assert!(p.contains("~1h30m"), "got {p}");
    }

    #[test]
    fn preview_of_a_bare_title_is_just_the_title() {
        let rows = streams();
        let c = parse_line("Buy milk", &rows, &contexts(), NOW_MS, &TimeZone::UTC);
        assert_eq!(
            preview_line(&c, &rows, &contexts(), &TimeZone::UTC),
            "title \"Buy milk\""
        );
    }

    #[test]
    fn a_known_context_resolves_rather_than_warning() {
        let c = parse_line(
            "Pick up milk @errands",
            &streams(),
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        assert_eq!(c.draft.title, "Pick up milk");
        assert_eq!(c.draft.contexts, vec![context_row(11, "errands").id]);
        assert_eq!(unresolved_note(&c.unresolved), None);
    }

    #[test]
    fn an_archived_context_is_not_a_candidate() {
        let mut rows = contexts();
        rows[0].archived = true;
        let c = parse_line(
            "Pick up milk @errands",
            &streams(),
            &rows,
            NOW_MS,
            &TimeZone::UTC,
        );
        assert!(c.draft.contexts.is_empty());
        assert_eq!(
            unresolved_note(&c.unresolved).as_deref(),
            Some("note: unknown context \"errands\"")
        );
    }

    #[test]
    fn an_unknown_context_is_reported() {
        let c = parse_line(
            "Pick up milk @erands",
            &streams(),
            &contexts(),
            NOW_MS,
            &TimeZone::UTC,
        );
        assert_eq!(
            unresolved_note(&c.unresolved).as_deref(),
            Some("note: unknown context \"erands\"")
        );
        assert_eq!(c.draft.title, "Pick up milk @erands");
    }

    #[test]
    fn duration_labels() {
        assert_eq!(duration_label(1800), "30m");
        assert_eq!(duration_label(3600), "1h");
        assert_eq!(duration_label(5400), "1h30m");
    }
}
