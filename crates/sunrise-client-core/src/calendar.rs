//! The calendar grid's arithmetic: which civil days it draws, where blocks go
//! on them, what a drag does, and the Resolve menu's **Merge**.
//!
//! A grid is drawn in epoch milliseconds, and every decision behind it is a
//! civil one: a day is 23 or 25 hours across a DST change, a week starts on
//! the `week_start` preference, a snap step counts from the day's own
//! midnight (so a half-hour zone snaps to :00 and :15), and a block dragged
//! an hour keeps the kind of time it was. The Apple app made each of these
//! decisions in Swift (`CalendarModel`, `BlockLayout`, `BlockDrag`); they are
//! here so every client draws the same grid.
//!
//! The overlap rule and the merged block's time kind stay the domain's
//! ([`sunrise_domain::overlaps`], [`sunrise_domain::merge_blocks`]).

use jiff::civil::{Date, Time};
use jiff::tz::TimeZone;
use jiff::Span;
use sunrise_core::{Command, Core, CoreError, Query, QueryResult};
use sunrise_domain::{Block, BlockDraft, BlockPatch, ValidationError, Weekday};
use sunrise_id::EntityRef;
use thiserror::Error;

use crate::civil::{instant, value_at, CivilError, TimeKind};
use crate::editor::{block_form, block_patch, EditorError};

/// The kind a block drawn on the grid, or made by dropping a task on it, is
/// written as unless the user picks another: 09:00 in this zone, named
/// (`docs/02-domain/time-blocks.md`). A one-off block is a commitment in a
/// place; a habit that follows the user is a routine (ADR-0066 §1).
pub const NEW_BLOCK_KIND: TimeKind = TimeKind::Zoned;

/// A dropped task's block is at least this long, in minutes, whatever the
/// snap step.
pub const MIN_DROP_MINUTES: i64 = 30;

/// Day or week.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarSpan {
    /// One civil day.
    Day,
    /// Seven civil days from the `week_start` preference.
    Week,
}

impl CalendarSpan {
    /// How many civil days the grid draws.
    #[must_use]
    pub const fn day_count(self) -> i32 {
        match self {
            Self::Day => 1,
            Self::Week => 7,
        }
    }
}

/// One civil day of the grid, as the half-open range `[start_ms, end_ms)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarDay {
    /// The date.
    pub date: Date,
    /// Its first instant in the reader's zone, epoch milliseconds.
    pub start_ms: i64,
    /// The next day's first instant: 23 or 25 hours on, across a DST change.
    pub end_ms: i64,
}

/// The days the grid draws for `span` around the instant `anchor_ms`.
///
/// A day grid draws the anchor's date; a week grid draws the seven days of
/// the week containing it that begins on `week_start`, which is the rule
/// `Query::WeekBlocks` uses ([`sunrise_domain::first_day_of_week`]), so the
/// columns and the blocks the query returned agree.
///
/// # Errors
/// [`CivilError`] when `anchor_ms` is no instant or a day falls off the
/// calendar.
pub fn calendar_days(
    anchor_ms: i64,
    span: CalendarSpan,
    week_start: &Weekday,
    reader: &TimeZone,
) -> Result<Vec<CalendarDay>, CivilError> {
    let anchor = instant(anchor_ms)?.to_zoned(reader.clone()).date();
    let first = match span {
        CalendarSpan::Day => anchor,
        CalendarSpan::Week => sunrise_domain::first_day_of_week(anchor, week_start),
    };
    (0..span.day_count())
        .map(|offset| {
            let date = add_days(first, offset)?;
            Ok(CalendarDay {
                date,
                start_ms: day_start_ms(date, reader)?,
                end_ms: day_start_ms(add_days(date, 1)?, reader)?,
            })
        })
        .collect()
}

/// The anchor moved by `direction` whole spans: civil days, so a week across
/// a DST change is seven days and not 168 hours, and the anchor keeps its
/// wall-clock time.
///
/// # Errors
/// [`CivilError`] when `anchor_ms` is no instant or the result falls off the
/// calendar.
pub fn step_anchor(
    anchor_ms: i64,
    span: CalendarSpan,
    direction: i32,
    reader: &TimeZone,
) -> Result<i64, CivilError> {
    let days = direction.saturating_mul(span.day_count());
    instant(anchor_ms)?
        .to_zoned(reader.clone())
        .checked_add(Span::new().days(days))
        .map(|z| z.timestamp().as_millisecond())
        .map_err(|_| CivilError::OutOfRange { ms: anchor_ms })
}

