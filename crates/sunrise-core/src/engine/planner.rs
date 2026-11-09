//! The planner's seam on the engine (`docs/08-features/planner.md`,
//! ADR-0048, #343): gather the inputs of the planner days a gesture touches,
//! run the pure solver over them, and write a previewed diff back as one
//! group of per-field ops.
//!
//! Nothing here decides a position. [`sunrise_domain::planner::solve`] does,
//! and this module only reads rows into its inputs and writes its answer.

use super::block::read_block;
use super::day_schedule::schedule_in;
use super::ids::{blob16, domain_bytes, ms_to_ts, time_from_parts};
use super::snapshot::in_tx;
use super::task::{read_task, TaskWrite};
use super::{Engine, EngineError};
use crate::inner_op::InnerOp;
use jiff::civil::Date;
use rusqlite::params;
use std::collections::BTreeSet;
use sunrise_domain::planner::{
    plan_base, solve, PlanBlock, PlanBlocker, PlanDay, PlanDiff, PlanInputs, PlanIntent,
    PlanPosition, PlanTask, SlotTarget, DEFAULT_SNAP_S, DEFAULT_TASK_DURATION_S, SEARCH_BUDGET,
};
use sunrise_domain::time::PREFILTER_SLACK_MS;
use sunrise_domain::{
    day_bounds, deadline_at, plannable, planner_day_of, DaySchedule, PrefValue, SunriseTime, Task,
    TaskPatch, TaskState,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// Why a plan could not be previewed or committed.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// `PLAN_STALE`: an input the diff was solved from changed since the
    /// preview. Preview again.
    #[error("PLAN_STALE: the plan's inputs changed since it was previewed")]
    Stale,
    /// `PLAN_UNKNOWN`: no such diff is remembered — evicted, cancelled, or
    /// the vault reopened.
    #[error("PLAN_UNKNOWN: no such plan is remembered")]
    Unknown,
    /// The diff carries a Blocking violation.
    #[error("the plan carries a blocking violation")]
    NotCommittable,
    /// The intent names nothing, or the wrong kind.
    #[error("invalid plan intent: {0}")]
    Invalid(String),
    /// The vault is closed.
    #[error("the vault is closed")]
    Closed,
    /// The read or the write failed.
    #[error(transparent)]
    Engine(#[from] EngineError),
}

impl From<rusqlite::Error> for PlanError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Engine(e.into())
    }
}

impl PlanError {
    /// The error code a client branches on.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Stale => "PLAN_STALE",
            Self::Unknown => "PLAN_UNKNOWN",
            Self::NotCommittable => "PLAN_NOT_COMMITTABLE",
            Self::Invalid(_) => "PLAN_INVALID",
            Self::Closed => "PLAN_CLOSED",
            Self::Engine(_) => "PLAN_FAILED",
        }
    }
}

impl Engine {
    /// Solve `intent` against the vault as it stands at `now_ms`, in the
    /// reader's zone. Writes nothing.
    ///
    /// # Errors
    /// [`PlanError::Invalid`] for an intent naming nothing the vault holds,
    /// or the wrong kind; [`PlanError::Engine`] for a failed read.
    pub fn plan_preview(
        &self,
        db: &Db,
        intent: &PlanIntent,
        now_ms: u64,
    ) -> Result<PlanDiff, PlanError> {
        let inputs = self.plan_inputs(db, intent, now_ms)?;
        solve(&inputs, intent).map_err(|e| PlanError::Invalid(e.to_string()))
    }

