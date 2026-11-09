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

mod rules;
mod search;

#[cfg(test)]
mod tests;

use crate::constraint::ScheduleConstraint;
use crate::time::SunriseTime;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::{EntityKind, EntityRef};

pub use rules::{PlanRule, PlanSeverity};

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
    /// False when any violation is Blocking.
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

/// The digest of every input a solve reads, independent of input order.
#[must_use]
pub fn plan_base(inputs: &PlanInputs) -> PlanBase {
    let mut f = Feed::new(b"sunrise.planner.base.v1");
    let snap = i64::from(inputs.snap_s.max(1));
    f.i64(inputs.now.as_second().div_euclid(snap));
    f.zone(&inputs.zone, inputs.now);
    let mut days: Vec<&PlanDay> = inputs.window.iter().collect();
    days.sort_by_key(|d| d.date);
    f.u64(days.len() as u64);
    for d in days {
        f.str(&d.date.to_string());
        f.ts(d.day.0);
        f.ts(d.day.1);
        f.ts(d.plannable.0);
        f.ts(d.plannable.1);
    }
    let mut tasks: Vec<&PlanTask> = inputs.tasks.iter().collect();
    tasks.sort_by_key(|t| t.id);
    f.u64(tasks.len() as u64);
    for t in tasks {
        f.id(t.id);
        f.opt_time(t.planned_at.as_ref());
        f.u64(t.estimate_s.unwrap_or(u64::MAX));
        f.opt_ts(t.target_due);
        f.opt_ts(t.hard_due);
        let mut blockers: Vec<&PlanBlocker> = t.blocked_by.iter().collect();
        blockers.sort_by_key(|b| b.task);
        f.u64(blockers.len() as u64);
        for b in blockers {
            f.id(b.task);
            f.opt_ts(b.end);
        }
        f.u64(t.constraints.len() as u64);
        for c in &t.constraints {
            f.str(&format!("{c:?}"));
        }
        f.u8(u8::from(t.in_progress) | (u8::from(t.bound) << 1));
    }
    let mut blocks: Vec<&PlanBlock> = inputs.blocks.iter().collect();
    blocks.sort_by_key(|b| b.id);
    f.u64(blocks.len() as u64);
    for b in blocks {
        f.id(b.id);
        f.time(&b.starts_at);
        f.time(&b.ends_at);
        f.u8(u8::from(b.fixed));
    }
    let mut events: Vec<&PlanEvent> = inputs.events.iter().collect();
    events.sort_by_key(|e| e.id);
    f.u64(events.len() as u64);
    for e in events {
        f.id(e.id);
        f.ts(e.start);
        f.ts(e.end);
        f.u8(u8::from(e.busy));
    }
    f.u64(u64::from(inputs.snap_s));
    f.u64(u64::from(inputs.default_task_duration_s));
    f.u64(u64::from(inputs.min_gap_s));
    f.u64(u64::from(inputs.search_budget));
    PlanBase(*f.0.finalize().as_bytes())
}

// ---- the world one solve works in ----

/// An internal position, in epoch seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pos {
    Slot(i64, i64),
    Day(Date),
    Unplanned,
}

impl Pos {
    fn out(&self) -> PlanPosition {
        match self {
            Self::Slot(..) => unreachable!("a slot is rendered from its item's kinds"),
            Self::Day(d) => PlanPosition::Day(*d),
            Self::Unplanned => PlanPosition::Unplanned,
        }
    }

    pub(crate) fn slot(&self) -> Option<(i64, i64)> {
        match self {
            Self::Slot(s, e) => Some((*s, *e)),
            _ => None,
        }
    }
}

/// How an item takes part in a solve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// Never moves: an event, a fixed block, a past or in-progress item.
    Fixed,
    /// May ripple.
    Movable,
    /// Named by the intent.
    Subject,
    /// Not on the grid: an untimed task, or one bound to a block.
    Off,
}

