//! The interactive planner across the seam (`docs/08-features/planner.md`,
//! #343): a client sends the gesture and renders the diff it gets back. It
//! computes no position, conflict or violation of its own.
//!
//! Additive only: nothing here changes a record or an enum case Swift
//! already reads.

use crate::dto::TimeValue;
use crate::{BindingError, SunriseCore};
use sunrise_core::{PlanCommitId, PlanError, PlanField};
use sunrise_domain::planner::{
    Edge, MoveRole, OccurrenceScope, PlanDiff, PlanDiffId, PlanIntent, PlanOccurrence,
    PlanPosition, PlanRule, PlanSeverity, SlotTarget, UnscheduleKeep, UnscheduleReason,
};
use sunrise_id::EntityRef;

/// A block's edge. See [`sunrise_domain::planner::Edge`]; named apart from it
/// so it does not shadow SwiftUI's `Edge` in a file that imports both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PlanEdge {
    /// The start.
    Start,
    /// The end.
    End,
}

impl From<PlanEdge> for Edge {
    fn from(e: PlanEdge) -> Self {
        match e {
            PlanEdge::Start => Self::Start,
            PlanEdge::End => Self::End,
        }
    }
}

/// See [`sunrise_domain::planner::UnscheduleKeep`].
#[uniffi::remote(Enum)]
pub enum UnscheduleKeep {
    Day,
    Nothing,
}

/// See [`sunrise_domain::planner::OccurrenceScope`].
#[uniffi::remote(Enum)]
pub enum OccurrenceScope {
    This,
    All,
}

/// See [`sunrise_domain::planner::MoveRole`].
#[uniffi::remote(Enum)]
pub enum MoveRole {
    Subject,
    Rippled,
}

/// See [`sunrise_domain::planner::UnscheduleReason`].
#[uniffi::remote(Enum)]
pub enum UnscheduleReason {
    NoRoomInDay,
    WouldViolateHard,
}

/// See [`sunrise_domain::planner::PlanRule`].
#[uniffi::remote(Enum)]
pub enum PlanRule {
    OverlapsFixed,
    Overlaps,
    AfterHardDeadline,
    AfterTarget,
    BeforeBlocker,
    OutsideTimeOfDay,
    WrongDayOfWeek,
    OutsideDateRange,
    OutsideDay,
    InPast,
    Invalid,
}

/// See [`sunrise_domain::planner::PlanSeverity`].
#[uniffi::remote(Enum)]
pub enum PlanSeverity {
    Info,
    Soft,
    Hard,
    Blocking,
}

/// A gesture. See [`sunrise_domain::planner::PlanIntent`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PlanIntentItem {
    /// Drag a task onto the grid, or along it.
    MoveTaskTo {
        /// The task.
        task: EntityRef,
        /// Its new start.
        at: TimeValue,
    },
    /// Drag a task onto a day: it becomes date-only on that day.
    MoveTaskToDay {
        /// The task.
        task: EntityRef,
        /// The day.
        day: jiff::civil::Date,
    },
    /// Drag a block with no rule. Its duration is kept.
    MoveBlock {
        /// The block.
        block: EntityRef,
        /// Its new start.
        to_start: TimeValue,
    },
    /// Drag a block with no rule by its top or bottom edge.
    ResizeBlock {
        /// The block.
        block: EntityRef,
        /// Which edge.
        edge: PlanEdge,
        /// Where the edge lands.
        to: TimeValue,
    },
    /// Drag one occurrence of a recurring block. Its duration is kept. The
    /// diff's subject move is that occurrence.
    MoveBlockOccurrence {
        /// The block.
        block: EntityRef,
        /// The occurrence key, as the calendar query gave it.
        key: String,
        /// How far the drag reaches.
        scope: OccurrenceScope,
        /// The occurrence's new start.
        to_start: TimeValue,
    },
    /// Drag one occurrence of a recurring block by its top or bottom edge.
    ResizeBlockOccurrence {
        /// The block.
        block: EntityRef,
        /// The occurrence key, as the calendar query gave it.
        key: String,
        /// How far the drag reaches.
        scope: OccurrenceScope,
        /// Which edge.
        edge: PlanEdge,
        /// Where the edge lands.
        to: TimeValue,
    },
    /// Move many tasks to a day; `place` slots them into its free time.
    BulkDefer {
        /// The tasks, in order.
        tasks: Vec<EntityRef>,
        /// The day.
        to: jiff::civil::Date,
        /// Whether to give them times.
        place: bool,
    },
    /// Take tasks off the grid.
    Unschedule {
        /// The tasks.
        items: Vec<EntityRef>,
        /// What they keep.
        keep: UnscheduleKeep,
    },
    /// Put tasks into the first free slots of a day, in order.
    AutoPlace {
        /// The tasks, in order.
        tasks: Vec<EntityRef>,
        /// The day.
        day: jiff::civil::Date,
        /// No slot starts before this.
        not_before: Option<TimeValue>,
    },
}

