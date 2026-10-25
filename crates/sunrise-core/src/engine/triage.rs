//! The triage queue and its four actions (ADR-0047 §3–§4, issue #335).
//!
//! `Query::Triage` is derived on every read from each open task's lateness
//! for the reader now (`engine::lateness`), its Keep and its stream's pause;
//! nothing about the queue is stored but the Keep itself. `Command::Triage`
//! applies one action to a selection: every id is checked and every write
//! planned against the vault as it stands before any is logged, one refusal
//! refuses the batch with nothing written, and the writes are one group
//! (ADR-0065 §6), each task with its own ops. Nothing here runs unless a user
//! command asks for it: no timer, sync handler or read writes a task.

use super::ids::{ms_to_ts, require_kind};
use super::lateness::LatenessReader;
use super::review::read_live_tasks;
use super::stream::read_stream;
use super::task::{read_task, TaskWrite};
use super::{Engine, EngineError};
use crate::commands::CommandResult;
use crate::queries::{QueryResult, TriageRow};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::{
    cancel_reason, check_completion, in_triage, starts_at, triage_order, DeferTarget, Stream,
    SunriseTime, Task, TaskPatch, TaskState, TriageAction,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    /// `Query::Triage`: every open task in the queue for the reader now, with
    /// its lateness, in the queue's order (`sunrise_domain::triage_order`).
    pub(super) fn query_triage(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let reader = self.lateness_reader(db, self.clock.now_ms())?;
        let mut streams: BTreeMap<[u8; 16], Option<Stream>> = BTreeMap::new();
        let mut rows = Vec::new();
        for task in read_live_tasks(db.conn())? {
            if !task.is_open() {
                continue;
            }
            let lateness = reader.of(&task);
            if !lateness.state.needs_decision() {
                continue;
            }
            let paused = match task.stream_id {
                Some(s) => {
                    let stream = match streams.get(s.bytes()) {
                        Some(held) => held.clone(),
                        None => {
                            let read = read_stream(db.conn(), s.bytes())?;
                            streams.insert(*s.bytes(), read.clone());
                            read
                        }
                    };
                    stream.is_some_and(|st| stream_paused(&st, &reader))
                }
                None => false,
            };
            if in_triage(&task, &lateness, paused) {
                rows.push(TriageRow { task, lateness });
            }
        }
        rows.sort_by(|a, b| triage_order((&a.task, &a.lateness), (&b.task, &b.lateness)));
        Ok(QueryResult::Triage(rows))
    }

    /// `Command::Triage { ids, action }`.
    pub(super) fn triage(
        &self,
        db: &mut Db,
        ids: Vec<EntityRef>,
        action: TriageAction,
    ) -> Result<CommandResult, EngineError> {
        if ids.is_empty() {
            return Err(EngineError::Invalid(
                "a triage action needs at least one task".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for id in &ids {
            require_kind(*id, EntityKind::Task)?;
            if !seen.insert(*id) {
                return Err(EngineError::Invalid(format!(
                    "{id} appears twice in one triage action"
                )));
            }
        }
        let now_ms = self.clock.now_ms();
        let reader = self.lateness_reader(db, now_ms)?;
        // Check the action itself once, before any task.
        let reason = match &action {
            TriageAction::Drop { reason } => cancel_reason(reason.as_deref())?,
            _ => None,
        };
        let mut writes = Vec::with_capacity(ids.len());
        for id in &ids {
            let conn = db.conn();
            let task = read_task(conn, id.bytes())?
                .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
            if task.deleted {
                return Err(EngineError::Invalid(format!("{id} is deleted")));
            }
            writes.push(match &action {
                TriageAction::Defer { to } => {
                    let to = match to {
                        DeferTarget::At(t) => t.clone(),
                        DeferTarget::NextFreeSlot => next_free_slot(&reader)?,
                    };
                    self.plan_defer(conn, *id, to, now_ms)?
                }
                TriageAction::AlreadyDone { completed_at } => {
                    self.plan_already_done(conn, &task, completed_at, &reader, now_ms)?
                }
                TriageAction::Drop { .. } => {
                    let patch = TaskPatch {
                        state: Some(TaskState::Cancelled),
                        ..Default::default()
                    };
                    let mut w = self.plan_update(conn, *id, patch, now_ms)?;
                    w.task.cancel_reason.clone_from(&reason);
                    w
                }
                TriageAction::Keep => {
                    let mut t = task;
                    t.late_acknowledged_at = Some(ms_to_ts(now_ms as i64));
                    t.updated_at = ms_to_ts(now_ms as i64);
                    TaskWrite::plain(t)
                }
            });
        }
        self.commit_task_writes(db, writes, now_ms)
    }

    /// The write an Already done of `task` at `completed_at` makes: `done`,
    /// with the given completion rather than now, checked by
    /// `sunrise_domain::check_completion` for the reader.
    fn plan_already_done(
        &self,
        conn: &rusqlite::Connection,
        task: &Task,
        completed_at: &SunriseTime,
        reader: &LatenessReader,
        now_ms: u64,
    ) -> Result<TaskWrite, EngineError> {
        check_completion(task, completed_at, reader.now, &reader.tz, &reader.schedule)
            .map_err(|e| EngineError::Invalid(e.to_string()))?;
        let patch = TaskPatch {
            state: Some(TaskState::Done),
            ..Default::default()
        };
        let mut w = self.plan_update(conn, task.id, patch, now_ms)?;
        w.task.completed_at = Some(completed_at.clone());
        // The streak reads when the work was done: an instant as it is, a
        // date from the start of its planner day.
        w.done_at_ms = starts_at(&reader.schedule, completed_at, &reader.tz)
            .and_then(|at| u64::try_from(at.as_millisecond()).ok());
        Ok(w)
    }
}

/// Whether `stream` is paused for the reader now: paused, with no expiry or
/// one still ahead (an all-day expiry ends at the start of its planner day,
/// `time.md` §1).
fn stream_paused(stream: &Stream, reader: &LatenessReader) -> bool {
    stream.paused
        && stream.paused_until.as_ref().is_none_or(|until| {
            starts_at(&reader.schedule, until, &reader.tz).is_none_or(|at| reader.now < at)
        })
}

/// The next free slot until the planner (#343) lands: the next planner day,
/// as an all-day value of its date, which starts at that day's start
/// (ADR-0047 §4, clarification C10).
fn next_free_slot(reader: &LatenessReader) -> Result<SunriseTime, EngineError> {
    reader
        .today
        .tomorrow()
        .map(SunriseTime::all_day)
        .map_err(|e| EngineError::Invalid(format!("next planner day: {e}")))
}
