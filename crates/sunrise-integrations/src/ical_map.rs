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
//! # What a Block cannot hold
//!
//! `DESCRIPTION`, `LOCATION` and `RRULE` are parsed and then reported as
//! dropped: `docs/02-domain/time-blocks.md` §"Specified but not modelled"
//! lists `notes`, `location` and `rrule` as fields that have not landed. They
//! are reported per event rather than dropped in silence, and adding them
//! later is a schema change, not a change here.

use crate::ical::{ICalEvent, ICalNotice, ICalTime, NoticeCode};
use jiff::{civil, tz::TimeZone, Span};
use sunrise_domain::{block_uid, Block, BlockDraft, SunriseTime};
use sunrise_id::EntityRef;

/// A `VEVENT` lowered to something the core can be handed.
#[derive(Debug, Clone)]
pub struct MappedEvent {
    /// The event's `UID`, which is the import key.
    pub uid: String,
    /// The Block to write.
    pub draft: BlockDraft,
    /// What the Block could not carry.
    pub notices: Vec<ICalNotice>,
}

/// Lower one `VEVENT` onto a [`BlockDraft`] in `stream_id`.
///
/// # Errors
///
/// An [`ICalNotice`] describing why this event has no Block form — a missing
/// `DTSTART`, a zero-length event, or an end that is not after its start. The
/// caller reports it and moves on to the next event: one unusable event must
/// not fail the whole import.
pub fn event_to_block(ev: &ICalEvent, stream_id: EntityRef) -> Result<MappedEvent, ICalNotice> {
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
    if ends_at.index_ms() <= starts_at.index_ms() {
        return Err(skip(format!(
            "DTEND is not after DTSTART ({} .. {})",
            starts_at.index_ms(),
            ends_at.index_ms()
        )));
    }

    for (name, why) in [
        (
            ev.description.as_ref().map(|_| "DESCRIPTION"),
            "a Block has no notes field in this schema",
        ),
        (
            ev.location.as_ref().map(|_| "LOCATION"),
            "a Block has no location field in this schema",
        ),
        (
            ev.rrule.as_ref().map(|_| "RRULE"),
            "recurring Blocks are not modelled; only the first occurrence was imported",
        ),
    ] {
        if let Some(name) = name {
            notices.push(ICalNotice {
                code: NoticeCode::UnmappedProperty,
                uid: uid(),
                detail: format!("{name} was dropped: {why}"),
            });
        }
    }

    // A Block with no bound Task needs a title of its own, and an untitled
    // event would otherwise produce an unlabelled bar on the calendar.
    let title = ev
        .summary
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map_or_else(|| "(untitled event)".to_string(), ToString::to_string);

    Ok(MappedEvent {
        uid: ev.uid.clone(),
        draft: BlockDraft {
            stream_id,
            starts_at,
            ends_at,
            title: Some(title),
            // An imported Block's title is the calendar's, not a shadow copy
            // of some task's, so tracking would be wrong even if a task were
            // later bound to it.
            title_track_task: false,
            tasks: Vec::new(),
        },
        notices,
    })
}

