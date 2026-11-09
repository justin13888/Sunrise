//! The interactive planner's solver (`docs/08-features/planner.md`,
//! ADR-0048, #343).
//!
//! [`solve`] takes a gesture ([`PlanIntent`]) and the inputs of the planner
//! days it touches ([`PlanInputs`]) and returns the [`PlanDiff`] the gesture
//! would make: every item that moves, every item that no longer fits its day,
//! and every violation left on the window afterwards. It is pure: no clock, no
//! database, no locale, no hash-ordered iteration and no floating point. The
//! same inputs, in any order, give a byte-identical diff.
//!
//! The subject lands where the user put it. Other movable items are moved out
//! of its way by a bounded depth-first search that minimises, in order, the
//! hard violations introduced on items other than the subject, the number of
//! items moved, the soft violations introduced, the total displacement in
//! seconds and the ordered list of moved ids (ADR-0048 §4). A rippled item
//! stays inside its own planner day's plannable window; one with no room
//! there is unscheduled onto that day rather than spilled into another.

mod encode;
mod rules;
mod search;
mod world;

#[cfg(test)]
mod tests;

use crate::constraint::ScheduleConstraint;
use crate::time::SunriseTime;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::{EntityKind, EntityRef};

pub use encode::plan_base;
pub use rules::{PlanRule, PlanSeverity};
use world::{Class, Pos, World};

/// How many placements one solve evaluates before it keeps the best it has
/// (`docs/08-features/planner.md` §Solver, step 5).
pub const SEARCH_BUDGET: u32 = 4096;

/// The default snap, `planner.snap_s`: 15 minutes.
pub const DEFAULT_SNAP_S: u32 = 900;

/// The default duration of a task with no estimate,
/// `planner.default_task_duration_s`: 30 minutes.
pub const DEFAULT_TASK_DURATION_S: u32 = 1800;

/// What unscheduling an item onto its day's no-time lane costs in the
/// objective's displacement term: one whole day, so any placement inside the
/// day is preferred to it when both move the same number of items.
const UNSCHEDULE_DISPLACEMENT_S: i64 = 86_400;

/// A gesture that moves work in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanIntent {
    /// Drag a task onto the grid, or along it. The duration is the task's
    /// estimate, or [`PlanInputs::default_task_duration_s`].
    MoveTask {
        /// The task.
        task: EntityRef,
        /// Where it lands.
        to: SlotTarget,
    },
    /// Drag a block. Its duration is kept.
    MoveBlock {
        /// The block.
        block: EntityRef,
        /// Its new start.
        to_start: SunriseTime,
    },
    /// Drag a block's top or bottom edge.
    ResizeBlock {
        /// The block.
        block: EntityRef,
        /// Which edge.
        edge: Edge,
        /// Where the edge lands.
        to: SunriseTime,
    },
    /// Move many tasks to a day. `place` = false keeps them date-only on that
    /// day; true slots them into its free time as [`PlanIntent::AutoPlace`]
    /// would.
    BulkDefer {
        /// The tasks, in order.
        tasks: Vec<EntityRef>,
        /// The day.
        to: Date,
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
    /// Put tasks into the first free slots of a day, in the given order.
    AutoPlace {
        /// The tasks, in order.
        tasks: Vec<EntityRef>,
        /// The day.
        day: Date,
        /// No slot starts before this.
        not_before: Option<SunriseTime>,
    },
}

/// Where a moved task lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotTarget {
    /// A time on the grid.
    At(SunriseTime),
    /// A day, with no time: the task becomes date-only on that day.
    Day(Date),
}

/// A block's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// The start.
    Start,
    /// The end.
    End,
}

/// What an unscheduled task keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnscheduleKeep {
    /// The civil date, without the time of day.
    Day,
    /// Nothing: `planned_at` is cleared.
    Nothing,
}

/// One planner day of the window: `day` is `[boundary(d), boundary(d+1))`,
/// and `plannable` is its wake-to-sleep window, the only span work is placed
/// in (`docs/02-domain/day-schedule.md` §Planner day).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDay {
    /// The planner day's date.
    pub date: Date,
    /// Its bounds.
    pub day: (Timestamp, Timestamp),
    /// Its plannable window.
    pub plannable: (Timestamp, Timestamp),
}

/// An open task as the planner reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTask {
    /// The task.
    pub id: EntityRef,
    /// Its plan.
    pub planned_at: Option<SunriseTime>,
    /// Its estimate, seconds.
    pub estimate_s: Option<u64>,
    /// When its soft deadline is missed (`deadline_at` of `target_at`).
    pub target_due: Option<Timestamp>,
    /// When its hard deadline is missed (`deadline_at` of `hard_due_at`).
    pub hard_due: Option<Timestamp>,
    /// Its open blockers.
    pub blocked_by: Vec<PlanBlocker>,
    /// Its scheduling constraints, evaluated in the reader's zone.
    pub constraints: Vec<ScheduleConstraint>,
    /// Started: it stays where it is.
    pub in_progress: bool,
    /// Bound to a block: it moves with the block and is not placed itself.
    pub bound: bool,
}

