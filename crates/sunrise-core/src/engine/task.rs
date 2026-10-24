//! The Task command path and reads of the `tasks` table.
//!
//! Task ops route under their owning Stream's id rather than the meta stream,
//! so a Task's `seq` advances with the Stream it lives in.
//!
//! The read spans more than one table on purpose: a Task's contexts, its
//! blockers and its search-index rows are written and re-read together with the
//! row itself, so they are one unit of work rather than five. `read_task` is
//! that unit's shared read, and is reached from five other modules. The write
//! half is the merge's (`merge::rows`): a command appends its op and folds it.

use super::block::read_task_blocks;
use super::ids::{
    blob16, decode_unknowns, ms_to_ts, parse_energy, parse_task_state, require_kind,
    require_writable_stream, time_from_parts,
};
use super::routine::read_routine;
use super::write::Appended;
use super::{Engine, EngineError, META_STREAM};
use crate::commands::{BulkTaskAction, BulkTaskItem, CommandResult};
use crate::inner_op::InnerOp;
use rusqlite::{params, OptionalExtension};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::Unknowns;
use sunrise_domain::{
    inbox_stream_ref, violations_by_severity, DependencyGraph, NoteBody, Routine,
    ScheduleConstraint, SunriseTime, Task, TaskDraft, TaskPatch, TaskState, ValidationError,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    pub(super) fn create_task(
        &self,
        db: &mut Db,
        d: TaskDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate(&self.device_zone())?;
        let now_ms = self.clock.now_ms();
        let task_id = self.fresh_id(EntityKind::Task, now_ms);
        let stream = d.stream_id.unwrap_or_else(inbox_stream_ref);
        require_writable_stream(stream)?;
        let task = Task {
            reminder_lead_s: d.reminder_lead_s,
            id: task_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            title: d.title.trim().to_string(),
            body: d.body.clone(),
            stream_id: stream,
            contexts: d.contexts.iter().copied().collect(),
            state: TaskState::Todo,
            priority: d.priority,
            energy: d.energy,
            estimated_duration_s: d.estimated_duration_s,
            scheduled_at: d.scheduled_at,
            due_at: d.due_at,
            scheduling_constraints: d.scheduling_constraints.clone(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: d.assignee,
            routine_id: None,
            routine_occurrence: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        };

        // Scheduling gate: a `hard` constraint violated by this `scheduled_at`
        // rejects the create outright; `soft` ones ride back on the result.
        let soft_violations = self.check_schedule_constraints(&task)?;

        let op = InnerOp::TaskCreate(task);
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, stream.bytes(), now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);

        Ok(
            CommandResult::new(task_id, Some(TaskState::Todo), op_id, seq)
                .with_soft_violations(soft_violations),
        )
    }

    pub(super) fn update_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: TaskPatch,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let write = self.plan_update(db.conn(), id, patch, now_ms)?;
        // Routine streak: the *first* pending -> done transition of an
        // occurrence moves the counter. `record_occurrence_completion` owns the
        // grace-window / forgiveness / idempotency rules and reports
        // `Duplicate` when this occurrence was already counted, in which case
        // no routine op is emitted at all.
        let streak_op = if write.completes {
            self.streak_advance(db, &write.task, now_ms)?
        } else {
            None
        };
        let (op, stream) = write.op();
        let done = db.with_tx(|tx| -> rusqlite::Result<Appended> {
            let done = self.append_and_fold(tx, &op, &stream, now_ms)?;
            // The streak advance is a routine.update op in the SAME
            // transaction, so the counter can never diverge from the
            // completion that caused it, and it converges on every other
            // replica through the ordinary full-state routine merge. Each op
            // is folded with its own stamp, the one every peer folds it with.
            if let Some(routine) = streak_op {
                self.append_and_fold(tx, &routine, &META_STREAM, now_ms)?;
            }
            Ok(done)
        })?;
        Ok(write.result(done))
    }

    /// The write [`Self::update_task`] makes of `patch`, validated against
    /// the task as `conn` holds it, and not yet logged.
    pub(super) fn plan_update(
        &self,
        conn: &rusqlite::Connection,
        id: EntityRef,
        patch: TaskPatch,
        now_ms: u64,
    ) -> Result<TaskWrite, EngineError> {
        require_kind(id, EntityKind::Task)?;
        if let Some(s) = patch.stream_id {
            require_writable_stream(s)?;
        }
        patch.validate()?;
        let mut task = read_task(conn, id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        let prev_state = task.state.clone();
        let sets_done = patch.state == Some(TaskState::Done);
        // Which gates this patch has to clear. Scheduling constraints are only
        // re-evaluated when the patch actually *schedules* (per
        // docs/02-domain/scheduling-constraints.md a `hard` violation "fails
        // validation when the user schedules a Task against it") — renaming a
        // task that already sits outside its window must not be rejected.
        let touches_schedule =
            patch.scheduled_at.is_some() || patch.scheduling_constraints.is_some();
        let touches_blockers = patch.blocked_by.is_some();

        if let Some(t) = patch.title {
            task.title = t.trim().to_string();
        }
        if let Some(b) = patch.body {
            task.body = b;
        }
        if let Some(s) = patch.stream_id {
            require_kind(s, EntityKind::Stream)?;
            task.stream_id = s;
        }
        if let Some(cs) = patch.contexts {
            task.contexts = cs.into_iter().collect();
        }
        if let Some(state) = patch.state {
            if !task.state.can_transition_to(&state) {
                return Err(EngineError::Invalid(format!(
                    "task state {:?} cannot transition to {:?}",
                    task.state, state
                )));
            }
            let done = state == TaskState::Done;
            task.state = state;
            if done {
                task.completed_at = Some(ms_to_ts(now_ms as i64).into());
            } else {
                task.completed_at = None;
            }
        }
        if let Some(p) = patch.priority {
            task.priority = p;
        }
        if let Some(e) = patch.energy {
            task.energy = e;
        }
        if let Some(d) = patch.estimated_duration_s {
            task.estimated_duration_s = d;
        }
        if let Some(s) = patch.scheduled_at {
            task.scheduled_at = s;
        }
        if let Some(d) = patch.due_at {
            task.due_at = d;
        }
        if let Some(list) = patch.scheduling_constraints {
            task.scheduling_constraints = list;
        }
        if let Some(bs) = patch.blocked_by {
            task.blocked_by = bs.into_iter().collect();
        }
        if let Some(a) = patch.assignee {
            task.assignee = a;
        }
        if let Some(lead) = patch.reminder_lead_s {
            task.reminder_lead_s = lead;
        }
        if let Some(arch) = patch.archived {
            task.archived = arch;
        }
        // Re-check cross-field invariants on the patched Task. A patch that
        // sets only `due_at` earlier than the existing `scheduled_at` (or an
        // invalid constraint list) would otherwise pass silently.
        task.validate_invariants(&self.device_zone())?;
        if touches_blockers {
            // Local cycle/self-block check over the dependency index, per
            // docs/02-domain/tasks.md §Validation. Deliberately local: a cycle
            // formed concurrently on two devices is broken by the merge
            // tie-breaker, not here.
            let graph = read_dependency_graph(conn)?;
            if graph.would_cycle(task.id, &task.blocked_by) {
                return Err(ValidationError::BlockedByCycle.into());
            }
        }
        let soft = if touches_schedule {
            self.check_schedule_constraints(&task)?
        } else {
            Vec::new()
        };
        task.updated_at = ms_to_ts(now_ms as i64);
        Ok(TaskWrite {
            task,
            delete: false,
            completes: sets_done && prev_state != TaskState::Done,
            soft,
        })
    }

    /// Evaluate a Task's `scheduled_at` against its scheduling constraints.
    ///
    /// Per `docs/02-domain/scheduling-constraints.md`: window dimensions are
    /// civil (zone-less) values pinned to instants by the **device-local**
    /// timezone for a Task, which reaches the engine through the injected
    /// [`Clock`](crate::config::Clock) rather than from ambient process state.
    /// A `hard` violation is rejected; the `soft` ones are returned so the
    /// caller can surface them.
    fn check_schedule_constraints(
        &self,
        task: &Task,
    ) -> Result<Vec<ScheduleConstraint>, EngineError> {
        let (hard, soft) = constraint_violations(task, &self.device_zone());
        if !hard.is_empty() {
            return Err(ValidationError::HardScheduleConstraint.into());
        }
        Ok(soft)
    }

    /// Every open, planned task whose plan violates a `hard` scheduling
    /// constraint for a reader in `reader`, by id.
    ///
    /// What a zone change re-evaluates (#333, `docs/10-cross-cutting/time.md`
    /// §7): a floating plan moves with the reader, so a window it kept in one
    /// zone it can break in another, and a window a DST gap empties breaks
    /// every plan inside it. A violation is reported, never repaired: nothing
    /// stored is rewritten.
    ///
    /// # Errors
    /// Storage.
    pub(crate) fn hard_constraint_violations(
        &self,
        db: &Db,
        reader: &jiff::tz::TimeZone,
    ) -> Result<Vec<EntityRef>, EngineError> {
        let mut out: Vec<EntityRef> = super::review::read_live_tasks(db.conn())?
            .iter()
            .filter(|t| {
                !t.archived
                    && matches!(t.state.effective(), TaskState::Todo | TaskState::InProgress)
            })
            .filter(|t| !constraint_violations(t, reader).0.is_empty())
            .map(|t| t.id)
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    pub(super) fn complete_task(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        let patch = TaskPatch {
            state: Some(TaskState::Done),
            ..Default::default()
        };
        self.update_task(db, id, patch)
    }

    /// Set `scheduled_at` to `to`, of whatever kind the caller meant, and
    /// count the deferral.
    ///
    /// `docs/11-adr/0066-times-survive-travel.md` §4: the target keeps its
    /// kind (a floating "tomorrow 09:00" stays floating) and the deferral
    /// passes the same checks as any edit of `scheduled_at`: the cross-field
    /// invariants in the reader's zone, then the scheduling constraints.
    pub(super) fn defer_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        to: SunriseTime,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let write = self.plan_defer(db.conn(), id, to, now_ms)?;
        self.emit_task_write(db, write, now_ms)
    }

    /// The write [`Self::defer_task`] makes, not yet logged.
    pub(super) fn plan_defer(
        &self,
        conn: &rusqlite::Connection,
        id: EntityRef,
        to: SunriseTime,
        now_ms: u64,
    ) -> Result<TaskWrite, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let mut task = read_task(conn, id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        task.scheduled_at = Some(to);
        // TODO(ADR-0065 PR7, #447, #334): a read-increment-write inside a
        // full-state op, so two concurrent defers fold to one count. The fold
        // already merges `deferred_count` as a PN-counter (`Crdt::Counter`,
        // `merge::state::add_delta`); what is missing is the emission: once a
        // vault requires `core.field_ops`, defer writes a `Patch` carrying
        // `{"deferred_count": {"inc": 1}}` and both counts survive.
        task.deferred_count = task.deferred_count.saturating_add(1);
        task.validate_invariants(&self.device_zone())?;
        // Deferring *is* scheduling, so it clears the same constraint gate.
        let soft = self.check_schedule_constraints(&task)?;
        task.updated_at = ms_to_ts(now_ms as i64);
        // Logged as the `task.update` it is, the kind a peer logs it under,
        // so compaction and the history readers see one kind on every replica.
        Ok(TaskWrite {
            task,
            delete: false,
            completes: false,
            soft,
        })
    }

    pub(super) fn delete_task(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let write = self.plan_tombstone(db.conn(), id, true, now_ms)?;
        self.emit_task_write(db, write, now_ms)
    }

    /// The write that sets `deleted` on task `id` to `deleted`, not yet
    /// logged: a delete, or (for `false`) the restore a bulk command makes,
    /// as `Command::Restore` makes it (clarification D11). A restore of a
    /// task that is not deleted is refused, as `Command::Restore` refuses it.
    pub(super) fn plan_tombstone(
        &self,
        conn: &rusqlite::Connection,
        id: EntityRef,
        deleted: bool,
        now_ms: u64,
    ) -> Result<TaskWrite, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let mut task = read_task(conn, id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        if !deleted && !task.deleted {
            return Err(EngineError::Invalid(format!("{id} is not deleted")));
        }
        task.deleted = deleted;
        task.updated_at = ms_to_ts(now_ms as i64);
        Ok(TaskWrite {
            task,
            delete: deleted,
            completes: false,
            soft: Vec::new(),
        })
    }

    /// Log and fold one planned task write, alone.
    fn emit_task_write(
        &self,
        db: &mut Db,
        write: TaskWrite,
        now_ms: u64,
    ) -> Result<CommandResult, EngineError> {
        let (op, stream) = write.op();
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &stream, now_ms))?;
        Ok(write.result(done))
    }

    /// [`Command::BulkTasks`](crate::commands::Command::BulkTasks): plan
    /// every item as its single command would, refuse the batch on the first
    /// refusal, then append every write, and the streak advances they make,
    /// as one group (ADR-0065 §6).
    pub(super) fn bulk_tasks(
        &self,
        db: &mut Db,
        items: Vec<BulkTaskItem>,
    ) -> Result<CommandResult, EngineError> {
        if items.is_empty() {
            return Err(EngineError::Invalid(
                "a bulk command needs at least one task".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for item in &items {
            require_kind(item.id, EntityKind::Task)?;
            if !seen.insert(item.id) {
                return Err(EngineError::Invalid(format!(
                    "{} appears twice in one bulk command",
                    item.id
                )));
            }
        }
        let now_ms = self.clock.now_ms();
        let mut writes = Vec::with_capacity(items.len());
        for item in items {
            let conn = db.conn();
            writes.push(match item.action {
                BulkTaskAction::Complete => {
                    let patch = TaskPatch {
                        state: Some(TaskState::Done),
                        ..Default::default()
                    };
                    self.plan_update(conn, item.id, patch, now_ms)?
                }
                BulkTaskAction::DeferTo(to) => self.plan_defer(conn, item.id, to, now_ms)?,
                BulkTaskAction::Update(patch) => self.plan_update(conn, item.id, *patch, now_ms)?,
                BulkTaskAction::Delete => self.plan_tombstone(conn, item.id, true, now_ms)?,
                BulkTaskAction::Restore => self.plan_tombstone(conn, item.id, false, now_ms)?,
            });
        }
        // Two occurrences of one routine completed together each count: the
        // routine is advanced in memory once per occurrence, in item order,
        // and written once.
        let mut routines: BTreeMap<EntityRef, (Routine, bool)> = BTreeMap::new();
        for write in writes.iter().filter(|w| w.completes) {
            let Some(rid) = write.task.routine_id else {
                continue;
            };
            let (routine, moved) = match routines.entry(rid) {
                Entry::Occupied(held) => held.into_mut(),
                Entry::Vacant(slot) => {
                    let Some(routine) = read_routine(db.conn(), rid.bytes())? else {
                        continue;
                    };
                    slot.insert((routine, false))
                }
            };
            *moved |= self.advance_streak_of(routine, &write.task, now_ms);
        }
        let mut members: Vec<(InnerOp, [u8; 16])> = writes.iter().map(TaskWrite::op).collect();
        for (routine, moved) in routines.into_values() {
            if moved {
                let routine = self.routine_op_form(db, &routine)?;
                members.push((InnerOp::RoutineUpdate(Box::new(routine)), META_STREAM));
            }
        }
        let done = db.with_tx(|tx| self.append_and_fold_group(tx, members, now_ms))?;
        let soft: Vec<ScheduleConstraint> =
            writes.iter().flat_map(|w| w.soft.iter().cloned()).collect();
        let first = writes.into_iter().next().map(|mut w| {
            w.soft = soft;
            w
        });
        match (first, done.first()) {
            (Some(write), Some(done)) => Ok(write.result(*done)),
            _ => Err(EngineError::Invalid("a bulk command wrote nothing".into())),
        }
    }

    pub(super) fn promote_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        stream: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        require_writable_stream(stream)?;
        let patch = TaskPatch {
            stream_id: Some(stream),
            ..Default::default()
        };
        self.update_task(db, id, patch)
    }
}

/// One task write a command has planned and validated, and not yet logged:
/// the task as it will be written, and what the command reports of it.
pub(super) struct TaskWrite {
    /// The whole task, as the full-state op carries it.
    pub(super) task: Task,
    /// A `TaskDelete` rather than a `TaskUpdate`.
    delete: bool,
    /// The task's first move to `done`, which advances its routine's streak.
    pub(super) completes: bool,
    /// The `soft` scheduling constraints the write violates.
    pub(super) soft: Vec<ScheduleConstraint>,
}

impl TaskWrite {
    /// The op and the stream it is logged in: the task's own.
    pub(super) fn op(&self) -> (InnerOp, [u8; 16]) {
        let stream = *self.task.stream_id.bytes();
        let op = if self.delete {
            InnerOp::TaskDelete(self.task.clone())
        } else {
            InnerOp::TaskUpdate(self.task.clone())
        };
        (op, stream)
    }

    /// What the command returns, once the op is logged as `done`.
    pub(super) fn result(self, done: Appended) -> CommandResult {
        CommandResult::new(self.task.id, Some(self.task.state), done.op_id, done.seq)
            .with_soft_violations(self.soft)
    }
}

/// The zone a task's scheduling constraints are evaluated in
/// (`docs/10-cross-cutting/time.md` §5): a routine occurrence planned `zoned`
/// (its routine is pinned) in that zone, everything else in the reader's.
/// A zone this build does not know is the reader's.
fn constraint_zone(task: &Task, reader: &jiff::tz::TimeZone) -> jiff::tz::TimeZone {
    match (&task.scheduled_at, task.routine_id) {
        (Some(SunriseTime::Zoned { tz, .. }), Some(_)) => {
            jiff::tz::TimeZone::get(tz).unwrap_or_else(|_| reader.clone())
        }
        _ => reader.clone(),
    }
}

/// `task`'s violated constraints at its plan, `(hard, soft)`, for a reader in
/// `reader`.
///
/// Constraints are civil windows, so the plan is resolved first (a zone-less
/// one in the reader's zone) and converted to civil time in the evaluation
/// zone ([`constraint_zone`]); no window bound is resolved (time.md §3). A
/// plan this build cannot place on the timeline is checked against nothing:
/// its stand-in instant would invent a violation.
fn constraint_violations(
    task: &Task,
    reader: &jiff::tz::TimeZone,
) -> (Vec<ScheduleConstraint>, Vec<ScheduleConstraint>) {
    if task.scheduling_constraints.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let Some(at) = task
        .scheduled_at
        .as_ref()
        .and_then(|s| s.resolve_in(reader))
    else {
        return (Vec::new(), Vec::new());
    };
    let zdt = at.to_zoned(constraint_zone(task, reader));
    violations_by_severity(&task.scheduling_constraints, &zdt)
}

// ---- table operations ----

/// The dependency-graph walk shared by [`Query::Actionable`](crate::queries::Query::Actionable) and
/// [`Query::FocusPlan`](crate::queries::Query::FocusPlan). Returns `(task id, open blockers, unblocks)`.
///
/// `open_blockers` counts an *unknown* blocker (LEFT JOIN miss) as open: the op
/// that creates it may simply not have arrived yet, and treating it as
/// satisfied would flash the task as actionable and then take it away again.
///
/// `actionable_only` pushes the planner's first criterion into SQL, so a vault
/// full of blocked work cannot crowd the scan cap with rows the planner would
/// throw away anyway.
pub(super) fn actionable_scan(
    conn: &rusqlite::Connection,
    stream: Option<EntityRef>,
    limit: u32,
    actionable_only: bool,
) -> Result<Vec<([u8; 16], u32, u32)>, EngineError> {
    let stream_blob: Option<Vec<u8>> = stream.map(|s| s.bytes().to_vec());
    let mut stmt = conn.prepare(
        "SELECT id, open_blockers, unblocks FROM (
             SELECT t.id AS id,
                    (SELECT COUNT(*) FROM task_blockers tb
                       LEFT JOIN tasks b ON b.id = tb.blocker_id
                      WHERE tb.task_id = t.id
                        AND (b.id IS NULL
                             OR (b.deleted = 0
                                 AND b.state NOT IN ('done', 'cancelled')))) AS open_blockers,
                    (SELECT COUNT(*) FROM task_blockers tb2
                       JOIN tasks d ON d.id = tb2.task_id
                      WHERE tb2.blocker_id = t.id
                        AND d.deleted = 0 AND d.archived = 0
                        AND d.state NOT IN ('done', 'cancelled')) AS unblocks
             FROM tasks t
             WHERE t.deleted = 0 AND t.archived = 0
               -- Open: not finished. Spelled as an exclusion so a state this
               -- build does not know counts as open, as `todo` (ADR-0045 §6).
               AND t.state NOT IN ('done', 'cancelled')
               AND (?1 IS NULL OR t.stream_id = ?1)
         )
         WHERE (?3 = 0 OR open_blockers = 0)
         ORDER BY open_blockers ASC, unblocks DESC, id ASC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(
            params![stream_blob, limit, i64::from(actionable_only)],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(raw, open, unblocks)| {
            (
                blob16(&raw),
                u32::try_from(open.max(0)).unwrap_or(u32::MAX),
                u32::try_from(unblocks.max(0)).unwrap_or(u32::MAX),
            )
        })
        .collect())
}

