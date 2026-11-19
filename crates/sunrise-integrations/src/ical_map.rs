//! The iCalendar ⇄ Sunrise domain mapping.
//!
//! Pure: no vault, no clock, no id generation. Everything here is a function
//! of its arguments, which is what lets the mapping be pinned by unit tests
//! rather than only by an end-to-end import.
//!
//! # `VEVENT` is a [`Block`], not a [`sunrise_domain::Task`]
//!
//! A `VEVENT` is "this happens between these two times". That is exactly a
//! Block — `docs/02-domain/time-blocks.md`: a scheduled range that may bind
//! 0..N Tasks — and it is *not* a Task, which is a thing to do with an
//! optional `scheduled_at` and no end. Mapping events onto Tasks would put
//! every meeting in the user's inbox as work to complete, and lose the end
//! time on the way. `docs/09-integrations/icalendar.md` §Import states the
//! same target: "parse `.ics` → create Blocks".
//!
//! `VTODO` really is a Task, and is **not** mapped here — see the crate docs
//! for what that costs and why.
//!
//! # The time mapping is the substance of this file
//!
//! RFC 5545 gives four ways to write a `DTSTART`, and
//! [`sunrise_domain::SunriseTime`] models the same four distinctions, so the
//! mapping is total in both directions and loses nothing:
//!
//! | iCalendar | Sunrise |
//! |---|---|
//! | `20260301T140000Z` | [`SunriseTime::Instant`] |
//! | `TZID=Europe/Berlin:20260301T090000` | [`SunriseTime::Zoned`] |
//! | `20260301T090000` (no zone) | [`SunriseTime::Floating`] |
//! | `VALUE=DATE:20260301` | [`SunriseTime::AllDay`] |
//!
//! # Recurrence, location and notes
//!
//! `docs/09-integrations/icalendar.md` §Mapping rules: `RRULE` is the
//! Block's `rrule`, each `EXDATE` a cancelled exception at its occurrence
//! key, each `RECURRENCE-ID` override an exception holding what it changes,
//! `LOCATION` the `location` and `DESCRIPTION` the `notes`. The `UID` is the
//! `external_id`, and stays the id derivation too
//! ([`sunrise_domain::imported_block_id`]).
//!
//! A recurring Block repeats a wall-clock time (`docs/10-cross-cutting/
//! time.md` §6), so its anchor is `zoned` or `floating`. A UTC `DTSTART`
//! is read as wall clock in the zone `UTC`, which names the same instants.
//! A rule this build cannot expand, or an all-day or mixed-kind anchor, is
//! reported, not guessed at: the event imports as its first occurrence.

use crate::ical::{bare_time_value, parse_bare_time, ICalEvent, ICalNotice, ICalTime, NoticeCode};
use jiff::{civil, tz::TimeZone, Span};
use std::collections::BTreeMap;
use sunrise_core::ImportedFields;
use sunrise_domain::routine_gen::parse_occurrence_key;
use sunrise_domain::rrule::RRule;
use sunrise_domain::validation::MAX_BLOCK_LOCATION_LEN;
use sunrise_domain::{
    block_uid, decode_note_body, encode_note_body, note_body_to_plain_text, Block, BlockDraft,
    BlockException, BlockSeries, Inline, NoteBlock, NoteBody, SunriseTime, MAX_BLOCK_TITLE_LEN,
};
use sunrise_id::EntityRef;

/// A `VEVENT` lowered to something the core can be handed.
#[derive(Debug, Clone)]
pub struct MappedEvent {
    /// The event's `UID`, which is the import key.
    pub uid: String,
    /// The Block to write.
    pub draft: BlockDraft,
    /// The Block's fields beyond the draft: its series, location, notes and
    /// foreign id.
    pub fields: ImportedFields,
    /// What the Block could not carry.
    pub notices: Vec<ICalNotice>,
}

/// The `VEVENT`s of one Block: an event, and the `RECURRENCE-ID` overrides
/// of its `UID`.
#[derive(Debug, Clone)]
pub struct EventGroup<'a> {
    /// The event, which states the series.
    pub event: &'a ICalEvent,
    /// The overrides of its occurrences, in file order.
    pub overrides: Vec<&'a ICalEvent>,
}

/// Pair each `RECURRENCE-ID` override with the event of its `UID`, in file
/// order.
///
/// Each event with no `RECURRENCE-ID` is a group. Overrides join the last
/// such event of their `UID`, which is the one whose write an import keeps.
/// An override whose `UID` has no such event in the file stands in for it:
/// the first one is the group's event, imported as a one-off, and the rest
/// are its overrides.
#[must_use]
pub fn group_events(events: &[ICalEvent]) -> Vec<EventGroup<'_>> {
    let has_event: std::collections::BTreeSet<&str> = events
        .iter()
        .filter(|e| e.recurrence_id.is_none())
        .map(|e| e.uid.as_str())
        .collect();
    // The group each UID's overrides join: its last event's, or its first
    // orphan override's.
    let mut owner: BTreeMap<&str, usize> = BTreeMap::new();
    let mut groups: Vec<EventGroup<'_>> = Vec::new();
    let mut overrides: Vec<&ICalEvent> = Vec::new();
    for ev in events {
        let uid = ev.uid.as_str();
        let stands_alone =
            ev.recurrence_id.is_none() || (!has_event.contains(uid) && !owner.contains_key(uid));
        if stands_alone {
            owner.insert(uid, groups.len());
            groups.push(EventGroup {
                event: ev,
                overrides: Vec::new(),
            });
        } else {
            overrides.push(ev);
        }
    }
    for ev in overrides {
        if let Some(&i) = owner.get(ev.uid.as_str()) {
            groups[i].overrides.push(ev);
        }
    }
    groups
}