/// A new block's draft from a range on the grid, both bounds in `kind`.
///
/// # Errors
/// [`CivilError::OutOfRange`] for milliseconds no timestamp holds.
pub fn new_block_draft(
    from_ms: i64,
    to_ms: i64,
    kind: TimeKind,
    stream_id: EntityRef,
    title: Option<String>,
    tasks: Vec<EntityRef>,
    reader: &TimeZone,
) -> Result<BlockDraft, CivilError> {
    Ok(BlockDraft {
        stream_id,
        starts_at: value_at(from_ms, kind, reader)?,
        ends_at: value_at(to_ms, kind, reader)?,
        title: title.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()),
        title_track_task: false,
        tasks,
    })
}

/// The patch a drag that moved or resized `block` to `[start_ms, end_ms)`
/// makes.
///
/// Only a bound that moved is sent, and it keeps its kind: a floating block
/// dragged an hour later stays floating, and a zoned one stays in its own
/// zone. An all-day block dragged onto a time is given one, so both its
/// bounds become zoned in the reader's zone; so does a kind this build does
/// not know, which the drag explicitly replaces. These are the block
/// editor's rules ([`crate::editor::block_patch`]) with the form untouched
/// but for the two times.
///
/// # Errors
/// [`EditorError::EndNotAfterStart`] for an empty or backward range, and
/// [`EditorError::Time`] when a bound cannot be written.
pub fn moved_block_patch(
    block: &Block,
    start_ms: i64,
    end_ms: i64,
    reader: &TimeZone,
) -> Result<BlockPatch, EditorError> {
    let mut form = block_form(block, reader);
    form.starts_at_ms = start_ms;
    form.ends_at_ms = end_ms;
    block_patch(block, &form, reader)
}

/// A range on the grid, epoch milliseconds, half-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridRange {
    /// Start.
    pub start_ms: i64,
    /// End.
    pub end_ms: i64,
}

/// `ms` snapped to the nearest `minutes` boundary of the day that starts at
/// `day_start_ms`, half a step rounding up.
///
/// Relative to the day's start rather than the epoch, so a half-hour zone
/// (India, Newfoundland) snaps to :00 and :15 on screen, not :00 and :45.
/// `docs/07-clients/interaction-patterns.md` §Drag-and-drop UX tokens sets
/// the default at 15 minutes and the choices at {5, 10, 15, 30, 60}; a step
/// under one minute is one minute.
#[must_use]
pub fn snap(ms: i64, minutes: i64, day_start_ms: i64) -> i64 {
    let step = step_ms(minutes);
    let offset = ms.saturating_sub(day_start_ms);
    let down = offset.div_euclid(step) * step;
    let up = if offset.rem_euclid(step) >= step / 2 {
        step
    } else {
        0
    };
    day_start_ms.saturating_add(down).saturating_add(up)
}

/// The block a drag from `from_ms` to `to_ms` on one day's column draws:
/// both ends snapped, in order, and never shorter than one step — a flick
/// that snapped to one slot becomes one slot long rather than nothing.
#[must_use]
pub fn drawn_range(from_ms: i64, to_ms: i64, minutes: i64, day_start_ms: i64) -> GridRange {
    let a = snap(from_ms, minutes, day_start_ms);
    let b = snap(to_ms, minutes, day_start_ms);
    let start_ms = a.min(b);
    GridRange {
        start_ms,
        end_ms: a.max(b).max(start_ms.saturating_add(step_ms(minutes))),
    }
}

/// The block a task dropped at `at_ms` makes: from the snapped drop point,
/// one step or [`MIN_DROP_MINUTES`] long, whichever is longer.
#[must_use]
pub fn dropped_range(at_ms: i64, minutes: i64, day_start_ms: i64) -> GridRange {
    let start_ms = snap(at_ms, minutes, day_start_ms);
    GridRange {
        start_ms,
        end_ms: start_ms.saturating_add(step_ms(minutes.max(MIN_DROP_MINUTES))),
    }
}

/// Which end of a block a drag holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragMode {
    /// Both ends: the block keeps its length and changes when it is.
    Move,
    /// The end only: the block keeps its start and changes its length.
    ResizeEnd,
}

