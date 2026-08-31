//! Capture parser per `docs/08-features/inbox-and-capture.md`.
//!
//! One pure function turns a single line of user input into a [`TaskDraft`]:
//!
//! ```text
//! Renew passport #travel @errands ^next saturday !1 ~1h
//! ```
//!
//! The spec calls capture "the most-used surface in the app", and requires the
//! same parse on every surface — TUI, desktop hotkey, share sheets, voice — so
//! that a captured Task is identical regardless of where it came from. That is
//! only achievable if exactly one implementation exists, which is why this
//! lives in `sunrise-domain` rather than in any client.
//!
//! # Purity
//!
//! [`parse`] takes `now` and `tz` as arguments and reads no clock, no
//! filesystem, and no global state, per the determinism rules in
//! `docs/01-architecture/shared-core.md`. Two devices parsing the same input
//! with the same arguments produce byte-identical drafts. This also makes the
//! whole surface unit-testable without a fixture.
//!
//! # What is deliberately not supported
//!
//! `^when` accepts a bounded, unambiguous subset of natural dates (see
//! [`parse_when`]). Open-ended natural-language date parsing is a well-known
//! source of silent misinterpretation — "next friday" alone is ambiguous in
//! common usage — and a capture surface that silently schedules a task to the
//! wrong week is worse than one that declines to guess. Anything unrecognised
//! is reported in [`Capture::unresolved`] and left in the title, so the input
//! is never silently discarded.
//!
//! The spec's `Standup ^weekday 9:30` example is *recurrence*, not a single
//! datetime; that belongs to a Routine's RRULE (see [`crate::rrule`]), not
//! here. It parses as unresolved rather than being guessed at.

use crate::common::Energy;
use crate::task::TaskDraft;
use jiff::civil::{Date, DateTime, Time, Weekday};
use jiff::tz::TimeZone;
use jiff::{Span, Timestamp};
use sunrise_id::EntityRef;

/// Maximum priority accepted from `!N`.
const MAX_PRIORITY: u8 = 5;

/// A named entity the parser can resolve `#stream` / `@context` against.
#[derive(Debug, Clone, Copy)]
pub struct NamedRef<'a> {
    /// The entity's id.
    pub id: EntityRef,
    /// Display name, matched case-insensitively.
    pub name: &'a str,
}

/// Why a token could not be applied. Surfaced so the capture UI can show an
/// inline preview and explain itself, rather than dropping input silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolved {
    /// `#name` matched no known stream.
    UnknownStream(String),
    /// `#name` matched more than one stream; candidates are listed so the UI
    /// can offer them.
    AmbiguousStream {
        /// The typed name.
        typed: String,
        /// Names that matched.
        candidates: Vec<String>,
    },
    /// `@name` matched no known context.
    UnknownContext(String),
    /// `@name` matched more than one context.
    AmbiguousContext {
        /// The typed name.
        typed: String,
        /// Names that matched.
        candidates: Vec<String>,
    },
    /// `^when` / `*due:when*` was not a date this parser understands.
    UnparseableDate(String),
    /// `!N` was out of the 1..=5 range.
    PriorityOutOfRange(String),
    /// `~Xm` / `~Xh` was not a duration.
    UnparseableDuration(String),
}

/// Result of parsing one capture line.
#[derive(Debug, Clone)]
pub struct Capture {
    /// The draft to submit.
    pub draft: TaskDraft,
    /// Tokens that looked like annotations but could not be applied. Their
    /// original text is retained in `draft.title` so nothing is lost.
    pub unresolved: Vec<Unresolved>,
}

