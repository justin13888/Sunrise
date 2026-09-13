//! The Routine command path, materialization, and the `routines` table.
//!
//! Routine ops route to the vault-meta log: a Routine is not owned by any one
//! Stream, and its occurrences are minted locally from the rule rather than
//! travelling as task ops.
//!
//! `streak_advance` lives here although the Task command path calls it. Its
//! whole body is routine bookkeeping — it is the only consumer of
//! `StreakStateProj` — so leaving it beside `complete_task` would drag
//! `routines`, `streak_state` and `materialized_until` into that module for one
//! function. `insert_task_row_or_ignore` is here on the same reasoning running
//! the other way: it writes `tasks`, but only routine materialization calls it,
//! and the `OR IGNORE` is a materialization concern — a regenerated occurrence
//! that already exists is not an error.

use super::ids::{
    decode_unknowns, encode_unknowns, energy_str, ms_to_ts, require_kind, task_state_str,
    time_to_parts,
};
use super::lww::LwwStamp;
use super::stream::ensure_stream_row;
use super::task::{
    decode_constraints, encode_constraints, ftsr_delete_task, ftsr_upsert_task,
    insert_task_contexts, read_task, update_task_row,
};
use super::{Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::{encode_inner_op, InnerOp};
use crate::queries::QueryResult;
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    materialization_horizon_days, occurrence_key_at, occurrence_task_id, Routine,
    RoutineCatchupPolicy, RoutineDraft, RoutinePatch, StreakOutcome, Task, TaskState, TaskTemplate,
};
use sunrise_id::{EntityKind, EntityRef, Ulid};
use sunrise_storage::Db;

impl Engine {
    /// Advance the owning Routine's streak for a task that just went `done`.
    ///
    /// Returns the mutated Routine plus its encoded `RoutineUpdate` inner op,
    /// or `None` when there is nothing to emit: the task is not
    /// routine-generated, its routine is gone, it carries no occurrence
    /// instant, or the occurrence was already counted (the idempotency key set
    /// makes a `done -> todo -> done` round trip a no-op).
    pub(super) fn streak_advance(
        &self,
        db: &Db,
        task: &Task,
        now_ms: u64,
    ) -> Result<Option<(Routine, Vec<u8>)>, EngineError> {
        let (Some(rid), Some(occurrence_at)) = (task.routine_id, task.routine_occurrence) else {
            return Ok(None);
        };
        let Some(mut routine) = read_routine(db.conn(), rid.bytes())? else {
            return Ok(None);
        };
        let Ok(key) = occurrence_key_at(&routine.timezone, occurrence_at) else {
            return Ok(None);
        };
        let outcome =
            routine.record_occurrence_completion(&key, occurrence_at, ms_to_ts(now_ms as i64));
        if outcome == StreakOutcome::Duplicate {
            return Ok(None);
        }
        routine.updated_at = ms_to_ts(now_ms as i64);
        let inner = encode_inner_op(&InnerOp::RoutineUpdate(Box::new(routine.clone())))?;
        Ok(Some((routine, inner)))
    }