#[derive(Debug, Clone)]
pub(crate) struct Item<'a> {
    pub(crate) id: EntityRef,
    pub(crate) is_task: bool,
    pub(crate) class: Class,
    pub(crate) before: Pos,
    /// The window day the item's slot starts in.
    pub(crate) day: Option<usize>,
    /// Its duration, seconds.
    pub(crate) dur: i64,
    pub(crate) start_kind: Option<SunriseTime>,
    pub(crate) end_kind: Option<SunriseTime>,
    pub(crate) task: Option<&'a PlanTask>,
}

#[derive(Debug)]
pub(crate) struct World<'a> {
    pub(crate) inputs: &'a PlanInputs,
    pub(crate) window: Vec<PlanDay>,
    /// Every item, by id.
    pub(crate) items: Vec<Item<'a>>,
    pub(crate) index: BTreeMap<EntityRef, usize>,
    pub(crate) subjects: Vec<usize>,
    /// Each task's dependents in the world.
    pub(crate) dependents: BTreeMap<EntityRef, Vec<usize>>,
    pub(crate) snap: i64,
    pub(crate) gap: i64,
}

impl<'a> World<'a> {
    fn build(inputs: &'a PlanInputs, intent: &PlanIntent) -> Result<Self, PlanIntentError> {
        let zone = &inputs.zone;
        let now = inputs.now.as_second();
        let mut window = inputs.window.clone();
        window.sort_by_key(|d| d.date);
        window.dedup_by_key(|d| d.date);
        let named: BTreeSet<EntityRef> = intent_items(intent)?;
        let day_of = |s: i64| {
            window
                .iter()
                .position(|d| d.day.0.as_second() <= s && s < d.day.1.as_second())
        };
        let mut items = Vec::new();
        for t in &inputs.tasks {
            let dur = i64::try_from(
                t.estimate_s
                    .filter(|&e| e > 0)
                    .unwrap_or(u64::from(inputs.default_task_duration_s.max(1))),
            )
            .unwrap_or(i64::MAX / 4)
            .min(7 * 86_400);
            let subject = named.contains(&t.id);
            let (before, start_kind) = match &t.planned_at {
                None => (Pos::Unplanned, None),
                Some(SunriseTime::AllDay { date }) => (Pos::Day(*date), None),
                Some(p) => match p.resolve_in(zone) {
                    Some(s) => {
                        let s = s.as_second();
                        (Pos::Slot(s, s + dur), Some(p.clone()))
                    }
                    None => (Pos::Unplanned, None),
                },
            };
            if t.bound && !subject {
                continue;
            }
            let day = before.slot().and_then(|(s, _)| day_of(s));
            let class = if subject {
                Class::Subject
            } else {
                match before {
                    Pos::Slot(s, _) if !t.in_progress && s >= now && day.is_some() => {
                        Class::Movable
                    }
                    Pos::Slot(..) if day.is_some() => Class::Fixed,
                    _ => Class::Off,
                }
            };
            if class == Class::Off && !subject {
                continue;
            }
            items.push(Item {
                id: t.id,
                is_task: true,
                class,
                before,
                day,
                dur,
                end_kind: start_kind.clone(),
                start_kind,
                task: Some(t),
            });
        }
        for b in &inputs.blocks {
            let (Some(s), Some(e)) = (b.starts_at.resolve_in(zone), b.ends_at.resolve_in(zone))
            else {
                continue;
            };
            let (s, e) = (s.as_second(), e.as_second());
            let day = day_of(s);
            let subject = named.contains(&b.id);
            let class = if subject {
                Class::Subject
            } else if day.is_none() && day_of(e - 1).is_none() {
                continue;
            } else if b.fixed || s < now || day.is_none() {
                Class::Fixed
            } else {
                Class::Movable
            };
            items.push(Item {
                id: b.id,
                is_task: false,
                class,
                before: Pos::Slot(s, e.max(s)),
                day,
                dur: (e - s).max(0),
                start_kind: Some(b.starts_at.clone()),
                end_kind: Some(b.ends_at.clone()),
                task: None,
            });
        }
        for ev in &inputs.events {
            if !ev.busy {
                continue;
            }
            let (s, e) = (ev.start.as_second(), ev.end.as_second());
            if e <= s {
                continue;
            }
            items.push(Item {
                id: ev.id,
                is_task: false,
                class: Class::Fixed,
                before: Pos::Slot(s, e),
                day: day_of(s),
                dur: e - s,
                start_kind: None,
                end_kind: None,
                task: None,
            });
        }
        items.sort_by_key(|i| i.id);
        items.dedup_by_key(|i| i.id);
        let index: BTreeMap<EntityRef, usize> =
            items.iter().enumerate().map(|(n, i)| (i.id, n)).collect();
        let mut subjects = Vec::new();
        for id in &named {
            match index.get(id) {
                Some(&i) => subjects.push(i),
                None => return Err(PlanIntentError::Unknown(*id)),
            }
        }
        let mut dependents: BTreeMap<EntityRef, Vec<usize>> = BTreeMap::new();
        for (n, item) in items.iter().enumerate() {
            if let Some(t) = item.task {
                for b in &t.blocked_by {
                    dependents.entry(b.task).or_default().push(n);
                }
            }
        }
        Ok(Self {
            inputs,
            window,
            items,
            index,
            subjects,
            dependents,
            snap: i64::from(inputs.snap_s.max(1)),
            gap: i64::from(inputs.min_gap_s),
        })
    }