/// An open blocker of a task, and when it ends, if it is planned at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBlocker {
    /// The blocker.
    pub task: EntityRef,
    /// Its planned end; `None` when it has no time.
    pub end: Option<Timestamp>,
}

/// A Sunrise block in the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBlock {
    /// The block.
    pub id: EntityRef,
    /// Its start.
    pub starts_at: SunriseTime,
    /// Its end.
    pub ends_at: SunriseTime,
    /// A fixed block never ripples.
    pub fixed: bool,
}

/// An external calendar event in the window. Always fixed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEvent {
    /// The event.
    pub id: EntityRef,
    /// Its start.
    pub start: Timestamp,
    /// Its end.
    pub end: Timestamp,
    /// False for an event whose busy treatment is `free`: drawn, never in the
    /// way.
    pub busy: bool,
}

/// Everything one solve reads.
#[derive(Debug, Clone)]
pub struct PlanInputs {
    /// Now. Past and in-progress items are fixed.
    pub now: Timestamp,
    /// The reader's zone; resolves floating and all-day values.
    pub zone: TimeZone,
    /// The planner days the intent touches.
    pub window: Vec<PlanDay>,
    /// Open tasks planned in the window, or named by the intent.
    pub tasks: Vec<PlanTask>,
    /// Blocks in the window.
    pub blocks: Vec<PlanBlock>,
    /// External events in the window.
    pub events: Vec<PlanEvent>,
    /// The grid's snap, seconds.
    pub snap_s: u32,
    /// The duration of a task with no estimate, seconds.
    pub default_task_duration_s: u32,
    /// The gap kept between placed items, seconds.
    pub min_gap_s: u32,
    /// How many placements the search may evaluate.
    pub search_budget: u32,
}

/// The identity of a diff: 16 bytes of BLAKE3 over its canonical encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlanDiffId(pub [u8; 16]);

/// A digest of every input a solve read: what a commit recomputes and
/// compares to refuse a stale diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlanBase(pub [u8; 32]);

/// A position in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPosition {
    /// A slot on the grid.
    Slot {
        /// Its start.
        start: SunriseTime,
        /// Its end.
        end: SunriseTime,
    },
    /// A day with no time.
    Day(Date),
    /// No plan.
    Unplanned,
}

/// Why an item moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MoveRole {
    /// The gesture moved it.
    Subject,
    /// It moved out of the subject's way.
    Rippled,
}

/// One item whose position changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanMove {
    /// The task or block.
    pub item: EntityRef,
    /// Why.
    pub role: MoveRole,
    /// Where it was.
    pub before: PlanPosition,
    /// Where it goes.
    pub after: PlanPosition,
}

/// Why an item lost its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnscheduleReason {
    /// No free slot in its day.
    NoRoomInDay,
    /// Every free slot would break a hard rule.
    WouldViolateHard,
}

/// An item moved to its day's no-time lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanUnscheduled {
    /// The task.
    pub item: EntityRef,
    /// Where it was.
    pub before: PlanPosition,
    /// The day it keeps.
    pub day: Date,
    /// Why.
    pub reason: UnscheduleReason,
}

/// One violation present after the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanViolation {
    /// The item it is on.
    pub item: EntityRef,
    /// The rule.
    pub rule: PlanRule,
    /// How bad.
    pub severity: PlanSeverity,
    /// True when this diff introduces it.
    pub introduced: bool,
    /// The other party, where there is one.
    pub with: Option<EntityRef>,
}

/// What a gesture would do. See `docs/08-features/planner.md` §`PlanDiff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDiff {
    /// Equal diffs have equal ids on every device.
    pub id: PlanDiffId,
    /// The digest of the inputs it was solved from.
    pub base: PlanBase,
    /// The gesture.
    pub intent: PlanIntent,
    /// Every item whose position changes: subjects first, then rippled, each
    /// by id.
    pub moves: Vec<PlanMove>,
    /// Items moved to their day's no-time lane, by id.
    pub unscheduled: Vec<PlanUnscheduled>,
    /// Every violation after the diff, by (severity desc, item, rule, with).
    pub violations: Vec<PlanViolation>,
    /// False when a Blocking violation is introduced by the diff or stands on
    /// an item it moves or unschedules. One already present on an item the
    /// diff leaves alone is reported, and does not refuse the commit.
    pub committable: bool,
    /// True when the search budget ran out; the diff may not be minimal.
    pub truncated: bool,
}

