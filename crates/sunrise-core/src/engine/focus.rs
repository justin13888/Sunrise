//! Focus sessions and interruptions (ADR-0013).
//!
//! The one entity family outside LWW. `materialize_remote` routes a focus op
//! away before the contest starts, because all three of these tables — focus
//! starts, focus ends and interruptions — are **append-only**: two devices
//! writing concurrently produce two rows, not two versions of one row, so there
//! is nothing for a stamp comparison to decide.
//!
//! That different storage contract is what makes the family worth seeing as one
//! unit: a read here folds rows into a session rather than loading one.

use super::ids::{
    decode_unknowns, encode_unknowns, energy_str, ms_to_ts, parse_energy, require_kind,
};
use super::lww::LwwStamp;
use super::routine::update_routine_row;
use super::stream::ensure_stream_row;
use super::task::{actionable_scan, ftsr_upsert_task, read_task, ref_of, update_task_row};
use super::{Engine, EngineError, FOCUS_PLAN_SCAN_CAP, META_STREAM};
use crate::commands::{CommandResult, FocusStartDraft};
use crate::inner_op::{encode_inner_op, InnerOp};
use crate::queries::{FocusPlanRow, FocusSessionRow, QueryResult};
use rusqlite::{params, Transaction};
use std::collections::BTreeMap;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    break_after, fold_focus_stats, plan_session, rank_focus_plan, Chunk, Energy, FocusEnd,
    FocusKind, FocusSession, FocusStart, Interruption, InterruptionReason, PlanCandidate, Routine,
    SessionLength, SessionRecord, Task, TaskState, POMODORO_MS,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    /// Open a focus session: ADR-0013's `start` op.
    ///
    /// Writes an append-only record keyed by a fresh `fcs_` id and touches the
    /// Task not at all. Two devices doing this concurrently mint different ids,
    /// so both rows survive a merge and both feed the stats — the property that
    /// used to need an OR-Set and now falls out of the key.
    ///
    /// The planned length, the break cadence and the `chunk N of M` marker are
    /// all **derived here** from the Task's estimate and its prior sessions,
    /// not taken from the caller, so every client records the same shape.
    pub(super) fn start_focus(
        &self,
        db: &mut Db,
        d: FocusStartDraft,
    ) -> Result<CommandResult, EngineError> {
        require_kind(d.task_id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let task = read_task(db.conn(), d.task_id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {}", d.task_id)))?;
        if task.deleted {
            return Err(EngineError::Invalid(
                "cannot start a focus session on a deleted task".into(),
            ));
        }
        let prior = count_work_sessions(db.conn(), d.task_id.bytes())?;
        // A break is sized by the pomodoro cycle rather than by the task's
        // estimate: every fourth one is the long break.
        let (planned_ms, chunk) = match d.kind {
            FocusKind::Break => (Some(break_after(prior.max(1)).default_ms()), None),
            FocusKind::Work => {
                let p = plan_session(d.length, task.estimated_duration_s, prior, POMODORO_MS);
                (p.planned_ms, p.chunk)
            }
        };
        let session_id = self.fresh_id(EntityKind::FocusSession, now_ms);
        let start = FocusStart {
            id: session_id,
            task_id: d.task_id,
            stream_id: task.stream_id,
            started_at: sunrise_domain::epoch_ms::from_u64(now_ms),
            planned_ms,
            // The declared budget wins; the Task's own facet is the fallback.
            energy: d.energy.or(task.energy),
            kind: d.kind,
            chunk,
            unknown: Unknowns::new(),
        };
        let inner = encode_inner_op(&InnerOp::FocusStart(Box::new(start.clone())))?;
        let op_id = self.fresh_op_id(now_ms);
        let stream_bytes = *start.stream_id.bytes();
        let seq = self.next_seq(db, &stream_bytes)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &start.stream_id, now_ms)?;
            insert_focus_start_row(tx, &start, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner,
                "focus.start",
                "focus_session",
                Some(session_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(session_id, None, op_id, seq))
    }

    /// Close a focus session: ADR-0013's `end` op, a *separate* append-only
    /// record addressed to the same session id.
    ///
    /// This is the one moment the elapsed time becomes a fact: everywhere else
    /// it is derived on read, and nothing ticking is ever written.
    pub(super) fn end_focus(
        &self,
        db: &mut Db,
        session: EntityRef,
        actual_focused_ms: Option<u64>,
        completed_task: bool,
    ) -> Result<CommandResult, EngineError> {
        require_kind(session, EntityKind::FocusSession)?;
        let now_ms = self.clock.now_ms();
        let view = read_focus_session(db.conn(), session.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("focus session {session}")))?;
        if view.end.is_some() {
            // A session is immutable once closed. Re-closing it would be the
            // mutable-register behaviour ADR-0013 exists to avoid.
            return Err(EngineError::Invalid(format!(
                "focus session {session} has already ended"
            )));
        }
        // Freeze the focused time. The caller may pass a smaller figure when it
        // tracked pauses; the default is the derived elapsed span.
        let actual = actual_focused_ms.unwrap_or_else(|| view.elapsed_ms(now_ms));
        let end = FocusEnd {
            session_id: session,
            ended_at: sunrise_domain::epoch_ms::from_u64(now_ms.max(view.start.started_at_ms())),
            actual_focused_ms: actual,
            interruptions: view.interruptions.clone(),
            completed_task,
            unknown: Unknowns::new(),
        };
        // Issue #10. The user pressed "complete" in the focus UI; the task
        // register is what has to change, and until now nothing changed it.
        // Derived HERE, on the originating device only, and emitted as an
        // ordinary `task.update` — see `autocomplete_focused_task`.
        let completion = if completed_task {
            self.autocomplete_focused_task(db, view.start.task_id, now_ms)?
        } else {
            None
        };
        let inner = encode_inner_op(&InnerOp::FocusEnd(Box::new(end.clone())))?;
        let op_id = self.fresh_op_id(now_ms);
        let stream_bytes = *view.start.stream_id.bytes();
        let seq = self.next_seq(db, &stream_bytes)?;
        let lww = self.lww_stamp(seq);
        let state = completion.as_ref().map(|c| c.task.state);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_focus_end_row(tx, &end, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner,
                "focus.end",
                "focus_session",
                Some(session.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            if let Some(c) = &completion {
                let task_seq = self.next_seq_tx(tx, c.task.stream_id.bytes())?;
                let task_lww = self.lww_stamp(task_seq);
                let task_op_id = self.fresh_op_id(now_ms);
                update_task_row(tx, &c.task, &task_lww)?;
                ftsr_upsert_task(tx, &c.task)?;
                self.ops_insert(
                    tx,
                    &task_op_id,
                    c.task.stream_id.bytes(),
                    task_seq,
                    task_lww.hlc,
                    &c.inner,
                    "task.update",
                    "task",
                    Some(c.task.id.bytes()),
                    Some(now_ms),
                    None,
                    now_ms,
                    &[],
                )?;
                // Same rule as a hand-completed occurrence: the streak advance
                // rides in the same transaction as the completion that caused
                // it (`docs/08-features/focus-mode.md`: "a focus session that
                // completes a routine occurrence increments the routine
                // streak").
                if let Some((routine, routine_inner)) = &c.streak {
                    let routine_seq = self.next_seq_tx(tx, &META_STREAM)?;
                    let routine_lww = self.lww_stamp(routine_seq);
                    let routine_op_id = self.fresh_op_id(now_ms);
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
            }
            Ok(())
        })?;
        Ok(CommandResult::new(session, state, op_id, seq))
    }

    /// **Auto-completion (issue #10).** Close the task a focus session says was
    /// finished, if it is still open.
    ///
    /// # The signal
    ///
    /// Exactly one: `EndFocus { completed_task: true }`. That is not an
    /// inference about the user, it is a **statement by the user** — the focus
    /// screen offers three actions and "complete" is one of them
    /// (`docs/08-features/focus-mode.md` §Focus mode). The fact was already
    /// recorded on the session; the task register simply never moved, which is
    /// what issue #10 calls out as "`completed_at` is never set automatically".
    ///
    /// Signals deliberately NOT taken, because each is an inference rather than
    /// a statement, and inferring is where a task manager turns into the
    /// "no if-this-then-that" non-goal:
    ///
    /// * *A routine occurrence whose window elapsed.* Missed is not done.
    ///   Marking it complete would inflate every completion count in the
    ///   review; the catch-up policy is where a missed occurrence belongs.
    /// * *All of a task's blockers completing.* Blockers describe order, not
    ///   content. A task with no remaining blockers is ready, which is exactly
    ///   what `EffectiveTaskState` already derives.
    /// * *A Block ending with the task still bound.* A Block is a plan for
    ///   when to do work; the spec says outright that there is no "ran the
    ///   block" state and that we trust the user.
    ///
    /// # Determinism and convergence
    ///
    /// The derivation happens **once, on the originating device**, and its
    /// result is an ordinary full-state `task.update` op. A replica applying
    /// the `focus.end` op derives nothing — `materialize_focus_remote` writes
    /// the session record and stops — so the completion cannot be re-derived,
    /// re-timed, or derived differently anywhere. It merges through the same
    /// entity-level LWW path as any hand-edit, so a concurrent edit of the same
    /// task on another device is resolved by the same `(hlc, device, seq)` rule
    /// as every other conflict, with no second writer of task state.
    ///
    /// Returns `None` — a plain no-op, never an error — when there is nothing
    /// to complete: the task is gone, already `done`, or `cancelled`. Cancelled
    /// is not a mistake to correct: `TaskState::can_transition_to` forbids
    /// `cancelled -> done`, and a session closing over a task the user
    /// cancelled must not silently overrule them.
    fn autocomplete_focused_task(
        &self,
        db: &Db,
        task_id: EntityRef,
        now_ms: u64,
    ) -> Result<Option<FocusCompletion>, EngineError> {
        let Some(mut task) = read_task(db.conn(), task_id.bytes())? else {
            return Ok(None);
        };
        if task.deleted || !matches!(task.state, TaskState::Todo | TaskState::InProgress) {
            return Ok(None);
        }
        task.state = TaskState::Done;
        task.completed_at = Some(ms_to_ts(now_ms as i64).into());
        task.updated_at = ms_to_ts(now_ms as i64);
        let streak = self.streak_advance(db, &task, now_ms)?;
        let inner = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        Ok(Some(FocusCompletion {
            task,
            inner,
            streak,
        }))
    }

    /// Log one interruption against a session — a grow-only set member, keyed
    /// by `(session, at_ms, reason)` so re-delivery is a no-op and two devices'
    /// interruptions both survive.
    ///
    /// Allowed against an already-ended session too: a client that closes the
    /// timer before the user picks a reason must not lose the reason.
    pub(super) fn log_interruption(
        &self,
        db: &mut Db,
        session: EntityRef,
        reason: InterruptionReason,
    ) -> Result<CommandResult, EngineError> {
        require_kind(session, EntityKind::FocusSession)?;
        let now_ms = self.clock.now_ms();
        let start = read_focus_start(db.conn(), session.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("focus session {session}")))?;
        let interruption = Interruption {
            session_id: session,
            at: sunrise_domain::epoch_ms::from_u64(now_ms),
            reason,
        };
        let inner = encode_inner_op(&InnerOp::FocusInterrupt(interruption))?;
        let op_id = self.fresh_op_id(now_ms);
        let stream_bytes = *start.stream_id.bytes();
        let seq = self.next_seq(db, &stream_bytes)?;
        let lww = self.lww_stamp(seq);
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_interruption_row(tx, &interruption)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner,
                "focus.interrupt",
                "focus_session",
                Some(session.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult::new(session, None, op_id, seq))
    }

    /// The Focus Planner (`docs/08-features/focus-mode.md` §Focus Planner).
    ///
    /// Reuses [`Self::query_actionable`]'s graph walk — same SQL, with the
    /// blocked rows filtered out in the database rather than after — and hands
    /// the result to the pure `rank_focus_plan`, which applies the energy match
    /// and the leverage ordering. The scan is capped (see
    /// [`FOCUS_PLAN_SCAN_CAP`]) because the ranking cannot be pushed into SQL.
    pub(super) fn query_focus_plan(
        &self,
        db: &Db,
        stream: Option<EntityRef>,
        energy: Option<Energy>,
        length: SessionLength,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        if let Some(s) = stream {
            require_kind(s, EntityKind::Stream)?;
        }
        let scan = actionable_scan(db.conn(), stream, FOCUS_PLAN_SCAN_CAP, true)?;
        let priors = work_session_counts(db.conn())?;
        let mut tasks: BTreeMap<EntityRef, Task> = BTreeMap::new();
        let mut candidates = Vec::with_capacity(scan.len());
        for (bytes, open_blockers, unblocks) in scan {
            let Some(task) = read_task(db.conn(), &bytes)? else {
                continue;
            };
            candidates.push(PlanCandidate {
                task: task.id,
                energy: task.energy,
                unblocks,
                open_blockers,
                priority: task.priority,
                due_at_ms: task
                    .due_at
                    .as_ref()
                    .and_then(|t| u64::try_from(t.index_ms()).ok()),
                scheduled_at_ms: task
                    .scheduled_at
                    .as_ref()
                    .and_then(|t| u64::try_from(t.index_ms()).ok()),
                estimated_duration_s: task.estimated_duration_s,
            });
            tasks.insert(task.id, task);
        }
        let ranked = rank_focus_plan(candidates, energy);
        let mut out = Vec::with_capacity(ranked.len().min(limit as usize));
        for r in ranked.into_iter().take(limit as usize) {
            let Some(task) = tasks.remove(&r.candidate.task) else {
                continue;
            };
            let prior = priors.get(&task.id).copied().unwrap_or(0);
            out.push(FocusPlanRow {
                unblocks: r.candidate.unblocks,
                energy_fit: r.fit,
                suggested: plan_session(length, task.estimated_duration_s, prior, POMODORO_MS),
                prior_sessions: prior,
                task,
            });
        }
        Ok(QueryResult::FocusPlan(out))
    }

    /// Focus sessions recorded against one Task, newest first — running ones
    /// included, because a `start` with no `end` is a valid session.
    pub(super) fn query_task_focus_sessions(
        &self,
        db: &Db,
        task: EntityRef,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        require_kind(task, EntityKind::Task)?;
        let sessions = read_focus_sessions(
            db.conn(),
            "WHERE s.task_id = ?1 ORDER BY s.started_at_ms DESC, s.id DESC LIMIT ?2",
            rusqlite::params![task.bytes().to_vec(), limit],
        )?;
        Ok(QueryResult::FocusSessions(self.focus_rows(sessions)))
    }

    /// Every session with no `end` op yet. This is how a client resumes after a
    /// crash: the dangling start is the state, and reading it is the repair.
    pub(super) fn query_running_focus_sessions(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let sessions = read_focus_sessions(
            db.conn(),
            "WHERE e.session_id IS NULL ORDER BY s.started_at_ms ASC, s.id ASC",
            rusqlite::params![],
        )?;
        Ok(QueryResult::FocusSessions(self.focus_rows(sessions)))
    }

    /// Decorate session views with the two clock-derived numbers, so callers
    /// never have to reach for a clock of their own.
    fn focus_rows(&self, sessions: Vec<FocusSession>) -> Vec<FocusSessionRow> {
        let now_ms = self.clock.now_ms();
        sessions
            .into_iter()
            .map(|session| FocusSessionRow {
                running: session.is_running(),
                focused_ms: session.focused_ms(now_ms),
                session,
            })
            .collect()
    }

    /// Estimate calibration + focus totals: read the immutable session log,
    /// join each session to its Task's estimate, and hand the whole thing to
    /// the pure `fold_focus_stats`. All the arithmetic lives in the domain
    /// crate so it unit-tests with no database at all.
    pub(super) fn query_focus_stats(
        &self,
        db: &Db,
        stream: Option<EntityRef>,
        since_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        if let Some(s) = stream {
            require_kind(s, EntityKind::Stream)?;
        }
        let stream_blob: Option<Vec<u8>> = stream.map(|s| s.bytes().to_vec());
        let since = since_ms.and_then(|v| i64::try_from(v).ok());
        let interruptions = read_all_interruptions(db.conn())?;
        let mut stmt = db.conn().prepare(
            "SELECT s.id, s.task_id, s.stream_id, s.energy, s.kind, s.started_at_ms,
                    e.ended_at_ms, e.actual_focused_ms, e.completed_task, t.estimated_min
             FROM focus_sessions s
               LEFT JOIN focus_session_ends e ON e.session_id = s.id
               LEFT JOIN tasks t ON t.id = s.task_id
             WHERE (?1 IS NULL OR s.stream_id = ?1)
               AND (?2 IS NULL OR s.started_at_ms >= ?2)
             ORDER BY s.started_at_ms ASC, s.id ASC",
        )?;
        let rows = stmt
            .query_map(params![stream_blob, since], |r| {
                Ok((
                    r.get::<_, Vec<u8>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let records: Vec<SessionRecord> = rows
            .into_iter()
            .map(|v| {
                let session = ref_of(EntityKind::FocusSession, &v.0);
                SessionRecord {
                    session,
                    task: ref_of(EntityKind::Task, &v.1),
                    stream: ref_of(EntityKind::Stream, &v.2),
                    energy: v.3.as_deref().and_then(parse_energy),
                    kind: FocusKind::from_str_opt(&v.4).unwrap_or(FocusKind::Work),
                    started_at_ms: u64::try_from(v.5.max(0)).unwrap_or(0),
                    ended_at_ms: v.6.and_then(|m| u64::try_from(m.max(0)).ok()),
                    actual_focused_ms: v.7.and_then(|m| u64::try_from(m.max(0)).ok()),
                    // `tasks.estimated_min` is the minute-granular projection
                    // of `Task.estimated_duration_s`; widen it back the same
                    // way `read_task` does so both reads agree.
                    estimated_duration_s: v.9.and_then(|m| {
                        let secs = m.checked_mul(60)?;
                        u64::try_from(secs).ok()
                    }),
                    completed_task: v.8.unwrap_or(0) != 0,
                    interruptions: interruptions.get(&session).cloned().unwrap_or_default(),
                }
            })
            .collect();
        Ok(QueryResult::FocusStats(Box::new(fold_focus_stats(
            &records, now_ms,
        ))))
    }
}

// ---- table operations ----

/// What `EndFocus` derives when the session says the task was finished: the
/// completed Task, its encoded `TaskUpdate` op, and the streak advance the
/// completion triggers when the task is a routine occurrence.
///
/// Built and consumed entirely on the local command path, by
/// [`Engine::autocomplete_focused_task`] and its one caller; a replica applying
/// a `focus.end` op derives nothing, so the remote materialization never sees
/// one. This is the first module scope after the focus-session methods, which
/// is as close to them as a method's return type can live.
struct FocusCompletion {
    task: Task,
    inner: Vec<u8>,
    streak: Option<(Routine, Vec<u8>)>,
}

/// Materialize one focus op. Every write is an insert that ignores a conflict
/// on its own primary key, which is what makes the whole family idempotent
/// under re-delivery and order-independent between `start` and `end`.
pub(super) fn materialize_focus_remote(
    tx: &Transaction<'_>,
    inner: &InnerOp,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let ts_ms = lww.hlc.physical_ms;
    match inner {
        InnerOp::FocusStart(f) => {
            // The owning stream row must exist (the session's stream is also
            // its op-log routing stream).
            ensure_stream_row(tx, &f.stream_id, ts_ms)?;
            insert_focus_start_row(tx, f, lww)
        }
        InnerOp::FocusEnd(f) => {
            // Deliberately no `ensure` of the start row: an `end` that arrived
            // first still lands, and the two join up when the start turns up.
            insert_focus_end_row(tx, f, lww)?;
            for i in &f.interruptions {
                insert_interruption_row(tx, i)?;
            }
            Ok(())
        }
        InnerOp::FocusInterrupt(i) => insert_interruption_row(tx, i),
        _ => Ok(()),
    }
}

/// Write the `start` record. Idempotent on the session id.
pub(super) fn insert_focus_start_row(
    tx: &Transaction<'_>,
    f: &FocusStart,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra_blob = encode_unknowns(&f.unknown)?;
    tx.execute(
        "INSERT OR IGNORE INTO focus_sessions
         (id, task_id, stream_id, started_at_ms, planned_ms, energy, kind,
          chunk_index, chunk_total, extra, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &f.id.bytes()[..],
            &f.task_id.bytes()[..],
            &f.stream_id.bytes()[..],
            f.started_at_ms() as i64,
            f.planned_ms.map(|v| v as i64),
            f.energy.map(energy_str),
            f.kind.as_str(),
            f.chunk.map(|c| i64::from(c.index)),
            f.chunk.map(|c| i64::from(c.total)),
            extra_blob,
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// Write the `end` record. Idempotent on the session id; a second `end` for the
/// same session (two devices closing one session concurrently) keeps the first
/// to arrive rather than letting a clock skew rewrite a frozen measurement.
pub(super) fn insert_focus_end_row(
    tx: &Transaction<'_>,
    f: &FocusEnd,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra_blob = encode_unknowns(&f.unknown)?;
    tx.execute(
        "INSERT OR IGNORE INTO focus_session_ends
         (session_id, ended_at_ms, actual_focused_ms, completed_task, extra,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &f.session_id.bytes()[..],
            f.ended_at_ms() as i64,
            f.actual_focused_ms as i64,
            i64::from(f.completed_task),
            extra_blob,
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// Add one interruption to the grow-only set.
fn insert_interruption_row(tx: &Transaction<'_>, i: &Interruption) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO focus_interruptions (session_id, at_ms, reason)
         VALUES (?, ?, ?)",
        params![
            &i.session_id.bytes()[..],
            i.at_ms() as i64,
            i.reason.as_str(),
        ],
    )?;
    Ok(())
}

/// How many **work** sessions a Task has already had — the input that makes a
/// chunk marker read "3 of 4" instead of always "1 of 4".
fn count_work_sessions(conn: &rusqlite::Connection, task: &[u8; 16]) -> Result<u32, EngineError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM focus_sessions WHERE task_id = ? AND kind = 'work'",
        params![&task[..]],
        |r| r.get(0),
    )?;
    Ok(u32::try_from(n.max(0)).unwrap_or(u32::MAX))
}

/// The same count for every task at once, for the planner.
fn work_session_counts(
    conn: &rusqlite::Connection,
) -> Result<BTreeMap<EntityRef, u32>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT task_id, COUNT(*) FROM focus_sessions WHERE kind = 'work' GROUP BY task_id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(raw, n)| {
            (
                ref_of(EntityKind::Task, &raw),
                u32::try_from(n.max(0)).unwrap_or(u32::MAX),
            )
        })
        .collect())
}

/// Every interruption in the vault, grouped by session.
fn read_all_interruptions(
    conn: &rusqlite::Connection,
) -> Result<BTreeMap<EntityRef, Vec<Interruption>>, EngineError> {
    let mut stmt = conn.prepare(
        "SELECT session_id, at_ms, reason FROM focus_interruptions
         ORDER BY session_id ASC, at_ms ASC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out: BTreeMap<EntityRef, Vec<Interruption>> = BTreeMap::new();
    for (raw, at, reason) in rows {
        let session_id = ref_of(EntityKind::FocusSession, &raw);
        out.entry(session_id).or_default().push(Interruption {
            session_id,
            at: sunrise_domain::epoch_ms::from_u64(u64::try_from(at.max(0)).unwrap_or(0)),
            reason: InterruptionReason::from_str_lossy(&reason),
        });
    }
    Ok(out)
}

/// The `start` record alone, without assembling the whole session view.
fn read_focus_start(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<FocusStart>, EngineError> {
    Ok(
        read_focus_sessions(conn, "WHERE s.id = ?1", rusqlite::params![&id[..]])?
            .pop()
            .map(|v| v.start),
    )
}

/// One assembled session view (start + optional end + interruptions).
fn read_focus_session(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<FocusSession>, EngineError> {
    Ok(read_focus_sessions(conn, "WHERE s.id = ?1", rusqlite::params![&id[..]])?.pop())
}

/// Assemble session views from the two append-only tables.
///
/// The `LEFT JOIN` is the mechanism behind "a dangling start is a valid state":
/// no `end` row simply means the session is still running, and no repair pass,
/// tombstone, or synthesized value is involved.
pub(super) fn read_focus_sessions(
    conn: &rusqlite::Connection,
    tail_sql: &str,
    p: &[&dyn rusqlite::ToSql],
) -> Result<Vec<FocusSession>, EngineError> {
    let sql = format!(
        "SELECT s.id, s.task_id, s.stream_id, s.started_at_ms, s.planned_ms, s.energy, s.kind,
                s.chunk_index, s.chunk_total, s.extra,
                e.ended_at_ms, e.actual_focused_ms, e.completed_task, e.extra
         FROM focus_sessions s
           LEFT JOIN focus_session_ends e ON e.session_id = s.id
         {tail_sql}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(p, |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<Vec<u8>>>(9)?,
                r.get::<_, Option<i64>>(10)?,
                r.get::<_, Option<i64>>(11)?,
                r.get::<_, Option<i64>>(12)?,
                r.get::<_, Option<Vec<u8>>>(13)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let interruptions = read_all_interruptions(conn)?;
    let mut out = Vec::with_capacity(rows.len());
    for v in rows {
        let id = ref_of(EntityKind::FocusSession, &v.0);
        let chunk = match (v.7, v.8) {
            (Some(index), Some(total)) => Some(Chunk {
                index: u32::try_from(index.max(0)).unwrap_or(0),
                total: u32::try_from(total.max(0)).unwrap_or(0),
            }),
            _ => None,
        };
        let start = FocusStart {
            id,
            task_id: ref_of(EntityKind::Task, &v.1),
            stream_id: ref_of(EntityKind::Stream, &v.2),
            started_at: sunrise_domain::epoch_ms::from_u64(u64::try_from(v.3.max(0)).unwrap_or(0)),
            planned_ms: v.4.and_then(|m| u64::try_from(m.max(0)).ok()),
            energy: v.5.as_deref().and_then(parse_energy),
            kind: FocusKind::from_str_opt(&v.6).unwrap_or(FocusKind::Work),
            chunk,
            unknown: decode_unknowns(v.9),
        };
        let mine = interruptions.get(&id).cloned().unwrap_or_default();
        let end = v.10.map(|ended| FocusEnd {
            session_id: id,
            ended_at: sunrise_domain::epoch_ms::from_u64(u64::try_from(ended.max(0)).unwrap_or(0)),
            actual_focused_ms: v.11.and_then(|m| u64::try_from(m.max(0)).ok()).unwrap_or(0),
            interruptions: mine.clone(),
            completed_task: v.12.unwrap_or(0) != 0,
            unknown: decode_unknowns(v.13),
        });
        out.push(FocusSession {
            start,
            end,
            interruptions: mine,
        });
    }
    Ok(out)
}