impl From<PlanIntentItem> for PlanIntent {
    fn from(i: PlanIntentItem) -> Self {
        match i {
            PlanIntentItem::MoveTaskTo { task, at } => Self::MoveTask {
                task,
                to: SlotTarget::At(at.into()),
            },
            PlanIntentItem::MoveTaskToDay { task, day } => Self::MoveTask {
                task,
                to: SlotTarget::Day(day),
            },
            PlanIntentItem::MoveBlock { block, to_start } => Self::MoveBlock {
                block,
                occurrence: None,
                to_start: to_start.into(),
            },
            PlanIntentItem::ResizeBlock { block, edge, to } => Self::ResizeBlock {
                block,
                occurrence: None,
                edge: edge.into(),
                to: to.into(),
            },
            PlanIntentItem::MoveBlockOccurrence {
                block,
                key,
                scope,
                to_start,
            } => Self::MoveBlock {
                block,
                occurrence: Some(PlanOccurrence { key, scope }),
                to_start: to_start.into(),
            },
            PlanIntentItem::ResizeBlockOccurrence {
                block,
                key,
                scope,
                edge,
                to,
            } => Self::ResizeBlock {
                block,
                occurrence: Some(PlanOccurrence { key, scope }),
                edge: edge.into(),
                to: to.into(),
            },
            PlanIntentItem::BulkDefer { tasks, to, place } => Self::BulkDefer { tasks, to, place },
            PlanIntentItem::Unschedule { items, keep } => Self::Unschedule { items, keep },
            PlanIntentItem::AutoPlace {
                tasks,
                day,
                not_before,
            } => Self::AutoPlace {
                tasks,
                day,
                not_before: not_before.map(Into::into),
            },
        }
    }
}

/// A position in time. See [`sunrise_domain::planner::PlanPosition`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PlanPositionItem {
    /// A slot on the grid.
    Slot {
        /// Its start.
        start: TimeValue,
        /// Its end.
        end: TimeValue,
    },
    /// A day with no time.
    Day {
        /// The day.
        date: jiff::civil::Date,
    },
    /// No plan.
    Unplanned,
}

impl From<&PlanPosition> for PlanPositionItem {
    fn from(p: &PlanPosition) -> Self {
        match p {
            PlanPosition::Slot { start, end } => Self::Slot {
                start: start.into(),
                end: end.into(),
            },
            PlanPosition::Day(date) => Self::Day { date: *date },
            PlanPosition::Unplanned => Self::Unplanned,
        }
    }
}

/// One item whose position changes: a ghost at `after`, an outline at
/// `before`. For a recurring block the intent names, the subject move is the
/// occurrence the intent names; no other occurrence ever moves.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlanMoveItem {
    /// The task or block.
    pub item: EntityRef,
    /// Why it moves.
    pub role: MoveRole,
    /// Where it was.
    pub before: PlanPositionItem,
    /// Where it goes.
    pub after: PlanPositionItem,
}

/// An item moved to its day's no-time lane.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlanUnscheduledItem {
    /// The task.
    pub item: EntityRef,
    /// Where it was.
    pub before: PlanPositionItem,
    /// The day it keeps.
    pub day: jiff::civil::Date,
    /// Why.
    pub reason: UnscheduleReason,
}