impl PlanDiff {
    /// Whether the diff changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty() && self.unscheduled.is_empty()
    }
}

/// An intent the inputs cannot answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanIntentError {
    /// The intent names an item the inputs do not hold.
    #[error("{0} is not in the plan's inputs")]
    Unknown(EntityRef),
    /// The intent names an item of the wrong kind for the gesture.
    #[error("{0} cannot take this gesture")]
    WrongKind(EntityRef),
    /// The intent names a day outside the window.
    #[error("{0} is not in the plan's window")]
    DayOutsideWindow(Date),
    /// A time in the intent has no instant for this reader.
    #[error("the intent's time has no instant for this reader")]
    Unresolvable,
    /// The intent names no item, or one twice.
    #[error("the intent names no item, or one twice")]
    BadList,
}

/// Solve `intent` over `inputs`. Pure and deterministic.
///
/// # Errors
/// [`PlanIntentError`] when the intent names something the inputs do not
/// hold, or a time that cannot be placed.
pub fn solve(inputs: &PlanInputs, intent: &PlanIntent) -> Result<PlanDiff, PlanIntentError> {
    let world = World::build(inputs, intent)?;
    let mut after: Vec<Pos> = world.items.iter().map(|i| i.before.clone()).collect();
    let mut out_after: BTreeMap<usize, PlanPosition> = BTreeMap::new();
    let mut unscheduled: BTreeMap<usize, UnscheduleReason> = BTreeMap::new();
    let mut truncated = false;
    let mut invalid_subject = None;

    match intent {
        PlanIntent::MoveTask { task, to } => {
            let i = world.subject_index(*task)?;
            match to {
                SlotTarget::At(t) => {
                    let s = resolve(t, &inputs.zone)?;
                    let e = s + world.items[i].dur;
                    after[i] = Pos::Slot(s, e);
                    out_after.insert(
                        i,
                        PlanPosition::Slot {
                            start: t.clone(),
                            end: retime(t, e, &inputs.zone),
                        },
                    );
                }
                SlotTarget::Day(d) => {
                    after[i] = Pos::Day(*d);
                    out_after.insert(i, PlanPosition::Day(*d));
                }
            }
        }
        PlanIntent::MoveBlock { block, to_start } => {
            let i = world.subject_index(*block)?;
            let s = resolve(to_start, &inputs.zone)?;
            let e = s + world.items[i].dur;
            after[i] = Pos::Slot(s, e);
            let end_kind = world.items[i].end_kind.clone().unwrap_or(to_start.clone());
            out_after.insert(
                i,
                PlanPosition::Slot {
                    start: to_start.clone(),
                    end: retime(&end_kind, e, &inputs.zone),
                },
            );
        }
        PlanIntent::ResizeBlock { block, edge, to } => {
            let i = world.subject_index(*block)?;
            let at = resolve(to, &inputs.zone)?;
            let Pos::Slot(s, e) = world.items[i].before else {
                return Err(PlanIntentError::Unresolvable);
            };
            let (start, end) = match edge {
                Edge::Start => (at, e),
                Edge::End => (s, at),
            };
            let item = &world.items[i];
            let (start_t, end_t) = match edge {
                Edge::Start => (to.clone(), item.end_kind.clone().unwrap_or(to.clone())),
                Edge::End => (item.start_kind.clone().unwrap_or(to.clone()), to.clone()),
            };
            out_after.insert(
                i,
                PlanPosition::Slot {
                    start: start_t,
                    end: end_t,
                },
            );
            if end <= start {
                invalid_subject = Some(i);
                after[i] = Pos::Slot(start, start);
            } else {
                after[i] = Pos::Slot(start, end);
            }
        }
        PlanIntent::BulkDefer {
            tasks,
            to,
            place: false,
        } => {
            for id in tasks {
                let i = world.subject_index(*id)?;
                after[i] = Pos::Day(*to);
                out_after.insert(i, PlanPosition::Day(*to));
            }
        }
        PlanIntent::BulkDefer {
            tasks,
            to: day,
            place: true,
        }
        | PlanIntent::AutoPlace {
            tasks,
            day,
            not_before: _,
        } => {
            let not_before = match intent {
                PlanIntent::AutoPlace {
                    not_before: Some(t),
                    ..
                } => Some(resolve(t, &inputs.zone)?),
                _ => None,
            };
            let d = world.day_index(*day)?;
            let order: Vec<usize> = tasks
                .iter()
                .map(|id| world.subject_index(*id))
                .collect::<Result<_, _>>()?;
            for (i, pos) in search::auto_place(&world, &order, d, not_before) {
                if let Some((s, e)) = pos {
                    after[i] = Pos::Slot(s, e);
                    out_after.insert(i, world.slot_out(i, s, e));
                } else {
                    after[i] = Pos::Day(*day);
                    unscheduled.insert(i, UnscheduleReason::NoRoomInDay);
                }
            }
        }
        PlanIntent::Unschedule { items, keep } => {
            for id in items {
                let i = world.subject_index(*id)?;
                if !world.items[i].is_task {
                    return Err(PlanIntentError::WrongKind(*id));
                }
                let pos = match keep {
                    UnscheduleKeep::Nothing => Pos::Unplanned,
                    UnscheduleKeep::Day => match &world.items[i].before {
                        Pos::Slot(s, _) => Pos::Day(world.date_of(*s)),
                        other => other.clone(),
                    },
                };
                out_after.insert(i, pos.out());
                after[i] = pos;
            }
        }
    }

    // The ripple: only a gesture that puts a subject onto the grid somewhere
    // new can push anything.
    let ripples = matches!(
        intent,
        PlanIntent::MoveTask {
            to: SlotTarget::At(_),
            ..
        } | PlanIntent::MoveBlock { .. }
            | PlanIntent::ResizeBlock { .. }
    ) && invalid_subject.is_none()
        && world
            .subjects
            .iter()
            .any(|&i| after[i] != world.items[i].before);
    if ripples {
        let found = search::ripple(&world, &after);
        truncated = found.truncated;
        for (i, choice) in found.assign {
            match choice {
                search::Choice::Slot(s, e) => {
                    after[i] = Pos::Slot(s, e);
                    out_after.insert(i, world.slot_out(i, s, e));
                }
                search::Choice::Stay => {}
                search::Choice::Unschedule(reason) => {
                    let Some(d) = world.items[i].day else {
                        continue;
                    };
                    after[i] = Pos::Day(world.window[d].date);
                    unscheduled.insert(i, reason);
                }
            }
        }
    }

    Ok(world.report(
        intent,
        &after,
        &out_after,
        &unscheduled,
        invalid_subject,
        truncated,
    ))
}