/// Where a block drawn at `range` ends up after a drag of `delta_ms` in
/// `mode` on the day `day`.
///
/// Snapped, and clamped inside the day: a block dragged off the bottom of a
/// column stops at the next midnight rather than becoming tomorrow's. A move
/// is clamped by its start with its length kept (clamping each end alone is
/// how a dragged block arrives three minutes long), and a resize never
/// shortens a block past one step. `day` is a [`CalendarDay`], so the day's
/// end is the next civil midnight; the Apple grid used `start + 24 h`, an
/// hour off on the two DST days.
#[must_use]
pub fn dragged_range(
    mode: DragMode,
    range: GridRange,
    delta_ms: i64,
    minutes: i64,
    day: &CalendarDay,
) -> GridRange {
    let step = step_ms(minutes);
    let snapped = |ms: i64| snap(ms, minutes, day.start_ms);
    match mode {
        DragMode::Move => {
            let length = step.max(range.end_ms.saturating_sub(range.start_ms));
            let latest = day.start_ms.max(day.end_ms.saturating_sub(length));
            let start_ms = snapped(range.start_ms.saturating_add(delta_ms))
                .max(day.start_ms)
                .min(latest);
            GridRange {
                start_ms,
                end_ms: start_ms.saturating_add(length),
            }
        }
        DragMode::ResizeEnd => {
            let lowest = range.start_ms.saturating_add(step).min(day.end_ms);
            GridRange {
                start_ms: range.start_ms,
                end_ms: snapped(range.end_ms.saturating_add(delta_ms))
                    .max(lowest)
                    .min(day.end_ms),
            }
        }
    }
}

/// One block placed on one day's column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedBlock {
    /// The block.
    pub id: EntityRef,
    /// Its start, clamped to the column, epoch milliseconds.
    pub start_ms: i64,
    /// Its end, clamped to the column.
    pub end_ms: i64,
    /// Which column it takes among the blocks it overlaps, from 0.
    pub lane: u32,
    /// How many columns its cluster of overlapping blocks needs.
    pub lane_count: u32,
}

/// Where `blocks` go on the column `day`.
///
/// Each bound is resolved in `reader`; a block with a bound that has no
/// instant there cannot be drawn and is left out, rather than drawn at a
/// stand-in time. A block is on the column when it overlaps it (half-open,
/// so one ending at midnight is not on the next day) and is clamped to it,
/// so one running in from yesterday is drawn from the top.
///
/// Overlapping blocks share the width: blocks are split into runs that
/// overlap **transitively** (A overlaps B and B overlaps C puts all three in
/// one run, even when A and C do not touch), and each takes the first column
/// whose last occupant has finished, so back-to-back blocks reuse a column.
/// Ordered by start, then id.
#[must_use]
pub fn place_blocks(blocks: &[Block], day: &CalendarDay, reader: &TimeZone) -> Vec<PlacedBlock> {
    let mut items: Vec<(EntityRef, i64, i64)> = blocks
        .iter()
        .filter_map(|b| {
            let start = b.starts_at.resolve_in(reader)?.as_millisecond();
            let end = b.ends_at.resolve_in(reader)?.as_millisecond();
            (end > day.start_ms && start < day.end_ms)
                .then(|| (b.id, start.max(day.start_ms), end.min(day.end_ms)))
        })
        .collect();
    items.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let mut placed = Vec::with_capacity(items.len());
    let mut cluster: Vec<(EntityRef, i64, i64)> = Vec::new();
    let mut cluster_end = i64::MIN;
    for item in items {
        if !cluster.is_empty() && item.1 >= cluster_end {
            pack(&cluster, &mut placed);
            cluster.clear();
        }
        cluster_end = if cluster.is_empty() {
            item.2
        } else {
            cluster_end.max(item.2)
        };
        cluster.push(item);
    }
    pack(&cluster, &mut placed);
    placed
}

/// Greedy column packing of one transitively-overlapping run.
fn pack(cluster: &[(EntityRef, i64, i64)], out: &mut Vec<PlacedBlock>) {
    let mut column_ends: Vec<i64> = Vec::new();
    let mut lanes = Vec::with_capacity(cluster.len());
    for &(_, start, end) in cluster {
        if let Some(free) = column_ends.iter().position(|e| *e <= start) {
            column_ends[free] = end;
            lanes.push(free);
        } else {
            column_ends.push(end);
            lanes.push(column_ends.len() - 1);
        }
    }
    let lane_count = u32::try_from(column_ends.len()).unwrap_or(u32::MAX);
    for (&(id, start_ms, end_ms), lane) in cluster.iter().zip(lanes) {
        out.push(PlacedBlock {
            id,
            start_ms,
            end_ms,
            lane: u32::try_from(lane).unwrap_or(u32::MAX),
            lane_count,
        });
    }
}