/// One violation present after the diff: tint the item it names.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlanViolationItem {
    /// The item.
    pub item: EntityRef,
    /// The rule.
    pub rule: PlanRule,
    /// How bad.
    pub severity: PlanSeverity,
    /// True when the diff introduces it.
    pub introduced: bool,
    /// The other party, where there is one.
    pub with: Option<EntityRef>,
}

/// What a gesture would do. See [`sunrise_domain::planner::PlanDiff`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct PlanDiffItem {
    /// The diff's id, 32 hex digits: what [`SunriseCore::plan_commit`]
    /// takes.
    pub id: String,
    /// Every item that moves, subjects first.
    pub moves: Vec<PlanMoveItem>,
    /// Items moved to their day's no-time lane.
    pub unscheduled: Vec<PlanUnscheduledItem>,
    /// Every violation after the diff, most severe first.
    pub violations: Vec<PlanViolationItem>,
    /// False when any violation is Blocking: render the drop as refused.
    pub committable: bool,
    /// True when the search ran out of budget; the diff is valid, and may
    /// not be minimal.
    pub truncated: bool,
}

impl From<&PlanDiff> for PlanDiffItem {
    fn from(d: &PlanDiff) -> Self {
        Self {
            id: hex(&d.id.0),
            moves: d
                .moves
                .iter()
                .map(|m| PlanMoveItem {
                    item: m.item,
                    role: m.role,
                    before: (&m.before).into(),
                    after: (&m.after).into(),
                })
                .collect(),
            unscheduled: d
                .unscheduled
                .iter()
                .map(|u| PlanUnscheduledItem {
                    item: u.item,
                    before: (&u.before).into(),
                    day: u.day,
                    reason: u.reason,
                })
                .collect(),
            violations: d
                .violations
                .iter()
                .map(|v| PlanViolationItem {
                    item: v.item,
                    rule: v.rule,
                    severity: v.severity,
                    introduced: v.introduced,
                    with: v.with,
                })
                .collect(),
            committable: d.committable,
            truncated: d.truncated,
        }
    }
}

/// What a commit wrote.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlanCommitItem {
    /// The commit's id, 32 hex digits: what [`SunriseCore::plan_undo`]
    /// takes.
    pub commit_id: String,
    /// How many tasks and blocks it moved.
    pub moved: u32,
}

/// See [`sunrise_core::PlanField`].
#[uniffi::remote(Enum)]
pub enum PlanField {
    PlannedAt,
    StartsAt,
    EndsAt,
}

/// A field an undo left as it is. See [`sunrise_core::PlanUndoSkip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct PlanUndoSkipItem {
    /// The task or block.
    pub item: EntityRef,
    /// The field.
    pub field: PlanField,
}

/// What an undo restored, and what it left. See
/// [`sunrise_core::PlanUndoResult`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlanUndoItem {
    /// How many fields it restored.
    pub restored: u32,
    /// Fields written again since the commit, or of an item deleted since,
    /// left as they are: "Restored 4 of 5; ‘Standup’ was changed since".
    pub skipped: Vec<PlanUndoSkipItem>,
}

