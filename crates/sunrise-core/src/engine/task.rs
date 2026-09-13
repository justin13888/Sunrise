//! The Task command path and the `tasks` table.
//!
//! Task ops route under their owning Stream's id rather than the meta stream,
//! so a Task's `seq` advances with the Stream it lives in.
//!
//! The table half spans more than one table on purpose: a Task's contexts, its
//! blockers and its search-index rows are written and re-read together with the
//! row itself, so they are one unit of work rather than five. `read_task` is
//! that unit's shared read, and is reached from five other modules.

use super::ids::{
    blob16, decode_unknowns, encode_unknowns, energy_str, ms_to_ts, parse_energy, parse_task_state,
    require_kind, task_state_str, time_from_parts, time_to_parts,
};
use super::lww::LwwStamp;
use super::routine::update_routine_row;
use super::stream::ensure_stream_row;
use super::{read_task_blocks, Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::{encode_inner_op, InnerOp};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    inbox_stream_ref, violations_by_severity, DependencyGraph, NoteBody, ScheduleConstraint, Task,
    TaskDraft, TaskPatch, TaskState, ValidationError,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    pub(super) fn create_task(
        &self,
        db: &mut Db,
        d: TaskDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let task_id = self.fresh_id(EntityKind::Task, now_ms);
        let stream = d.stream_id.unwrap_or_else(inbox_stream_ref);
        let op_id = self.fresh_op_id(now_ms);
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

        let inner_op = encode_inner_op(&InnerOp::TaskCreate(task.clone()))?;
        let seq = self.next_seq(db, stream.bytes())?;
        let lww = self.lww_stamp(seq);

        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms)?;
            insert_task_row(tx, &task, &lww)?;
            insert_task_contexts(tx, &task)?;
            replace_task_blockers(tx, &task)?;
            ftsr_upsert_task(tx, &task)?;
            self.ops_insert(
                tx,
                &op_id,
                stream.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.create",
                "task",
                Some(task_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

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
        require_kind(id, EntityKind::Task)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        let prev_state = task.state;
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
            if !task.state.can_transition_to(state) {
                return Err(EngineError::Invalid(format!(
                    "task state {:?} cannot transition to {:?}",
                    task.state, state
                )));
            }
            task.state = state;
            if state == TaskState::Done {
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
        task.validate_invariants()?;
        if touches_blockers {
            // Local cycle/self-block check over the dependency index, per
            // docs/02-domain/tasks.md §Validation. Deliberately local: a cycle
            // formed concurrently on two devices is broken by the merge
            // tie-breaker, not here.
            let graph = read_dependency_graph(db.conn())?;
            if graph.would_cycle(task.id, &task.blocked_by) {
                return Err(ValidationError::BlockedByCycle.into());
            }
        }
        let soft_violations = if touches_schedule {
            self.check_schedule_constraints(&task)?
        } else {
            Vec::new()
        };
        task.updated_at = ms_to_ts(now_ms as i64);

        // Routine streak: the *first* pending -> done transition of an
        // occurrence moves the counter. `record_occurrence_completion` owns the
        // grace-window / forgiveness / idempotency rules and reports
        // `Duplicate` when this occurrence was already counted, in which case
        // no routine op is emitted at all.
        let streak_op = if patch.state == Some(TaskState::Done) && prev_state != TaskState::Done {
            self.streak_advance(db, &task, now_ms)?
        } else {
            None
        };

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        let seq = self.next_seq(db, task.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);

        let task_for_persist = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &task_for_persist.stream_id, now_ms)?;
            update_task_row(tx, &task_for_persist, &lww)?;
            replace_task_contexts(tx, &task_for_persist)?;
            replace_task_blockers(tx, &task_for_persist)?;
            ftsr_upsert_task(tx, &task_for_persist)?;
            self.ops_insert(
                tx,
                &op_id,
                task_for_persist.stream_id.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.update",
                "task",
                Some(task_for_persist.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            // The streak advance is a routine.update op in the SAME
            // transaction, so the counter can never diverge from the
            // completion that caused it, and it converges on every other
            // replica through the ordinary full-state routine LWW path.
            if let Some((routine, routine_inner)) = &streak_op {
                let routine_seq = self.next_seq_tx(tx, &META_STREAM)?;
                let routine_lww = self.lww_stamp(routine_seq);
                let routine_op_id = self.fresh_op_id(now_ms);
                // Stamped with the ROUTINE op's own stamp, which is what every
                // remote replica will stamp this row with when the op merges.
                update_routine_row(tx, routine, &routine_lww)?;
                self.ops_insert(
                    tx,
                    &routine_op_id,
                    &META_STREAM,
                    routine_seq,
                    routine_lww.hlc,
                    routine_inner,
                    "routine.update",
                    "routine",
                    Some(routine.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
            }
            Ok(())
        })?;

        Ok(CommandResult::new(id, Some(task.state), op_id, seq)
            .with_soft_violations(soft_violations))
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
        let Some(at) = task.scheduled_at.as_ref() else {
            return Ok(Vec::new());
        };
        if task.scheduling_constraints.is_empty() {
            return Ok(Vec::new());
        }
        let tz = jiff::tz::TimeZone::get(&self.clock.timezone()).unwrap_or(jiff::tz::TimeZone::UTC);
        // Constraints are civil windows, so a zone-less `scheduled_at` must be
        // resolved in the DEVICE zone before they can be evaluated — which is
        // exactly what `to_instant` does, and the reason it takes a zone.
        let zdt = at.to_instant(&tz).to_zoned(tz);
        let (hard, soft) = violations_by_severity(&task.scheduling_constraints, &zdt);
        if !hard.is_empty() {
            return Err(ValidationError::HardScheduleConstraint.into());
        }
        Ok(soft)
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

    pub(super) fn defer_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        to_ms: u64,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        task.scheduled_at = Some(ms_to_ts(to_ms as i64).into());
        task.deferred_count = task.deferred_count.saturating_add(1);
        // Deferring *is* scheduling, so it clears the same constraint gate.
        let soft_violations = self.check_schedule_constraints(&task)?;
        task.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        let seq = self.next_seq(db, task.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let task_clone = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_task_row(tx, &task_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                task_clone.stream_id.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.defer",
                "task",
                Some(task_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(id, Some(task.state), op_id, seq)
            .with_soft_violations(soft_violations))
    }

    pub(super) fn delete_task(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        task.deleted = true;
        task.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskDelete(task.clone()))?;
        let seq = self.next_seq(db, task.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let task_clone = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_task_row(tx, &task_clone, &lww)?;
            ftsr_delete_task(tx, &task_clone.id)?;
            self.ops_insert(
                tx,
                &op_id,
                task_clone.stream_id.bytes(),
                seq,
                lww.hlc,
                &inner_op,
                "task.delete",
                "task",
                Some(task_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult::new(id, Some(task.state), op_id, seq))
    }

    pub(super) fn promote_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        stream: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        require_kind(stream, EntityKind::Stream)?;
        let patch = TaskPatch {
            stream_id: Some(stream),
            ..Default::default()
        };
        self.update_task(db, id, patch)
    }
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
               AND t.state IN ('todo', 'in_progress')
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

pub(super) fn insert_task_row(
    tx: &Transaction<'_>,
    t: &Task,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let (sched_ms, sched_kind, sched_tz) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz) = time_to_parts(t.completed_at.as_ref());
    let extra_blob = encode_unknowns(&t.unknown)?;

    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "INSERT INTO tasks
         (id, stream_id, title, state, priority, energy, estimated_min,
          scheduled_at_ms, scheduled_at_kind, scheduled_at_tz,
          due_at_ms, due_at_kind, due_at_tz,
          completed_at_ms, completed_at_kind, completed_at_tz, deferred_count,
          routine_id, routine_occurrence, reminder_lead_s, archived, deleted, body,
          scheduling_constraints, extra, head_root,
          created_at_ms, updated_at_ms, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL,
                 ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            t.title,
            task_state_str(t.state),
            t.priority.map(i64::from),
            t.energy.map(energy_str),
            t.estimated_duration_s.and_then(|s| {
                let m = s / 60;
                i64::try_from(m).ok()
            }),
            sched_ms,
            sched_kind,
            sched_tz,
            due_ms,
            due_kind,
            due_tz,
            done_ms,
            done_kind,
            done_tz,
            t.deferred_count,
            t.routine_id.as_ref().map(|r| r.bytes().to_vec()),
            t.routine_occurrence.map(|d| d.as_millisecond()),
            t.reminder_lead_s,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            extra_blob,
            t.created_at.as_millisecond(),
            t.updated_at.as_millisecond(),
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

pub(super) fn update_task_row(
    tx: &Transaction<'_>,
    t: &Task,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let (sched_ms, sched_kind, sched_tz) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz) = time_to_parts(t.completed_at.as_ref());
    let extra_blob = encode_unknowns(&t.unknown)?;

    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "UPDATE tasks SET
            stream_id = ?, title = ?, state = ?, priority = ?,
            energy = ?, estimated_min = ?,
            scheduled_at_ms = ?, scheduled_at_kind = ?, scheduled_at_tz = ?,
            due_at_ms = ?, due_at_kind = ?, due_at_tz = ?,
            completed_at_ms = ?, completed_at_kind = ?, completed_at_tz = ?,
            deferred_count = ?, reminder_lead_s = ?, archived = ?, deleted = ?,
            body = ?, scheduling_constraints = ?, extra = ?,
            updated_at_ms = ?, lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            stream_blob,
            t.title,
            task_state_str(t.state),
            t.priority.map(i64::from),
            t.energy.map(energy_str),
            t.estimated_duration_s
                .and_then(|s| i64::try_from(s / 60).ok()),
            sched_ms,
            sched_kind,
            sched_tz,
            due_ms,
            due_kind,
            due_tz,
            done_ms,
            done_kind,
            done_tz,
            t.deferred_count,
            t.reminder_lead_s,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            extra_blob,
            t.updated_at.as_millisecond(),
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    Ok(())
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

pub(super) fn insert_task_contexts(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    for c in &t.contexts {
        tx.execute(
            "INSERT OR IGNORE INTO task_contexts (task_id, context_id) VALUES (?, ?)",
            params![t.id.bytes().to_vec(), c.bytes().to_vec()],
        )?;
    }
    Ok(())
}

/// Replace a task's rows in the dependency index with its current
/// `blocked_by` set.
///
/// The index is what makes both derived directions cheap — `blocked` (forward,
/// covered by the primary key) and `blocks_others` (reverse, covered by
/// `task_blockers_by_blocker`). Edges naming a blocker this replica has not
/// materialized yet are written unchanged: see `0008_task_blockers.sql` for why
/// the table carries no foreign keys.
pub(super) fn replace_task_blockers(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM task_blockers WHERE task_id = ?",
        params![t.id.bytes().to_vec()],
    )?;
    for b in &t.blocked_by {
        tx.execute(
            "INSERT OR IGNORE INTO task_blockers (task_id, blocker_id) VALUES (?, ?)",
            params![t.id.bytes().to_vec(), b.bytes().to_vec()],
        )?;
    }
    Ok(())
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

pub(super) fn replace_task_contexts(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM task_contexts WHERE task_id = ?",
        params![t.id.bytes().to_vec()],
    )?;
    insert_task_contexts(tx, t)
}

pub(super) fn ftsr_upsert_task(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    tx.execute(
        "DELETE FROM search_idx WHERE kind = 'task' AND id = ?",
        params![id_blob.clone()],
    )?;
    if t.deleted {
        return Ok(());
    }
    let body_text = t
        .body
        .as_ref()
        .map(|b| String::from_utf8_lossy(&b.0).to_string())
        .unwrap_or_default();
    let contexts_text = t
        .contexts
        .iter()
        .map(EntityRef::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    tx.execute(
        "INSERT INTO search_idx (kind, id, stream_id, title, body, contexts)
         VALUES ('task', ?, ?, ?, ?, ?)",
        params![id_blob, stream_blob, t.title, body_text, contexts_text],
    )?;
    Ok(())
}

pub(super) fn ftsr_delete_task(tx: &Transaction<'_>, id: &EntityRef) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = id.bytes().to_vec();
    tx.execute(
        "DELETE FROM search_idx WHERE kind = 'task' AND id = ?",
        params![id_blob],
    )?;
    Ok(())
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
                    completed_at_kind, completed_at_tz, extra, reminder_lead_s
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
        energy: t.4.as_deref().and_then(parse_energy),
        estimated_duration_s: t.5.and_then(|m| {
            let secs = m.checked_mul(60)?;
            u64::try_from(secs).ok()
        }),
        scheduled_at: time_from_parts(t.6, t.18.as_deref(), t.19.as_deref()),
        due_at: time_from_parts(t.7, t.20.as_deref(), t.21.as_deref()),
        scheduling_constraints: decode_constraints(t.13)?,
        completed_at: time_from_parts(t.8, t.22.as_deref(), t.23.as_deref()),
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
