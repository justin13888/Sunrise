//! The planner's seam on the engine (`docs/08-features/planner.md`,
//! ADR-0048, #343): gather the inputs of the planner days a gesture touches,
//! run the pure solver over them, and write a previewed diff back as one
//! group of per-field ops.
//!
//! Nothing here decides a position. [`sunrise_domain::planner::solve`] does,
//! and this module only reads rows into its inputs and writes its answer.

use super::block::{move_anchor, read_block};
use super::day_schedule::schedule_in;
use super::ids::{blob16, domain_bytes, ms_to_ts, time_from_parts, time_to_parts};
use super::snapshot::in_tx;
use super::task::{read_task, TaskWrite};
use super::{Engine, EngineError};
use crate::inner_op::InnerOp;
use jiff::civil::Date;
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::planner::{
    plan_base, retime, solve, OccurrenceScope, PlanBlock, PlanBlocker, PlanDay, PlanDiff,
    PlanInputs, PlanIntent, PlanOccurrence, PlanPosition, PlanTask, SlotTarget, DEFAULT_SNAP_S,
    DEFAULT_TASK_DURATION_S, SEARCH_BUDGET,
};
use sunrise_domain::time::PREFILTER_SLACK_MS;
use sunrise_domain::{
    day_bounds, deadline_at, expand_block, plannable, planner_day_of, Block, BlockException,
    BlockKind, DaySchedule, PrefValue, SunriseTime, Task, TaskPatch, TaskState,
};
use sunrise_error::ErrorCode;
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
    /// The intent names nothing, or the wrong kind; or, at commit, a block
    /// move would leave a recurring block's exception applying to nothing.
    #[error("invalid plan intent: {0}")]
    Invalid(String),
    /// The vault is closed.
    #[error("the vault is closed")]
    Closed,
    /// The read or the write failed.
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Names one planner commit, whose inverse [`Engine::plan_undo`] restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlanCommitId(pub [u8; 16]);

/// How many planner commits keep their inverse: committing one more forgets
/// the oldest, whose undo is then `PLAN_UNKNOWN`.
pub const PLAN_UNDO_HISTORY: u32 = 64;

/// A field a planner commit writes, and its undo restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlanField {
    /// A task's `planned_at`.
    PlannedAt,
    /// A block's `starts_at`.
    StartsAt,
    /// A block's `ends_at`.
    EndsAt,
    /// The start one occurrence's exception overrides, of a recurring block
    /// a drag of that event alone moved.
    OccurrenceStartsAt,
    /// The end one occurrence's exception overrides.
    OccurrenceEndsAt,
}

impl PlanField {
    /// The field's register name (`merge_registers.field`), or for an
    /// occurrence's field, the name of its part of the exception.
    const fn register(self) -> &'static str {
        match self {
            Self::PlannedAt => "scheduled_at",
            Self::StartsAt | Self::OccurrenceStartsAt => "starts_at",
            Self::EndsAt | Self::OccurrenceEndsAt => "ends_at",
        }
    }

    const fn of_occurrence(self) -> bool {
        matches!(self, Self::OccurrenceStartsAt | Self::OccurrenceEndsAt)
    }
}

/// The field of the exceptions map an occurrence's fields live in.
const EXCEPTIONS: &str = "exceptions";

/// One field a commit writes on one item: a register, or one occurrence's
/// part of a recurring block's exception, whose stamp is its map entry's
/// (`merge_map_entries`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Written {
    field: PlanField,
    /// The occurrence key, for an occurrence's field.
    key: Option<String>,
}

impl Written {
    const fn plain(field: PlanField) -> Self {
        Self { field, key: None }
    }

    /// The name `planner_commit_fields.field` stores: the register's, or
    /// `exceptions[KEY].starts_at` for an occurrence's.
    fn column(&self) -> String {
        match &self.key {
            Some(key) => format!("{EXCEPTIONS}[{key}].{}", self.field.register()),
            None => self.field.register().to_owned(),
        }
    }

    fn from_column(name: &str) -> Option<Self> {
        if let Some(rest) = name.strip_prefix("exceptions[") {
            let (key, part) = rest.rsplit_once("].")?;
            let field = [PlanField::OccurrenceStartsAt, PlanField::OccurrenceEndsAt]
                .into_iter()
                .find(|f| f.register() == part)?;
            return Some(Self {
                field,
                key: Some(key.to_owned()),
            });
        }
        [PlanField::PlannedAt, PlanField::StartsAt, PlanField::EndsAt]
            .into_iter()
            .find(|f| f.register() == name)
            .map(Self::plain)
    }
}

/// A field an undo left as it is, because it was written again since the
/// commit, or its item is gone or would be invalid with it restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanUndoSkip {
    /// The task or block.
    pub item: EntityRef,
    /// The field.
    pub field: PlanField,
}

/// What [`Engine::plan_undo`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanUndone {
    /// The items it wrote.
    pub items: Vec<EntityRef>,
    /// How many fields it restored.
    pub restored: u32,
    /// The fields it left as they are.
    pub skipped: Vec<PlanUndoSkip>,
}

/// One field a commit is about to write: the value it replaces.
struct Prior {
    item: EntityRef,
    field: Written,
    value: Option<SunriseTime>,
    /// A deferral: restoring the field also takes one off `deferred_count`.
    undefer: bool,
}

