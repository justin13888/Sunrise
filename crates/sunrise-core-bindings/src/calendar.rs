//! The calendar grid, lowered onto [`sunrise_client_core::calendar`].
//!
//! Which civil days a day or week grid draws, the block value a drag or a
//! drop writes, where blocks sit in lanes, what a snap and a drag do, and the
//! Resolve menu's **Merge** as one call. None of it is computed here.
//!
//! Two of the client layer's names are already taken by the Apple app's own
//! types (`CalendarSpan`, `BlockDragMode`), and the generated Swift is
//! compiled into that module, so the records here are named
//! [`CalendarGridSpan`] and [`BlockDragGesture`].
//!
//! `tz` is an IANA zone name; an unknown one is [`BindingError::BadTime`],
//! never UTC (ADR-0066 §3).

use sunrise_client_core::calendar::{self, CalendarDay, CalendarSpan, DragMode, GridRange};
use sunrise_domain::Weekday;
use sunrise_id::EntityRef;

use crate::dto::{BlockDraftIn, BlockEdit, BlockGridRow, BlockItem, TimeValue};
use crate::editing::{block_edit, civil_error, editor_error, zone};
use crate::{BindingError, SunriseCore};
use sunrise_client_core::civil::TimeKind;

/// Day or week. See [`sunrise_client_core::calendar::CalendarSpan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CalendarGridSpan {
    /// One civil day.
    Day,
    /// Seven civil days from the `week_start` preference.
    Week,
}

impl From<CalendarGridSpan> for CalendarSpan {
    fn from(s: CalendarGridSpan) -> Self {
        match s {
            CalendarGridSpan::Day => Self::Day,
            CalendarGridSpan::Week => Self::Week,
        }
    }
}

/// Which end of a block a drag holds. See
/// [`sunrise_client_core::calendar::DragMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BlockDragGesture {
    /// Both ends: the block keeps its length.
    Move,
    /// The end only: the block keeps its start.
    ResizeEnd,
}

impl From<BlockDragGesture> for DragMode {
    fn from(g: BlockDragGesture) -> Self {
        match g {
            BlockDragGesture::Move => Self::Move,
            BlockDragGesture::ResizeEnd => Self::ResizeEnd,
        }
    }
}

/// One civil day of the grid, `[start_ms, end_ms)`: 23 or 25 hours across a
/// DST change. See [`sunrise_client_core::calendar::CalendarDay`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct CalendarDayBounds {
    /// The date.
    pub date: jiff::civil::Date,
    /// Its first instant, epoch milliseconds.
    pub start_ms: i64,
    /// The next day's first instant.
    pub end_ms: i64,
}

impl From<CalendarDay> for CalendarDayBounds {
    fn from(d: CalendarDay) -> Self {
        Self {
            date: d.date,
            start_ms: d.start_ms,
            end_ms: d.end_ms,
        }
    }
}

impl From<CalendarDayBounds> for CalendarDay {
    fn from(d: CalendarDayBounds) -> Self {
        Self {
            date: d.date,
            start_ms: d.start_ms,
            end_ms: d.end_ms,
        }
    }
}

/// A range on the grid, epoch milliseconds, half-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct BlockBounds {
    /// Start.
    pub start_ms: i64,
    /// End.
    pub end_ms: i64,
}

impl From<GridRange> for BlockBounds {
    fn from(r: GridRange) -> Self {
        Self {
            start_ms: r.start_ms,
            end_ms: r.end_ms,
        }
    }
}

/// One block placed on a day's column. See
/// [`sunrise_client_core::calendar::PlacedBlock`].
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct BlockPlacement {
    /// The block.
    pub block: EntityRef,
    /// Start, clamped to the column.
    pub start_ms: i64,
    /// End, clamped to the column.
    pub end_ms: i64,
    /// Its column among the blocks it overlaps, from 0.
    pub lane: u32,
    /// How many columns its overlapping run needs.
    pub lane_count: u32,
}

/// The days a grid of `span` draws around the instant `anchor_ms`, a week
/// starting on `week_start` (the rule `WeekBlocks` uses).
#[uniffi::export]
pub fn calendar_days(
    anchor_ms: i64,
    span: CalendarGridSpan,
    week_start: Weekday,
    tz: String,
) -> Result<Vec<CalendarDayBounds>, BindingError> {
    calendar::calendar_days(anchor_ms, span.into(), &week_start, &zone(&tz)?)
        .map(|days| days.into_iter().map(CalendarDayBounds::from).collect())
        .map_err(civil_error)
}

/// The anchor moved by `direction` whole spans, in civil days.
#[uniffi::export]
pub fn calendar_step(
    anchor_ms: i64,
    span: CalendarGridSpan,
    direction: i32,
    tz: String,
) -> Result<i64, BindingError> {
    calendar::step_anchor(anchor_ms, span.into(), direction, &zone(&tz)?).map_err(civil_error)
}

