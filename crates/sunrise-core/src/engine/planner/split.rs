//! A planner drag of this and the following events of a recurring block
//! (`docs/08-features/planner.md` §Recurring blocks, #1324): the series split
//! at the dragged event and the continuation moved, and the undo that joins
//! the series back.

use super::{
    occurrence_at, place_block, InverseRow, PlanError, PlanField, PlanOccurrence, PlanPosition,
    Written,
};
use crate::engine::block::{move_anchor, read_block, split_series, SeriesSplit};
use crate::engine::ids::ms_to_ts;
use crate::engine::EngineError;
use rusqlite::Transaction;
use std::collections::BTreeMap;
use sunrise_domain::planner::OccurrenceScope;
use sunrise_domain::{Block, RRule, SunriseTime};
use sunrise_id::{EntityKind, EntityRef};

/// What a planner move of one block writes.
pub(super) enum Placed {
    /// The block, moved.
    One(Box<Block>),
    /// A drag of this and the following events: the series split at the
    /// dragged occurrence, the continuation moved, and where the
    /// continuation's anchor stood before the move.
    Split {
        split: Box<SeriesSplit>,
        from: (SunriseTime, SunriseTime),
    },
}

/// Write a planner move of `block` to `[start, end)`, with the position it
/// moves from, `before`, as the commit writes it at `now`: as
/// [`place_block`] does, and for a drag of this and the following events
/// ([`OccurrenceScope::ThisAndFollowing`]) as an edit of "this and future"
/// events does, the series split at the occurrence ([`split_series`]) and the
/// block that continues it moved as a drag of every event moves a series. At
/// the series' first occurrence nothing is left before it, and the drag
/// reaches every event.
///
/// # Errors
/// As [`place_block`]; and [`PlanError::Invalid`] where the series was split
/// at the occurrence already and the block that continues it is live.
#[allow(
    clippy::too_many_arguments,
    reason = "place_block's arguments, and the connection and time a split reads"
)]
pub(super) fn place_move(
    conn: &rusqlite::Connection,
    mut block: Block,
    occurrence: Option<&PlanOccurrence>,
    before: &PlanPosition,
    start: SunriseTime,
    end: SunriseTime,
    now: jiff::Timestamp,
    tz: &jiff::tz::TimeZone,
) -> Result<Placed, PlanError> {
    let invalid = |e: EngineError| match e {
        EngineError::Invalid(why) | EngineError::NotFound(why) => PlanError::Invalid(why),
        other => PlanError::Engine(other),
    };
    let Some(o) = occurrence.filter(|o| o.scope == OccurrenceScope::ThisAndFollowing) else {
        place_block(&mut block, occurrence, before, start, end, tz)?;
        return Ok(Placed::One(Box::new(block)));
    };
    let every = PlanOccurrence {
        key: o.key.clone(),
        scope: OccurrenceScope::All,
    };
    occurrence_at(&block, &o.key, tz)?;
    let Some(mut split) = split_series(conn, &block, &o.key, now, tz).map_err(invalid)? else {
        place_block(&mut block, Some(&every), before, start, end, tz)?;
        return Ok(Placed::One(Box::new(block)));
    };
    if split.existing.as_ref().is_some_and(|b| !b.deleted) {
        return Err(PlanError::Invalid(format!(
            "block {} was split at {} already",
            block.id, o.key
        )));
    }
    let from = (
        split.continuation.starts_at.clone(),
        split.continuation.ends_at.clone(),
    );
    place_block(
        &mut split.continuation,
        Some(&every),
        before,
        start,
        end,
        tz,
    )?;
    Ok(Placed::Split {
        split: Box::new(split),
        from,
    })
}

/// The rows of a commit's split of a recurring block's series, taken out of
/// `by_item`: the block the split ended, whose rule's `UNTIL` row marks it,
/// then that row and the anchor rows of the block that continued the series.
/// A commit splits at most one series, and the continuation is the block of
/// its rows that the same series root was split into. `None` for a commit
/// that split nothing.
pub(super) fn split_rows(
    tx: &Transaction<'_>,
    by_item: &mut BTreeMap<EntityRef, Vec<InverseRow>>,
) -> Result<Option<(EntityRef, Vec<InverseRow>)>, EngineError> {
    let Some(ended) = by_item
        .iter()
        .find(|(_, rows)| rows.iter().any(|r| r.field.field == PlanField::Until))
        .map(|(item, _)| *item)
    else {
        return Ok(None);
    };
    let mut rows = by_item.remove(&ended).unwrap_or_default();
    let root = read_block(tx, ended.bytes())?.map(|b| b.split_from.unwrap_or(b.id));
    let mut continuation = None;
    for item in by_item.keys().filter(|i| i.kind() == EntityKind::Block) {
        let from = read_block(tx, item.bytes())?.and_then(|b| b.split_from);
        if from.is_some() && from == root {
            continuation = Some(*item);
            break;
        }
    }
    if let Some(next) = continuation {
        rows.extend(by_item.remove(&next).unwrap_or_default());
    }
    Ok(Some((ended, rows)))
}