/// Lower one `VEVENT` onto a [`BlockDraft`] in `stream_id`, or in no stream
/// for `None` (ADR-0046), with `overrides` as the exceptions of its series.
///
/// # Errors
///
/// An [`ICalNotice`] describing why this event has no Block form — a missing
/// `DTSTART`, a zero-length event, or an end that is not after its start. The
/// caller reports it and moves on to the next event: one unusable event must
/// not fail the whole import. An override that cannot be carried is reported
/// in the result, and the rest of the series kept.
pub fn event_to_block(
    ev: &ICalEvent,
    overrides: &[&ICalEvent],
    stream_id: Option<EntityRef>,
) -> Result<MappedEvent, ICalNotice> {
    let uid = || Some(ev.uid.clone());
    let skip = |detail: String| ICalNotice {
        code: NoticeCode::Skipped,
        uid: uid(),
        detail,
    };

    let Some(start) = ev.dtstart.as_ref() else {
        return Err(skip(
            "no DTSTART: an event with no start is not a Block".into(),
        ));
    };
    let mut notices = Vec::new();
    let end = resolve_end(start, ev, &mut notices).map_err(skip)?;

    let starts_at = to_sunrise(start);
    let ends_at = to_sunrise(&end);
    if refused_order(&starts_at, &ends_at) {
        return Err(skip(format!(
            "DTEND is not after DTSTART ({starts_at} .. {ends_at})"
        )));
    }

    // A Block with no bound Task needs a title of its own, and an untitled
    // event would otherwise produce an unlabelled bar on the calendar.
    let title = ev
        .summary
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map_or_else(|| "(untitled event)".to_string(), ToString::to_string);

    let mut draft = BlockDraft {
        stream_id,
        starts_at,
        ends_at,
        title: Some(title),
        // An imported Block's title is the calendar's, not a shadow copy of
        // some task's, so tracking would be wrong even if a task were later
        // bound to it.
        title_track_task: false,
        tasks: Vec::new(),
    };
    let mut fields = ImportedFields {
        location: location_of(ev, &mut notices),
        notes: ev.description.as_deref().and_then(note_from),
        external_id: Some(ev.uid.clone()),
        ..ImportedFields::default()
    };
    if let Some(series) = recurrence_of(ev, &draft, &mut notices) {
        draft.starts_at = series.starts_at.clone();
        draft.ends_at = series.ends_at.clone();
        let exceptions = exceptions_of(ev, overrides, &series, &draft, &fields, &mut notices);
        fields.rrule = Some(series.rrule);
        fields.exceptions = exceptions;
    } else {
        drop_recurrence(ev, overrides, &mut notices);
    }

    Ok(MappedEvent {
        uid: ev.uid.clone(),
        draft,
        fields,
        notices,
    })
}

/// The series `ev` states, when a Block can hold it; otherwise `None`, with
/// why reported for a rule a Block cannot hold and for an orphan override.
fn recurrence_of(
    ev: &ICalEvent,
    draft: &BlockDraft,
    notices: &mut Vec<ICalNotice>,
) -> Option<BlockSeries> {
    match (&ev.rrule, &ev.recurrence_id) {
        (Some(raw), None) => series_of(raw, draft)
            .map_err(|why| {
                notices.push(unmapped(
                    ev,
                    format!(
                        "RRULE:{raw} was dropped: {why}; only the first occurrence was imported"
                    ),
                ));
            })
            .ok(),
        (_, Some(rid)) => {
            notices.push(unmapped(
                ev,
                format!(
                    "RECURRENCE-ID:{} has no recurring event of its UID in this file; imported \
                     as a single event",
                    bare_time_value(rid)
                ),
            ));
            None
        }
        (None, None) => None,
    }
}

/// The exceptions of `series`: a cancelled one per `EXDATE` of `ev`, and
/// one per override in `overrides` that changes its occurrence. What names
/// no occurrence is reported and dropped.
fn exceptions_of(
    ev: &ICalEvent,
    overrides: &[&ICalEvent],
    series: &BlockSeries,
    draft: &BlockDraft,
    fields: &ImportedFields,
    notices: &mut Vec<ICalNotice>,
) -> BTreeMap<String, BlockException> {
    let mut exceptions = BTreeMap::new();
    for t in &ev.exdates {
        if let Some(key) = occurrence_key(t, series) {
            exceptions.insert(key, BlockException::cancelled());
        } else {
            notices.push(unmapped(
                ev,
                format!(
                    "EXDATE:{} names no occurrence of the rule and was dropped",
                    bare_time_value(t)
                ),
            ));
        }
    }
    for o in overrides {
        match exception_of(o, series, draft, fields, notices) {
            Ok(Some((key, x))) => {
                exceptions.insert(key, x);
            }
            Ok(None) => {}
            Err(why) => notices.push(ICalNotice {
                code: NoticeCode::Skipped,
                uid: Some(ev.uid.clone()),
                detail: format!("an override was dropped: {why}"),
            }),
        }
    }
    exceptions
}

/// Report the `EXDATE`s and overrides of an event that does not recur in
/// Sunrise, which have no series to land in.
fn drop_recurrence(ev: &ICalEvent, overrides: &[&ICalEvent], notices: &mut Vec<ICalNotice>) {
    if !ev.exdates.is_empty() {
        notices.push(unmapped(
            ev,
            "EXDATE was dropped: the event does not recur in Sunrise".into(),
        ));
    }
    for o in overrides {
        notices.push(ICalNotice {
            code: NoticeCode::Skipped,
            uid: Some(ev.uid.clone()),
            detail: format!(
                "the override of {} was dropped: the event does not recur in Sunrise",
                o.recurrence_id
                    .as_ref()
                    .map_or_else(String::new, bare_time_value)
            ),
        });
    }
}

/// Whether the order of `starts_at` and `ends_at` is every reader's, and
/// inverted.
///
/// A pair whose order is the same for every reader is judged here, with no
/// reader at all, in UTC:
///
/// * two bounds that each resolve without a reader (an instant, or a time
///   in a zone the tzdb knows) resolve to the same instants in every zone,
///   whatever their kinds or zones;
/// * two bounds of one kind (and one zone) order the same everywhere,
///   because in UTC, which has no transitions, a civil order and its
///   resolved order agree.
///
/// Any other pair (a floating or all-day bound beside one of another kind)
/// orders differently in different zones (`docs/10-cross-cutting/time.md`
/// §2); it is left to the core, which validates the draft in the reader's
/// zone when it is written, and `import` reports a refusal there per event.
fn refused_order(starts_at: &SunriseTime, ends_at: &SunriseTime) -> bool {
    let same_kind = match (starts_at, ends_at) {
        (SunriseTime::Zoned { tz: a, .. }, SunriseTime::Zoned { tz: b, .. }) => a == b,
        (a, b) => a.kind_str() == b.kind_str(),
    };
    let reader_free = same_kind || (anchored(starts_at) && anchored(ends_at));
    // On resolved instants, as the core's range check compares them, so an
    // end at the start's instant under another kind is refused here too.
    let inverted = matches!(
        (ends_at.resolve_in(&TimeZone::UTC), starts_at.resolve_in(&TimeZone::UTC)),
        (Some(e), Some(s)) if e <= s
    );
    reader_free && inverted
}

/// Whether `t` resolves to one instant for every reader.
fn anchored(t: &SunriseTime) -> bool {
    match t {
        SunriseTime::Instant { .. } => true,
        SunriseTime::Zoned { tz, .. } => TimeZone::get(tz).is_ok(),
        _ => false,
    }
}

/// Whether `a` and `b` are the same time: equal, or two times that resolve
/// without a reader to the same instant.
fn same_time(a: &SunriseTime, b: &SunriseTime) -> bool {
    a == b
        || (anchored(a)
            && anchored(b)
            && a.resolve_in(&TimeZone::UTC) == b.resolve_in(&TimeZone::UTC))
}