/// The stamp a register holds: `(hlc_ms, hlc_logical, device, seq, stream)`
/// from `merge_registers`, or for an occurrence's field from its exception's
/// `merge_map_entries` row, compared whole.
type RegisterStamp = (i64, i64, Vec<u8>, i64, Vec<u8>);

/// The stamp of `item`'s `field` in `tx`, or `None` when it has no register
/// row (an occurrence with no exception has none).
fn register_stamp(
    tx: &Transaction<'_>,
    item: EntityRef,
    field: &Written,
) -> rusqlite::Result<Option<RegisterStamp>> {
    let row = |r: &rusqlite::Row<'_>| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?));
    match &field.key {
        Some(key) => tx
            .query_row(
                "SELECT hlc_ms, hlc_logical, device, seq, stream FROM merge_map_entries
                 WHERE entity_id = ?1 AND field = ?2 AND map_key = ?3",
                params![&item.bytes()[..], EXCEPTIONS, key],
                row,
            )
            .optional(),
        None => tx
            .query_row(
                "SELECT hlc_ms, hlc_logical, device, seq, stream FROM merge_registers
                 WHERE entity_id = ?1 AND field = ?2",
                params![&item.bytes()[..], field.field.register()],
                row,
            )
            .optional(),
    }
}

impl From<rusqlite::Error> for PlanError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Engine(e.into())
    }
}

impl PlanError {
    /// The registry's error code a client branches on
    /// (`crates/sunrise-error/codes.toml`).
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Stale => ErrorCode::PlanStale,
            Self::Unknown => ErrorCode::PlanUnknown,
            Self::NotCommittable => ErrorCode::PlanNotCommittable,
            Self::Invalid(_) => ErrorCode::PlanInvalid,
            Self::Closed => ErrorCode::PlanClosed,
            Self::Engine(_) => ErrorCode::PlanFailed,
        }
    }
}

impl Engine {
    /// Solve `intent` against the vault as it stands at `now_ms`, in the
    /// reader's zone. Writes nothing.
    ///
    /// A recurring block is read as its occurrences in the window, and a
    /// block intent on one names the occurrence it drags
    /// (`docs/08-features/planner.md` §Recurring blocks).
    ///
    /// A block move the commit would refuse because it strands a recurring
    /// block's exception (`move_anchor`), or a drag of every event from one
    /// an exception already moved on its own, is flagged here, as a Blocking
    /// `Invalid` violation on the block, and the diff is not committable.
    ///
    /// # Errors
    /// [`PlanError::Invalid`] for an intent naming nothing the vault holds,
    /// or the wrong kind, or a recurring block without one of its
    /// occurrences, a cancelled one, or one its rule does not produce;
    /// [`PlanError::Engine`] for a failed read.
    pub fn plan_preview(
        &self,
        db: &Db,
        intent: &PlanIntent,
        now_ms: u64,
    ) -> Result<PlanDiff, PlanError> {
        let inputs = self.plan_inputs(db, intent, now_ms)?;
        let mut diff = solve(&inputs, intent).map_err(|e| PlanError::Invalid(e.to_string()))?;
        let tz = self.device_zone();
        let mut refused = Vec::new();
        for m in &diff.moves {
            let PlanPosition::Slot { start, end } = &m.after else {
                continue;
            };
            if m.item.kind() != EntityKind::Block {
                continue;
            }
            let Some(mut block) = read_block(db.conn(), m.item.bytes())? else {
                continue;
            };
            let occurrence = m.occurrence.as_ref().and(diff.intent.occurrence());
            let placed = place_block(
                &mut block,
                occurrence,
                &m.before,
                start.clone(),
                end.clone(),
                &tz,
            );
            if let Err(PlanError::Invalid(_)) = placed {
                refused.push(m.item);
            }
        }
        for item in refused {
            diff.refuse(item);
        }
        Ok(diff)
    }