    pub(super) fn create_routine(
        &self,
        db: &mut Db,
        d: RoutineDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let routine_id = self.fresh_id(EntityKind::Routine, now_ms);
        let routine = Routine {
            id: routine_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            template: d.template,
            rrule: d.rrule,
            timezone: d.timezone,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            scheduling_constraints: d.scheduling_constraints,
            skip_dates: Vec::new(),
            skipped_keys: Vec::new(),
            catchup_policy: d.catchup_policy,
            streak_counter: 0,
            last_completed_at: None,
            grace_window_s: None,
            forgiveness_enabled: true,
            streak_started_at: None,
            forgivenesses_in_window: 0,
            streak_keys: Vec::new(),
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        };
        let stream = routine.template.stream_id;
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineCreate(Box::new(routine.clone())))?;
        // Routine ops route to the vault-meta log.
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms)?;
            insert_routine_row(tx, &routine_clone, now_ms, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.create",
                "routine",
                Some(routine_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        // Materialize the near-horizon occurrences for the freshly-created
        // routine (first run: `materialized_until` is 0, so the window opens at
        // `starts_at` and any missed occurrences run the catchup policy).
        self.materialize_one_routine(db, &routine, now_ms)?;

        Ok(CommandResult::new(routine_id, None, op_id, seq))
    }

    pub(super) fn update_routine(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: RoutinePatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut routine = read_routine(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("routine {id}")))?;

        // Track whether the change affects which occurrences exist (and thus
        // requires regenerating future not-started tasks).
        let mut structural = false;
        if let Some(t) = patch.template {
            routine.template = t;
        }
        if let Some(r) = patch.rrule {
            structural |= routine.rrule != r;
            routine.rrule = r;
        }
        if let Some(tz) = patch.timezone {
            structural |= routine.timezone != tz;
            routine.timezone = tz;
        }
        if let Some(s) = patch.starts_at {
            structural |= routine.starts_at != s;
            routine.starts_at = s;
        }
        if let Some(e) = patch.ends_at {
            structural |= routine.ends_at != e;
            routine.ends_at = e;
        }
        if let Some(list) = patch.scheduling_constraints {
            routine.scheduling_constraints = list;
        }
        if let Some(c) = patch.catchup_policy {
            routine.catchup_policy = c;
        }
        if let Some(g) = patch.grace_window_s {
            routine.grace_window_s = g;
        }
        if let Some(f) = patch.forgiveness_enabled {
            routine.forgiveness_enabled = f;
        }
        if let Some(p) = patch.paused {
            routine.paused = p;
        }
        if let Some(pu) = patch.paused_until {
            routine.paused_until = pu;
        }
        if let Some(a) = patch.archived {
            routine.archived = a;
        }
        routine.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineUpdate(Box::new(routine.clone())))?;
        let stream = routine.template.stream_id;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms)?;
            update_routine_row(tx, &routine_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.update",
                "routine",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        if structural {
            self.regenerate_future_tasks(db, &routine, now_ms)?;
        }
        self.materialize_one_routine(db, &routine, now_ms)?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    pub(super) fn delete_routine(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        let now_ms = self.clock.now_ms();
        let mut routine = read_routine(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("routine {id}")))?;
        routine.deleted = true;
        routine.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineDelete(Box::new(routine.clone())))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_routine_row(tx, &routine_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.delete",
                "routine",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    pub(super) fn skip_routine_occurrence(
        &self,
        db: &mut Db,
        id: EntityRef,
        occurrence_key: String,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        let now_ms = self.clock.now_ms();
        let mut routine = read_routine(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("routine {id}")))?;
        if !routine.skipped_keys.contains(&occurrence_key) {
            routine.skipped_keys.push(occurrence_key.clone());
        }
        routine.updated_at = ms_to_ts(now_ms as i64);

        // Tombstone the already-materialized task for this occurrence, but only
        // if it exists and is still an untouched (Todo, non-deferred) routine
        // task — a user who already started/edited it keeps it.
        let task_id = occurrence_task_id(&id, &occurrence_key);
        let existing = read_task(db.conn(), task_id.bytes())?;
        let drop_task = existing
            .as_ref()
            .filter(|t| {
                !t.deleted
                    && t.state == TaskState::Todo
                    && t.deferred_count == 0
                    && t.routine_id == Some(id)
            })
            .cloned();

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineUpdate(Box::new(routine.clone())))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_routine_row(tx, &routine_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
                &inner_op,
                "routine.update",
                "routine",
                Some(id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            if let Some(mut t) = drop_task {
                t.deleted = true;
                t.updated_at = ms_to_ts(now_ms as i64);
                let task_seq = self.next_seq_tx(tx, t.stream_id.bytes())?;
                let task_lww = self.lww_stamp(task_seq);
                let del_op = encode_inner_op(&InnerOp::TaskDelete(t.clone()))
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                // Stamped with the TASK op's own stamp, not the routine op's:
                // see the note on `Engine::lww_stamp`.
                update_task_row(tx, &t, &task_lww)?;
                ftsr_delete_task(tx, &t.id)?;
                let del_op_id = self.fresh_op_id(now_ms);
                self.ops_insert(
                    tx,
                    &del_op_id,
                    t.stream_id.bytes(),
                    task_seq,
                    task_lww.hlc,
                    &del_op,
                    "task.delete",
                    "task",
                    Some(t.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
            }
            Ok(())
        })?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    pub(super) fn materialize_routines(
        &self,
        db: &mut Db,
        now_ms: u64,
    ) -> Result<CommandResult, EngineError> {
        let routines = read_routines(db.conn())?;
        for routine in routines {
            self.materialize_one_routine(db, &routine, now_ms)?;
        }
        // No single target entity; return a routine-kind sentinel.
        Ok(CommandResult::new(
            EntityRef::new(EntityKind::Routine, [0u8; 16]),
            None,
            [0u8; 16],
            0,
        ))
    }

    /// Expand a routine over `[max(starts_at, materialized_until), now+horizon)`
    /// and INSERT-OR-IGNORE the resulting tasks, applying the catchup policy to
    /// occurrences at or before `now`. Runs in one transaction per routine:
    /// the inserts and the `materialized_until` advance commit together, so a
    /// crash never leaves the watermark ahead of the tasks it represents.
    pub(super) fn materialize_one_routine(
        &self,
        db: &mut Db,
        routine: &Routine,
        now_ms: u64,
    ) -> Result<(), EngineError> {
        if routine.paused || routine.archived || routine.deleted {
            return Ok(());
        }
        let mat_until = read_materialized_until(db.conn(), routine.id.bytes())?;
        let horizon_ms = u64::from(materialization_horizon_days(routine.rrule.freq)) * 86_400_000;
        let window_end_ms = now_ms.saturating_add(horizon_ms);
        let starts_ms = u64::try_from(routine.starts_at.as_millisecond().max(0)).unwrap_or(0);
        let window_start_ms = starts_ms.max(mat_until);
        if window_end_ms <= window_start_ms {
            return Ok(());
        }
        let window = (
            ms_to_ts(window_start_ms as i64),
            ms_to_ts(window_end_ms as i64),
        );
        let occ = routine
            .occurrences_in(window)
            .map_err(|e| EngineError::Invalid(format!("routine expand: {e}")))?;
        let now_ts = ms_to_ts(now_ms as i64);

        // Partition into missed (<= now) and future (> now).
        let (missed, future): (Vec<_>, Vec<_>) = occ.into_iter().partition(|o| o.at <= now_ts);

        // Build the (key, instant, optional-title-override) tuples to insert.
        let mut jobs: Vec<(String, jiff::Timestamp, Option<String>)> = Vec::new();
        match routine.catchup_policy {
            RoutineCatchupPolicy::Skip => {}
            RoutineCatchupPolicy::Queue => {
                for o in &missed {
                    jobs.push((o.key.clone(), o.at, None));
                }
            }
            RoutineCatchupPolicy::Merge => {
                let n = missed.len();
                if n >= 2 {
                    let latest = &missed[n - 1];
                    let title = format!("{} (x{n} catch-up)", routine.template.title);
                    jobs.push((latest.key.clone(), latest.at, Some(title)));
                } else if let Some(o) = missed.first() {
                    jobs.push((o.key.clone(), o.at, None));
                }
            }
        }
        for o in &future {
            jobs.push((o.key.clone(), o.at, None));
        }

        let routine = routine.clone();
        let clock = self.clock.clone();
        let rng = self.rng.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &routine.template.stream_id, now_ms)?;
            for (key, at, title_override) in &jobs {
                let task = build_routine_task(&routine, key, *at, title_override.clone(), now_ms);
                // Each materialized occurrence is its OWN op, so it gets its own
                // seq and its own HLC tick. Minting both before the insert keeps
                // the row's stamp and the op's envelope in agreement.
                let seq = self.next_seq_tx(tx, task.stream_id.bytes())?;
                let lww = self.lww_stamp(seq);
                let inserted = insert_task_row_or_ignore(tx, &task, &lww)?;
                if !inserted {
                    continue;
                }
                insert_task_contexts(tx, &task)?;
                ftsr_upsert_task(tx, &task)?;
                let inner_op = encode_inner_op(&InnerOp::TaskCreate(task.clone()))
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                let mut rand = [0u8; 10];
                rng.fill_bytes(&mut rand);
                let op_id = *Ulid::from_timestamp_and_random(clock.now_ms(), rand).as_bytes();
                self.ops_insert(
                    tx,
                    &op_id,
                    task.stream_id.bytes(),
                    seq,
                    lww.hlc,
                    &inner_op,
                    "task.create",
                    "task",
                    Some(task.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
            }
            set_materialized_until(tx, routine.id.bytes(), window_end_ms)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Regeneration policy (v1): after a structural routine edit, delete future
    /// routine-materialized tasks that are still untouched (Todo, non-deferred)
    /// and whose deterministic id no longer matches any current future
    /// occurrence. Started, completed, deferred, or user-moved tasks are kept.
    fn regenerate_future_tasks(
        &self,
        db: &mut Db,
        routine: &Routine,
        now_ms: u64,
    ) -> Result<(), EngineError> {
        let horizon_ms = u64::from(materialization_horizon_days(routine.rrule.freq)) * 86_400_000;
        let now_ts = ms_to_ts(now_ms as i64);
        let window = (now_ts, ms_to_ts(now_ms.saturating_add(horizon_ms) as i64));
        let valid: BTreeSet<[u8; 16]> = routine
            .occurrences_in(window)
            .map_err(|e| EngineError::Invalid(format!("routine expand: {e}")))?
            .iter()
            .map(|o| *occurrence_task_id(&routine.id, &o.key).bytes())
            .collect();

        // Candidate future, not-started routine tasks for this routine.
        let rid_blob: Vec<u8> = routine.id.bytes().to_vec();
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE routine_id = ? AND deleted = 0 AND state = 'todo'
               AND deferred_count = 0
               AND scheduled_at_ms IS NOT NULL AND scheduled_at_ms > ?",
        )?;
        let ids = stmt
            .query_map(params![rid_blob, now_ms as i64], |row| {
                row.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if valid.contains(&bytes) {
                continue;
            }
            let Some(mut t) = read_task(db.conn(), &bytes)? else {
                continue;
            };
            t.deleted = true;
            t.updated_at = ms_to_ts(now_ms as i64);
            let op_id = self.fresh_op_id(now_ms);
            let inner_op = encode_inner_op(&InnerOp::TaskDelete(t.clone()))?;
            let seq = self.next_seq(db, t.stream_id.bytes())?;
            let lww = self.lww_stamp(seq);
            let t_clone = t.clone();
            db.with_tx(|tx| -> rusqlite::Result<()> {
                update_task_row(tx, &t_clone, &lww)?;
                ftsr_delete_task(tx, &t_clone.id)?;
                self.ops_insert(
                    tx,
                    &op_id,
                    t_clone.stream_id.bytes(),
                    seq,
                    lww.hlc,
                    &inner_op,
                    "task.delete",
                    "task",
                    Some(t_clone.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    pub(super) fn query_routines(&self, db: &Db) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Routines(read_routines(db.conn())?))
    }
}

// ---- table operations ----

/// INSERT OR IGNORE a task row (materialization dedup by deterministic id).
/// Returns `true` when a new row was actually inserted.
fn insert_task_row_or_ignore(
    tx: &Transaction<'_>,
    t: &Task,
    lww: &LwwStamp,
) -> rusqlite::Result<bool> {
    let (sched_ms, sched_kind, sched_tz) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz) = time_to_parts(t.completed_at.as_ref());
    let extra_blob = encode_unknowns(&t.unknown)?;
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    let changed = tx.execute(
        "INSERT OR IGNORE INTO tasks
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
    Ok(changed > 0)
}

/// Build a materialized routine task for `(key, at)`. `title_override` carries
/// the merge suffix when a catch-up collapses ≥2 missed occurrences.
fn build_routine_task(
    routine: &Routine,
    key: &str,
    at: jiff::Timestamp,
    title_override: Option<String>,
    now_ms: u64,
) -> Task {
    let draft = routine.template.to_draft(Some(at));
    Task {
        reminder_lead_s: draft.reminder_lead_s,
        id: occurrence_task_id(&routine.id, key),
        created_at: ms_to_ts(now_ms as i64),
        updated_at: ms_to_ts(now_ms as i64),
        title: title_override.unwrap_or(draft.title),
        body: draft.body,
        stream_id: routine.template.stream_id,
        contexts: draft.contexts.into_iter().collect(),
        state: TaskState::Todo,
        priority: draft.priority,
        energy: draft.energy,
        estimated_duration_s: draft.estimated_duration_s,
        scheduled_at: Some(at.into()),
        due_at: None,
        // Routine constraints are copied verbatim onto each occurrence.
        scheduling_constraints: routine.scheduling_constraints.clone(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: Some(routine.id),
        routine_occurrence: Some(at),
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

fn catchup_policy_str(p: RoutineCatchupPolicy) -> &'static str {
    match p {
        RoutineCatchupPolicy::Skip => "skip",
        RoutineCatchupPolicy::Merge => "merge",
        RoutineCatchupPolicy::Queue => "queue",
    }
}

fn parse_catchup_policy(s: &str) -> RoutineCatchupPolicy {
    match s {
        "merge" => RoutineCatchupPolicy::Merge,
        "queue" => RoutineCatchupPolicy::Queue,
        _ => RoutineCatchupPolicy::Skip,
    }
}

/// Encode a CBOR blob for a non-empty serializable value, or `None` when empty
/// (mirrors `encode_constraints`, keeping empty lists as SQL NULL).
fn encode_blob_opt<T: serde::Serialize>(
    value: &T,
    is_empty: bool,
) -> rusqlite::Result<Option<Vec<u8>>> {
    if is_empty {
        return Ok(None);
    }
    let bytes = sunrise_cbor::encode_canonical(value)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(Some(bytes))
}

fn decode_blob_opt<T: serde::de::DeserializeOwned + serde::Serialize + Default>(
    blob: Option<Vec<u8>>,
) -> Result<T, EngineError> {
    match blob {
        None => Ok(T::default()),
        Some(bytes) => {
            sunrise_cbor::decode_canonical(&bytes).map_err(|e| EngineError::Cbor(e.to_string()))
        }
    }
}

/// The streak fields of a Routine that migration 0009 projects into the single
/// `routines.streak_state` blob.
///
/// `streak_counter` and `last_completed_at_ms` keep their own columns (0001 /
/// 0004); these five are read and written as a unit and are opaque to SQL, so
/// one canonical-CBOR blob beats five more positional columns. All-default
/// means `NULL`, which is also what every pre-v9 row upgrades into.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct StreakStateProj {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grace_window_s: Option<u64>,
    #[serde(default)]
    forgiveness_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    streak_started_at_ms: Option<i64>,
    #[serde(default)]
    forgivenesses_in_window: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    streak_keys: Vec<String>,
}

impl StreakStateProj {
    fn from_routine(r: &Routine) -> Self {
        Self {
            grace_window_s: r.grace_window_s,
            forgiveness_disabled: !r.forgiveness_enabled,
            streak_started_at_ms: r.streak_started_at.map(|t| t.as_millisecond()),
            forgivenesses_in_window: r.forgivenesses_in_window,
            streak_keys: r.streak_keys.clone(),
        }
    }

    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Encode the streak projection, storing `NULL` for the all-default state.
fn encode_streak_state(r: &Routine) -> rusqlite::Result<Option<Vec<u8>>> {
    let proj = StreakStateProj::from_routine(r);
    let empty = proj.is_default();
    encode_blob_opt(&proj, empty)
}

pub(super) fn insert_routine_row(
    tx: &Transaction<'_>,
    r: &Routine,
    now_ms: u64,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    let streak_blob = encode_streak_state(r)?;
    let extra_blob = encode_unknowns(&r.unknown)?;
    tx.execute(
        "INSERT INTO routines
         (id, stream_id, rrule_text, timezone, starts_at_ms, ends_at_ms,
          streak_counter, paused, archived, deleted, scheduling_constraints,
          template, skip_dates, skipped_keys, catchup_policy,
          last_completed_at_ms, paused_until_ms, created_at_ms, updated_at_ms,
          streak_state, extra, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            rrule_text,
            r.timezone,
            r.starts_at.as_millisecond(),
            r.ends_at.map(|d| d.as_millisecond()),
            r.streak_counter,
            r.paused as i64,
            r.archived as i64,
            r.deleted as i64,
            constraints_blob,
            template_blob,
            skip_dates_blob,
            skipped_keys_blob,
            catchup_policy_str(r.catchup_policy),
            r.last_completed_at.map(|d| d.as_millisecond()),
            r.paused_until.map(|d| d.as_millisecond()),
            now_ms,
            now_ms,
            streak_blob,
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

pub(super) fn update_routine_row(
    tx: &Transaction<'_>,
    r: &Routine,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    let streak_blob = encode_streak_state(r)?;
    let extra_blob = encode_unknowns(&r.unknown)?;
    tx.execute(
        "UPDATE routines SET
            stream_id = ?, rrule_text = ?, timezone = ?,
            starts_at_ms = ?, ends_at_ms = ?, streak_counter = ?, paused = ?,
            archived = ?, deleted = ?, scheduling_constraints = ?, template = ?,
            skip_dates = ?, skipped_keys = ?, catchup_policy = ?,
            last_completed_at_ms = ?, paused_until_ms = ?, updated_at_ms = ?,
            streak_state = ?, extra = ?,
            lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            stream_blob,
            rrule_text,
            r.timezone,
            r.starts_at.as_millisecond(),
            r.ends_at.map(|d| d.as_millisecond()),
            r.streak_counter,
            r.paused as i64,
            r.archived as i64,
            r.deleted as i64,
            constraints_blob,
            template_blob,
            skip_dates_blob,
            skipped_keys_blob,
            catchup_policy_str(r.catchup_policy),
            r.last_completed_at.map(|d| d.as_millisecond()),
            r.paused_until.map(|d| d.as_millisecond()),
            r.updated_at.as_millisecond(),
            streak_blob,
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    Ok(())
}

fn read_materialized_until(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<u64, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let v: Option<i64> = conn
        .query_row(
            "SELECT materialized_until_ms FROM routines WHERE id = ?",
            params![id_blob],
            |r| r.get(0),
        )
        .optional()?;
    Ok(u64::try_from(v.unwrap_or(0).max(0)).unwrap_or(0))
}

fn set_materialized_until(tx: &Transaction<'_>, id: &[u8; 16], ms: u64) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = id.to_vec();
    tx.execute(
        "UPDATE routines SET materialized_until_ms = ? WHERE id = ?",
        params![i64::try_from(ms).unwrap_or(i64::MAX), id_blob],
    )?;
    Ok(())
}

fn routine_from_row(
    id: &[u8; 16],
    rrule_text: &str,
    timezone: String,
    starts_ms: i64,
    ends_ms: Option<i64>,
    streak: i64,
    paused: i64,
    archived: i64,
    deleted: i64,
    constraints: Option<Vec<u8>>,
    template: Option<Vec<u8>>,
    skip_dates: Option<Vec<u8>>,
    skipped_keys: Option<Vec<u8>>,
    catchup: &str,
    last_completed_ms: Option<i64>,
    paused_until_ms: Option<i64>,
    created_ms: i64,
    updated_ms: i64,
    streak_state: Option<Vec<u8>>,
    extra: Option<Vec<u8>>,
) -> Result<Routine, EngineError> {
    let rrule = sunrise_domain::RRule::parse(rrule_text)
        .map_err(|e| EngineError::Invalid(format!("stored rrule: {e}")))?;
    let template: TaskTemplate = match template {
        Some(b) => {
            sunrise_cbor::decode_canonical(&b).map_err(|e| EngineError::Cbor(e.to_string()))?
        }
        None => return Err(EngineError::Invalid("routine missing template".into())),
    };
    // NULL (every pre-v9 row, and every routine never completed) decodes to the
    // all-default streak state.
    let streak_proj: StreakStateProj = decode_blob_opt(streak_state)?;
    Ok(Routine {
        id: EntityRef::new(EntityKind::Routine, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        template,
        rrule,
        timezone,
        starts_at: ms_to_ts(starts_ms.max(0)),
        ends_at: ends_ms.map(|m| ms_to_ts(m.max(0))),
        scheduling_constraints: decode_constraints(constraints)?,
        skip_dates: decode_blob_opt(skip_dates)?,
        skipped_keys: decode_blob_opt(skipped_keys)?,
        catchup_policy: parse_catchup_policy(catchup),
        streak_counter: streak,
        last_completed_at: last_completed_ms.map(|m| ms_to_ts(m.max(0))),
        grace_window_s: streak_proj.grace_window_s,
        forgiveness_enabled: !streak_proj.forgiveness_disabled,
        streak_started_at: streak_proj.streak_started_at_ms.map(|m| ms_to_ts(m.max(0))),
        forgivenesses_in_window: streak_proj.forgivenesses_in_window,
        streak_keys: streak_proj.streak_keys,
        paused: paused != 0,
        paused_until: paused_until_ms.map(|m| ms_to_ts(m.max(0))),
        archived: archived != 0,
        deleted: deleted != 0,
        unknown: decode_unknowns(extra),
    })
}

const ROUTINE_COLUMNS: &str = "rrule_text, timezone, starts_at_ms, ends_at_ms,
     streak_counter, paused, archived, deleted, scheduling_constraints,
     template, skip_dates, skipped_keys, catchup_policy, last_completed_at_ms,
     paused_until_ms, created_at_ms, updated_at_ms, streak_state, extra";

pub(super) fn read_routine(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Routine>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let sql = format!("SELECT {ROUTINE_COLUMNS} FROM routines WHERE id = ?");
    let raw = conn
        .query_row(&sql, params![id_blob], |r| {
            // Collect the columns as owned values first (query_row's closure
            // must return rusqlite::Result); decode outside.
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, Option<Vec<u8>>>(8)?,
                r.get::<_, Option<Vec<u8>>>(9)?,
                r.get::<_, Option<Vec<u8>>>(10)?,
                r.get::<_, Option<Vec<u8>>>(11)?,
                r.get::<_, String>(12)?,
                r.get::<_, Option<i64>>(13)?,
                r.get::<_, Option<i64>>(14)?,
                r.get::<_, i64>(15)?,
                r.get::<_, i64>(16)?,
                r.get::<_, Option<Vec<u8>>>(17)?,
                r.get::<_, Option<Vec<u8>>>(18)?,
            ))
        })
        .optional()?;
    let Some(v) = raw else {
        return Ok(None);
    };
    let routine = routine_from_row(
        id, &v.0, v.1, v.2, v.3, v.4, v.5, v.6, v.7, v.8, v.9, v.10, v.11, &v.12, v.13, v.14, v.15,
        v.16, v.17, v.18,
    )?;
    Ok(Some(routine))
}

pub(super) fn read_routines(conn: &rusqlite::Connection) -> Result<Vec<Routine>, EngineError> {
    let sql =
        format!("SELECT id, {ROUTINE_COLUMNS} FROM routines WHERE deleted = 0 ORDER BY id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            let id_raw: Vec<u8> = r.get(0)?;
            Ok((
                id_raw,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, Option<Vec<u8>>>(9)?,
                r.get::<_, Option<Vec<u8>>>(10)?,
                r.get::<_, Option<Vec<u8>>>(11)?,
                r.get::<_, Option<Vec<u8>>>(12)?,
                r.get::<_, String>(13)?,
                r.get::<_, Option<i64>>(14)?,
                r.get::<_, Option<i64>>(15)?,
                r.get::<_, i64>(16)?,
                r.get::<_, i64>(17)?,
                r.get::<_, Option<Vec<u8>>>(18)?,
                r.get::<_, Option<Vec<u8>>>(19)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for v in rows {
        let mut id = [0u8; 16];
        let take = v.0.len().min(16);
        id[..take].copy_from_slice(&v.0[..take]);
        out.push(routine_from_row(
            &id, &v.1, v.2, v.3, v.4, v.5, v.6, v.7, v.8, v.9, v.10, v.11, v.12, &v.13, v.14, v.15,
            v.16, v.17, v.18, v.19,
        )?);
    }
    Ok(out)
}