/// An [`NoticeCode::UnmappedProperty`] notice about `ev`.
fn unmapped(ev: &ICalEvent, detail: String) -> ICalNotice {
    ICalNotice {
        code: NoticeCode::UnmappedProperty,
        uid: Some(ev.uid.clone()),
        detail,
    }
}

/// `text` cut to `max` characters, with a notice saying so when it was cut.
fn bounded(
    ev: &ICalEvent,
    name: &str,
    text: &str,
    max: usize,
    notices: &mut Vec<ICalNotice>,
) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    notices.push(unmapped(
        ev,
        format!("{name} was cut to the {max} characters a Block holds"),
    ));
    text.chars().take(max).collect()
}

/// The `LOCATION` of `ev` as a Block holds one: trimmed, `None` when empty,
/// and cut to [`MAX_BLOCK_LOCATION_LEN`] characters.
fn location_of(ev: &ICalEvent, notices: &mut Vec<ICalNotice>) -> Option<String> {
    let text = ev
        .location
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())?;
    Some(bounded(
        ev,
        "LOCATION",
        text,
        MAX_BLOCK_LOCATION_LEN,
        notices,
    ))
}

/// A `DESCRIPTION` as a note: one paragraph per blank-line-separated run,
/// as the CLI writes plain text into a note. `None` when it holds no text.
fn note_from(text: &str) -> Option<NoteBody> {
    let blocks: Vec<NoteBlock> = text
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| NoteBlock::Paragraph {
            inline: vec![Inline::text(p)],
        })
        .collect();
    (!blocks.is_empty()).then(|| encode_note_body(&blocks))
}

/// A note as a `DESCRIPTION`: the inverse of [`note_from`] for a note of
/// paragraphs, and the note's plain text for any other. `None` when it holds
/// no text.
fn description_of(body: &NoteBody) -> Option<String> {
    let doc = decode_note_body(body);
    let paragraphs: Option<Vec<String>> = doc
        .blocks
        .iter()
        .map(|b| match b {
            NoteBlock::Paragraph { inline } => {
                Some(inline.iter().map(Inline::plain_text).collect::<String>())
            }
            _ => None,
        })
        .collect();
    let text = paragraphs.map_or_else(|| note_body_to_plain_text(&doc), |p| p.join("\n\n"));
    (!text.trim().is_empty()).then_some(text)
}

/// The series an event with rule `raw` states, its anchor the draft's
/// bounds read as wall clock.
fn series_of(raw: &str, draft: &BlockDraft) -> Result<BlockSeries, String> {
    let rrule = parse_rrule(raw)?;
    let (starts_at, ends_at) = wall_clock_anchor(&draft.starts_at, &draft.ends_at)?;
    let series = BlockSeries {
        starts_at,
        ends_at,
        rrule,
        exceptions: BTreeMap::new(),
    };
    series.validate().map_err(|e| e.to_string())?;
    Ok(series)
}

/// An `RRULE` value as a rule this build expands.
///
/// `UNTIL` is read with the iCalendar time grammar (`20270101T000000Z`),
/// which the domain parser does not speak, and every other part by
/// [`RRule::parse`]. A value outside the engine subset
/// ([`RRule::first_unknown`]) is refused rather than kept: a rule this build
/// cannot expand would draw no occurrence at all.
fn parse_rrule(raw: &str) -> Result<RRule, String> {
    let mut until = None;
    let mut rest = Vec::new();
    for part in raw.trim().split(';').filter(|p| !p.is_empty()) {
        match part.split_once('=') {
            Some((k, v)) if k.eq_ignore_ascii_case("UNTIL") => {
                let t = parse_bare_time(v).ok_or_else(|| format!("UNTIL={v} did not parse"))?;
                until = Some(to_sunrise(&t));
            }
            _ => rest.push(part),
        }
    }
    let mut rule = RRule::parse(&rest.join(";")).map_err(|e| e.to_string())?;
    if let Some((part, value)) = rule.first_unknown() {
        return Err(format!(
            "{part}={value} is outside the rules Sunrise expands"
        ));
    }
    rule.until = until;
    Ok(rule)
}

/// The rule's text as an `RRULE` value, its `UNTIL` in the iCalendar time
/// grammar: a UTC instant for an instant or a zoned time (RFC 5545 §3.3.10
/// wants UTC beside a zoned start), otherwise its own form.
fn rrule_value(rule: &RRule) -> String {
    let mut text = RRule {
        until: None,
        ..rule.clone()
    }
    .to_rfc5545();
    let until = match &rule.until {
        Some(SunriseTime::Floating { civil }) => Some(ICalTime::Floating(*civil)),
        Some(SunriseTime::AllDay { date }) => Some(ICalTime::Date(*date)),
        Some(t) => t.resolve_in(&TimeZone::UTC).map(ICalTime::Utc),
        None => None,
    };
    if let Some(u) = until {
        text.push_str(";UNTIL=");
        text.push_str(&bare_time_value(&u));
    }
    text
}

/// A recurring event's bounds as a series anchor: wall clock, both of one
/// kind and zone.
///
/// A zoned start keeps its zone, and an end in another zone or in UTC is
/// read as wall clock in it. A UTC start is wall clock in `UTC`. A floating
/// start needs a floating end. A whole-day start has no wall-clock time to
/// repeat.
fn wall_clock_anchor(
    starts_at: &SunriseTime,
    ends_at: &SunriseTime,
) -> Result<(SunriseTime, SunriseTime), String> {
    let in_zone = |t: &SunriseTime, tz: &str| -> Option<SunriseTime> {
        let zone = TimeZone::get(tz).ok()?;
        match t {
            SunriseTime::Zoned { tz: other, .. } if other == tz => Some(t.clone()),
            SunriseTime::Zoned { .. } | SunriseTime::Instant { .. } => {
                let at = t.resolve_in(&TimeZone::UTC)?;
                Some(SunriseTime::zoned(at.to_zoned(zone).datetime(), tz))
            }
            _ => None,
        }
    };
    let mixed = || "its end is not of its start's kind".to_owned();
    match starts_at {
        SunriseTime::Zoned { tz, .. } => {
            Ok((starts_at.clone(), in_zone(ends_at, tz).ok_or_else(mixed)?))
        }
        SunriseTime::Instant { .. } => Ok((
            in_zone(starts_at, "UTC").ok_or_else(mixed)?,
            in_zone(ends_at, "UTC").ok_or_else(mixed)?,
        )),
        SunriseTime::Floating { .. } if matches!(ends_at, SunriseTime::Floating { .. }) => {
            Ok((starts_at.clone(), ends_at.clone()))
        }
        SunriseTime::Floating { .. } => Err(mixed()),
        _ => Err(
            "a recurring whole-day event is not modelled; a recurring Block repeats a \
                  wall-clock time"
                .into(),
        ),
    }
}