    /// Write `diff`, previewed at `now_ms`, exactly: `planned_at` for each
    /// task, `starts_at` and `ends_at` for each block, and for a
    /// [`PlanIntent::BulkDefer`] one more `deferred_count` on each task it
    /// names, as one group (ADR-0065 §6). A drag of one event of a recurring
    /// block writes that occurrence's exception instead; a drag of every
    /// event moves the anchor by as much as the event moved, and that write,
    /// like any move of an anchor, re-keys the block's `exceptions` and
    /// `occurrence_tasks` to the occurrences they named, as an edit of the
    /// whole block does (`move_anchor`). Returns how many items it moved, and
    /// the commit whose inverse it stored.
    ///
    /// In the same transaction it stores the commit's inverse
    /// (`planner_commits`, `planner_commit_fields`): for each field whose
    /// register the write changed, the value it replaced and the stamp the
    /// write left. [`Self::plan_undo`] restores from it. Only the newest
    /// [`PLAN_UNDO_HISTORY`] commits are kept.
    ///
    /// It never re-solves. The inputs are gathered again and digested; any
    /// difference from the diff's base refuses with nothing written.
    ///
    /// # Errors
    /// [`PlanError::Stale`] when an input changed; [`PlanError::NotCommittable`]
    /// for a diff with a Blocking violation; [`PlanError::Invalid`] for a
    /// block move that would leave a recurring block's exception applying to
    /// nothing, which [`Self::plan_preview`] flags unless the block changed
    /// since in a way the diff's base does not digest (its rule);
    /// [`PlanError::Engine`] for another write the domain refuses or a failed
    /// transaction.
    #[allow(
        clippy::too_many_lines,
        reason = "its steps run in order over shared locals; splitting it is a refactor of its own"
    )]
    pub fn plan_apply(
        &self,
        db: &mut Db,
        diff: &PlanDiff,
        now_ms: u64,
    ) -> Result<(u32, PlanCommitId), PlanError> {
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
        let mut priors: Vec<Prior> = Vec::new();
        // Each written block as it stood, and the fields this commit stores
        // for it, so the rest of what its full-state write renews is carried.
        let mut carried: Vec<(Block, Vec<Written>)> = Vec::new();
        let positions = diff
            .moves
            .iter()
            .map(|m| (m.item, m.occurrence.is_some(), &m.before, m.after.clone()))
            .chain(
                diff.unscheduled
                    .iter()
                    .map(|u| (u.item, false, &u.before, PlanPosition::Day(u.day))),
            );
        for (item, of_occurrence, before, after) in positions {
            match item.kind() {
                EntityKind::Task => {
                    let planned = match after {
                        PlanPosition::Slot { start, .. } => Some(start),
                        PlanPosition::Day(d) => Some(SunriseTime::all_day(d)),
                        PlanPosition::Unplanned => None,
                    };
                    let conn = db.conn();
                    let prior = read_task(conn, item.bytes())?.and_then(|t| t.planned_at);
                    priors.push(Prior {
                        item,
                        field: Written::plain(PlanField::PlannedAt),
                        value: prior,
                        undefer: defers && planned.is_some(),
                    });
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
                    let occurrence = diff.intent.occurrence().filter(|_| of_occurrence);
                    let stored: Vec<(Written, Option<SunriseTime>)> = match occurrence {
                        // One event: its exception's start and end.
                        Some(o) if o.scope == OccurrenceScope::This => {
                            let x = block.exceptions.get(&o.key);
                            let key = Some(o.key.clone());
                            vec![
                                (
                                    Written {
                                        field: PlanField::OccurrenceStartsAt,
                                        key: key.clone(),
                                    },
                                    x.and_then(|x| x.starts_at.clone()),
                                ),
                                (
                                    Written {
                                        field: PlanField::OccurrenceEndsAt,
                                        key,
                                    },
                                    x.and_then(|x| x.ends_at.clone()),
                                ),
                            ]
                        }
                        // The block, or every event: its anchor.
                        _ => vec![
                            (
                                Written::plain(PlanField::StartsAt),
                                Some(block.starts_at.clone()),
                            ),
                            (
                                Written::plain(PlanField::EndsAt),
                                Some(block.ends_at.clone()),
                            ),
                        ],
                    };
                    let was = block.clone();
                    place_block(
                        &mut block,
                        occurrence,
                        before,
                        start,
                        end,
                        &self.device_zone(),
                    )?;
                    block.updated_at = ms_to_ts(write_ms.cast_signed());
                    carried.push((was, stored.iter().map(|(w, _)| w.clone()).collect()));
                    priors.extend(stored.into_iter().map(|(field, value)| Prior {
                        item,
                        field,
                        value,
                        undefer: false,
                    }));
                    blocks.push(block);
                }
                _ => return Err(PlanError::Invalid(format!("{item} cannot be planned"))),
            }
        }
        let moved = u32::try_from(tasks.len() + blocks.len()).unwrap_or(u32::MAX);
        let mut commit = [0u8; 16];
        self.rng.fill_bytes(&mut commit);
        let commit = PlanCommitId(commit);
        let renewed: Vec<(EntityRef, Vec<Written>)> = carried
            .iter()
            .zip(&blocks)
            .map(|((was, own), now)| (now.id, renewed(was, now, own)))
            .collect();
        in_tx(db, |tx| {
            let before = priors
                .iter()
                .map(|p| register_stamp(tx, p.item, &p.field))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let kept = stamps_of(tx, &renewed)?;
            if moved > 0 {
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
                self.append_and_fold_group(tx, members, write_ms)?;
            }
            carry(tx, kept)?;
            record_inverse(tx, commit, write_ms, &priors, &before)?;
            Ok(())
        })?;
        Ok((moved, commit))
    }

    /// Undo planner commit `commit` from its stored inverse, per field, in
    /// one local transaction (`docs/08-features/planner.md` §Undo).
    ///
    /// A field whose register still holds the commit's write is restored to
    /// the value the commit replaced, as a new write; a restored deferral
    /// also takes one off `deferred_count`, and a drag of one event of a
    /// recurring block restores that occurrence's exception's start and end
    /// (skipped where the exception is gone or cancelled since). A field
    /// this undo's write renews and leaves as it was keeps every other kept
    /// commit's hold on it. A field written again since, by
    /// any device, is skipped and reported, as is every field of an item
    /// deleted since or one its restored fields would leave invalid. Never
    /// refuses as stale and never re-solves. The commit is forgotten: a
    /// second undo of it is [`PlanError::Unknown`].
    ///
    /// Undos of commits on one item restore newest-first: a field an older
    /// kept commit wrote, and this one wrote over directly, is held again by
    /// that older commit once this undo writes the field back. Where this
    /// undo leaves the field as it is, the older commit's write stays
    /// written over, and its undo skips the field.
    ///
    /// # Errors
    /// [`PlanError::Unknown`] for a commit that is not kept (undone already,
    /// forgotten past [`PLAN_UNDO_HISTORY`], or never made);
    /// [`PlanError::Engine`] for a failed transaction.
    #[allow(
        clippy::too_many_lines,
        reason = "its steps run in order over shared locals; splitting it is a refactor of its own"
    )]
    pub fn plan_undo(&self, db: &mut Db, commit: PlanCommitId) -> Result<PlanUndone, PlanError> {
        let write_ms = self.clock.now_ms();
        let tz = self.device_zone();
        let done = in_tx(db, |tx| {
            let Some((n, by_item)) = held_inverse(tx, commit)? else {
                return Ok(None);
            };
            let mut out = PlanUndone::default();
            let mut members: Vec<(InnerOp, [u8; 16])> = Vec::new();
            // Each stored row, and whether this undo writes it back.
            let mut relinks: Vec<(InverseRow, bool)> = Vec::new();
            // What each block's write renews and leaves as it was.
            let mut renewing: Vec<(EntityRef, Vec<Written>)> = Vec::new();
            for (item, fields) in by_item {
                let mut restored = 0u32;
                let mut skipped: Vec<PlanUndoSkip> = Vec::new();
                // The fields this undo writes back.
                let mut put_back: Vec<Written> = Vec::new();
                let op = match item.kind() {
                    EntityKind::Task => {
                        let task = read_task(tx, item.bytes())?.filter(|t| !t.deleted);
                        task.and_then(|mut task| {
                            for row in &fields {
                                if row.restorable() && row.field.field == PlanField::PlannedAt {
                                    task.planned_at.clone_from(&row.prior);
                                    if row.undefer {
                                        task.deferred_count = task.deferred_count.saturating_sub(1);
                                    }
                                    put_back.push(row.field.clone());
                                    restored += 1;
                                } else {
                                    skipped.push(PlanUndoSkip {
                                        item,
                                        field: row.field.field,
                                    });
                                }
                            }
                            task.updated_at = ms_to_ts(write_ms.cast_signed());
                            task.validate_invariants(&tz).ok()?;
                            Some(TaskWrite::plain(task).op())
                        })
                    }
                    EntityKind::Block => {
                        let block = read_block(tx, item.bytes())?.filter(|b| !b.deleted);
                        block.and_then(|mut block| {
                            let before = block.clone();
                            let anchor = block.starts_at.clone();
                            let was = block.series();
                            for row in &fields {
                                let restorable = row.restorable();
                                let exception = row
                                    .field
                                    .key
                                    .as_ref()
                                    .and_then(|k| block.exceptions.get_mut(k))
                                    .filter(|x| !x.cancelled);
                                match (row.field.field, &row.prior, restorable, exception) {
                                    (PlanField::StartsAt, Some(v), true, _) => {
                                        block.starts_at = v.clone();
                                    }
                                    (PlanField::EndsAt, Some(v), true, _) => {
                                        block.ends_at = v.clone();
                                    }
                                    (PlanField::OccurrenceStartsAt, v, true, Some(x)) => {
                                        x.starts_at.clone_from(v);
                                    }
                                    (PlanField::OccurrenceEndsAt, v, true, Some(x)) => {
                                        x.ends_at.clone_from(v);
                                    }
                                    _ => {
                                        skipped.push(PlanUndoSkip {
                                            item,
                                            field: row.field.field,
                                        });
                                        continue;
                                    }
                                }
                                put_back.push(row.field.clone());
                                restored += 1;
                            }
                            block.updated_at = ms_to_ts(write_ms.cast_signed());
                            // The restored anchor carries the bindings and
                            // exceptions back to the occurrences they named;
                            // one it would strand leaves the block as it is,
                            // as does an occurrence it would leave ending
                            // before it starts.
                            move_anchor(&mut block, &anchor, was, &tz).ok()?;
                            for row in fields.iter().filter(|r| r.field.field.of_occurrence()) {
                                let key = row.field.key.as_deref().unwrap_or_default();
                                occurrence_at(&block, key, &tz).ok()?;
                            }
                            let own: Vec<Written> =
                                fields.iter().map(|r| r.field.clone()).collect();
                            renewing.push((item, renewed(&before, &block, &own)));
                            let stream = domain_bytes(block.stream_id);
                            Some((InnerOp::BlockUpdate(Box::new(block)), stream))
                        })
                    }
                    _ => None,
                };
                let written = op.is_some() && restored > 0;
                match op {
                    Some(op) if restored > 0 => {
                        members.push(op);
                        out.items.push(item);
                        out.restored += restored;
                        out.skipped.extend(skipped);
                    }
                    Some(_) => out.skipped.extend(skipped),
                    // Gone, or invalid with its fields restored: every field
                    // is left as it is.
                    None => out.skipped.extend(fields.iter().map(|row| PlanUndoSkip {
                        item,
                        field: row.field.field,
                    })),
                }
                relinks.extend(fields.into_iter().map(|row| {
                    let back = written && put_back.contains(&row.field);
                    (row, back)
                }));
            }
            let kept = stamps_of(tx, &renewing)?;
            if !members.is_empty() {
                self.append_and_fold_group(tx, members, write_ms)?;
            }
            carry(tx, kept)?;
            relink(tx, n, relinks)?;
            tx.execute(
                "DELETE FROM planner_commits WHERE commit_id = ?1",
                params![&commit.0[..]],
            )?;
            Ok(Some(out))
        })?;
        done.ok_or(PlanError::Unknown)
    }

    /// The [`PlanInputs`] of the planner days `intent` touches.
    #[allow(
        clippy::too_many_lines,
        reason = "its steps run in order over shared locals; splitting it is a refactor of its own"
    )]
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
        // Where each subject is now: a task's plan, a block's start, or the
        // start of the occurrence of a recurring block the intent names.
        let mut subjects: Vec<Option<SunriseTime>> = Vec::new();
        // That occurrence, which the window must hold wherever it is.
        let mut dragged: Option<PlanBlock> = None;
        for id in &named {
            subjects.push(match id.kind() {
                EntityKind::Task => read_live_task(conn, *id)?.planned_at,
                EntityKind::Block => {
                    let block = read_block(conn, id.bytes())?
                        .filter(|b| !b.deleted)
                        .ok_or_else(|| PlanError::Invalid(format!("no block {id}")))?;
                    match (intent.occurrence(), block.rrule.is_some()) {
                        (None, false) => Some(block.starts_at),
                        (Some(o), true) => {
                            let (s, e) = occurrence_at(&block, &o.key, &tz)?;
                            dragged = Some(plan_occurrence(&block, o.key.clone(), s.clone(), e));
                            Some(s)
                        }
                        (None, true) => {
                            return Err(PlanError::Invalid(format!(
                                "block {id} repeats: a drag names one of its occurrences"
                            )))
                        }
                        (Some(_), false) => {
                            return Err(PlanError::Invalid(format!("block {id} does not repeat")))
                        }
                    }
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

        // Blocks and occurrences overlapping the window, and every subject.
        let mut blocks = Vec::new();
        let mut seen = BTreeSet::new();
        if !window.is_empty() {
            for b in self.blocks_overlapping(conn, from, to)? {
                seen.insert((b.id, b.occurrence.clone()));
                blocks.push(b);
            }
        }
        if let Some(b) = dragged {
            if !seen.contains(&(b.id, b.occurrence.clone())) {
                blocks.push(b);
            }
        } else {
            for id in named.iter().filter(|id| id.kind() == EntityKind::Block) {
                if seen.contains(&(*id, None)) {
                    continue;
                }
                if let Some(b) = read_block(conn, id.bytes())? {
                    blocks.push(plan_block(b));
                }
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
    /// in the reader's zone, and every occurrence of a recurring one that
    /// does, each named by its key, as the calendar query expands them.
    fn blocks_overlapping(
        &self,
        conn: &rusqlite::Connection,
        from: i64,
        to: i64,
    ) -> Result<Vec<PlanBlock>, PlanError> {
        let tz = self.device_zone();
        let mut stmt = conn.prepare(
            "SELECT id FROM blocks WHERE deleted = 0 AND starts_at_ms < ?1 AND ends_at_ms > ?2
             UNION
             SELECT id FROM blocks WHERE deleted = 0 AND rrule_cbor IS NOT NULL",
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
        let span = (ms_to_ts(from), ms_to_ts(to));
        let mut out = Vec::new();
        for raw in ids {
            let Some(block) = read_block(conn, &blob16(&raw))? else {
                continue;
            };
            if let Some(series) = block.series() {
                // A series this build cannot expand places nothing, as the
                // calendar query places nothing for it.
                for occ in expand_block(&series, span, &tz).unwrap_or_default() {
                    out.push(plan_occurrence(&block, occ.key, occ.starts_at, occ.ends_at));
                }
                continue;
            }
            if let (Some(s), Some(e)) = (
                block.starts_at.resolve_in(&tz),
                block.ends_at.resolve_in(&tz),
            ) {
                if s.as_millisecond() < to && e.as_millisecond() > from {
                    out.push(plan_block(block));
                }
            }
        }
        Ok(out)
    }
}

/// Store commit `commit`'s inverse: each field of `priors` whose register
/// the write changed from its stamp in `before`, with the value it replaced
/// and the stamp it left; an older commit's row this write replaced directly
/// takes that stamp too (see [`restamp`]). Then forget all but the newest
/// [`PLAN_UNDO_HISTORY`] commits.
fn record_inverse(
    tx: &Transaction<'_>,
    commit: PlanCommitId,
    write_ms: u64,
    priors: &[Prior],
    before: &[Option<RegisterStamp>],
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO planner_commits (commit_id, committed_ms) VALUES (?1, ?2)",
        params![&commit.0[..], i64::try_from(write_ms).unwrap_or(i64::MAX)],
    )?;
    let n = tx.last_insert_rowid();
    for (p, was) in priors.iter().zip(before) {
        let Some(now) = register_stamp(tx, p.item, &p.field)? else {
            continue;
        };
        if was.as_ref() == Some(&now) {
            continue;
        }
        // An older commit whose write this one replaces directly now holds
        // this one's stamp: it is restorable again once this one is undone.
        if let Some(was) = was {
            restamp(tx, n, p.item, &p.field, was, Some(&now))?;
        }
        let (ms, kind, zone, civil) = time_to_parts(p.value.as_ref());
        let (hlc_ms, hlc_logical, device, seq, stream) = now;
        tx.execute(
            "INSERT INTO planner_commit_fields
                 (commit_id, entity_id, field, prior_ms, prior_kind, prior_tz, prior_civil,
                  hlc_ms, hlc_logical, device, seq, stream, undefer)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                &commit.0[..],
                &p.item.bytes()[..],
                p.field.column(),
                ms,
                kind,
                zone,
                civil,
                hlc_ms,
                hlc_logical,
                device,
                seq,
                stream,
                p.undefer,
            ],
        )?;
    }
    tx.execute(
        "DELETE FROM planner_commits
         WHERE n <= (SELECT MAX(n) FROM planner_commits) - ?1",
        params![PLAN_UNDO_HISTORY],
    )?;
    Ok(())
}

/// The stamp a stored row is given when no register can ever hold it again:
/// no HLC is negative.
static NEVER_HELD: RegisterStamp = (-1, -1, Vec::new(), -1, Vec::new());

/// Move every stored row of `item`'s `field` that holds stamp `from`, of a
/// commit older than the one numbered `n`, to stamp `to`, or to
/// [`NEVER_HELD`] for `None`.
///
/// This is how successive undos restore newest-first: a commit that replaces
/// an older commit's write directly links the older row to its own stamp,
/// and its undo moves that row on to the stamp its restoring write left, or,
/// where it leaves the field as it is, to one no register holds.
fn restamp(
    tx: &Transaction<'_>,
    n: i64,
    item: EntityRef,
    field: &Written,
    from: &RegisterStamp,
    to: Option<&RegisterStamp>,
) -> rusqlite::Result<()> {
    let (to_ms, to_logical, to_device, to_seq, to_stream) = to.unwrap_or(&NEVER_HELD);
    let (ms, logical, device, seq, stream) = from;
    tx.execute(
        "UPDATE planner_commit_fields
         SET hlc_ms = ?1, hlc_logical = ?2, device = ?3, seq = ?4, stream = ?5
         WHERE entity_id = ?6 AND field = ?7
           AND hlc_ms = ?8 AND hlc_logical = ?9 AND device = ?10 AND seq = ?11
           AND stream = ?12
           AND commit_id IN (SELECT commit_id FROM planner_commits WHERE n < ?13)",
        params![
            to_ms,
            to_logical,
            to_device,
            to_seq,
            to_stream,
            &item.bytes()[..],
            field.column(),
            ms,
            logical,
            device,
            seq,
            stream,
            n,
        ],
    )?;
    Ok(())
}

/// Commit `commit`'s number and its stored inverse by item, each row's
/// `held` filled in, or `None` for a commit that is not kept.
#[allow(
    clippy::type_complexity,
    reason = "a pair read once, by its one caller"
)]
fn held_inverse(
    tx: &Transaction<'_>,
    commit: PlanCommitId,
) -> rusqlite::Result<Option<(i64, BTreeMap<EntityRef, Vec<InverseRow>>)>> {
    let kept: Option<i64> = tx
        .query_row(
            "SELECT n FROM planner_commits WHERE commit_id = ?1",
            params![&commit.0[..]],
            |r| r.get(0),
        )
        .optional()?;
    let Some(n) = kept else {
        return Ok(None);
    };
    let mut by_item: BTreeMap<EntityRef, Vec<InverseRow>> = BTreeMap::new();
    for mut row in inverse_rows(tx, commit)? {
        row.held = register_stamp(tx, row.item, &row.field)?.as_ref() == Some(&row.stamp)
            && !held_by_newer(tx, n, row.item, &row.field, &row.stamp)?;
        by_item.entry(row.item).or_default().push(row);
    }
    Ok(Some((n, by_item)))
}

/// After the undo of the commit numbered `n` wrote its group, move each
/// older commit's row its rows replaced directly: to the stamp the undo's
/// write left where it wrote the field back (`true`), so that older commit
/// holds the field again, and to [`NEVER_HELD`] where it left this commit's
/// write standing.
fn relink(tx: &Transaction<'_>, n: i64, rows: Vec<(InverseRow, bool)>) -> rusqlite::Result<()> {
    for (row, back) in rows {
        let now = if back {
            register_stamp(tx, row.item, &row.field)?
        } else {
            None
        };
        restamp(tx, n, row.item, &row.field, &row.stamp, now.as_ref())?;
    }
    Ok(())
}

/// The fields of a block a full-state write from `was` to `now` renews and
/// leaves as they were, other than `own`, the ones the commit or undo
/// writing it stores itself: the anchor's, and each exception's that the
/// write keeps under the same key.
fn renewed(was: &Block, now: &Block, own: &[Written]) -> Vec<Written> {
    let mut out = Vec::new();
    if was.starts_at == now.starts_at {
        out.push(Written::plain(PlanField::StartsAt));
    }
    if was.ends_at == now.ends_at {
        out.push(Written::plain(PlanField::EndsAt));
    }
    for (key, x) in &was.exceptions {
        if now.exceptions.get(key) == Some(x) {
            for field in [PlanField::OccurrenceStartsAt, PlanField::OccurrenceEndsAt] {
                out.push(Written {
                    field,
                    key: Some(key.clone()),
                });
            }
        }
    }
    out.retain(|w| !own.contains(w));
    out
}

/// One field a write renews and leaves as it was, and its stamp before the
/// write.
type Kept = (EntityRef, Written, RegisterStamp);

/// The stamps of the `renewed` fields of each item before a write.
fn stamps_of(
    tx: &Transaction<'_>,
    renewed: &[(EntityRef, Vec<Written>)],
) -> rusqlite::Result<Vec<Kept>> {
    let mut out = Vec::new();
    for (item, fields) in renewed {
        for field in fields {
            if let Some(stamp) = register_stamp(tx, *item, field)? {
                out.push((*item, field.clone(), stamp));
            }
        }
    }
    Ok(out)
}

/// After a planner write, move every kept commit's row of a field the write
/// renewed and left as it was on to the stamp the write left: the value
/// that row's commit wrote is still there, so its undo still restores it.
/// Without this, dragging one event of a series and then another would make
/// the first drag's undo skip a field nobody changed.
fn carry(tx: &Transaction<'_>, kept: Vec<Kept>) -> rusqlite::Result<()> {
    for (item, field, was) in kept {
        let now = register_stamp(tx, item, &field)?;
        if now.as_ref().is_some_and(|now| *now != was) {
            restamp(tx, i64::MAX, item, &field, &was, now.as_ref())?;
        }
    }
    Ok(())
}

/// Whether a commit kept after the one numbered `n` stores a row of `item`'s
/// `field` with stamp `stamp`: then that newer commit's write is the one the
/// register holds, and the older commit's is not.
fn held_by_newer(
    tx: &Transaction<'_>,
    n: i64,
    item: EntityRef,
    field: &Written,
    stamp: &RegisterStamp,
) -> rusqlite::Result<bool> {
    let (ms, logical, device, seq, stream) = stamp;
    tx.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM planner_commit_fields f
             JOIN planner_commits c ON c.commit_id = f.commit_id
             WHERE c.n > ?1 AND f.entity_id = ?2 AND f.field = ?3
               AND f.hlc_ms = ?4 AND f.hlc_logical = ?5 AND f.device = ?6
               AND f.seq = ?7 AND f.stream = ?8)",
        params![
            n,
            &item.bytes()[..],
            field.column(),
            ms,
            logical,
            device,
            seq,
            stream,
        ],
        |r| r.get(0),
    )
}