/// A split joined back: the block the split ended, as it stands and with
/// its series restored, and the block that continued it, tombstoned.
pub(super) struct Joined {
    pub(super) was: Block,
    pub(super) ended: Block,
    pub(super) continuation: Block,
}

/// Join back the series a drag of this and the following events split, from
/// its rows ([`split_rows`]): the continuation's anchor goes back to where
/// the split put it, which carries its exceptions and occurrence bindings
/// back to the keys they had in the series; the ended block's rule gets its
/// `UNTIL` back and takes those exceptions and bindings; and the
/// continuation is tombstoned.
///
/// `None`, leaving both as they are, where any row is no longer held (either
/// block written again since), either block is deleted, the continuation
/// was bound to a task as a whole since, its rule is no longer the one the
/// split gave it ([`given_by_split`]), or the joined series would be
/// invalid.
pub(super) fn join_split(
    tx: &Transaction<'_>,
    ended: EntityRef,
    rows: &[InverseRow],
    write_ms: u64,
    tz: &jiff::tz::TimeZone,
) -> Result<Option<Joined>, EngineError> {
    let until = rows
        .iter()
        .find(|r| r.item == ended && r.field.field == PlanField::Until);
    let anchor = |field| {
        rows.iter()
            .find(|r| r.item != ended && r.field == Written::plain(field))
    };
    let (Some(until), Some(start), Some(end)) = (
        until,
        anchor(PlanField::StartsAt),
        anchor(PlanField::EndsAt),
    ) else {
        return Ok(None);
    };
    if rows.len() != 3 || !rows.iter().all(InverseRow::restorable) {
        return Ok(None);
    }
    let live = |id: EntityRef| -> Result<Option<Block>, EngineError> {
        Ok(read_block(tx, id.bytes())?.filter(|b| !b.deleted))
    };
    let (Some(was), Some(next)) = (live(ended)?, live(start.item)?) else {
        return Ok(None);
    };
    if !next.tasks.is_empty() {
        return Ok(None);
    }
    let (Some(start), Some(end)) = (start.prior.clone(), end.prior.clone()) else {
        return Ok(None);
    };
    let now = ms_to_ts(write_ms.cast_signed());
    // The continuation where the split put it: its exceptions and bindings
    // under the keys the series gave them.
    let mut back = next.clone();
    let moved_to = back.starts_at.clone();
    let series = back.series();
    back.starts_at = start;
    back.ends_at = end;
    if move_anchor(&mut back, &moved_to, series, tz).is_err() {
        return Ok(None);
    }
    let mut joined = was.clone();
    let Some(rule) = joined.rrule.as_mut() else {
        return Ok(None);
    };
    rule.until.clone_from(&until.prior);
    if !given_by_split(rule, back.rrule.as_ref()) {
        return Ok(None);
    }
    joined.exceptions.extend(back.exceptions);
    joined.occurrence_tasks.extend(back.occurrence_tasks);
    joined.updated_at = now;
    if joined.validate_invariants(tz).is_err()
        || joined.series().is_some_and(|s| s.validate().is_err())
    {
        return Ok(None);
    }
    let mut continuation = next;
    continuation.deleted = true;
    continuation.updated_at = now;
    Ok(Some(Joined {
        was,
        ended: joined,
        continuation,
    }))
}

/// Whether `continuation` is still the rule a split of the series whose rule
/// is `series` gave the block that continues it ([`split_series`]): the
/// series' own rule, its `UNTIL` included, with a `COUNT` no larger than the
/// series' (the split reduces it by the events before the key).
///
/// A later split of the continuation always writes its `UNTIL`, so this is
/// what refuses to join a series whose continuation was split again since:
/// a later drag of this and the following events of the continuation
/// renews its anchor and leaves it as it was, so the anchor's rows alone
/// still read as held, and joining would restore the whole series beside
/// the second continuation, every event from its key on shown twice.
fn given_by_split(series: &RRule, continuation: Option<&RRule>) -> bool {
    let Some(continuation) = continuation else {
        return false;
    };
    let counted = match (series.count, continuation.count) {
        (None, None) => true,
        (Some(series), Some(continuation)) => continuation <= series,
        _ => false,
    };
    counted
        && RRule {
            count: series.count,
            ..continuation.clone()
        } == *series
}