    fn subject_index(&self, id: EntityRef) -> Result<usize, PlanIntentError> {
        self.index
            .get(&id)
            .copied()
            .ok_or(PlanIntentError::Unknown(id))
    }

    fn day_index(&self, date: Date) -> Result<usize, PlanIntentError> {
        self.window
            .iter()
            .position(|d| d.date == date)
            .ok_or(PlanIntentError::DayOutsideWindow(date))
    }

    /// The planner date an instant falls on: its window day's, else its civil
    /// date in the reader's zone.
    fn date_of(&self, s: i64) -> Date {
        self.window
            .iter()
            .find(|d| d.day.0.as_second() <= s && s < d.day.1.as_second())
            .map_or_else(
                || to_ts(s).to_zoned(self.inputs.zone.clone()).date(),
                |d| d.date,
            )
    }

    /// The window day an instant is in.
    pub(crate) fn day_at(&self, s: i64) -> Option<usize> {
        self.window
            .iter()
            .position(|d| d.day.0.as_second() <= s && s < d.day.1.as_second())
    }

    /// A slot `[s, e)` for item `i`, written in the kinds its times have.
    fn slot_out(&self, i: usize, s: i64, e: i64) -> PlanPosition {
        let zone = &self.inputs.zone;
        let item = &self.items[i];
        let fresh = || match zone.iana_name() {
            Some(name) => SunriseTime::zoned(to_ts(s).to_zoned(zone.clone()).datetime(), name),
            None => SunriseTime::instant(to_ts(s)),
        };
        let start_kind = item.start_kind.clone().unwrap_or_else(fresh);
        let end_kind = item.end_kind.clone().unwrap_or_else(|| start_kind.clone());
        PlanPosition::Slot {
            start: retime(&start_kind, s, zone),
            end: retime(&end_kind, e, zone),
        }
    }

    /// The item's position before the diff, in its stored kinds.
    fn before_out(&self, i: usize) -> PlanPosition {
        let item = &self.items[i];
        match (&item.before, &item.start_kind) {
            (Pos::Slot(_, e), Some(start)) => PlanPosition::Slot {
                start: start.clone(),
                end: match (&item.end_kind, item.is_task) {
                    (Some(end), false) => end.clone(),
                    _ => retime(start, *e, &self.inputs.zone),
                },
            },
            (Pos::Slot(s, e), None) => PlanPosition::Slot {
                start: SunriseTime::instant(to_ts(*s)),
                end: SunriseTime::instant(to_ts(*e)),
            },
            (Pos::Day(d), _) => PlanPosition::Day(*d),
            (Pos::Unplanned, _) => PlanPosition::Unplanned,
        }
    }

