//! A recurring `VEVENT` as one Block series: its `RRULE` as the Block's
//! rule, each `EXDATE` a cancelled exception and each `RECURRENCE-ID`
//! override an exception holding what it changes. See the parent module for
//! the mapping rules.

use super::{
    bounded, location_of, note_from, refused_order, resolve_end, same_time, to_sunrise, unmapped,
};
use crate::ical::{bare_time_value, parse_bare_time, ICalEvent, ICalNotice, ICalTime, NoticeCode};
use jiff::tz::TimeZone;
use std::collections::BTreeMap;
use sunrise_core::ImportedFields;
use sunrise_domain::routine_gen::parse_occurrence_key;
use sunrise_domain::rrule::RRule;
use sunrise_domain::{BlockDraft, BlockException, BlockSeries, SunriseTime, MAX_BLOCK_TITLE_LEN};

/// The series `ev` states, when a Block can hold it; otherwise `None`, with
/// why reported for a rule a Block cannot hold and for an orphan override.
pub(super) fn recurrence_of(
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
                     as a single event{}",
                    bare_time_value(rid),
                    if ev.cancelled {
                        ", though its STATUS:CANCELLED says it does not take place"
                    } else {
                        ""
                    }
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
pub(super) fn exceptions_of(
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
pub(super) fn drop_recurrence(
    ev: &ICalEvent,
    overrides: &[&ICalEvent],
    notices: &mut Vec<ICalNotice>,
) {
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

/// The series an event with rule `raw` states, its anchor the draft's
/// bounds read as wall clock.
fn series_of(raw: &str, draft: &BlockDraft) -> Result<BlockSeries, String> {
    let mut rrule = parse_rrule(raw)?;
    let (starts_at, ends_at) = wall_clock_anchor(&draft.starts_at, &draft.ends_at)?;
    rrule.until = rrule.until.map(|u| civil_until(u, &starts_at));
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

/// An iCalendar `UNTIL` in the anchor's civil frame, the kind an
/// `RRule.until` holds (`docs/10-cross-cutting/time.md` §Which kind each
/// field uses): floating wall clock, or a whole date as it is.
///
/// A UTC or zoned value beside a zoned anchor is the wall clock it falls on
/// in the anchor's zone. Beside a floating anchor, which has no zone, a
/// zoned value bounds its own wall clock and a UTC one its UTC wall clock,
/// as the engine reads them (`routine_gen::civil_frame_rule`).
fn civil_until(until: SunriseTime, anchor: &SunriseTime) -> SunriseTime {
    let zone = match anchor {
        SunriseTime::Zoned { tz, .. } => TimeZone::get(tz).ok(),
        _ => None,
    };
    match (&until, zone) {
        (SunriseTime::Zoned { .. } | SunriseTime::Instant { .. }, Some(zone)) => until
            .resolve_in(&TimeZone::UTC)
            .map_or(until, |at| SunriseTime::floating(zone.to_datetime(at))),
        (SunriseTime::Zoned { civil, .. }, None) => SunriseTime::floating(*civil),
        (SunriseTime::Instant { at }, None) => {
            SunriseTime::floating(TimeZone::UTC.to_datetime(*at))
        }
        _ => until,
    }
}

/// The rule's text as an `RRULE` value, its `UNTIL` in the iCalendar time
/// grammar. Beside a zoned `anchor` a time is the UTC instant it names in
/// the anchor's zone (RFC 5545 §3.3.10 wants UTC beside a zoned start);
/// beside a floating one a floating `UNTIL` stays floating; a whole date
/// stays a date.
pub(super) fn rrule_value(rule: &RRule, anchor: &SunriseTime) -> String {
    let mut text = RRule {
        until: None,
        ..rule.clone()
    }
    .to_rfc5545();
    let zone = match anchor {
        SunriseTime::Zoned { tz, .. } => TimeZone::get(tz).ok(),
        _ => None,
    };
    let until = match (&rule.until, zone) {
        (Some(SunriseTime::AllDay { date }), _) => Some(ICalTime::Date(*date)),
        (Some(SunriseTime::Floating { civil }), Some(zone)) => zone
            .to_ambiguous_timestamp(*civil)
            .compatible()
            .ok()
            .map(ICalTime::Utc),
        (Some(SunriseTime::Floating { civil }), None) => Some(ICalTime::Floating(*civil)),
        (Some(t), _) => t.resolve_in(&TimeZone::UTC).map(ICalTime::Utc),
        (None, _) => None,
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
/// a cancelled one for a `STATUS:CANCELLED` override, otherwise the bounds
/// it moves and the title, location and notes it changes from the series'.
/// `None` when it changes nothing.
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
    if o.cancelled {
        return Ok(Some((key, BlockException::cancelled())));
    }
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

/// The start of occurrence `key` of `series`, of the anchor's kind, as an
/// `EXDATE` or `RECURRENCE-ID` names it.
pub(super) fn key_time(key: &str, series: &BlockSeries) -> Option<ICalTime> {
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

#[cfg(test)]
mod tests {
    use super::super::fixtures::{berlin, block_from, event, standup, stream, utc, utc_instant};
    use super::super::{block_to_events, event_to_block, group_events, to_sunrise};
    use super::*;
    use jiff::civil;
    use sunrise_domain::{imported_block_id, Block};

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
        // `UNTIL=20260330T080000Z` is 10:00 in Berlin (CEST), held as the
        // wall clock it falls on in the anchor's zone.
        assert_eq!(
            rule.until,
            Some(SunriseTime::floating(
                civil::date(2026, 3, 30).at(10, 0, 0, 0)
            ))
        );
        assert!(
            rrule_value(rule, &m.draft.starts_at).ends_with(";UNTIL=20260330T080000Z"),
            "exports as the UTC instant it was"
        );

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

    /// A `STATUS:CANCELLED` override cancels its occurrence, as an `EXDATE`
    /// does, whatever else it states; parsed from the file, so the parser's
    /// reading of `STATUS` is reached too.
    #[test]
    fn a_cancelled_override_is_a_cancelled_exception() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:standup@example.com\r\n\
                   DTSTART;TZID=Europe/Berlin:20260302T090000\r\n\
                   DTEND;TZID=Europe/Berlin:20260302T093000\r\n\
                   RRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20260330T080000Z\r\n\
                   SUMMARY:Standup\r\nEND:VEVENT\r\n\
                   BEGIN:VEVENT\r\nUID:standup@example.com\r\n\
                   RECURRENCE-ID;TZID=Europe/Berlin:20260309T090000\r\n\
                   DTSTART;TZID=Europe/Berlin:20260309T100000\r\n\
                   DTEND;TZID=Europe/Berlin:20260309T110000\r\n\
                   SUMMARY:Standup\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n\
                   END:VCALENDAR\r\n";
        let cal = crate::ical::parse(ics).expect("parses");
        assert!(cal.notices.is_empty(), "{:?}", cal.notices);
        let groups = group_events(&cal.events);
        assert_eq!(groups.len(), 1);
        let m =
            event_to_block(groups[0].event, &groups[0].overrides, Some(stream())).expect("mapped");
        assert!(m.notices.is_empty(), "{:?}", m.notices);
        assert_eq!(
            m.fields.exceptions,
            BTreeMap::from([("2026-03-09T09:00".to_owned(), BlockException::cancelled())])
        );
    }

    /// A cancelled override with no recurring event of its UID is imported
    /// as the single event it is, and the notice says it was cancelled.
    #[test]
    fn a_cancelled_orphan_override_is_reported_as_cancelled() {
        let orphan = ICalEvent {
            recurrence_id: Some(berlin(9, 9)),
            dtstart: Some(berlin(9, 9)),
            dtend: Some(berlin(9, 10)),
            cancelled: true,
            ..ICalEvent::new("lone@example.com")
        };
        let m = event_to_block(&orphan, &[], Some(stream())).expect("mapped");
        assert_eq!(m.notices.len(), 1, "{:?}", m.notices);
        assert!(
            m.notices[0].detail.contains("STATUS:CANCELLED"),
            "{:?}",
            m.notices
        );
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

    /// Beside a floating anchor an `UNTIL` stays wall clock: a floating one
    /// as written and exported so, a zoned one at its own wall clock, a UTC
    /// one at its UTC wall clock.
    #[test]
    fn until_beside_a_floating_anchor_is_wall_clock() {
        let at = |d: i8, h: i8| civil::date(2026, 3, d).at(h, 0, 0, 0);
        for (until, want) in [
            ("UNTIL=20260310T090000", at(10, 9)),
            ("UNTIL=20260310T090000Z", at(10, 9)),
        ] {
            let ev = ICalEvent {
                dtstart: Some(ICalTime::Floating(at(2, 9))),
                dtend: Some(ICalTime::Floating(at(2, 10))),
                rrule: Some(format!("FREQ=DAILY;{until}")),
                ..event()
            };
            let m = event_to_block(&ev, &[], Some(stream())).expect("mapped");
            assert!(m.notices.is_empty(), "{:?}", m.notices);
            let rule = m.fields.rrule.as_ref().expect("a rule");
            assert_eq!(rule.until, Some(SunriseTime::floating(want)), "{until}");
            assert!(
                rrule_value(rule, &m.draft.starts_at).ends_with(";UNTIL=20260310T090000"),
                "{until}"
            );
        }
        assert_eq!(
            civil_until(to_sunrise(&berlin(10, 9)), &SunriseTime::floating(at(2, 9))),
            SunriseTime::floating(at(10, 9))
        );
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
}
