//! The **annotate** grammar: edit any facet of an existing Task by typing.
//!
//! Capture can set a Task's stream, contexts, priority, schedule, due date and
//! estimate in one line. Until now none of it could be *changed* afterwards:
//! the TUI could rename a task, complete it, defer it, schedule it and move
//! it, and that was the whole of the write surface. Every other field
//! `TaskPatch` carries — priority, energy, estimate, due date, contexts — was
//! set-once-at-capture, and a mistake meant deleting the task and retyping it.
//!
//! So `A` opens a prompt that speaks the capture sigils the user already
//! knows, plus the two things editing needs and capture does not:
//!
//! | Token | Effect |
//! |---|---|
//! | `#stream` | move to that Stream |
//! | `@ctx` | add a Context |
//! | `@-ctx` | remove a Context |
//! | `!N` | set priority (1-5) |
//! | `%low\|med\|high` | set energy |
//! | `~30m` | set the estimate |
//! | `^when` | set `scheduled_at` |
//! | `due:when` | set `due_at` |
//! | `!-` `%-` `~-` `^-` `due:-` `@-` | clear that facet |
//!
//! Two deliberate differences from [`crate::capture`]:
//!
//! * **Bare words are refused, not absorbed.** Capture's governing rule is
//!   that unrecognised text survives in the title, because a capture line *is*
//!   a title. An annotate line is not: silently appending "tomorow" to a task's
//!   name because the date failed to parse would be the opposite of helpful.
//!   Use `e` to edit a title.
//! * **Clearing is expressible.** `TaskPatch` distinguishes "leave alone"
//!   (`None`) from "set to nothing" (`Some(None)`), and without a `-` form the
//!   second is unreachable — a priority set by accident could never be removed.
//!
//! The grammar is parsed here, pure, so the whole of it is unit-tested without
//! a terminal; the reducer only turns the result into commands.

use crate::capture::now_ts;
use jiff::tz::TimeZone;
use sunrise_core::queries::{ContextRow, StreamRow};
use sunrise_domain::capture::{parse_energy, parse_when};
use sunrise_domain::{Energy, TaskPatch};
use sunrise_id::EntityRef;

/// A field the annotate line touches, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Set<T> {
    /// Set the field to this value.
    To(T),
    /// Clear the field.
    Clear,
}

/// One token the parser could not apply, kept so nothing is silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// `#name` matched no live Stream (or matched several).
    UnknownStream(String),
    /// `@name` matched no live Context (or matched several).
    UnknownContext(String),
    /// `!N` was not 1-5.
    BadPriority(String),
    /// `%x` was not a known energy level.
    BadEnergy(String),
    /// `~x` was not a duration.
    BadDuration(String),
    /// `^x` / `due:x` was not a date.
    BadDate(String),
    /// A bare word: an annotate line is not a title.
    NotAToken(String),
}

impl EditError {
    /// Human phrasing for the status line.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::UnknownStream(s) => format!("no such stream \"{s}\""),
            Self::UnknownContext(s) => format!("no such context \"{s}\""),
            Self::BadPriority(s) => format!("priority \"{s}\" is not 1-5"),
            Self::BadEnergy(s) => format!("energy \"{s}\" is not low/med/high"),
            Self::BadDuration(s) => format!("could not read the duration \"{s}\""),
            Self::BadDate(s) => format!("could not read the date \"{s}\""),
            Self::NotAToken(s) => format!("\"{s}\" is not an edit token (e edits the title)"),
        }
    }
}

/// A parsed annotate line: the facets to change, resolved against the vault's
/// live streams and contexts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskEdit {
    stream: Option<EntityRef>,
    add_contexts: Vec<EntityRef>,
    remove_contexts: Vec<EntityRef>,
    clear_contexts: bool,
    priority: Option<Set<u8>>,
    energy: Option<Set<Energy>>,
    duration_s: Option<Set<u64>>,
    scheduled: Option<Set<jiff::Timestamp>>,
    due: Option<Set<jiff::Timestamp>>,
    /// Tokens that could not be applied.
    pub errors: Vec<EditError>,
}