/// One stored field of a commit's inverse.
struct InverseRow {
    item: EntityRef,
    field: Written,
    prior: Option<SunriseTime>,
    stamp: RegisterStamp,
    undefer: bool,
    /// The register still holds `stamp`, and no newer kept commit stores
    /// it: filled in by the undo.
    held: bool,
}

impl InverseRow {
    /// The undo can write this field back: it is held, and a block's anchor
    /// always has a value to restore. A task's plan and an occurrence's
    /// override may be restored to none.
    fn restorable(&self) -> bool {
        self.held
            && (matches!(
                self.field.field,
                PlanField::PlannedAt | PlanField::OccurrenceStartsAt | PlanField::OccurrenceEndsAt
            ) || self.prior.is_some())
    }
}

/// Commit `commit`'s stored inverse. A row naming a field this build does
/// not plan is left out.
fn inverse_rows(tx: &Transaction<'_>, commit: PlanCommitId) -> rusqlite::Result<Vec<InverseRow>> {
    let mut stmt = tx.prepare(
        "SELECT entity_id, field, prior_ms, prior_kind, prior_tz, prior_civil,
                hlc_ms, hlc_logical, device, seq, stream, undefer
         FROM planner_commit_fields WHERE commit_id = ?1
         ORDER BY entity_id, field",
    )?;
    let rows = stmt.query_map(params![&commit.0[..]], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, String>(1)?,
            time_from_parts(
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<String>>(3)?.as_deref(),
                r.get::<_, Option<String>>(4)?.as_deref(),
                r.get::<_, Option<String>>(5)?.as_deref(),
            ),
            (r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?),
            r.get::<_, bool>(11)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (raw, field, prior, stamp, undefer) = row?;
        let Some(field) = Written::from_column(&field) else {
            continue;
        };
        let kind = match field.field {
            PlanField::PlannedAt => EntityKind::Task,
            PlanField::StartsAt
            | PlanField::EndsAt
            | PlanField::OccurrenceStartsAt
            | PlanField::OccurrenceEndsAt => EntityKind::Block,
        };
        out.push(InverseRow {
            item: EntityRef::new(kind, blob16(&raw)),
            field,
            prior,
            stamp,
            undefer,
            held: false,
        });
    }
    Ok(out)
}