    /// Write `diff`, previewed at `now_ms`, exactly: `planned_at` for each
    /// task, `starts_at` and `ends_at` for each block, and for a
    /// [`PlanIntent::BulkDefer`] one more `deferred_count` on each task it
    /// names, as one group (ADR-0065 §6). Returns how many items it moved.
    ///
    /// It never re-solves. The inputs are gathered again and digested; any
    /// difference from the diff's base refuses with nothing written.
    ///
    /// # Errors
    /// [`PlanError::Stale`] when an input changed; [`PlanError::NotCommittable`]
    /// for a diff with a Blocking violation; [`PlanError::Engine`] for a write
    /// the domain refuses or a failed transaction.
    pub fn plan_apply(&self, db: &mut Db, diff: &PlanDiff, now_ms: u64) -> Result<u32, PlanError> {
        if !diff.committable {
            return Err(PlanError::NotCommittable);
        }
        let inputs = self.plan_inputs(db, &diff.intent, now_ms)?;
        if plan_base(&inputs) != diff.base {
            return Err(PlanError::Stale);
        }
        let write_ms = self.clock.now_ms();
        let defers = matches!(diff.intent, PlanIntent::BulkDefer { .. });
        let mut tasks: Vec<TaskWrite> = Vec::new();
        let mut blocks = Vec::new();
        let positions = diff.moves.iter().map(|m| (m.item, m.after.clone())).chain(
            diff.unscheduled
                .iter()
                .map(|u| (u.item, PlanPosition::Day(u.day))),
        );
        for (item, after) in positions {
            match item.kind() {
                EntityKind::Task => {
                    let planned = match after {
                        PlanPosition::Slot { start, .. } => Some(start),
                        PlanPosition::Day(d) => Some(SunriseTime::all_day(d)),
                        PlanPosition::Unplanned => None,
                    };
                    let conn = db.conn();
                    tasks.push(match (defers, planned) {
                        (true, Some(to)) => self.plan_defer(conn, item, to, write_ms)?,
                        (_, planned) => self.plan_update(
                            conn,
                            item,
                            TaskPatch {
                                planned_at: Some(planned),
                                ..TaskPatch::default()
                            },
                            write_ms,
                        )?,
                    });
                }
                EntityKind::Block => {
                    let PlanPosition::Slot { start, end } = after else {
                        return Err(PlanError::Invalid(format!(
                            "{item} is a block off the grid"
                        )));
                    };
                    let mut block = read_block(db.conn(), item.bytes())?
                        .ok_or_else(|| EngineError::NotFound(format!("block {item}")))?;
                    block.starts_at = start;
                    block.ends_at = end;
                    block.updated_at = ms_to_ts(write_ms as i64);
                    block
                        .validate_invariants(&self.device_zone())
                        .map_err(EngineError::from)?;
                    blocks.push(block);
                }
                _ => return Err(PlanError::Invalid(format!("{item} cannot be planned"))),
            }
        }
        let moved = u32::try_from(tasks.len() + blocks.len()).unwrap_or(u32::MAX);
        if moved == 0 {
            return Ok(0);
        }
        in_tx(db, |tx| {
            let mode = self.write_mode(tx, write_ms)?;
            let mut members: Vec<(InnerOp, [u8; 16])> = Vec::new();
            for w in &tasks {
                members.extend(w.op_in(self, tx, mode)?);
            }
            for b in &blocks {
                members.push((
                    InnerOp::BlockUpdate(Box::new(b.clone())),
                    domain_bytes(b.stream_id),
                ));
            }
            Ok(self.append_and_fold_group(tx, members, write_ms)?)
        })?;
        Ok(moved)
    }