impl TaskEdit {
    /// Whether the line asked for anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stream.is_none()
            && self.add_contexts.is_empty()
            && self.remove_contexts.is_empty()
            && !self.clear_contexts
            && self.priority.is_none()
            && self.energy.is_none()
            && self.duration_s.is_none()
            && self.scheduled.is_none()
            && self.due.is_none()
    }

    /// The Stream this edit moves tasks to, if any.
    ///
    /// Separate from the patch because moving between Streams is
    /// `Command::PromoteToStream`, not a `TaskPatch` field the core would
    /// honour: the move has to re-key the task's storage.
    #[must_use]
    pub const fn stream(&self) -> Option<EntityRef> {
        self.stream
    }

    /// The patch this edit means for a task whose contexts are `current`.
    ///
    /// Per-task rather than shared because contexts are a *replace* field:
    /// "add `@home`" is only expressible as the union of `@home` with whatever
    /// that particular task already carries, so a bulk annotate over five
    /// tasks is five different patches. Takes the context list rather than the
    /// `Task` so a row the view no longer holds can still be patched.
    #[must_use]
    pub fn patch_for(&self, current: &[EntityRef]) -> TaskPatch {
        let mut patch = TaskPatch::default();
        if let Some(set) = self.priority {
            patch.priority = Some(match set {
                Set::To(p) => Some(p),
                Set::Clear => None,
            });
        }
        if let Some(set) = self.energy {
            patch.energy = Some(match set {
                Set::To(e) => Some(e),
                Set::Clear => None,
            });
        }
        if let Some(set) = self.duration_s {
            patch.estimated_duration_s = Some(match set {
                Set::To(s) => Some(s),
                Set::Clear => None,
            });
        }
        if let Some(set) = self.scheduled {
            patch.scheduled_at = Some(match set {
                Set::To(t) => Some(t.into()),
                Set::Clear => None,
            });
        }
        if let Some(set) = self.due {
            patch.due_at = Some(match set {
                Set::To(t) => Some(t.into()),
                Set::Clear => None,
            });
        }
        if self.clear_contexts {
            patch.contexts = Some(Vec::new());
        } else if !self.add_contexts.is_empty() || !self.remove_contexts.is_empty() {
            let mut next: Vec<EntityRef> = current
                .iter()
                .copied()
                .filter(|c| !self.remove_contexts.contains(c))
                .collect();
            for c in &self.add_contexts {
                if !next.contains(c) {
                    next.push(*c);
                }
            }
            patch.contexts = Some(next);
        }
        patch
    }

    /// One-line description of what the edit will do, for the live preview.
    #[must_use]
    pub fn preview(&self, streams: &[StreamRow], contexts: &[ContextRow], tz: &TimeZone) -> String {
        let name = |id: EntityRef, rows: &[ContextRow]| {
            rows.iter()
                .find(|c| c.id == id)
                .map_or_else(|| id.to_str(), |c| c.name.clone())
        };
        let mut parts: Vec<String> = Vec::new();
        if let Some(id) = self.stream {
            let n = streams
                .iter()
                .find(|s| s.id == id)
                .map_or_else(|| id.to_str(), |s| s.name.clone());
            parts.push(format!("→ #{n}"));
        }
        for c in &self.add_contexts {
            parts.push(format!("+@{}", name(*c, contexts)));
        }
        for c in &self.remove_contexts {
            parts.push(format!("-@{}", name(*c, contexts)));
        }
        if self.clear_contexts {
            parts.push("clear contexts".into());
        }
        push_set(&mut parts, self.priority, "priority", |p| format!("!{p}"));
        push_set(&mut parts, self.energy, "energy", |e| {
            energy_word(e).to_string()
        });
        push_set(&mut parts, self.duration_s, "estimate", |s| {
            format!("~{}", duration_label(s))
        });
        push_set(&mut parts, self.scheduled, "schedule", |t| {
            format!("^{}", stamp(t, tz))
        });
        push_set(&mut parts, self.due, "due", |t| {
            format!("due {}", stamp(t, tz))
        });
        if parts.is_empty() {
            return "nothing to change".into();
        }
        parts.join(" · ")
    }

    /// Status-line note for tokens that could not be applied.
    #[must_use]
    pub fn error_note(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }
        Some(format!(
            "note: {}",
            self.errors
                .iter()
                .map(EditError::describe)
                .collect::<Vec<_>>()
                .join("; ")
        ))
    }
}