/// The key of the occurrence of `series` that starts at `t` (an `EXDATE` or
/// a `RECURRENCE-ID`), when the rule produces one there.
///
/// `t` is read as wall clock in the anchor's frame: in its zone for a zoned
/// anchor, as written for a floating one. A whole date takes the anchor's
/// time of day. A UTC or zoned value beside a floating anchor names a
/// different occurrence for each reader, so it names none.
fn occurrence_key(t: &ICalTime, series: &BlockSeries) -> Option<String> {
    let anchor_time = match &series.starts_at {
        SunriseTime::Zoned { civil, .. } | SunriseTime::Floating { civil } => civil.time(),
        _ => return None,
    };
    let civil = match (&series.starts_at, t) {
        (_, ICalTime::Date(d)) => d.to_datetime(anchor_time),
        (_, ICalTime::Floating(c)) => *c,
        (SunriseTime::Zoned { tz, .. }, ICalTime::Zoned { civil, tz: other }) if other == tz => {
            *civil
        }
        (SunriseTime::Zoned { tz, .. }, _) => {
            let at = to_sunrise(t).resolve_in(&TimeZone::UTC)?;
            at.to_zoned(TimeZone::get(tz).ok()?).datetime()
        }
        _ => return None,
    };
    let key = civil.strftime("%Y-%m-%dT%H:%M").to_string();
    series.produces_key(&key).then_some(key)
}

/// The exception a `RECURRENCE-ID` override `o` writes at its occurrence:
/// the bounds it moves and the title, location and notes it changes from
/// the series'. `None` when it changes nothing.
fn exception_of(
    o: &ICalEvent,
    series: &BlockSeries,
    draft: &BlockDraft,
    fields: &ImportedFields,
    notices: &mut Vec<ICalNotice>,
) -> Result<Option<(String, BlockException)>, String> {
    let rid = o.recurrence_id.as_ref().ok_or("it has no RECURRENCE-ID")?;
    let key = occurrence_key(rid, series).ok_or_else(|| {
        format!(
            "RECURRENCE-ID:{} names no occurrence of the rule",
            bare_time_value(rid)
        )
    })?;
    let (rule_start, rule_end) = series
        .occurrence_bounds(&key)
        .ok_or_else(|| format!("occurrence {key} has no bounds"))?;
    let mut x = BlockException::default();
    if let Some(start) = &o.dtstart {
        let end = resolve_end(start, o, notices)?;
        let (s, e) = (to_sunrise(start), to_sunrise(&end));
        if refused_order(&s, &e) {
            return Err(format!("its DTEND is not after its DTSTART ({s} .. {e})"));
        }
        x.starts_at = (!same_time(&s, &rule_start)).then_some(s);
        x.ends_at = (!same_time(&e, &rule_end)).then_some(e);
    }
    let title = o
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(t) = title.filter(|t| Some(*t) != draft.title.as_deref()) {
        x.title = Some(bounded(o, "SUMMARY", t, MAX_BLOCK_TITLE_LEN, notices));
    }
    let location = location_of(o, notices);
    if location.is_some() && location != fields.location {
        x.location = location;
    }
    let notes = o.description.as_deref().and_then(note_from);
    if notes.is_some() && notes != fields.notes {
        x.notes = notes;
    }
    Ok((x != BlockException::default()).then_some((key, x)))
}

/// Raise a Block back to a `VEVENT`: for a recurring Block, the event that
/// states its series, with its rule and an `EXDATE` per cancelled
/// occurrence. [`block_to_events`] adds the overrides.
///
/// `block` is the Block as stored, a series' anchor and not one occurrence
/// of it. `title` is the *resolved* title
/// (`sunrise_core::queries::BlockRow::title`), not the stored one: a Block
/// that tracks its bound Task's title should export what a reader would see.
#[must_use]
pub fn block_to_event(block: &Block, title: Option<&str>) -> ICalEvent {
    let series = block.series();
    let exdates = series
        .iter()
        .flat_map(|s| {
            s.exceptions
                .iter()
                .filter(|(_, x)| x.cancelled)
                .filter_map(|(key, _)| key_time(key, s))
        })
        .collect();
    ICalEvent {
        uid: export_uid(block),
        summary: title.map(ToString::to_string),
        dtstart: Some(from_sunrise(&block.starts_at)),
        dtend: Some(from_sunrise(&block.ends_at)),
        // A Block holds an end, never a duration, so exporting one would be
        // inventing content.
        duration: None,
        description: block.notes.as_ref().and_then(description_of),
        location: block.location.clone(),
        rrule: block.rrule.as_ref().map(rrule_value),
        exdates,
        recurrence_id: None,
    }
}

/// Raise a Block back to its `VEVENT`s: [`block_to_event`], then one
/// `RECURRENCE-ID` override per occurrence an exception changes, in key
/// order. An override states the occurrence whole, as RFC 5545 §3.8.4.4
/// wants: what the exception leaves absent is the series'.
#[must_use]
pub fn block_to_events(block: &Block, title: Option<&str>) -> Vec<ICalEvent> {
    let event = block_to_event(block, title);
    let mut out = Vec::new();
    if let Some(series) = block.series() {
        for (key, x) in series.exceptions.iter().filter(|(_, x)| !x.cancelled) {
            let (Some(rid), Some((rule_start, rule_end))) =
                (key_time(key, &series), series.occurrence_bounds(key))
            else {
                continue;
            };
            out.push(ICalEvent {
                uid: event.uid.clone(),
                summary: x.title.clone().or_else(|| event.summary.clone()),
                dtstart: Some(from_sunrise(x.starts_at.as_ref().unwrap_or(&rule_start))),
                dtend: Some(from_sunrise(x.ends_at.as_ref().unwrap_or(&rule_end))),
                duration: None,
                description: x
                    .notes
                    .as_ref()
                    .and_then(description_of)
                    .or_else(|| event.description.clone()),
                location: x.location.clone().or_else(|| event.location.clone()),
                rrule: None,
                exdates: Vec::new(),
                recurrence_id: Some(rid),
            });
        }
    }
    out.insert(0, event);
    out
}

/// The `UID` a Block exports under: the foreign id it was imported with,
/// so the calendar it came from recognises the event, or else the Block's
/// own id, so re-importing the file finds this Block rather than making a
/// second one. See `sunrise_domain::import`.
fn export_uid(block: &Block) -> String {
    block
        .external_id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| block_uid(block.id))
}

/// The start of occurrence `key` of `series`, of the anchor's kind, as an
/// `EXDATE` or `RECURRENCE-ID` names it.
fn key_time(key: &str, series: &BlockSeries) -> Option<ICalTime> {
    let civil = parse_occurrence_key(key)?;
    match &series.starts_at {
        SunriseTime::Zoned { tz, .. } => Some(ICalTime::Zoned {
            civil,
            tz: tz.clone(),
        }),
        SunriseTime::Floating { .. } => Some(ICalTime::Floating(civil)),
        _ => None,
    }
}