    /// The [`PlanInputs`] of the planner days `intent` touches.
    pub(super) fn plan_inputs(
        &self,
        db: &Db,
        intent: &PlanIntent,
        now_ms: u64,
    ) -> Result<PlanInputs, PlanError> {
        let conn = db.conn();
        let tz = self.device_zone();
        let schedule = schedule_in(conn)?;
        let named = named(intent);
        // Where each subject is now: a task's plan, a block's start.
        let mut subjects: Vec<Option<SunriseTime>> = Vec::new();
        for id in &named {
            subjects.push(match id.kind() {
                EntityKind::Task => read_live_task(conn, *id)?.planned_at,
                EntityKind::Block => {
                    let block = read_block(conn, id.bytes())?
                        .filter(|b| !b.deleted)
                        .ok_or_else(|| PlanError::Invalid(format!("no block {id}")))?;
                    Some(block.starts_at)
                }
                _ => return Err(PlanError::Invalid(format!("{id} cannot be planned"))),
            });
        }
        // The planner days: each subject's own, and each the intent targets.
        let mut dates: BTreeSet<Date> = BTreeSet::new();
        for at in &subjects {
            if let Some(d) = at.as_ref().and_then(|v| planner_day_of(&schedule, v, &tz)) {
                dates.insert(d);
            }
        }
        let target = match intent {
            PlanIntent::MoveTask {
                to: SlotTarget::At(t),
                ..
            }
            | PlanIntent::MoveBlock { to_start: t, .. }
            | PlanIntent::ResizeBlock { to: t, .. } => planner_day_of(&schedule, t, &tz),
            PlanIntent::MoveTask {
                to: SlotTarget::Day(d),
                ..
            }
            | PlanIntent::BulkDefer { to: d, .. }
            | PlanIntent::AutoPlace { day: d, .. } => Some(*d),
            PlanIntent::Unschedule { .. } => None,
        };
        dates.extend(target);
        let mut window = Vec::with_capacity(dates.len());
        for date in &dates {
            window.push(plan_day(&schedule, *date, &tz)?);
        }
        let (from, to) = match (window.first(), window.last()) {
            (Some(a), Some(b)) => (a.day.0.as_millisecond(), b.day.1.as_millisecond()),
            _ => (0, 0),
        };

        // Open tasks planned in the window, and every subject.
        let mut ids: BTreeSet<EntityRef> = named
            .iter()
            .copied()
            .filter(|id| id.kind() == EntityKind::Task)
            .collect();
        if !window.is_empty() {
            // The index prefilter is widened by the slack, and each row's
            // plan is resolved from its columns before the whole row is read,
            // so a neighbouring day costs one cheap row each.
            let mut stmt = conn.prepare(
                "SELECT id, planned_at_ms, planned_at_kind, planned_at_tz, planned_at_civil
                 FROM tasks
                 WHERE deleted = 0 AND archived = 0
                   AND state != 'done' AND state != 'cancelled'
                   AND planned_at_ms >= ?1 AND planned_at_ms < ?2",
            )?;
            let rows = stmt.query_map(
                params![
                    from.saturating_sub(PREFILTER_SLACK_MS),
                    to.saturating_add(PREFILTER_SLACK_MS)
                ],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        time_from_parts(
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, Option<String>>(2)?.as_deref(),
                            r.get::<_, Option<String>>(3)?.as_deref(),
                            r.get::<_, Option<String>>(4)?.as_deref(),
                        ),
                    ))
                },
            )?;
            for row in rows {
                let (raw, planned) = row?;
                let in_window = planned
                    .as_ref()
                    .and_then(|v| planner_day_of(&schedule, v, &tz))
                    .is_some_and(|d| dates.contains(&d));
                if in_window {
                    ids.insert(EntityRef::new(EntityKind::Task, blob16(&raw)));
                }
            }
        }
        let mut tasks = Vec::new();
        for id in ids {
            let Some(task) = read_task(conn, id.bytes())? else {
                continue;
            };
            let subject = named.contains(&id);
            if !subject {
                let in_window = task
                    .planned_at
                    .as_ref()
                    .and_then(|v| planner_day_of(&schedule, v, &tz))
                    .is_some_and(|d| dates.contains(&d));
                if !in_window || task.deleted || !is_open(&task) {
                    continue;
                }
            }
            tasks.push(self.plan_task(conn, &schedule, &task)?);
        }

        // Blocks overlapping the window, and every subject. Every block is
        // fixed until a Block carries its fixed/flexible kind, which no build
        // has yet.
        let mut blocks = Vec::new();
        let mut seen = BTreeSet::new();
        if !window.is_empty() {
            for row in self.blocks_overlapping(conn, from, to)? {
                seen.insert(row.id);
                blocks.push(PlanBlock {
                    id: row.id,
                    starts_at: row.starts_at,
                    ends_at: row.ends_at,
                    fixed: true,
                });
            }
        }
        for id in named.iter().filter(|id| id.kind() == EntityKind::Block) {
            if seen.contains(id) {
                continue;
            }
            if let Some(b) = read_block(conn, id.bytes())? {
                blocks.push(PlanBlock {
                    id: b.id,
                    starts_at: b.starts_at,
                    ends_at: b.ends_at,
                    fixed: true,
                });
            }
        }

        let pref = |key: &str, default: u32| -> Result<u32, PlanError> {
            Ok(match self.resolved_preference(db, key)? {
                Some(PrefValue::Uint(v)) => u32::try_from(v).unwrap_or(default),
                _ => default,
            })
        };
        Ok(PlanInputs {
            now: ms_to_ts(i64::try_from(now_ms).unwrap_or(i64::MAX)),
            zone: tz,
            window,
            tasks,
            blocks,
            // External events join as fixed inputs with #930.
            events: Vec::new(),
            snap_s: pref("planner.snap_s", DEFAULT_SNAP_S)?,
            default_task_duration_s: pref(
                "planner.default_task_duration_s",
                DEFAULT_TASK_DURATION_S,
            )?,
            min_gap_s: pref("planner.min_gap_s", 0)?,
            search_budget: SEARCH_BUDGET,
        })
    }

    /// One task as the planner reads it: deadlines at their due instants,
    /// and its open blockers with their planned ends.
    fn plan_task(
        &self,
        conn: &rusqlite::Connection,
        schedule: &DaySchedule,
        task: &Task,
    ) -> Result<PlanTask, PlanError> {
        let tz = self.device_zone();
        let mut blocked_by = Vec::new();
        for b in &task.blocked_by {
            let Some(blocker) = read_task(conn, b.bytes())? else {
                continue;
            };
            if blocker.deleted || !is_open(&blocker) {
                continue;
            }
            let end = blocker.planned_at.as_ref().and_then(|p| match p {
                SunriseTime::AllDay { .. } => None,
                _ => p.resolve_in(&tz).and_then(|s| {
                    let est = blocker
                        .estimated_duration_s
                        .filter(|&e| e > 0)
                        .unwrap_or(u64::from(DEFAULT_TASK_DURATION_S));
                    s.checked_add(jiff::SignedDuration::from_secs(
                        i64::try_from(est).unwrap_or(0),
                    ))
                    .ok()
                }),
            });
            blocked_by.push(PlanBlocker { task: *b, end });
        }
        Ok(PlanTask {
            id: task.id,
            planned_at: task.planned_at.clone(),
            estimate_s: task.estimated_duration_s,
            target_due: task
                .target_at
                .as_ref()
                .and_then(|v| deadline_at(schedule, v, &tz)),
            hard_due: task
                .hard_due_at
                .as_ref()
                .and_then(|v| deadline_at(schedule, v, &tz)),
            blocked_by,
            constraints: task.scheduling_constraints.clone(),
            in_progress: task.state == TaskState::InProgress,
            bound: !task.blocks.is_empty(),
        })
    }

    /// Every live block overlapping `[from, to)` epoch milliseconds, resolved
    /// in the reader's zone.
    fn blocks_overlapping(
        &self,
        conn: &rusqlite::Connection,
        from: i64,
        to: i64,
    ) -> Result<Vec<sunrise_domain::Block>, PlanError> {
        let tz = self.device_zone();
        let mut stmt = conn.prepare(
            "SELECT id FROM blocks WHERE deleted = 0 AND starts_at_ms < ? AND ends_at_ms > ?",
        )?;
        let ids = stmt
            .query_map(
                params![
                    to.saturating_add(PREFILTER_SLACK_MS),
                    from.saturating_sub(PREFILTER_SLACK_MS)
                ],
                |r| r.get::<_, Vec<u8>>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for raw in ids {
            let Some(block) = read_block(conn, &blob16(&raw))? else {
                continue;
            };
            if let (Some(s), Some(e)) = (
                block.starts_at.resolve_in(&tz),
                block.ends_at.resolve_in(&tz),
            ) {
                if s.as_millisecond() < to && e.as_millisecond() > from {
                    out.push(block);
                }
            }
        }
        Ok(out)
    }
}