/// Render one `Set` into the preview list.
fn push_set<T: Copy>(
    parts: &mut Vec<String>,
    set: Option<Set<T>>,
    label: &str,
    show: impl Fn(T) -> String,
) {
    match set {
        Some(Set::To(v)) => parts.push(show(v)),
        Some(Set::Clear) => parts.push(format!("clear {label}")),
        None => {}
    }
}

/// Parse an annotate line against the vault's live streams and contexts.
///
/// Archived rows are excluded from resolution for the same reason capture
/// excludes them: they stay on the tasks that carry them, but must not be
/// targets for new input.
#[must_use]
pub fn parse_edit(
    input: &str,
    streams: &[StreamRow],
    contexts: &[ContextRow],
    now_ms: u64,
    tz: &TimeZone,
) -> TaskEdit {
    let now = now_ts(now_ms);
    let mut edit = TaskEdit::default();
    // `^when` and `due:when` take a date that can span words ("next friday"),
    // so the tail of the line is offered to the date parser longest-first.
    let words: Vec<&str> = input.split_whitespace().collect();
    let mut i = 0usize;
    while i < words.len() {
        let w = words[i];
        i += 1;
        if let Some(body) = w.strip_prefix('#') {
            match resolve(body, streams.iter().filter(|s| !s.archived).map(row_name)) {
                Some(id) => edit.stream = Some(id),
                None => edit.errors.push(EditError::UnknownStream(body.into())),
            }
        } else if let Some(body) = w.strip_prefix('@') {
            parse_context_token(&mut edit, body, contexts);
        } else if let Some(body) = w.strip_prefix('!') {
            match set_or_clear(body, |b| {
                b.parse::<u8>().ok().filter(|p| (1..=5).contains(p))
            }) {
                Some(v) => edit.priority = Some(v),
                None => edit.errors.push(EditError::BadPriority(body.into())),
            }
        } else if let Some(body) = w.strip_prefix('%') {
            match set_or_clear(body, parse_energy) {
                Some(v) => edit.energy = Some(v),
                None => edit.errors.push(EditError::BadEnergy(body.into())),
            }
        } else if let Some(body) = w.strip_prefix('~') {
            match set_or_clear(body, parse_duration_s) {
                Some(v) => edit.duration_s = Some(v),
                None => edit.errors.push(EditError::BadDuration(body.into())),
            }
        } else if let Some(body) = strip_due(w) {
            let (consumed, parsed) = take_date(&words, i, body, now, tz);
            i += consumed;
            match parsed {
                Some(v) => edit.due = Some(v),
                None => edit.errors.push(EditError::BadDate(body.into())),
            }
        } else if let Some(body) = w.strip_prefix('^') {
            let (consumed, parsed) = take_date(&words, i, body, now, tz);
            i += consumed;
            match parsed {
                Some(v) => edit.scheduled = Some(v),
                None => edit.errors.push(EditError::BadDate(body.into())),
            }
        } else {
            edit.errors.push(EditError::NotAToken(w.into()));
        }
    }
    edit
}

/// `@ctx`, `@-ctx`, or a bare `@-` meaning "clear them all".
fn parse_context_token(edit: &mut TaskEdit, body: &str, contexts: &[ContextRow]) {
    let live = || contexts.iter().filter(|c| !c.archived).map(row_name_ctx);
    match body.strip_prefix('-') {
        Some("") => edit.clear_contexts = true,
        Some(name) => match resolve(name, live()) {
            Some(id) => edit.remove_contexts.push(id),
            None => edit.errors.push(EditError::UnknownContext(name.into())),
        },
        None => match resolve(body, live()) {
            Some(id) => edit.add_contexts.push(id),
            None => edit.errors.push(EditError::UnknownContext(body.into())),
        },
    }
}

/// `-` clears; anything else goes through `f`.
fn set_or_clear<T>(body: &str, f: impl Fn(&str) -> Option<T>) -> Option<Set<T>> {
    if body == "-" {
        return Some(Set::Clear);
    }
    f(body).map(Set::To)
}