    fn report(
        &self,
        intent: &PlanIntent,
        after: &[Pos],
        out_after: &BTreeMap<usize, PlanPosition>,
        unscheduled: &BTreeMap<usize, UnscheduleReason>,
        invalid_subject: Option<usize>,
        truncated: bool,
    ) -> PlanDiff {
        let before: Vec<Pos> = self.items.iter().map(|i| i.before.clone()).collect();
        let was = rules::all_violations(self, &before, None);
        let now = rules::all_violations(self, after, invalid_subject);
        let was: BTreeSet<(EntityRef, PlanRule, Option<EntityRef>)> =
            was.into_iter().map(|v| (v.item, v.rule, v.with)).collect();
        let mut violations: Vec<PlanViolation> = now
            .into_iter()
            .map(|mut v| {
                v.introduced = !was.contains(&(v.item, v.rule, v.with));
                v
            })
            .collect();
        violations.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(a.item.cmp(&b.item))
                .then(a.rule.cmp(&b.rule))
                .then(a.with.cmp(&b.with))
        });
        let mut moves = Vec::new();
        for (&i, pos) in out_after {
            if unscheduled.contains_key(&i) || after[i] == self.items[i].before {
                continue;
            }
            let role = if self.items[i].class == Class::Subject {
                MoveRole::Subject
            } else {
                MoveRole::Rippled
            };
            moves.push(PlanMove {
                item: self.items[i].id,
                role,
                before: self.before_out(i),
                after: pos.clone(),
            });
        }
        moves.sort_by(|a, b| a.role.cmp(&b.role).then(a.item.cmp(&b.item)));
        let unscheduled: Vec<PlanUnscheduled> = unscheduled
            .iter()
            .filter_map(|(&i, &reason)| match after[i] {
                Pos::Day(day) => Some(PlanUnscheduled {
                    item: self.items[i].id,
                    before: self.before_out(i),
                    day,
                    reason,
                }),
                _ => None,
            })
            .collect();
        let committable = !violations
            .iter()
            .any(|v| v.severity == PlanSeverity::Blocking);
        let base = plan_base(self.inputs);
        let mut diff = PlanDiff {
            id: PlanDiffId([0; 16]),
            base,
            intent: intent.clone(),
            moves,
            unscheduled,
            violations,
            committable,
            truncated,
        };
        diff.id = diff_id(&diff);
        diff
    }
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

// ---- canonical encoding ----

/// A BLAKE3 feed of length-prefixed, fixed-order fields.
struct Feed(blake3::Hasher);