/// The end of an event, from `DTEND`, `DURATION`, or RFC 5545's default.
///
/// RFC 5545 §3.6.1 is explicit about the no-`DTEND`, no-`DURATION` case, and
/// it differs by type: a `DATE` start lasts one day, a `DATE-TIME` start lasts
/// **zero** time. A zero-length range is not a Block — `Block::ends_at` must
/// resolve strictly after `starts_at` — so that event is refused rather than
/// given an invented hour, which would put a fictitious commitment on the
/// user's calendar.
fn resolve_end(
    start: &ICalTime,
    ev: &ICalEvent,
    notices: &mut Vec<ICalNotice>,
) -> Result<ICalTime, String> {
    if let Some(end) = ev.dtend.as_ref() {
        if ev.duration.is_some() {
            // RFC 5545 §3.6.1 forbids both. Preferring DTEND is the reading
            // every major client takes, and the duration is reported so the
            // choice is visible rather than assumed.
            notices.push(ICalNotice {
                code: NoticeCode::UnmappedProperty,
                uid: Some(ev.uid.clone()),
                detail: "DTEND and DURATION were both present; DURATION was ignored".into(),
            });
        }
        return Ok(end.clone());
    }
    if let Some(raw) = ev.duration.as_ref() {
        let span: Span = raw
            .parse()
            .map_err(|e| format!("DURATION:{raw} did not parse ({e})"))?;
        return add_span(start, span).map_err(|e| format!("DTSTART + DURATION:{raw} ({e})"));
    }
    match start {
        ICalTime::Date(d) => d
            .tomorrow()
            .map(ICalTime::Date)
            .map_err(|e| format!("DTSTART + 1 day ({e})")),
        _ => Err(
            "no DTEND and no DURATION: RFC 5545 gives this event zero length, which is not \
             a time block"
                .into(),
        ),
    }
}

/// `start + span`, keeping the kind of `start`.
fn add_span(start: &ICalTime, span: Span) -> Result<ICalTime, jiff::Error> {
    match start {
        // Via a UTC zoned value, not the bare timestamp: a `P1D` or `P1W`
        // duration carries calendar units, which an instant cannot add on its
        // own.
        ICalTime::Utc(at) => at
            .to_zoned(TimeZone::UTC)
            .checked_add(span)
            .map(|z| ICalTime::Utc(z.timestamp())),
        ICalTime::Zoned { civil, tz } => civil.checked_add(span).map(|c| ICalTime::Zoned {
            civil: c,
            tz: tz.clone(),
        }),
        ICalTime::Floating(civil) => civil.checked_add(span).map(ICalTime::Floating),
        // A DATE start with a duration carrying time-of-day units stops being
        // a whole-day range, so it becomes a floating datetime rather than
        // being rounded to a date and quietly losing the hours.
        ICalTime::Date(d) => {
            let midnight = d.to_datetime(civil::Time::midnight());
            let moved = midnight.checked_add(span)?;
            Ok(if moved.time() == civil::Time::midnight() {
                ICalTime::Date(moved.date())
            } else {
                ICalTime::Floating(moved)
            })
        }
    }
}

/// iCalendar time → domain time. Total, and kind-preserving.
fn to_sunrise(t: &ICalTime) -> SunriseTime {
    match t {
        ICalTime::Utc(at) => SunriseTime::instant(*at),
        ICalTime::Zoned { civil, tz } => SunriseTime::zoned(*civil, tz.clone()),
        ICalTime::Floating(civil) => SunriseTime::floating(*civil),
        ICalTime::Date(d) => SunriseTime::all_day(*d),
    }
}