/// Greedily read a date starting at `head`, extending into the following words
/// (up to three) so "next friday" beats "next". Returns how many extra words
/// were consumed alongside the result.
fn take_date(
    words: &[&str],
    next: usize,
    head: &str,
    now: jiff::Timestamp,
    tz: &TimeZone,
) -> (usize, Option<Set<jiff::Timestamp>>) {
    /// Words a date expression may span, matching the capture parser.
    const MAX_DATE_WORDS: usize = 3;
    if head == "-" {
        return (0, Some(Set::Clear));
    }
    let available = words.len().saturating_sub(next).min(MAX_DATE_WORDS - 1);
    for extra in (0..=available).rev() {
        let mut text = head.to_string();
        for w in &words[next..next + extra] {
            text.push(' ');
            text.push_str(w);
        }
        if let Some(ts) = parse_when(&text, now, tz) {
            return (extra, Some(Set::To(ts)));
        }
    }
    (0, None)
}

/// `due:tomorrow` and the capture-style `*due:tomorrow*` both work: the second
/// is what the capture grammar documents, and typing the first is what people
/// actually do.
fn strip_due(w: &str) -> Option<&str> {
    let body = w
        .strip_prefix("*due:")
        .and_then(|b| b.strip_suffix('*'))
        .or_else(|| w.strip_prefix("due:"))?;
    (!body.is_empty()).then_some(body)
}

/// Name/id pair for a stream row.
fn row_name(s: &StreamRow) -> (EntityRef, &str) {
    (s.id, s.name.as_str())
}

/// Name/id pair for a context row.
fn row_name_ctx(c: &ContextRow) -> (EntityRef, &str) {
    (c.id, c.name.as_str())
}

/// Case-insensitive exact-then-unique-prefix resolution, matching the capture
/// parser's rule so `#trav` means the same thing in both prompts.
fn resolve<'a>(
    typed: &str,
    candidates: impl Iterator<Item = (EntityRef, &'a str)>,
) -> Option<EntityRef> {
    let lower = typed.to_lowercase();
    let rows: Vec<(EntityRef, String)> = candidates.map(|(id, n)| (id, n.to_lowercase())).collect();
    if let Some((id, _)) = rows.iter().find(|(_, n)| *n == lower) {
        return Some(*id);
    }
    let mut hits = rows.iter().filter(|(_, n)| n.starts_with(&lower));
    match (hits.next(), hits.next()) {
        (Some((id, _)), None) => Some(*id),
        _ => None,
    }
}

/// `30m` / `2h` / `1h30m` / `90` (bare minutes) → seconds.
fn parse_duration_s(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Ok(mins) = t.parse::<u64>() {
        return mins.checked_mul(60);
    }
    let mut total = 0u64;
    let mut digits = String::new();
    for c in t.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let n: u64 = digits.parse().ok()?;
        digits.clear();
        let unit = match c {
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(unit)?)?;
    }
    // A trailing bare number ("1h30") is minutes.
    if !digits.is_empty() {
        total = total.checked_add(digits.parse::<u64>().ok()?.checked_mul(60)?)?;
    }
    (total > 0).then_some(total)
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