impl Feed {
    fn new(domain: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(domain);
        Self(h)
    }
    fn u8(&mut self, v: u8) {
        self.0.update(&[v]);
    }
    fn u64(&mut self, v: u64) {
        self.0.update(&v.to_be_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.update(&v.to_be_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u64(s.len() as u64);
        self.0.update(s.as_bytes());
    }
    fn id(&mut self, id: EntityRef) {
        self.str(id.kind().prefix());
        self.0.update(id.bytes());
    }
    fn ts(&mut self, t: Timestamp) {
        self.i64(t.as_second());
        self.i64(i64::from(t.subsec_nanosecond()));
    }
    fn opt_ts(&mut self, t: Option<Timestamp>) {
        match t {
            Some(t) => {
                self.u8(1);
                self.ts(t);
            }
            None => self.u8(0),
        }
    }
    fn time(&mut self, t: &SunriseTime) {
        let (ms, kind, tz) = t.to_parts();
        self.i64(ms);
        self.str(kind);
        self.str(tz.as_deref().unwrap_or(""));
        self.str(t.civil_part().as_deref().unwrap_or(""));
    }
    fn opt_time(&mut self, t: Option<&SunriseTime>) {
        match t {
            Some(t) => {
                self.u8(1);
                self.time(t);
            }
            None => self.u8(0),
        }
    }
    fn zone(&mut self, zone: &TimeZone, now: Timestamp) {
        if let Some(name) = zone.iana_name() {
            self.str(name);
        } else {
            self.str("");
            self.i64(i64::from(zone.to_offset(now).seconds()));
        }
    }
    fn position(&mut self, p: &PlanPosition) {
        match p {
            PlanPosition::Slot { start, end } => {
                self.u8(0);
                self.time(start);
                self.time(end);
            }
            PlanPosition::Day(d) => {
                self.u8(1);
                self.str(&d.to_string());
            }
            PlanPosition::Unplanned => self.u8(2),
        }
    }
    fn intent(&mut self, intent: &PlanIntent) {
        let ids = |f: &mut Self, list: &[EntityRef]| {
            f.u64(list.len() as u64);
            for id in list {
                f.id(*id);
            }
        };
        match intent {
            PlanIntent::MoveTask { task, to } => {
                self.u8(0);
                self.id(*task);
                match to {
                    SlotTarget::At(t) => {
                        self.u8(0);
                        self.time(t);
                    }
                    SlotTarget::Day(d) => {
                        self.u8(1);
                        self.str(&d.to_string());
                    }
                }
            }
            PlanIntent::MoveBlock { block, to_start } => {
                self.u8(1);
                self.id(*block);
                self.time(to_start);
            }
            PlanIntent::ResizeBlock { block, edge, to } => {
                self.u8(2);
                self.id(*block);
                self.u8(u8::from(*edge == Edge::End));
                self.time(to);
            }
            PlanIntent::BulkDefer { tasks, to, place } => {
                self.u8(3);
                ids(self, tasks);
                self.str(&to.to_string());
                self.u8(u8::from(*place));
            }
            PlanIntent::Unschedule { items, keep } => {
                self.u8(4);
                ids(self, items);
                self.u8(u8::from(*keep == UnscheduleKeep::Nothing));
            }
            PlanIntent::AutoPlace {
                tasks,
                day,
                not_before,
            } => {
                self.u8(5);
                ids(self, tasks);
                self.str(&day.to_string());
                self.opt_time(not_before.as_ref());
            }
        }
    }
}

/// The diff's id: BLAKE3 over (base, intent, moves, unscheduled,
/// violations), truncated to 16 bytes.
fn diff_id(diff: &PlanDiff) -> PlanDiffId {
    let mut f = Feed::new(b"sunrise.planner.diff.v1");
    f.0.update(&diff.base.0);
    f.intent(&diff.intent);
    f.u64(diff.moves.len() as u64);
    for m in &diff.moves {
        f.id(m.item);
        f.u8(m.role as u8);
        f.position(&m.before);
        f.position(&m.after);
    }
    f.u64(diff.unscheduled.len() as u64);
    for u in &diff.unscheduled {
        f.id(u.item);
        f.position(&u.before);
        f.str(&u.day.to_string());
        f.u8(u.reason as u8);
    }
    f.u64(diff.violations.len() as u64);
    for v in &diff.violations {
        f.id(v.item);
        f.str(v.rule.code());
        f.u8(v.severity as u8);
        f.u8(u8::from(v.introduced));
        match v.with {
            Some(w) => {
                f.u8(1);
                f.id(w);
            }
            None => f.u8(0),
        }
    }
    f.u8(u8::from(diff.truncated));
    let bytes = f.0.finalize();
    let mut id = [0; 16];
    id.copy_from_slice(&bytes.as_bytes()[..16]);
    PlanDiffId(id)
}