/// Why a merge did not happen, or happened in part.
#[derive(Debug, Error)]
pub enum MergeError {
    /// The union does not validate: in practice two or more bound tasks and
    /// no title on either block.
    #[error(transparent)]
    Invalid(#[from] ValidationError),
    /// A block named is not in the vault (deleted elsewhere, or another
    /// vault's id), or is not a block.
    #[error("no block {0}")]
    NotABlock(EntityRef),
    /// The vault refused a write or a read. `created` is the merged block
    /// when the create had already landed, so the caller can say that the
    /// union exists and an original is still standing.
    #[error("{source}")]
    Core {
        /// What the core said.
        source: CoreError,
        /// The merged block, when it was created before the failure.
        created: Option<EntityRef>,
    },
}

/// The commands that merge `a` and `b`, in the order they must be submitted:
/// create the union, then tombstone the two originals.
///
/// `docs/02-domain/time-blocks.md` §Conflicts defines merge as exactly those
/// three writes, and the core has no command that makes them one transaction
/// (nor a multi-command one). So the order is the guarantee: the create goes
/// first, and a failure there leaves the vault untouched. A failure after it
/// leaves the union and an original standing, which the user can see and
/// delete. The Apple app tombstoned first and created last, so a failed
/// create left both blocks gone and nothing in their place.
///
/// # Errors
/// [`ValidationError`] when the union does not validate.
pub fn merge_commands(a: &Block, b: &Block, reader: &TimeZone) -> Result<[Command; 3], MergeError> {
    let draft = sunrise_domain::merge_blocks(a, b, reader)?;
    Ok([
        Command::CreateBlock(draft),
        Command::DeleteBlock(a.id),
        Command::DeleteBlock(b.id),
    ])
}

/// Merge the blocks `a` and `b` in `core`, read in `reader`: one call for
/// the Resolve menu's **Merge**, submitting [`merge_commands`] in order.
/// Returns the merged block.
///
/// # Errors
/// [`MergeError`]; see [`merge_commands`] for what a failure part-way
/// leaves.
pub async fn merge_blocks(
    core: &Core,
    a: EntityRef,
    b: EntityRef,
    reader: &TimeZone,
) -> Result<EntityRef, MergeError> {
    let first = read_block(core, a).await?;
    let second = read_block(core, b).await?;
    let [create, delete_a, delete_b] = merge_commands(&first, &second, reader)?;
    let created = core
        .submit(create)
        .await
        .map_err(|source| MergeError::Core {
            source,
            created: None,
        })?
        .entity;
    for delete in [delete_a, delete_b] {
        core.submit(delete)
            .await
            .map_err(|source| MergeError::Core {
                source,
                created: Some(created),
            })?;
    }
    Ok(created)
}

/// One block, read from the vault.
async fn read_block(core: &Core, id: EntityRef) -> Result<Block, MergeError> {
    match core.query(Query::EntityById(id)).await {
        Ok(QueryResult::Blocks(mut rows)) if rows.len() == 1 => Ok(rows
            .pop()
            .map(|r| r.block)
            .ok_or(MergeError::NotABlock(id))?),
        Ok(_) => Err(MergeError::NotABlock(id)),
        Err(source) => Err(MergeError::Core {
            source,
            created: None,
        }),
    }
}

/// The first instant of `date` in `reader`.
fn day_start_ms(date: Date, reader: &TimeZone) -> Result<i64, CivilError> {
    reader
        .to_zoned(date.to_datetime(Time::midnight()))
        .map(|z| z.timestamp().as_millisecond())
        .map_err(|_| out_of_range(date))
}

/// `date` moved `days` on.
fn add_days(date: Date, days: i32) -> Result<Date, CivilError> {
    date.checked_add(Span::new().days(days))
        .map_err(|_| out_of_range(date))
}

/// A snap step of `minutes`, at least one minute, in milliseconds.
const fn step_ms(minutes: i64) -> i64 {
    let minutes = if minutes < 1 { 1 } else { minutes };
    minutes.saturating_mul(60_000)
}

/// The domain's out-of-range error for a date.
fn out_of_range(date: Date) -> CivilError {
    CivilError::Unresolvable(sunrise_domain::Unresolvable::OutOfRange {
        value: date.to_string(),
    })
}

#[cfg(test)]
mod tests;