/// Convert a client's `now_ms` into the [`Timestamp`] the parsers want.
///
/// Saturates to the epoch rather than failing: an out-of-range clock reading
/// is not a reason to stop accepting input. Lives here because
/// [`parse`] and [`crate::annotate::parse`] both take an instant and every
/// client holds milliseconds.
#[must_use]
pub fn now_ts(now_ms: u64) -> Timestamp {
    i64::try_from(now_ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .unwrap_or(Timestamp::UNIX_EPOCH)
}

/// Normalised title used for the capture dedup key, per the spec:
/// lowercase, trimmed, internal whitespace collapsed.
#[must_use]
pub fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse one capture line into a [`TaskDraft`].
///
/// `now` and `tz` anchor relative dates (`tomorrow`, `+3d`, weekday names).
/// `streams` and `contexts` are the candidate sets `#`/`@` resolve against;
/// pass empty slices to leave every such token unresolved.
///
/// Never fails: unparseable annotations are reported in
/// [`Capture::unresolved`] and their text stays in the title. An empty or
/// whitespace-only input yields an empty title, which
/// [`TaskDraft::validate`] will reject — validation stays the caller's job so
/// the parser can be used for live preview on partial input.
#[must_use]
pub fn parse(
    input: &str,
    now: Timestamp,
    tz: &TimeZone,
    streams: &[NamedRef<'_>],
    contexts: &[NamedRef<'_>],
) -> Capture {
    let mut draft = TaskDraft::default();
    let mut unresolved = Vec::new();
    let mut title_words: Vec<&str> = Vec::new();

    // `^when` and `*due:...*` can span several words ("next saturday", "9 am"),
    // so we walk tokens with an index rather than using a simple iterator.
    let words: Vec<&str> = input.split_whitespace().collect();
    let mut i = 0usize;
    while i < words.len() {
        let w = words[i];
        let (tag, rest) = split_tag(w);
        match tag {
            Some('#') if !rest.is_empty() => {
                resolve_named(rest, streams, &mut unresolved, NameKind::Stream)
                    .map_or_else(|| title_words.push(w), |id| draft.stream_id = Some(id));
                i += 1;
            }
            Some('@') if !rest.is_empty() => {
                resolve_named(rest, contexts, &mut unresolved, NameKind::Context)
                    .map_or_else(|| title_words.push(w), |id| draft.contexts.push(id));
                i += 1;
            }
            Some('!') if !rest.is_empty() => {
                match rest.parse::<u8>() {
                    Ok(p) if (1..=MAX_PRIORITY).contains(&p) => draft.priority = Some(p),
                    _ => {
                        unresolved.push(Unresolved::PriorityOutOfRange(rest.to_string()));
                        title_words.push(w);
                    }
                }
                i += 1;
            }
            Some('~') if !rest.is_empty() => {
                if let Some(secs) = parse_duration_s(rest) {
                    draft.estimated_duration_s = Some(secs);
                } else {
                    unresolved.push(Unresolved::UnparseableDuration(rest.to_string()));
                    title_words.push(w);
                }
                i += 1;
            }
            Some('^') => {
                // Greedily take up to MAX_DATE_WORDS words, longest match wins,
                // so "next saturday" beats "next".
                let (consumed, parsed) = take_date(&words, i, rest, now, tz);
                if let Some(ts) = parsed {
                    draft.scheduled_at = Some(ts.into());
                } else {
                    unresolved.push(Unresolved::UnparseableDate(
                        words[i..i + consumed].join(" "),
                    ));
                    title_words.extend_from_slice(&words[i..i + consumed]);
                }
                i += consumed;
            }
            _ => {
                if let Some(body) = strip_due(w) {
                    // `*due:tomorrow*` — single-token form.
                    if let Some(ts) = parse_when(body, now, tz) {
                        draft.due_at = Some(ts.into());
                    } else {
                        unresolved.push(Unresolved::UnparseableDate(body.to_string()));
                        title_words.push(w);
                    }
                } else {
                    title_words.push(w);
                }
                i += 1;
            }
        }
    }

    draft.title = title_words.join(" ");
    Capture { draft, unresolved }
}

/// Splits a leading annotation sigil off a word.
fn split_tag(w: &str) -> (Option<char>, &str) {
    let mut cs = w.chars();
    match cs.next() {
        Some(c @ ('#' | '@' | '!' | '~' | '^')) => (Some(c), &w[c.len_utf8()..]),
        _ => (None, w),
    }
}

/// `*due:tomorrow*` → `Some("tomorrow")`.
fn strip_due(w: &str) -> Option<&str> {
    let inner = w.strip_prefix("*due:")?.strip_suffix('*')?;
    (!inner.is_empty()).then_some(inner)
}

/// Which sigil a name was typed behind, so an unresolved one names itself
/// correctly ("no such stream" vs "no such context").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    /// A `#stream` token.
    Stream,
    /// An `@context` token.
    Context,
}

/// Case-insensitive resolution. An exact match wins outright; otherwise a
/// unique prefix match is accepted, and anything else is reported rather than
/// guessed.
///
/// Public because it is **the** rule for turning a typed name into an id, and
/// `docs/07-clients/overview.md` §"What clients share" puts that vocabulary in
/// this crate rather than in each client: a CLI that resolved `#work` by its
/// own rule would eventually disagree with the capture bar about which Stream
/// a user meant. Callers that accept a raw name — `sunrise stream <name>`, say
/// — get the same exact-then-unique-prefix behaviour, and the same
/// [`Unresolved`] values to report, that every capture line already gets.
pub fn resolve_named(
    typed: &str,
    candidates: &[NamedRef<'_>],
    unresolved: &mut Vec<Unresolved>,
    kind: NameKind,
) -> Option<EntityRef> {
    let lower = typed.to_lowercase();
    if let Some(exact) = candidates.iter().find(|c| c.name.to_lowercase() == lower) {
        return Some(exact.id);
    }
    let prefixed: Vec<&NamedRef<'_>> = candidates
        .iter()
        .filter(|c| c.name.to_lowercase().starts_with(&lower))
        .collect();
    match prefixed.len() {
        1 => Some(prefixed[0].id),
        0 => {
            unresolved.push(match kind {
                NameKind::Stream => Unresolved::UnknownStream(typed.to_string()),
                NameKind::Context => Unresolved::UnknownContext(typed.to_string()),
            });
            None
        }
        _ => {
            let names = prefixed.iter().map(|c| c.name.to_string()).collect();
            unresolved.push(match kind {
                NameKind::Stream => Unresolved::AmbiguousStream {
                    typed: typed.to_string(),
                    candidates: names,
                },
                NameKind::Context => Unresolved::AmbiguousContext {
                    typed: typed.to_string(),
                    candidates: names,
                },
            });
            None
        }
    }
}

/// `30m`, `2h`, `90` (bare = minutes), `1h30m`.
fn parse_duration_s(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Ok(mins) = s.parse::<u64>() {
        return Some(mins * 60);
    }
    let mut total = 0u64;
    let mut num = String::new();
    let mut saw_unit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: u64 = num.parse().ok()?;
            num.clear();
            total += match c {
                'h' => n.checked_mul(3600)?,
                'm' => n.checked_mul(60)?,
                'd' => n.checked_mul(86_400)?,
                _ => return None,
            };
            saw_unit = true;
        }
    }
    // A trailing number with no unit ("1h30") is a typo, not 30 seconds.
    (saw_unit && num.is_empty() && total > 0).then_some(total)
}

/// How many words after `^` a date may span.
const MAX_DATE_WORDS: usize = 3;

/// Try the longest date phrase first so `^next saturday 9am` beats `^next`.
fn take_date(
    words: &[&str],
    at: usize,
    first_rest: &str,
    now: Timestamp,
    tz: &TimeZone,
) -> (usize, Option<Timestamp>) {
    let max = MAX_DATE_WORDS.min(words.len() - at);
    for span in (1..=max).rev() {
        let mut phrase = String::from(first_rest);
        for w in &words[at + 1..at + span] {
            phrase.push(' ');
            phrase.push_str(w);
        }
        let phrase = phrase.trim();
        if phrase.is_empty() {
            continue;
        }
        if let Some(ts) = parse_when(phrase, now, tz) {
            return (span, Some(ts));
        }
    }
    (1, None)
}

/// Parse the supported natural-date subset.
///
/// Accepted, case-insensitively:
/// - `today`, `tomorrow`, `tonight` (tonight = 20:00 local)
/// - a weekday name or 3-letter abbreviation, optionally preceded by `next`.
///   A bare weekday means the *next* occurrence strictly after today; `next
///   <weekday>` means the occurrence in the following week, which is the
///   reading that surprises people least.
/// - `YYYY-MM-DD`
/// - a relative span: `+3d`, `+2w`, `+6h`, `+90m`
/// - any of the date forms above followed by a time: `9am`, `9:30`, `14:00`
/// - a bare time, meaning today at that time
///
/// Anything else returns `None` rather than guessing.
#[must_use]
pub fn parse_when(s: &str, now: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let today = now.to_zoned(tz.clone()).date();

    // Relative spans are self-contained.
    if let Some(rest) = s.strip_prefix('+') {
        return parse_relative(rest, now, tz);
    }

    // Split an optional trailing time off the date part.
    let mut parts: Vec<&str> = s.split_whitespace().collect();
    let mut time: Option<Time> = None;
    if parts.len() > 1 {
        if let Some(t) = parse_time(parts[parts.len() - 1]) {
            time = Some(t);
            parts.pop();
        }
    }
    let date_str = parts.join(" ");

    // A bare time means today.
    if date_str.is_empty() {
        let t = time?;
        return civil_to_ts(DateTime::from_parts(today, t), tz);
    }
    if let Some(t) = parse_time(&date_str) {
        return civil_to_ts(DateTime::from_parts(today, t), tz);
    }

    let (date, default_time) = match date_str.as_str() {
        "today" => (today, Time::midnight()),
        "tonight" => (today, Time::new(20, 0, 0, 0).ok()?),
        "tomorrow" => (today.tomorrow().ok()?, Time::midnight()),
        other => {
            if let Ok(d) = other.parse::<Date>() {
                (d, Time::midnight())
            } else {
                let (name, next_week) = other
                    .strip_prefix("next ")
                    .map_or((other, false), |r| (r.trim(), true));
                let wd = parse_weekday(name)?;
                (next_weekday(today, wd, next_week), Time::midnight())
            }
        }
    };
    civil_to_ts(DateTime::from_parts(date, time.unwrap_or(default_time)), tz)
}

/// Resolve a civil datetime in `tz`, taking the DST-compatible reading for
/// gaps and folds (same policy as routine expansion — see
/// [`crate::routine_gen`]).
fn civil_to_ts(dt: DateTime, tz: &TimeZone) -> Option<Timestamp> {
    tz.to_ambiguous_zoned(dt)
        .compatible()
        .ok()
        .map(|z| z.timestamp())
}

/// Relative offsets are applied in the *zoned* domain, not to the bare
/// `Timestamp`. Days and weeks are calendar units: across a DST transition a
/// day is 23 or 25 hours, so "+3d" must mean the same wall-clock time three
/// days later, not exactly 72 hours. jiff enforces this by refusing to add
/// calendar units to a `Timestamp` at all, which is what surfaced the bug.
fn parse_relative(rest: &str, now: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
    let (num, unit) = rest.split_at(rest.len().checked_sub(1)?);
    let n: i64 = num.parse().ok()?;
    let span = match unit {
        "d" => Span::new().try_days(n).ok()?,
        "w" => Span::new().try_weeks(n).ok()?,
        "h" => Span::new().try_hours(n).ok()?,
        "m" => Span::new().try_minutes(n).ok()?,
        _ => return None,
    };
    now.to_zoned(tz.clone())
        .checked_add(span)
        .ok()
        .map(|z| z.timestamp())
}

fn parse_weekday(s: &str) -> Option<Weekday> {
    Some(match s {
        "mon" | "monday" => Weekday::Monday,
        "tue" | "tues" | "tuesday" => Weekday::Tuesday,
        "wed" | "weds" | "wednesday" => Weekday::Wednesday,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thursday,
        "fri" | "friday" => Weekday::Friday,
        "sat" | "saturday" => Weekday::Saturday,
        "sun" | "sunday" => Weekday::Sunday,
        _ => return None,
    })
}

/// Next `wd` strictly after `from`; `next_week` adds a further 7 days.
fn next_weekday(from: Date, wd: Weekday, next_week: bool) -> Date {
    let cur = from.weekday().to_monday_zero_offset();
    let want = wd.to_monday_zero_offset();
    let mut delta = i64::from(want) - i64::from(cur);
    if delta <= 0 {
        delta += 7;
    }
    if next_week {
        delta += 7;
    }
    from.checked_add(Span::new().days(delta)).unwrap_or(from)
}

/// `9am`, `9pm`, `9:30`, `09:30`, `14:00`, `9:30pm`.
fn parse_time(s: &str) -> Option<Time> {
    let s = s.trim();
    let (body, ampm) = if let Some(b) = s.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = s.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (s, None)
    };
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<i8>().ok()?, m.parse::<i8>().ok()?),
        None => (body.parse::<i8>().ok()?, 0),
    };
    let h = match ampm {
        // 12am is 00:xx and 12pm is 12:xx — the two cases people get wrong.
        Some(true) => {
            if h == 12 {
                12
            } else {
                h.checked_add(12)?
            }
        }
        Some(false) => {
            if h == 12 {
                0
            } else {
                h
            }
        }
        None => h,
    };
    Time::new(h, m, 0, 0).ok()
}

/// Map an `Energy` word, for callers that want `@energy:high` style contexts
/// later. Exposed now so the mapping lives in one place.
#[must_use]
pub fn parse_energy(s: &str) -> Option<Energy> {
    match s.trim().to_lowercase().as_str() {
        "low" => Some(Energy::Low),
        "med" | "medium" => Some(Energy::Med),
        "high" => Some(Energy::High),
        _ => None,
    }
}