/// A block as the planner reads it: fixed unless its effective kind is
/// `flexible` (`docs/02-domain/time-blocks.md` §Migration default). The solve
/// itself holds a past or in-progress block fixed whatever its kind.
fn plan_block(block: Block) -> PlanBlock {
    PlanBlock {
        fixed: block.effective_kind() != BlockKind::Flexible,
        id: block.id,
        occurrence: None,
        starts_at: block.starts_at,
        ends_at: block.ends_at,
    }
}

/// Occurrence `key` of recurring block `block` as the planner reads it.
fn plan_occurrence(
    block: &Block,
    key: String,
    starts_at: SunriseTime,
    ends_at: SunriseTime,
) -> PlanBlock {
    PlanBlock {
        fixed: block.effective_kind() != BlockKind::Flexible,
        id: block.id,
        occurrence: Some(key),
        starts_at,
        ends_at,
    }
}

/// Where occurrence `key` of recurring block `block` is: the rule's start
/// and end, under what its exception overrides.
///
/// # Errors
/// [`PlanError::Invalid`] for a block with no rule, a key its rule does not
/// produce, a cancelled occurrence, or one that ends at or before its start
/// for a reader in `tz`.
fn occurrence_at(
    block: &Block,
    key: &str,
    tz: &jiff::tz::TimeZone,
) -> Result<(SunriseTime, SunriseTime), PlanError> {
    let id = block.id;
    let series = block
        .series()
        .ok_or_else(|| PlanError::Invalid(format!("block {id} does not repeat")))?;
    series
        .check_key(key)
        .map_err(|e| PlanError::Invalid(format!("block {id}: {e}")))?;
    let exception = series.exceptions.get(key);
    if exception.is_some_and(|x| x.cancelled) {
        return Err(PlanError::Invalid(format!(
            "occurrence {key} of block {id} is cancelled"
        )));
    }
    let (rule_start, rule_end) = series
        .occurrence_bounds(key)
        .ok_or_else(|| PlanError::Invalid(format!("block {id} has no occurrence {key}")))?;
    let starts_at = exception
        .and_then(|x| x.starts_at.clone())
        .unwrap_or(rule_start);
    let ends_at = exception
        .and_then(|x| x.ends_at.clone())
        .unwrap_or(rule_end);
    match (starts_at.resolve_in(tz), ends_at.resolve_in(tz)) {
        (Some(s), Some(e)) if e > s => Ok((starts_at, ends_at)),
        _ => Err(PlanError::Invalid(format!(
            "occurrence {key} of block {id} ends before it starts"
        ))),
    }
}