/// The task an intent names, which must exist and not be deleted.
fn read_live_task(conn: &rusqlite::Connection, id: EntityRef) -> Result<Task, PlanError> {
    read_task(conn, id.bytes())?
        .filter(|t| !t.deleted)
        .ok_or_else(|| PlanError::Invalid(format!("no task {id}")))
}

fn is_open(task: &Task) -> bool {
    !matches!(task.state, TaskState::Done | TaskState::Cancelled)
}

/// Every item the intent names, in order of first mention.
fn named(intent: &PlanIntent) -> BTreeSet<EntityRef> {
    match intent {
        PlanIntent::MoveTask { task, .. } => [*task].into(),
        PlanIntent::MoveBlock { block, .. } | PlanIntent::ResizeBlock { block, .. } => {
            [*block].into()
        }
        PlanIntent::BulkDefer { tasks, .. } | PlanIntent::AutoPlace { tasks, .. } => {
            tasks.iter().copied().collect()
        }
        PlanIntent::Unschedule { items, .. } => items.iter().copied().collect(),
    }
}

/// Planner day `date` for the reader: its bounds, and its plannable window,
/// the whole day when the date has no schedule.
fn plan_day(
    schedule: &DaySchedule,
    date: Date,
    tz: &jiff::tz::TimeZone,
) -> Result<PlanDay, PlanError> {
    let calendar = |e: sunrise_domain::OutOfCalendar| {
        PlanError::Engine(EngineError::Invalid(format!("planner day: {e}")))
    };
    let day = day_bounds(schedule, date, tz).map_err(calendar)?;
    let window = plannable(schedule, date, tz).map_err(calendar)?;
    Ok(PlanDay {
        date,
        day,
        plannable: window.unwrap_or(day),
    })
}