/// Widen a stored 16-byte id back into a typed [`EntityRef`].
pub(super) fn ref_of(kind: EntityKind, raw: &[u8]) -> EntityRef {
    EntityRef::new(kind, blob16(raw))
}

/// Encode a scheduling-constraint list to a canonical-CBOR blob for the
/// `scheduling_constraints` column. An empty list stores `NULL`.
pub(super) fn encode_constraints(list: &[ScheduleConstraint]) -> rusqlite::Result<Option<Vec<u8>>> {
    if list.is_empty() {
        return Ok(None);
    }
    let v = list.to_vec();
    let bytes = sunrise_cbor::encode_canonical(&v)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(Some(bytes))
}

/// Decode the `scheduling_constraints` column blob (NULL == empty list).
pub(super) fn decode_constraints(
    blob: Option<Vec<u8>>,
) -> Result<Vec<ScheduleConstraint>, EngineError> {
    match blob {
        None => Ok(Vec::new()),
        Some(bytes) => {
            sunrise_cbor::decode_canonical(&bytes).map_err(|e| EngineError::Cbor(e.to_string()))
        }
    }
}

/// A task's `blocked_by` set, read back out of the dependency index.
fn read_task_blockers(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> rusqlite::Result<BTreeSet<EntityRef>> {
    let mut stmt =
        conn.prepare("SELECT blocker_id FROM task_blockers WHERE task_id = ? ORDER BY blocker_id")?;
    let rows = stmt.query_map(params![id.to_vec()], |r| r.get::<_, Vec<u8>>(0))?;
    let mut out = BTreeSet::new();
    for raw in rows {
        let raw = raw?;
        let mut a = [0u8; 16];
        let take = raw.len().min(16);
        a[..take].copy_from_slice(&raw[..take]);
        out.insert(EntityRef::new(EntityKind::Task, a));
    }
    Ok(out)
}

/// The whole dependency graph, for the local submit-time cycle check.
fn read_dependency_graph(conn: &rusqlite::Connection) -> Result<DependencyGraph, EngineError> {
    let mut stmt = conn.prepare("SELECT task_id, blocker_id FROM task_blockers")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut graph = DependencyGraph::new();
    for row in rows {
        let (task, blocker) = row?;
        graph.add_edge(task_ref(&task), task_ref(&blocker));
    }
    Ok(graph)
}

/// Widen a stored 16-byte id back into a Task [`EntityRef`].
fn task_ref(raw: &[u8]) -> EntityRef {
    let mut a = [0u8; 16];
    let take = raw.len().min(16);
    a[..take].copy_from_slice(&raw[..take]);
    EntityRef::new(EntityKind::Task, a)
}

pub(super) fn read_task(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Task>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let row = conn
        .query_row(
            "SELECT stream_id, title, state, priority, energy, estimated_min,
                    scheduled_at_ms, due_at_ms, completed_at_ms, deferred_count,
                    archived, deleted, body, scheduling_constraints,
                    routine_id, routine_occurrence, created_at_ms, updated_at_ms,
                    scheduled_at_kind, scheduled_at_tz,
                    due_at_kind, due_at_tz,
                    completed_at_kind, completed_at_tz, extra, reminder_lead_s,
                    scheduled_at_civil, due_at_civil, completed_at_civil
             FROM tasks WHERE id = ?",
            params![id_blob],
            |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, i64>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, Option<Vec<u8>>>(12)?,
                    r.get::<_, Option<Vec<u8>>>(13)?,
                    r.get::<_, Option<Vec<u8>>>(14)?,
                    r.get::<_, Option<i64>>(15)?,
                    r.get::<_, i64>(16)?,
                    r.get::<_, i64>(17)?,
                    r.get::<_, Option<String>>(18)?,
                    r.get::<_, Option<String>>(19)?,
                    r.get::<_, Option<String>>(20)?,
                    r.get::<_, Option<String>>(21)?,
                    r.get::<_, Option<String>>(22)?,
                    r.get::<_, Option<String>>(23)?,
                    r.get::<_, Option<Vec<u8>>>(24)?,
                    r.get::<_, Option<u32>>(25)?,
                    r.get::<_, Option<String>>(26)?,
                    r.get::<_, Option<String>>(27)?,
                    r.get::<_, Option<String>>(28)?,
                ))
            },
        )
        .optional()?;
    let Some(t) = row else {
        return Ok(None);
    };
    let mut stream_bytes = [0u8; 16];
    let take = t.0.len().min(16);
    stream_bytes[..take].copy_from_slice(&t.0[..take]);
    let mut contexts = BTreeSet::new();
    {
        let mut stmt = conn.prepare("SELECT context_id FROM task_contexts WHERE task_id = ?")?;
        let rows = stmt.query_map(params![id_blob.clone()], |row| row.get::<_, Vec<u8>>(0))?;
        for r in rows {
            let raw = r?;
            let mut a = [0u8; 16];
            let take = raw.len().min(16);
            a[..take].copy_from_slice(&raw[..take]);
            contexts.insert(EntityRef::new(EntityKind::Context, a));
        }
    }
    let task = Task {
        reminder_lead_s: t.25,
        id: EntityRef::new(EntityKind::Task, *id),
        created_at: ms_to_ts(t.16.max(0)),
        updated_at: ms_to_ts(t.17.max(0)),
        title: t.1,
        body: t.12.map(NoteBody),
        stream_id: EntityRef::new(EntityKind::Stream, stream_bytes),
        contexts,
        state: parse_task_state(&t.2),
        priority: t.3.and_then(|v| u8::try_from(v).ok()),
        energy: t.4.as_deref().map(parse_energy),
        estimated_duration_s: t.5.and_then(|m| {
            let secs = m.checked_mul(60)?;
            u64::try_from(secs).ok()
        }),
        scheduled_at: time_from_parts(t.6, t.18.as_deref(), t.19.as_deref(), t.26.as_deref()),
        due_at: time_from_parts(t.7, t.20.as_deref(), t.21.as_deref(), t.27.as_deref()),
        scheduling_constraints: decode_constraints(t.13)?,
        completed_at: time_from_parts(t.8, t.22.as_deref(), t.23.as_deref(), t.28.as_deref()),
        deferred_count: t.9,
        // Derived from `block_tasks`, never stored on the task row: the Block
        // op is the only writer of the binding, so the symmetry
        // `docs/02-domain/time-blocks.md` asks for holds by construction and
        // cannot be lost to an entity-level LWW contest over the Task.
        blocks: read_task_blocks(conn, id)?,
        // Restored from the dependency index (migration 0008). Before it
        // existed, `blocked_by` survived only inside the op-log inner op and
        // the materialized projection always read back empty, so nothing could
        // derive `blocked` from it.
        blocked_by: read_task_blockers(conn, id)?,
        assignee: None,
        routine_id: t.14.map(|b| {
            let mut a = [0u8; 16];
            let take = b.len().min(16);
            a[..take].copy_from_slice(&b[..take]);
            EntityRef::new(EntityKind::Routine, a)
        }),
        routine_occurrence: t.15.map(|m| ms_to_ts(m.max(0))),
        archived: t.10 != 0,
        deleted: t.11 != 0,
        unknown: decode_unknowns(t.24),
    };
    Ok(Some(task))
}