/// Every item an intent names, refusing a list that is empty or repeats one.
fn intent_items(intent: &PlanIntent) -> Result<BTreeSet<EntityRef>, PlanIntentError> {
    let list: Vec<EntityRef> = match intent {
        PlanIntent::MoveTask { task, .. } => {
            if task.kind() != EntityKind::Task {
                return Err(PlanIntentError::WrongKind(*task));
            }
            vec![*task]
        }
        PlanIntent::MoveBlock { block, .. } | PlanIntent::ResizeBlock { block, .. } => {
            if block.kind() != EntityKind::Block {
                return Err(PlanIntentError::WrongKind(*block));
            }
            vec![*block]
        }
        PlanIntent::BulkDefer { tasks, .. } | PlanIntent::AutoPlace { tasks, .. } => {
            if let Some(bad) = tasks.iter().find(|t| t.kind() != EntityKind::Task) {
                return Err(PlanIntentError::WrongKind(*bad));
            }
            tasks.clone()
        }
        PlanIntent::Unschedule { items, .. } => {
            if let Some(bad) = items.iter().find(|t| t.kind() != EntityKind::Task) {
                return Err(PlanIntentError::WrongKind(*bad));
            }
            items.clone()
        }
    };
    let set: BTreeSet<EntityRef> = list.iter().copied().collect();
    if set.is_empty() || set.len() != list.len() {
        return Err(PlanIntentError::BadList);
    }
    Ok(set)
}

fn resolve(t: &SunriseTime, zone: &TimeZone) -> Result<i64, PlanIntentError> {
    match t {
        SunriseTime::AllDay { .. } => Err(PlanIntentError::Unresolvable),
        _ => t
            .resolve_in(zone)
            .map(Timestamp::as_second)
            .ok_or(PlanIntentError::Unresolvable),
    }
}

pub(crate) fn to_ts(s: i64) -> Timestamp {
    Timestamp::from_second(s).unwrap_or(Timestamp::MAX)
}

/// `at`, written in the kind `template` is: an instant stays an instant, a
/// zoned time keeps its zone, and a floating time stays floating in the
/// reader's zone.
fn retime(template: &SunriseTime, at: i64, zone: &TimeZone) -> SunriseTime {
    let ts = to_ts(at);
    match template {
        SunriseTime::Zoned { tz, .. } => {
            let z = TimeZone::get(tz).unwrap_or_else(|_| zone.clone());
            SunriseTime::zoned(ts.to_zoned(z).datetime(), tz.clone())
        }
        SunriseTime::Floating { .. } => SunriseTime::floating(ts.to_zoned(zone.clone()).datetime()),
        _ => SunriseTime::instant(ts),
    }
}