/// Raise a Block back to a `VEVENT`.
///
/// `title` is the *resolved* title (`sunrise_core::queries::BlockRow::title`),
/// not the stored one: a Block that tracks its bound Task's title should
/// export what a reader would see.
#[must_use]
pub fn block_to_event(block: &Block, title: Option<&str>) -> ICalEvent {
    ICalEvent {
        // The Block's own id, so re-importing this file finds this Block
        // rather than making a second one. See `sunrise_domain::import`.
        uid: block_uid(block.id),
        summary: title.map(ToString::to_string),
        dtstart: Some(from_sunrise(&block.starts_at)),
        dtend: Some(from_sunrise(&block.ends_at)),
        // Nothing in the Block schema backs these; exporting an empty one
        // would be inventing content.
        duration: None,
        description: None,
        location: None,
        rrule: None,
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

/// Domain time → iCalendar time. The exact inverse of [`to_sunrise`].
fn from_sunrise(t: &SunriseTime) -> ICalTime {
    match t {
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
        let m = event_to_block(&event(), stream()).expect("mapped");
        assert_eq!(m.uid, "ev1@example.com");
        assert_eq!(m.draft.title.as_deref(), Some("Coffee with Alice"));
        assert_eq!(m.draft.stream_id, stream());
        assert!(m.draft.tasks.is_empty());
        assert!(!m.draft.title_track_task);
        m.draft.validate().expect("a mapped draft is valid");
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
        let m = event_to_block(&ev, stream()).expect("mapped");
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
        let err = event_to_block(&ev, stream()).expect_err("no end");
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
        let m = event_to_block(&ev, stream()).expect("mapped");
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
        let m = event_to_block(&ev, stream()).expect("mapped");
        assert_eq!(m.draft.ends_at, to_sunrise(&utc(2026, 3, 2, 9)));
    }

    #[test]
    fn dtend_wins_over_duration_and_the_conflict_is_reported() {
        let ev = ICalEvent {
            duration: Some("PT9H".into()),
            ..event()
        };
        let m = event_to_block(&ev, stream()).expect("mapped");
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
        assert!(event_to_block(&ev, stream()).is_err());
    }

    #[test]
    fn an_inverted_range_is_refused() {
        let ev = ICalEvent {
            dtend: Some(utc(2026, 3, 1, 13)),
            ..event()
        };
        let err = event_to_block(&ev, stream()).expect_err("inverted");
        assert!(err.detail.contains("not after"), "{}", err.detail);
    }

    #[test]
    fn content_a_block_cannot_hold_is_reported_per_event() {
        let ev = ICalEvent {
            description: Some("Long notes".into()),
            location: Some("Room 3".into()),
            rrule: Some("FREQ=WEEKLY".into()),
            ..event()
        };
        let m = event_to_block(&ev, stream()).expect("mapped");
        let details: Vec<&str> = m.notices.iter().map(|n| n.detail.as_str()).collect();
        assert_eq!(details.len(), 3, "{details:?}");
        assert!(details.iter().any(|d| d.starts_with("DESCRIPTION")));
        assert!(details.iter().any(|d| d.starts_with("LOCATION")));
        assert!(details.iter().any(|d| d.starts_with("RRULE")));
    }

    #[test]
    fn an_untitled_event_still_gets_a_label() {
        let ev = ICalEvent {
            summary: Some("   ".into()),
            ..event()
        };
        let m = event_to_block(&ev, stream()).expect("mapped");
        assert_eq!(m.draft.title.as_deref(), Some("(untitled event)"));
        m.draft.validate().expect("valid");
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
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    /// Export → import is the identity on the Block, which is what stops a
    /// user's own `.ics` from doubling their calendar when they read it back.
    #[test]
    fn a_block_exported_and_re_imported_maps_to_the_same_block() {
        let m = event_to_block(&event(), stream()).expect("mapped");
        let id = imported_block_id("ics", &m.uid);
        let block = block_from(&m, id);

        let exported = block_to_event(&block, block.title.as_deref());
        assert_eq!(imported_block_id("ics", &exported.uid), id);
        // Under a different source too: the id is in the UID, not the hash.
        assert_eq!(imported_block_id("other", &exported.uid), id);

        let back = event_to_block(&exported, stream()).expect("mapped");
        assert_eq!(back.draft.starts_at, m.draft.starts_at);
        assert_eq!(back.draft.ends_at, m.draft.ends_at);
        assert_eq!(back.draft.title, m.draft.title);
    }

    #[test]
    fn export_uses_the_resolved_title_not_the_stored_one() {
        let m = event_to_block(&event(), stream()).expect("mapped");
        let block = block_from(&m, imported_block_id("ics", &m.uid));
        let ev = block_to_event(&block, Some("What the grid shows"));
        assert_eq!(ev.summary.as_deref(), Some("What the grid shows"));
    }
}
