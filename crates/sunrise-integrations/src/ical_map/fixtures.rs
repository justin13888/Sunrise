//! Events and Blocks the mapping's unit tests share.

use super::MappedEvent;
use crate::ical::{ICalEvent, ICalTime};
use jiff::{civil, tz::TimeZone, Timestamp};
use std::collections::BTreeSet;
use sunrise_domain::{Block, Unknowns};
use sunrise_id::{EntityKind, EntityRef};

pub(super) fn stream() -> EntityRef {
    EntityRef::new(EntityKind::Stream, [7u8; 16])
}

pub(super) fn utc(y: i16, m: i8, d: i8, h: i8) -> ICalTime {
    ICalTime::Utc(
        civil::date(y, m, d)
            .at(h, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp(),
    )
}

pub(super) fn event() -> ICalEvent {
    ICalEvent {
        summary: Some("Coffee with Alice".into()),
        dtstart: Some(utc(2026, 3, 1, 14)),
        dtend: Some(utc(2026, 3, 1, 15)),
        ..ICalEvent::new("ev1@example.com")
    }
}

pub(super) fn berlin(d: i8, h: i8) -> ICalTime {
    ICalTime::Zoned {
        civil: civil::date(2026, 3, d).at(h, 0, 0, 0),
        tz: "Europe/Berlin".into(),
    }
}

/// A weekly standup, Mondays 09:00–09:30 Berlin from 2026-03-02.
pub(super) fn standup() -> ICalEvent {
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

/// The Block an import of `m` writes under `id`.
pub(super) fn block_from(m: &MappedEvent, id: EntityRef) -> Block {
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

pub(super) fn utc_instant(y: i16, m: i8, d: i8) -> Timestamp {
    civil::date(y, m, d)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp()
}
