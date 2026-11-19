//! A Block raised back to the `VEVENT`s that state it.

use super::recurrence::{key_time, rrule_value};
use super::{description_of, from_sunrise};
use crate::ical::ICalEvent;
use sunrise_domain::{block_uid, Block};

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
        rrule: block
            .rrule
            .as_ref()
            .map(|r| rrule_value(r, &block.starts_at)),
        exdates,
        recurrence_id: None,
        cancelled: false,
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
                // A cancelled occurrence is an `EXDATE` on the series.
                cancelled: false,
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

#[cfg(test)]
mod tests {
    use super::super::event_to_block;
    use super::super::fixtures::{block_from, event, stream};
    use super::*;
    use sunrise_domain::imported_block_id;

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