/// The moment `ms` as a block bound of `kind`, read in `tz`.
#[uniffi::export]
pub fn block_time_value(ms: i64, kind: TimeKind, tz: String) -> Result<TimeValue, BindingError> {
    sunrise_client_core::civil::value_at(ms, kind, &zone(&tz)?)
        .map(|v| TimeValue::from(&v))
        .map_err(civil_error)
}

/// The draft for a block drawn on the grid or made by dropping tasks on it,
/// both bounds in `kind` (`TimeKind::Zoned` is the grid's default).
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub fn grid_block_draft(
    from_ms: i64,
    to_ms: i64,
    kind: TimeKind,
    stream_id: EntityRef,
    title: Option<String>,
    tasks: Vec<EntityRef>,
    tz: String,
) -> Result<BlockDraftIn, BindingError> {
    let d = calendar::new_block_draft(from_ms, to_ms, kind, stream_id, title, tasks, &zone(&tz)?)
        .map_err(civil_error)?;
    Ok(BlockDraftIn {
        stream_id: d.stream_id,
        starts_at: TimeValue::from(&d.starts_at),
        ends_at: TimeValue::from(&d.ends_at),
        title: d.title,
        title_track_task: d.title_track_task,
        tasks: d.tasks,
    })
}

/// The edit a drag that moved or resized `block` to `[start_ms, end_ms)`
/// makes: only the bound that moved, in the kind it already had.
#[uniffi::export]
pub fn moved_block_edit(
    block: BlockItem,
    start_ms: i64,
    end_ms: i64,
    tz: String,
) -> Result<BlockEdit, BindingError> {
    calendar::moved_block_patch(&block.to_domain(), start_ms, end_ms, &zone(&tz)?)
        .map(block_edit)
        .map_err(editor_error)
}

/// Where `rows` go on the column `day`, in lanes.
#[uniffi::export]
pub fn lay_out_blocks(
    rows: Vec<BlockGridRow>,
    day: CalendarDayBounds,
    tz: String,
) -> Result<Vec<BlockPlacement>, BindingError> {
    let blocks: Vec<_> = rows.iter().map(|r| r.block.to_domain()).collect();
    Ok(
        calendar::place_blocks(&blocks, &CalendarDay::from(day), &zone(&tz)?)
            .into_iter()
            .map(|p| BlockPlacement {
                block: p.id,
                start_ms: p.start_ms,
                end_ms: p.end_ms,
                lane: p.lane,
                lane_count: p.lane_count,
            })
            .collect(),
    )
}

/// `ms` snapped to the nearest `minutes` step of the day starting at
/// `day_start_ms`.
#[uniffi::export]
#[must_use]
pub fn snap_to_grid(ms: i64, minutes: i64, day_start_ms: i64) -> i64 {
    calendar::snap(ms, minutes, day_start_ms)
}

/// The block a drag from `from_ms` to `to_ms` draws: snapped, ordered, at
/// least one step.
#[uniffi::export]
#[must_use]
pub fn drawn_block_bounds(
    from_ms: i64,
    to_ms: i64,
    minutes: i64,
    day_start_ms: i64,
) -> BlockBounds {
    calendar::drawn_range(from_ms, to_ms, minutes, day_start_ms).into()
}

/// The block a task dropped at `at_ms` makes: one step or 30 minutes,
/// whichever is longer.
#[uniffi::export]
#[must_use]
pub fn dropped_task_bounds(at_ms: i64, minutes: i64, day_start_ms: i64) -> BlockBounds {
    calendar::dropped_range(at_ms, minutes, day_start_ms).into()
}

/// Where a block at `bounds` ends up after a drag of `delta_ms`, snapped and
/// clamped inside `day` (whose end is the next civil midnight).
#[uniffi::export]
#[must_use]
pub fn drag_block_bounds(
    gesture: BlockDragGesture,
    bounds: BlockBounds,
    delta_ms: i64,
    minutes: i64,
    day: CalendarDayBounds,
) -> BlockBounds {
    let range = GridRange {
        start_ms: bounds.start_ms,
        end_ms: bounds.end_ms,
    };
    calendar::dragged_range(gesture.into(), range, delta_ms, minutes, &day.into()).into()
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// The Resolve menu's **Merge**, in one call: create the union of blocks
    /// `a` and `b` and tombstone both, as one commit. Returns the merged
    /// block.
    ///
    /// All of it is written or none, here and on every peer (ADR-0065 §6).
    /// See [`sunrise_client_core::calendar::merge_command`].
    pub async fn merge_blocks(
        &self,
        a: EntityRef,
        b: EntityRef,
        tz: String,
    ) -> Result<EntityRef, BindingError> {
        let reader = zone(&tz)?;
        calendar::merge_blocks(&self.inner, a, b, &reader)
            .await
            .map_err(|e| match e {
                calendar::MergeError::Core { source, .. } => BindingError::from(source),
                other => BindingError::Core(other.to_string()),
            })
    }
}

#[cfg(test)]
mod tests;