/// `2026-01-02 09:00` in the user's zone.
fn stamp(ts: jiff::Timestamp, tz: &TimeZone) -> String {
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

/// Word for an energy level.
const fn energy_word(e: Energy) -> &'static str {
    match e {
        Energy::Low => "%low",
        Energy::Med => "%med",
        Energy::High => "%high",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::fixtures::{context_row, stream_row};
    use sunrise_id::{EntityKind, EntityRef};

    /// A fixed instant: 2026-01-01 00:00:00 UTC.
    const NOW_MS: u64 = 1_767_225_600_000;

    fn sid(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [n; 16])
    }
    fn cid(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Context, [n; 16])
    }

    fn streams() -> Vec<StreamRow> {
        vec![stream_row(1, "Inbox", 0), stream_row(2, "Travel", 0)]
    }

    fn contexts() -> Vec<ContextRow> {
        vec![context_row(1, "home"), context_row(2, "deep-work")]
    }

    fn parse(input: &str) -> TaskEdit {
        parse_edit(input, &streams(), &contexts(), NOW_MS, &TimeZone::UTC)
    }

    #[test]
    fn every_facet_the_patch_carries_is_reachable() {
        let e = parse("!2 %high ~90m ^tomorrow due:tomorrow @home #travel");
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        let p = e.patch_for(&[]);
        assert_eq!(p.priority, Some(Some(2)));
        assert_eq!(p.energy, Some(Some(Energy::High)));
        assert_eq!(p.estimated_duration_s, Some(Some(90 * 60)));
        assert!(p.scheduled_at.is_some());
        assert!(p.due_at.is_some());
        assert_eq!(p.contexts, Some(vec![cid(1)]));
        assert_eq!(e.stream(), Some(sid(2)));
    }

    #[test]
    fn a_dash_clears_rather_than_leaving_alone() {
        // `TaskPatch` distinguishes "leave alone" from "set to nothing"; with
        // no `-` form the second is unreachable, so a priority set by accident
        // could never be removed.
        let e = parse("!- %- ~- ^- due:-");
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        let p = e.patch_for(&[]);
        assert_eq!(p.priority, Some(None));
        assert_eq!(p.energy, Some(None));
        assert_eq!(p.estimated_duration_s, Some(None));
        assert_eq!(p.scheduled_at, Some(None));
        assert_eq!(p.due_at, Some(None));
    }

    #[test]
    fn contexts_add_and_remove_against_what_the_task_already_carries() {
        let e = parse("@deep-work @-home");
        assert_eq!(e.patch_for(&[cid(1)]).contexts, Some(vec![cid(2)]));
        // Adding one already present is idempotent, not a duplicate.
        let e = parse("@home");
        assert_eq!(e.patch_for(&[cid(1)]).contexts, Some(vec![cid(1)]));
    }

    #[test]
    fn a_bare_at_dash_clears_every_context() {
        let e = parse("@-");
        assert_eq!(e.patch_for(&[cid(1)]).contexts, Some(vec![]));
    }

    #[test]
    fn a_bare_word_is_refused_rather_than_becoming_a_title() {
        // Capture absorbs unrecognised text into the title because a capture
        // line *is* a title. An annotate line is not.
        let e = parse("tomorow");
        assert!(e.is_empty());
        assert_eq!(e.errors, vec![EditError::NotAToken("tomorow".into())]);
        assert!(e.error_note().unwrap().contains("e edits the title"));
    }

    #[test]
    fn unresolvable_tokens_are_reported_one_by_one() {
        let e = parse("#nope @nope !9 %sideways ~banana");
        assert_eq!(e.errors.len(), 5, "{:?}", e.errors);
        assert!(e.is_empty());
    }

    #[test]
    fn a_multi_word_date_does_not_swallow_the_token_after_it() {
        let c = parse("^tomorrow !1");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.patch_for(&[]).priority, Some(Some(1)));
        assert!(c.patch_for(&[]).scheduled_at.is_some());
    }

    #[test]
    fn durations_read_the_forms_people_type() {
        for (text, secs) in [
            ("~30m", 30 * 60),
            ("~2h", 2 * 3600),
            ("~1h30m", 5400),
            ("~90", 5400),
        ] {
            let e = parse(text);
            assert_eq!(
                e.patch_for(&[]).estimated_duration_s,
                Some(Some(secs)),
                "{text}"
            );
        }
    }

    #[test]
    fn archived_rows_are_not_resolution_targets() {
        let mut ctxs = contexts();
        ctxs[0].archived = true;
        let e = parse_edit("@home", &streams(), &ctxs, NOW_MS, &TimeZone::UTC);
        assert_eq!(e.errors, vec![EditError::UnknownContext("home".into())]);
    }

    #[test]
    fn a_prefix_resolves_exactly_as_it_does_in_capture() {
        assert_eq!(parse("#trav").stream(), Some(sid(2)));
        assert_eq!(parse("@deep").patch_for(&[]).contexts, Some(vec![cid(2)]));
    }

    #[test]
    fn the_preview_says_what_will_change() {
        let line =
            parse("!1 %low @home #travel !-").preview(&streams(), &contexts(), &TimeZone::UTC);
        // The last `!-` wins: an annotate line is applied left to right.
        assert!(line.contains("clear priority"), "{line}");
        assert!(line.contains("+@home"), "{line}");
        assert!(line.contains("\u{2192} #Travel"), "{line}");
    }

    #[test]
    fn an_empty_edit_previews_as_nothing_to_change() {
        assert_eq!(
            TaskEdit::default().preview(&streams(), &contexts(), &TimeZone::UTC),
            "nothing to change"
        );
    }
}