impl From<PlanError> for BindingError {
    fn from(e: PlanError) -> Self {
        match e {
            PlanError::Stale => Self::PlanStale,
            PlanError::Unknown => Self::PlanUnknown,
            PlanError::NotCommittable | PlanError::Invalid(_) => Self::PlanRefused(e.to_string()),
            PlanError::Closed | PlanError::Engine(_) => Self::Core(e.to_string()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `id`, 32 hex digits, as 16 bytes, or [`BindingError::BadFixedBytes`]
/// naming `field`.
fn bytes16(id: &str, field: &str) -> Result<[u8; 16], BindingError> {
    let bad = || BindingError::BadFixedBytes {
        field: field.into(),
        expected: 16,
    };
    if id.len() != 32 || !id.is_ascii() {
        return Err(bad());
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&id[i * 2..i * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(out)
}

/// `id` as a [`PlanDiffId`], or [`BindingError::BadFixedBytes`].
fn diff_id(id: &str) -> Result<PlanDiffId, BindingError> {
    bytes16(id, "diff_id").map(PlanDiffId)
}

/// `id` as a [`PlanCommitId`], or [`BindingError::BadFixedBytes`].
fn commit_id(id: &str) -> Result<PlanCommitId, BindingError> {
    bytes16(id, "commit_id").map(PlanCommitId)
}

#[uniffi::export]
impl SunriseCore {
    /// Preview `intent` as a diff, solved by the core against the vault as
    /// it stands now. Writes nothing. Call it off the main thread on each
    /// change of snapped position.
    ///
    /// # Errors
    /// [`BindingError::PlanRefused`] for an intent naming nothing the vault
    /// holds, or the wrong kind.
    pub fn plan_preview(&self, intent: PlanIntentItem) -> Result<PlanDiffItem, BindingError> {
        let diff = self.inner.plan_preview(&intent.into())?;
        Ok((&diff).into())
    }

    /// Commit exactly the previewed diff `diff_id`, or nothing.
    ///
    /// # Errors
    /// [`BindingError::PlanStale`] or [`BindingError::PlanUnknown`]: preview
    /// again at the same position, and commit the new diff only if its moves
    /// are the same. [`BindingError::PlanRefused`] for a diff with a Blocking
    /// violation, or a block move that would leave a recurring block's
    /// exception applying to nothing (the preview flags it unless the block
    /// changed since); [`BindingError::BadFixedBytes`] for an id that is not 32
    /// hex digits.
    pub fn plan_commit(&self, diff_id: String) -> Result<PlanCommitItem, BindingError> {
        let result = self.inner.plan_commit(self::diff_id(&diff_id)?)?;
        Ok(PlanCommitItem {
            commit_id: hex(&result.commit.0),
            moved: result.moved,
        })
    }

    /// Undo the commit `commit_id` per field: each field still holding the
    /// commit's write is restored, and each written again since is left and
    /// reported in `skipped`. Never refuses as stale.
    ///
    /// # Errors
    /// [`BindingError::PlanUnknown`] for a commit undone already or too old
    /// to keep; [`BindingError::BadFixedBytes`] for an id that is not 32 hex
    /// digits.
    pub fn plan_undo(&self, commit_id: String) -> Result<PlanUndoItem, BindingError> {
        let result = self.inner.plan_undo(self::commit_id(&commit_id)?)?;
        Ok(PlanUndoItem {
            restored: result.restored,
            skipped: result
                .skipped
                .into_iter()
                .map(|s| PlanUndoSkipItem {
                    item: s.item,
                    field: s.field,
                })
                .collect(),
        })
    }

    /// Forget the previewed diff `diff_id` (the gesture was cancelled).
    /// Optional: the core forgets old previews on its own.
    ///
    /// # Errors
    /// [`BindingError::BadFixedBytes`] for an id that is not 32 hex digits.
    pub fn plan_cancel(&self, diff_id: String) -> Result<(), BindingError> {
        self.inner.plan_cancel(self::diff_id(&diff_id)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diff_id_round_trips_through_hex_and_a_bad_one_is_refused() {
        let id = PlanDiffId([0xab; 16]);
        assert_eq!(diff_id(&hex(&id.0)).unwrap(), id);
        let commit = PlanCommitId([0x3c; 16]);
        assert_eq!(commit_id(&hex(&commit.0)).unwrap(), commit);
        assert!(matches!(
            commit_id("00"),
            Err(BindingError::BadFixedBytes { field, .. }) if field == "commit_id"
        ));
        for bad in ["", "ab", &"zz".repeat(16), &"é".repeat(16)] {
            assert!(matches!(
                diff_id(bad),
                Err(BindingError::BadFixedBytes { .. })
            ));
        }
    }

    #[test]
    fn stale_and_unknown_keep_their_own_variants() {
        assert!(matches!(
            BindingError::from(PlanError::Stale),
            BindingError::PlanStale
        ));
        assert!(matches!(
            BindingError::from(PlanError::Unknown),
            BindingError::PlanUnknown
        ));
        assert!(matches!(
            BindingError::from(PlanError::NotCommittable),
            BindingError::PlanRefused(_)
        ));
    }
}
