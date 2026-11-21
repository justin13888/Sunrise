//! The iCalendar ⇄ Sunrise domain mapping.
//!
//! Pure: no vault, no clock, no id generation. Everything here is a function
//! of its arguments, which is what lets the mapping be pinned by unit tests
//! rather than only by an end-to-end import.
//!
//! # `VEVENT` is a [`sunrise_domain::Block`], not a [`sunrise_domain::Task`]
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
//!
//! This file maps one event's range and fields; the `recurrence` submodule
//! maps its series and exceptions, and [`block_to_events`] raises a Block
//! back to events.

mod export;
#[cfg(test)]
mod fixtures;
mod recurrence;

pub use export::{block_to_event, block_to_events};

use crate::ical::{ICalEvent, ICalNotice, ICalTime, NoticeCode};
use jiff::{civil, tz::TimeZone, Span};
use recurrence::{drop_recurrence, exceptions_of, recurrence_of};
use std::collections::BTreeMap;
use sunrise_core::ImportedFields;
use sunrise_domain::validation::MAX_BLOCK_LOCATION_LEN;
use sunrise_domain::{
    decode_note_body, encode_note_body, note_body_from_plain_text, note_body_to_plain_text,
    BlockDraft, Inline, NoteBlock, NoteBody, SunriseTime,
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
/// as the CLI writes plain text into a note ([`note_body_from_plain_text`]).
/// `None` when it holds no text.
fn note_from(text: &str) -> Option<NoteBody> {
    let blocks = note_body_from_plain_text(text);
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
    use super::fixtures::{event, stream, utc};
    use super::*;

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

    /// The paragraphs a `DESCRIPTION` imports as.
    fn paragraphs(text: &str) -> Option<Vec<String>> {
        note_from(text).map(|body| {
            decode_note_body(&body)
                .blocks
                .iter()
                .map(|b| match b {
                    NoteBlock::Paragraph { inline } => {
                        inline.iter().map(Inline::plain_text).collect()
                    }
                    other => panic!("not a paragraph: {other:?}"),
                })
                .collect()
        })
    }

    /// A `DESCRIPTION` splits into paragraphs at blank lines: `\r\n` reads as
    /// `\n`, a run of blank lines is one break, single line breaks stay inside
    /// a paragraph, and whitespace around each paragraph goes.
    #[test]
    fn a_description_is_one_paragraph_per_blank_line_separated_run() {
        assert_eq!(
            paragraphs("  Agenda\r\n\r\n\r\nBring\nthe deck  \n\n"),
            Some(vec!["Agenda".to_owned(), "Bring\nthe deck".to_owned()])
        );
    }

    /// A line holding only whitespace is blank, so it separates paragraphs
    /// as an empty line does.
    #[test]
    fn a_whitespace_only_line_separates_paragraphs() {
        assert_eq!(
            paragraphs("Agenda\n  \t\nBring the deck"),
            Some(vec!["Agenda".to_owned(), "Bring the deck".to_owned()])
        );
    }

    /// A `DESCRIPTION` with no words is no note.
    #[test]
    fn a_description_with_no_words_is_no_note() {
        assert_eq!(paragraphs(""), None);
        assert_eq!(paragraphs(" \r\n\n \t "), None);
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
}