/// Write a planner move of `block` to `[start, end)` onto it, as the commit
/// writes it, with the position it moves from, `before`:
///
/// - a block with no rule, or a rippled one: its start and end;
/// - one occurrence, [`OccurrenceScope::This`]: that occurrence's exception,
///   as an edit of one occurrence writes it;
/// - every occurrence, [`OccurrenceScope::All`]: the series' anchor, moved by
///   as much as the occurrence moved, as the calendar's edit of every event
///   moves it.
///
/// A move of the anchor carries the block's bindings and exceptions with it
/// ([`move_anchor`]).
///
/// # Errors
/// [`PlanError::Invalid`] for a write the block's invariants refuse that a
/// solve cannot see: a move that would strand an exception, a drag of every
/// event from one an exception already moved on its own, or an occurrence
/// that is not there to move; [`PlanError::Engine`] for another invariant.
fn place_block(
    block: &mut Block,
    occurrence: Option<&PlanOccurrence>,
    before: &PlanPosition,
    start: SunriseTime,
    end: SunriseTime,
    tz: &jiff::tz::TimeZone,
) -> Result<(), PlanError> {
    let invalid = |e: EngineError| match e {
        EngineError::Invalid(why) => PlanError::Invalid(why),
        other => PlanError::Engine(other),
    };
    let anchor = block.starts_at.clone();
    let was = block.series();
    let id = block.id;
    match occurrence {
        None => {
            block.starts_at = start;
            block.ends_at = end;
        }
        Some(o) if o.scope == OccurrenceScope::This => {
            let mut series =
                was.ok_or_else(|| PlanError::Invalid(format!("block {id} does not repeat")))?;
            let mut exception = series.exceptions.get(&o.key).cloned().unwrap_or_default();
            if exception.cancelled {
                return Err(PlanError::Invalid(format!(
                    "occurrence {} of block {id} is cancelled",
                    o.key
                )));
            }
            exception.starts_at = Some(start);
            exception.ends_at = Some(end);
            series
                .except(&o.key, exception)
                .map_err(|e| PlanError::Invalid(format!("block {id}: {e}")))?;
            block.exceptions = series.exceptions;
            occurrence_at(block, &o.key, tz)?;
            block.validate_invariants(tz).map_err(EngineError::from)?;
            return Ok(());
        }
        Some(o) => {
            if block
                .exceptions
                .get(&o.key)
                .is_some_and(BlockException::moves)
            {
                return Err(PlanError::Invalid(format!(
                    "occurrence {} of block {id} was moved on its own; drag it as this event",
                    o.key
                )));
            }
            let PlanPosition::Slot {
                start: was_start,
                end: was_end,
            } = before
            else {
                return Err(PlanError::Invalid(format!("{id} is a block off the grid")));
            };
            let secs = |t: &SunriseTime| {
                t.resolve_in(tz)
                    .map(jiff::Timestamp::as_second)
                    .ok_or_else(|| PlanError::Invalid(format!("block {id} has no instant here")))
            };
            let moved_start = secs(&start)? - secs(was_start)?;
            let moved_end = secs(&end)? - secs(was_end)?;
            let (anchor_start, anchor_end) = (secs(&block.starts_at)?, secs(&block.ends_at)?);
            block.starts_at = retime(&block.starts_at, anchor_start + moved_start, tz);
            block.ends_at = retime(&block.ends_at, anchor_end + moved_end, tz);
        }
    }
    // A recurring block's bindings and exceptions move with the occurrences
    // they named, as an edit of the whole block moves them; a move that
    // would strand one is refused, and nothing is written.
    move_anchor(block, &anchor, was, tz).map_err(invalid)
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