/// Domain time → iCalendar time. The exact inverse of [`to_sunrise`] for the
/// four kinds iCalendar can express. A kind this build does not know exports
/// at the instant it sorts at ([`SunriseTime::instant_or_last`]): its `at`
/// anchor, or the far end of the calendar when it has none, never the epoch.
/// iCalendar has no way to carry the kind itself.
fn from_sunrise(t: &SunriseTime) -> ICalTime {
    match t {
        SunriseTime::Unknown { .. } => ICalTime::Utc(t.instant_or_last(&jiff::tz::TimeZone::UTC)),
        SunriseTime::Instant { at } => ICalTime::Utc(*at),
        SunriseTime::Zoned { civil, tz } => ICalTime::Zoned {
            civil: *civil,
            tz: tz.clone(),
        },
        SunriseTime::Floating { civil } => ICalTime::Floating(*civil),
        SunriseTime::AllDay { date } => ICalTime::Date(*date),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Timestamp;
    use std::collections::BTreeSet;
    use sunrise_domain::{imported_block_id, Unknowns};
    use sunrise_id::EntityKind;

    fn stream() -> EntityRef {
        EntityRef::new(EntityKind::Stream, [7u8; 16])
    }

    fn utc(y: i16, m: i8, d: i8, h: i8) -> ICalTime {
        ICalTime::Utc(
            civil::date(y, m, d)
                .at(h, 0, 0, 0)
                .to_zoned(TimeZone::UTC)
                .unwrap()
                .timestamp(),
        )
    }

    fn event() -> ICalEvent {
        ICalEvent {
            summary: Some("Coffee with Alice".into()),
            dtstart: Some(utc(2026, 3, 1, 14)),
            dtend: Some(utc(2026, 3, 1, 15)),
            ..ICalEvent::new("ev1@example.com")
        }
    }

    #[test]
    fn a_timed_event_becomes_a_block_over_the_same_range() {
        let m = event_to_block(&event(), &[], Some(stream())).expect("mapped");
        assert_eq!(m.uid, "ev1@example.com");
        assert_eq!(m.draft.title.as_deref(), Some("Coffee with Alice"));
        assert_eq!(m.draft.stream_id, Some(stream()));
        assert!(m.draft.tasks.is_empty());
        assert!(!m.draft.title_track_task);
        m.draft
            .validate(&TimeZone::UTC)
            .expect("a mapped draft is valid");
        assert!(m.notices.is_empty(), "{:?}", m.notices);
    }

    /// The mapping's whole point: a zoned event stays zoned, so "09:00 in
    /// Berlin" does not become an instant that drifts on the next DST change.
    #[test]
    fn each_time_kind_maps_to_its_own_domain_kind() {
        let cases: Vec<(ICalTime, &str)> = vec![
            (utc(2026, 3, 1, 9), "instant"),
            (
                ICalTime::Zoned {
                    civil: civil::date(2026, 3, 1).at(9, 0, 0, 0),
                    tz: "Europe/Berlin".into(),
                },
                "zoned",
            ),
            (
                ICalTime::Floating(civil::date(2026, 3, 1).at(9, 0, 0, 0)),
                "floating",
            ),
            (ICalTime::Date(civil::date(2026, 3, 1)), "all_day"),
        ];
        for (t, kind) in cases {
            assert_eq!(to_sunrise(&t).kind_str(), kind);
            // And back again, unchanged.
            assert_eq!(from_sunrise(&to_sunrise(&t)), t);
        }
    }

    #[test]
    fn an_all_day_event_with_no_dtend_lasts_exactly_one_day() {
        let ev = ICalEvent {
            dtstart: Some(ICalTime::Date(civil::date(2026, 3, 1))),
            ..ICalEvent::new("ev1")
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert_eq!(
            m.draft.starts_at,
            SunriseTime::all_day(civil::date(2026, 3, 1))
        );
        assert_eq!(
            m.draft.ends_at,
            SunriseTime::all_day(civil::date(2026, 3, 2))
        );
    }

    /// RFC 5545 gives a DATE-TIME start with no end zero length. A Block
    /// cannot be zero-length, and inventing an hour would put a commitment on
    /// the calendar that the file never claimed.
    #[test]
    fn a_zero_length_timed_event_is_refused_rather_than_padded() {
        let ev = ICalEvent {
            dtstart: Some(utc(2026, 3, 1, 9)),
            ..ICalEvent::new("ev1")
        };
        let err = event_to_block(&ev, &[], Some(stream())).expect_err("no end");
        assert_eq!(err.code, NoticeCode::Skipped);
        assert!(err.detail.contains("zero length"), "{}", err.detail);
    }

    #[test]
    fn duration_supplies_the_end_when_dtend_is_absent() {
        let ev = ICalEvent {
            dtstart: Some(utc(2026, 3, 1, 9)),
            duration: Some("PT90M".into()),
            ..ICalEvent::new("ev1")
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert_eq!(
            m.draft.ends_at,
            SunriseTime::instant(
                civil::date(2026, 3, 1)
                    .at(10, 30, 0, 0)
                    .to_zoned(TimeZone::UTC)
                    .unwrap()
                    .timestamp()
            )
        );
    }

    /// A calendar-unit duration cannot be added to a bare instant; the mapping
    /// goes through a UTC zoned value so `P1D` works.
    #[test]
    fn a_calendar_unit_duration_applies_to_an_instant() {
        let ev = ICalEvent {
            dtstart: Some(utc(2026, 3, 1, 9)),
            duration: Some("P1D".into()),
            ..ICalEvent::new("ev1")
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert_eq!(m.draft.ends_at, to_sunrise(&utc(2026, 3, 2, 9)));
    }

    #[test]
    fn dtend_wins_over_duration_and_the_conflict_is_reported() {
        let ev = ICalEvent {
            duration: Some("PT9H".into()),
            ..event()
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert_eq!(m.draft.ends_at, to_sunrise(&utc(2026, 3, 1, 15)));
        assert!(m
            .notices
            .iter()
            .any(|n| n.detail.contains("DURATION was ignored")));
    }

    #[test]
    fn an_event_with_no_start_is_refused() {
        let ev = ICalEvent {
            dtstart: None,
            ..event()
        };
        assert!(event_to_block(&ev, &[], Some(stream())).is_err());
    }

    #[test]
    fn an_inverted_range_is_refused() {
        let ev = ICalEvent {
            dtend: Some(utc(2026, 3, 1, 13)),
            ..event()
        };
        let err = event_to_block(&ev, &[], Some(stream())).expect_err("inverted");
        assert!(err.detail.contains("not after"), "{}", err.detail);
    }

    /// A start in a named zone and an end in UTC are two kinds, but both
    /// resolve without a reader, so their order is every reader's: 09:00 New
    /// York is 14:00Z, after a 12:00Z end, and the mapper refuses it.
    #[test]
    fn an_inverted_range_across_a_tzid_and_utc_is_refused() {
        let ev = ICalEvent {
            dtstart: Some(ICalTime::Zoned {
                civil: civil::date(2026, 3, 1).at(9, 0, 0, 0),
                tz: "America/New_York".into(),
            }),
            dtend: Some(utc(2026, 3, 1, 12)),
            ..event()
        };
        let err = event_to_block(&ev, &[], Some(stream())).expect_err("inverted");
        assert_eq!(err.code, NoticeCode::Skipped);
        assert!(err.detail.contains("not after"), "{}", err.detail);

        // Two TZIDs, likewise: 09:00 New York (14:00Z) to 15:00 Berlin
        // (14:00Z) is zero length.
        let ev = ICalEvent {
            dtend: Some(ICalTime::Zoned {
                civil: civil::date(2026, 3, 1).at(15, 0, 0, 0),
                tz: "Europe/Berlin".into(),
            }),
            ..ev
        };
        assert!(event_to_block(&ev, &[], Some(stream())).is_err());
    }

    /// A floating start beside a UTC end orders differently in different
    /// zones, so the mapper leaves it to the core, which judges it in the
    /// reader's zone.
    #[test]
    fn a_floating_start_beside_a_utc_end_is_left_to_the_core() {
        let ev = ICalEvent {
            dtstart: Some(ICalTime::Floating(civil::date(2026, 3, 1).at(13, 0, 0, 0))),
            dtend: Some(utc(2026, 3, 1, 12)),
            ..event()
        };
        assert!(event_to_block(&ev, &[], Some(stream())).is_ok());
    }

    /// `LOCATION`, `DESCRIPTION` and the `UID` land on the Block's fields,
    /// with nothing to report.
    #[test]
    fn location_description_and_uid_are_carried() {
        let ev = ICalEvent {
            description: Some("Agenda\n\nBring the deck".into()),
            location: Some("  Room 3 ".into()),
            ..event()
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert!(m.notices.is_empty(), "{:?}", m.notices);
        assert_eq!(m.fields.location.as_deref(), Some("Room 3"));
        assert_eq!(m.fields.external_id.as_deref(), Some("ev1@example.com"));
        assert_eq!(m.fields.rrule, None);
        let notes = m.fields.notes.as_ref().expect("notes");
        assert_eq!(
            description_of(notes).as_deref(),
            Some("Agenda\n\nBring the deck")
        );
    }

    #[test]
    fn a_location_longer_than_a_block_holds_is_cut_and_reported() {
        let ev = ICalEvent {
            location: Some("x".repeat(MAX_BLOCK_LOCATION_LEN + 5)),
            ..event()
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert_eq!(
            m.fields.location.as_ref().map(|l| l.chars().count()),
            Some(MAX_BLOCK_LOCATION_LEN)
        );
        assert_eq!(m.notices.len(), 1, "{:?}", m.notices);
    }

    fn berlin(d: i8, h: i8) -> ICalTime {
        ICalTime::Zoned {
            civil: civil::date(2026, 3, d).at(h, 0, 0, 0),
            tz: "Europe/Berlin".into(),
        }
    }

    /// A weekly standup, Mondays 09:00–09:30 Berlin from 2026-03-02.
    fn standup() -> ICalEvent {
        ICalEvent {
            summary: Some("Standup".into()),
            dtstart: Some(berlin(2, 9)),
            dtend: Some(ICalTime::Zoned {
                civil: civil::date(2026, 3, 2).at(9, 30, 0, 0),
                tz: "Europe/Berlin".into(),
            }),
            rrule: Some("FREQ=WEEKLY;BYDAY=MO;UNTIL=20260330T080000Z".into()),
            ..ICalEvent::new("standup@example.com")
        }
    }

    /// The issue's case: one recurring block, its rule, an `EXDATE` as a
    /// cancelled occurrence and an override as the fields it changes.
    #[test]
    fn a_recurring_event_maps_onto_one_series_with_its_exceptions() {
        let ev = ICalEvent {
            // One in the anchor's zone, one in UTC: 08:00Z is 09:00 Berlin.
            exdates: vec![berlin(9, 9), utc(2026, 3, 16, 8)],
            ..standup()
        };
        let moved = ICalEvent {
            recurrence_id: Some(berlin(23, 9)),
            summary: Some("Standup (long)".into()),
            dtstart: Some(berlin(23, 10)),
            dtend: Some(berlin(23, 11)),
            location: Some("Room 9".into()),
            ..ICalEvent::new("standup@example.com")
        };
        let m = event_to_block(&ev, &[&moved], Some(stream())).expect("mapped");
        assert!(m.notices.is_empty(), "{:?}", m.notices);
        let rule = m.fields.rrule.as_ref().expect("a rule");
        assert_eq!(rule.to_rfc5545().split(';').next(), Some("FREQ=WEEKLY"));
        assert!(rule.until.is_some());

        let x = &m.fields.exceptions;
        assert_eq!(
            x.keys().map(String::as_str).collect::<Vec<_>>(),
            ["2026-03-09T09:00", "2026-03-16T09:00", "2026-03-23T09:00"]
        );
        assert!(x["2026-03-09T09:00"].cancelled);
        assert!(x["2026-03-16T09:00"].cancelled);
        let o = &x["2026-03-23T09:00"];
        assert!(!o.cancelled);
        assert_eq!(o.starts_at, Some(to_sunrise(&berlin(23, 10))));
        assert_eq!(o.ends_at, Some(to_sunrise(&berlin(23, 11))));
        assert_eq!(o.title.as_deref(), Some("Standup (long)"));
        assert_eq!(o.location.as_deref(), Some("Room 9"));

        let block = Block {
            id: imported_block_id("ics", &m.uid),
            ..block_from(&m, imported_block_id("ics", &m.uid))
        };
        block.validate_invariants(&TimeZone::UTC).expect("valid");
        let series = block.series().expect("a series");
        let window = (utc_instant(2026, 3, 1), utc_instant(2026, 4, 1));
        let keys: Vec<String> = sunrise_domain::expand_block(&series, window, &TimeZone::UTC)
            .unwrap()
            .into_iter()
            .map(|o| o.key)
            .collect();
        // 9 and 16 cancelled, 23 moved, 30 the last before UNTIL.
        assert_eq!(
            keys,
            ["2026-03-02T09:00", "2026-03-23T09:00", "2026-03-30T09:00"]
        );
    }

    /// An override that restates its occurrence changes nothing, so it
    /// writes no exception; one at a time the rule never produces is
    /// reported and the series kept.
    #[test]
    fn an_unchanged_override_writes_nothing_and_a_stray_one_is_reported() {
        let same = ICalEvent {
            recurrence_id: Some(berlin(9, 9)),
            summary: Some("Standup".into()),
            dtstart: Some(berlin(9, 9)),
            dtend: Some(ICalTime::Zoned {
                civil: civil::date(2026, 3, 9).at(9, 30, 0, 0),
                tz: "Europe/Berlin".into(),
            }),
            ..ICalEvent::new("standup@example.com")
        };
        let stray = ICalEvent {
            recurrence_id: Some(berlin(10, 9)),
            ..same.clone()
        };
        let m = event_to_block(&standup(), &[&same, &stray], Some(stream())).expect("mapped");
        assert!(m.fields.exceptions.is_empty(), "{:?}", m.fields.exceptions);
        assert!(m.fields.rrule.is_some());
        assert_eq!(m.notices.len(), 1, "{:?}", m.notices);
        assert_eq!(m.notices[0].code, NoticeCode::Skipped);
        assert!(m.notices[0].detail.contains("names no occurrence"));
    }

    /// A UTC start is wall clock in `UTC`, which names the same instants.
    #[test]
    fn a_utc_recurring_event_is_anchored_in_utc() {
        let ev = ICalEvent {
            rrule: Some("FREQ=DAILY;COUNT=3".into()),
            exdates: vec![utc(2026, 3, 2, 14)],
            ..event()
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert!(m.notices.is_empty(), "{:?}", m.notices);
        assert_eq!(
            m.draft.starts_at,
            SunriseTime::zoned(civil::date(2026, 3, 1).at(14, 0, 0, 0), "UTC")
        );
        assert_eq!(
            m.draft.starts_at.resolve_in(&TimeZone::UTC),
            to_sunrise(&utc(2026, 3, 1, 14)).resolve_in(&TimeZone::UTC)
        );
        assert!(m.fields.exceptions["2026-03-02T14:00"].cancelled);
    }

    /// What a Block cannot recur is reported, and the event imports as its
    /// first occurrence: a rule outside the engine subset, and a whole-day
    /// event. Its `EXDATE`s and overrides go with the rule.
    #[test]
    fn a_rule_a_block_cannot_hold_imports_the_first_occurrence_and_says_so() {
        for (ev, why) in [
            (
                ICalEvent {
                    rrule: Some("FREQ=HOURLY".into()),
                    ..standup()
                },
                "outside the rules",
            ),
            (
                ICalEvent {
                    rrule: Some("FREQ=YEARLY;BYWEEKNO=20".into()),
                    ..standup()
                },
                "BYWEEKNO",
            ),
            (
                ICalEvent {
                    dtstart: Some(ICalTime::Date(civil::date(2026, 3, 2))),
                    dtend: None,
                    ..standup()
                },
                "whole-day",
            ),
        ] {
            let ev = ICalEvent {
                exdates: vec![berlin(9, 9)],
                ..ev
            };
            let o = ICalEvent {
                recurrence_id: Some(berlin(16, 9)),
                dtstart: Some(berlin(16, 10)),
                dtend: Some(berlin(16, 11)),
                ..ICalEvent::new("standup@example.com")
            };
            let m = event_to_block(&ev, &[&o], Some(stream())).expect("mapped");
            assert_eq!(m.fields.rrule, None);
            assert!(m.fields.exceptions.is_empty());
            let details: Vec<&str> = m.notices.iter().map(|n| n.detail.as_str()).collect();
            assert_eq!(details.len(), 3, "{details:?}");
            assert!(
                details[0].starts_with("RRULE") && details[0].contains(why),
                "{details:?}"
            );
            assert!(details[1].starts_with("EXDATE"), "{details:?}");
            assert!(details[2].contains("override"), "{details:?}");
        }
    }

    /// An override whose UID has no recurring event in the file is the
    /// event; a second one of the same UID joins it, and is reported.
    #[test]
    fn overrides_group_under_the_event_of_their_uid() {
        let master = standup();
        let o1 = ICalEvent {
            recurrence_id: Some(berlin(9, 9)),
            ..ICalEvent::new("standup@example.com")
        };
        let orphan = ICalEvent {
            recurrence_id: Some(berlin(9, 9)),
            dtstart: Some(berlin(9, 9)),
            dtend: Some(berlin(9, 10)),
            ..ICalEvent::new("lone@example.com")
        };
        let orphan2 = ICalEvent {
            recurrence_id: Some(berlin(16, 9)),
            ..orphan.clone()
        };
        // The override comes before its event in the file.
        let events = [o1.clone(), orphan.clone(), master.clone(), orphan2.clone()];
        let groups = group_events(&events);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].event, &orphan);
        assert_eq!(groups[0].overrides, [&orphan2]);
        assert_eq!(groups[1].event, &master);
        assert_eq!(groups[1].overrides, [&o1]);

        let m =
            event_to_block(groups[0].event, &groups[0].overrides, Some(stream())).expect("mapped");
        assert_eq!(m.fields.rrule, None);
        assert_eq!(m.notices.len(), 2, "{:?}", m.notices);
        assert!(m.notices[0].detail.contains("no recurring event"));
        assert_eq!(m.notices[1].code, NoticeCode::Skipped);
    }

    /// Export writes the series back: the rule, an `EXDATE` per cancelled
    /// occurrence and an override per changed one, which import reads back
    /// onto the same series.
    #[test]
    fn a_recurring_block_exports_and_re_imports_as_the_same_series() {
        let ev = ICalEvent {
            exdates: vec![berlin(9, 9)],
            location: Some("Room 3".into()),
            description: Some("Agenda".into()),
            ..standup()
        };
        let moved = ICalEvent {
            recurrence_id: Some(berlin(23, 9)),
            dtstart: Some(berlin(23, 10)),
            dtend: Some(berlin(23, 11)),
            summary: Some("Standup (moved)".into()),
            ..ICalEvent::new("standup@example.com")
        };
        let m = event_to_block(&ev, &[&moved], Some(stream())).expect("mapped");
        let block = block_from(&m, imported_block_id("ics", &m.uid));

        let events = block_to_events(&block, block.title.as_deref());
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].uid, "standup@example.com");
        assert_eq!(events[0].location.as_deref(), Some("Room 3"));
        assert_eq!(events[0].description.as_deref(), Some("Agenda"));
        assert_eq!(events[0].exdates, [berlin(9, 9)]);
        assert_eq!(
            events[0].rrule.as_deref(),
            Some("FREQ=WEEKLY;BYDAY=MO;UNTIL=20260330T080000Z")
        );
        assert_eq!(events[1].recurrence_id, Some(berlin(23, 9)));
        assert_eq!(events[1].location.as_deref(), Some("Room 3"));

        // Through the text, and back onto the same Block.
        let parsed = crate::ical::parse(&crate::ical::write(&events)).unwrap();
        let groups = group_events(&parsed.events);
        assert_eq!(groups.len(), 1);
        let back =
            event_to_block(groups[0].event, &groups[0].overrides, Some(stream())).expect("mapped");
        assert!(back.notices.is_empty(), "{:?}", back.notices);
        assert_eq!(imported_block_id("ics", &back.uid), block.id);
        assert_eq!(back.draft.starts_at, m.draft.starts_at);
        assert_eq!(back.draft.ends_at, m.draft.ends_at);
        assert_eq!(back.fields, m.fields);
    }

    #[test]
    fn an_untitled_event_still_gets_a_label() {
        let ev = ICalEvent {
            summary: Some("   ".into()),
            ..event()
        };
        let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
        assert_eq!(m.draft.title.as_deref(), Some("(untitled event)"));
        m.draft.validate(&TimeZone::UTC).expect("valid");
    }

    fn block_from(m: &MappedEvent, id: EntityRef) -> Block {
        Block {
            id,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            stream_id: m.draft.stream_id,
            starts_at: m.draft.starts_at.clone(),
            ends_at: m.draft.ends_at.clone(),
            title: m.draft.title.clone(),
            title_track_task: false,
            tasks: BTreeSet::new(),
            occurrence_tasks: BTreeSet::new(),
            deleted: false,
            kind: None,
            rrule: m.fields.rrule.clone(),
            exceptions: m.fields.exceptions.clone(),
            split_from: None,
            location: m.fields.location.clone(),
            notes: m.fields.notes.clone(),
            source: None,
            external_id: m.fields.external_id.clone(),
            unknown: Unknowns::new(),
        }
    }

    fn utc_instant(y: i16, m: i8, d: i8) -> Timestamp {
        civil::date(y, m, d)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp()
    }

    /// Export → import is the identity on the Block, which is what stops a
    /// user's own `.ics` from doubling their calendar when they read it back.
    #[test]
    fn a_block_exported_and_re_imported_maps_to_the_same_block() {
        let m = event_to_block(&event(), &[], Some(stream())).expect("mapped");
        let id = imported_block_id("ics", &m.uid);
        let block = block_from(&m, id);

        // An imported block exports under the UID it was imported with, so
        // the same source finds it again.
        let exported = block_to_event(&block, block.title.as_deref());
        assert_eq!(exported.uid, "ev1@example.com");
        assert_eq!(imported_block_id("ics", &exported.uid), id);

        // A block made in Sunrise has no foreign id and exports under its
        // own: under any source, since the id is in the UID, not the hash.
        let own = Block {
            external_id: None,
            ..block.clone()
        };
        let exported_own = block_to_event(&own, own.title.as_deref());
        assert_eq!(imported_block_id("other", &exported_own.uid), id);

        let back = event_to_block(&exported, &[], Some(stream())).expect("mapped");
        assert_eq!(back.draft.starts_at, m.draft.starts_at);
        assert_eq!(back.draft.ends_at, m.draft.ends_at);
        assert_eq!(back.draft.title, m.draft.title);
    }

    #[test]
    fn export_uses_the_resolved_title_not_the_stored_one() {
        let m = event_to_block(&event(), &[], Some(stream())).expect("mapped");
        let block = block_from(&m, imported_block_id("ics", &m.uid));
        let ev = block_to_event(&block, Some("What the grid shows"));
        assert_eq!(ev.summary.as_deref(), Some("What the grid shows"));
    }
}
