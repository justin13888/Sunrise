//! Command/query engine pipeline.
//!
//! Translates a [`Command`] into:
//! 1. a new op-id (ULID; high 48 bits = clock, low 80 bits = RNG),
//! 2. a CBOR-encoded inner-op blob (the canonical op record),
//! 3. an op log row (envelope = inner-op for now; the encrypted
//!    `OpEnvelope` wrapping happens once `sunrise-sync` actually emits
//!    on the wire — locally we store the plaintext inner-op as the
//!    envelope blob since SQLCipher already encrypts at rest),
//! 4. a materialized state mutation (insert/update of `tasks`/`streams`
//!    rows), and
//! 5. a side-effect-free `CommandResult` returned to the caller.
//!
//! All five happen inside one `BEGIN IMMEDIATE` transaction.
//!
//! Read queries (Today, Inbox, StreamTasks, EntityById) materialise
//! results from the same tables; they do not re-read the op log.
//!
//! The engine is parameterized over a [`crate::config::Clock`] and
//! [`crate::config::Rng`], so tests inject fakes and the engine remains
//! deterministic.

// Stylistic clippy lints relaxed while the engine surface is in active
// expansion (Phase 17 retighten). The substantive ones (correctness,
// integer truncation, etc.) remain enforced.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::needless_pass_by_value,
    clippy::single_match_else,
    clippy::field_reassign_with_default,
    clippy::bool_assert_comparison,
    clippy::needless_collect,
    clippy::manual_let_else,
    clippy::option_map_unit_fn,
    clippy::option_if_let_else,
    clippy::unnecessary_wraps,
    clippy::missing_const_for_fn,
    clippy::map_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::unused_self,
    clippy::unnecessary_cast
)]

use crate::commands::{Command, CommandResult};
use crate::config::{Clock, Rng};
use crate::queries::{DeviceRow, Query, QueryResult, StreamRow};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use std::sync::Arc;
use sunrise_domain::{
    inbox_stream_ref, materialization_horizon_days, occurrence_task_id, NoteBody, Routine,
    RoutineCatchupPolicy, RoutineDraft, RoutinePatch, ScheduleConstraint, Stream, StreamColor,
    StreamDraft, StreamPatch, StreamReviewCadence, Task, TaskDraft, TaskPatch, TaskState,
    TaskTemplate,
};
use sunrise_id::{EntityKind, EntityRef, Ulid};
use sunrise_storage::{Db, OpLog};
use thiserror::Error;

/// Engine error. Maps to `CoreError::Engine` at the public API.
#[derive(Debug, Error)]
pub enum EngineError {
    /// Underlying SQLite/storage failure.
    #[error("storage: {0}")]
    Storage(#[from] sunrise_storage::DbError),
    /// SQL error during a transaction.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Domain validation rejected the input.
    #[error("validation: {0}")]
    Validation(#[from] sunrise_domain::ValidationError),
    /// Op-log error.
    #[error("op-log: {0}")]
    OpLog(#[from] sunrise_storage::OpLogError),
    /// CBOR encode/decode failure for an inner-op blob.
    #[error("cbor: {0}")]
    Cbor(String),
    /// Target entity not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// Invalid state transition or policy violation.
    #[error("invalid: {0}")]
    Invalid(String),
}

/// One command-application pipeline. Stateless; holds references to the
/// injected clock + rng so tests can produce deterministic op-ids.
#[derive(Clone)]
pub struct Engine {
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
    device_id: [u8; 16],
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("device_id", &hex_short(&self.device_id))
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Construct.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>, rng: Arc<dyn Rng>, device_id: [u8; 16]) -> Self {
        Self {
            clock,
            rng,
            device_id,
        }
    }

    /// Apply a command end-to-end inside a single transaction.
    pub fn apply(&self, db: &mut Db, cmd: Command) -> Result<CommandResult, EngineError> {
        match cmd {
            Command::CreateTask(d) => self.create_task(db, d),
            Command::UpdateTask { id, patch } => self.update_task(db, id, patch),
            Command::CompleteTask(id) => self.complete_task(db, id),
            Command::DeferTask { id, to_ms } => self.defer_task(db, id, to_ms),
            Command::DeleteTask(id) => self.delete_task(db, id),
            Command::PromoteToStream { id, stream } => self.promote_task(db, id, stream),
            Command::CreateStream(d) => self.create_stream(db, d),
            Command::UpdateStream { id, patch } => self.update_stream(db, id, patch),
            Command::DeleteStream(id) => self.delete_stream(db, id),
            Command::CreateRoutine(d) => self.create_routine(db, d),
            Command::UpdateRoutine { id, patch } => self.update_routine(db, id, patch),
            Command::DeleteRoutine(id) => self.delete_routine(db, id),
            Command::SkipRoutineOccurrence { id, occurrence_key } => {
                self.skip_routine_occurrence(db, id, occurrence_key)
            }
            Command::MaterializeRoutines { now_ms } => self.materialize_routines(db, now_ms),
        }
    }

    /// Run a read query against the materialized tables.
    pub fn query(&self, db: &Db, q: Query) -> Result<QueryResult, EngineError> {
        match q {
            Query::Today { now_ms, contexts } => self.query_today(db, now_ms, &contexts),
            Query::Inbox => self.query_stream_tasks(db, &inbox_stream_ref()),
            Query::StreamTasks(s) => self.query_stream_tasks(db, &s),
            Query::EntityById(r) => self.query_entity(db, r),
            Query::DeviceList => self.query_device_list(db),
            Query::StreamList => self.query_stream_list(db),
            Query::Routines => self.query_routines(db),
            Query::Search { text, limit } => self.query_search(db, &text, limit),
            Query::SyncStatus => Ok(QueryResult::SyncStatus(crate::events::SyncStatus {
                state: sunrise_sync::SyncState::Disconnected,
                outbox_pending: 0,
                peer_devices: 0,
                last_sync_ms: None,
            })),
        }
    }

    // ---- command handlers ----

    fn create_task(&self, db: &mut Db, d: TaskDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let task_id = self.fresh_id(EntityKind::Task, now_ms);
        let stream = d.stream_id.unwrap_or_else(inbox_stream_ref);
        let op_id = self.fresh_op_id(now_ms);
        let task = Task {
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
        };

        let inner_op = encode_inner_op(&InnerOp::TaskCreate(task.clone()))?;
        let device_id = self.device_id;
        let seq = next_seq(db, stream.bytes())?;

        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms, None, false, false, false)?;
            insert_task_row(tx, &task)?;
            insert_task_contexts(tx, &task)?;
            ftsr_upsert_task(tx, &task)?;
            ops_insert(
                tx,
                &op_id,
                stream.bytes(),
                &device_id,
                seq,
                now_ms,
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

        Ok(CommandResult {
            entity: task_id,
            state: Some(TaskState::Todo),
            op_id,
            seq,
        })
    }

    fn update_task(
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
                task.completed_at = Some(ms_to_ts(now_ms as i64));
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
        if let Some(arch) = patch.archived {
            task.archived = arch;
        }
        // Re-check cross-field invariants on the patched Task. A patch that
        // sets only `due_at` earlier than the existing `scheduled_at` (or an
        // invalid constraint list) would otherwise pass silently.
        task.validate_invariants()?;
        task.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        let device_id = self.device_id;
        let seq = next_seq(db, task.stream_id.bytes())?;

        let task_for_persist = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(
                tx,
                &task_for_persist.stream_id,
                now_ms,
                None,
                false,
                false,
                false,
            )?;
            update_task_row(tx, &task_for_persist)?;
            replace_task_contexts(tx, &task_for_persist)?;
            ftsr_upsert_task(tx, &task_for_persist)?;
            ops_insert(
                tx,
                &op_id,
                task_for_persist.stream_id.bytes(),
                &device_id,
                seq,
                now_ms,
                &inner_op,
                "task.update",
                "task",
                Some(task_for_persist.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult {
            entity: id,
            state: Some(task.state),
            op_id,
            seq,
        })
    }

    fn complete_task(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        let patch = TaskPatch {
            state: Some(TaskState::Done),
            ..Default::default()
        };
        self.update_task(db, id, patch)
    }

    fn defer_task(
        &self,
        db: &mut Db,
        id: EntityRef,
        to_ms: u64,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        task.scheduled_at = Some(ms_to_ts(to_ms as i64));
        task.deferred_count = task.deferred_count.saturating_add(1);
        task.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskUpdate(task.clone()))?;
        let device_id = self.device_id;
        let seq = next_seq(db, task.stream_id.bytes())?;
        let task_clone = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_task_row(tx, &task_clone)?;
            ops_insert(
                tx,
                &op_id,
                task_clone.stream_id.bytes(),
                &device_id,
                seq,
                now_ms,
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

        Ok(CommandResult {
            entity: id,
            state: Some(task.state),
            op_id,
            seq,
        })
    }

    fn delete_task(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Task)?;
        let now_ms = self.clock.now_ms();
        let mut task = read_task(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
        task.deleted = true;
        task.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::TaskDelete(task.id))?;
        let device_id = self.device_id;
        let seq = next_seq(db, task.stream_id.bytes())?;
        let task_clone = task.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_task_row(tx, &task_clone)?;
            ftsr_delete_task(tx, &task_clone.id)?;
            ops_insert(
                tx,
                &op_id,
                task_clone.stream_id.bytes(),
                &device_id,
                seq,
                now_ms,
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

        Ok(CommandResult {
            entity: id,
            state: Some(task.state),
            op_id,
            seq,
        })
    }

    fn promote_task(
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

    fn create_stream(&self, db: &mut Db, d: StreamDraft) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        let stream_id = self.fresh_id(EntityKind::Stream, now_ms);
        let stream = Stream {
            id: stream_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            name: d.name.trim().to_string(),
            description: d.description.clone(),
            color: d.color.unwrap_or(StreamColor::Slate),
            icon: None,
            parent_id: d.parent_id,
            sort_order: String::from("a0"),
            archived: false,
            paused: false,
            paused_until: None,
            review_cadence: d.review_cadence.unwrap_or(StreamReviewCadence::Weekly),
            default_context: None,
            deleted: false,
        };

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamCreate(stream.clone()))?;
        let device_id = self.device_id;
        let seq = next_seq(db, stream_id.bytes())?;
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_stream_row(tx, &stream_clone)?;
            ops_insert(
                tx,
                &op_id,
                stream_id.bytes(),
                &device_id,
                seq,
                now_ms,
                &inner_op,
                "stream.create",
                "stream",
                Some(stream_id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult {
            entity: stream_id,
            state: None,
            op_id,
            seq,
        })
    }

    fn update_stream(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: StreamPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Stream)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut stream = read_stream(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("stream {id}")))?;
        if let Some(n) = patch.name {
            stream.name = n.trim().to_string();
        }
        if let Some(desc) = patch.description {
            stream.description = desc;
        }
        if let Some(c) = patch.color {
            stream.color = c;
        }
        if let Some(p) = patch.parent_id {
            stream.parent_id = p;
        }
        if let Some(rc) = patch.review_cadence {
            stream.review_cadence = rc;
        }
        if let Some(a) = patch.archived {
            stream.archived = a;
        }
        if let Some(p) = patch.paused {
            stream.paused = p;
        }
        if let Some(pu) = patch.paused_until {
            stream.paused_until = pu;
        }
        stream.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamUpdate(stream.clone()))?;
        let device_id = self.device_id;
        let seq = next_seq(db, stream.id.bytes())?;
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_stream_row(tx, &stream_clone)?;
            ops_insert(
                tx,
                &op_id,
                stream_clone.id.bytes(),
                &device_id,
                seq,
                now_ms,
                &inner_op,
                "stream.update",
                "stream",
                Some(stream_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;

        Ok(CommandResult {
            entity: id,
            state: None,
            op_id,
            seq,
        })
    }

    fn delete_stream(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Stream)?;
        if id.bytes() == &[0u8; 16] {
            return Err(EngineError::Invalid(
                "cannot delete the inbox stream".into(),
            ));
        }
        let now_ms = self.clock.now_ms();
        let mut stream = read_stream(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("stream {id}")))?;
        stream.deleted = true;
        stream.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamDelete(stream.id))?;
        let device_id = self.device_id;
        let seq = next_seq(db, stream.id.bytes())?;
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_stream_row(tx, &stream_clone)?;
            ops_insert(
                tx,
                &op_id,
                stream_clone.id.bytes(),
                &device_id,
                seq,
                now_ms,
                &inner_op,
                "stream.delete",
                "stream",
                Some(stream_clone.id.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
            )?;
            Ok(())
        })?;
        Ok(CommandResult {
            entity: id,
            state: None,
            op_id,
            seq,
        })
    }

    // ---- routine command handlers ----

    fn create_routine(&self, db: &mut Db, d: RoutineDraft) -> Result<CommandResult, EngineError> {
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
            paused: false,
            paused_until: None,
            archived: false,
            deleted: false,
        };
        let stream = routine.template.stream_id;
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineCreate(Box::new(routine.clone())))?;
        let device_id = self.device_id;
        let seq = next_seq(db, stream.bytes())?;
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms, None, false, false, false)?;
            insert_routine_row(tx, &routine_clone, now_ms)?;
            ops_insert(
                tx,
                &op_id,
                stream.bytes(),
                &device_id,
                seq,
                now_ms,
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

        Ok(CommandResult {
            entity: routine_id,
            state: None,
            op_id,
            seq,
        })
    }

    fn update_routine(
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
        let device_id = self.device_id;
        let stream = routine.template.stream_id;
        let seq = next_seq(db, stream.bytes())?;
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(tx, &stream, now_ms, None, false, false, false)?;
            update_routine_row(tx, &routine_clone)?;
            ops_insert(
                tx,
                &op_id,
                stream.bytes(),
                &device_id,
                seq,
                now_ms,
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

        Ok(CommandResult {
            entity: id,
            state: None,
            op_id,
            seq,
        })
    }

    fn delete_routine(&self, db: &mut Db, id: EntityRef) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        let now_ms = self.clock.now_ms();
        let mut routine = read_routine(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("routine {id}")))?;
        routine.deleted = true;
        routine.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::RoutineDelete(id))?;
        let device_id = self.device_id;
        let stream = routine.template.stream_id;
        let seq = next_seq(db, stream.bytes())?;
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_routine_row(tx, &routine_clone)?;
            ops_insert(
                tx,
                &op_id,
                stream.bytes(),
                &device_id,
                seq,
                now_ms,
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
        Ok(CommandResult {
            entity: id,
            state: None,
            op_id,
            seq,
        })
    }

    fn skip_routine_occurrence(
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
        let device_id = self.device_id;
        let stream = routine.template.stream_id;
        let seq = next_seq(db, stream.bytes())?;
        let routine_clone = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_routine_row(tx, &routine_clone)?;
            ops_insert(
                tx,
                &op_id,
                stream.bytes(),
                &device_id,
                seq,
                now_ms,
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
                let task_seq = next_seq_tx(tx, t.stream_id.bytes())?;
                let del_op = encode_inner_op(&InnerOp::TaskDelete(t.id))
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                update_task_row(tx, &t)?;
                ftsr_delete_task(tx, &t.id)?;
                let del_op_id = self.fresh_op_id(now_ms);
                ops_insert(
                    tx,
                    &del_op_id,
                    t.stream_id.bytes(),
                    &device_id,
                    task_seq,
                    now_ms,
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

        Ok(CommandResult {
            entity: id,
            state: None,
            op_id,
            seq,
        })
    }

    fn materialize_routines(&self, db: &mut Db, now_ms: u64) -> Result<CommandResult, EngineError> {
        let routines = read_routines(db.conn())?;
        for routine in routines {
            self.materialize_one_routine(db, &routine, now_ms)?;
        }
        // No single target entity; return a routine-kind sentinel.
        Ok(CommandResult {
            entity: EntityRef::new(EntityKind::Routine, [0u8; 16]),
            state: None,
            op_id: [0u8; 16],
            seq: 0,
        })
    }

    /// Expand a routine over `[max(starts_at, materialized_until), now+horizon)`
    /// and INSERT-OR-IGNORE the resulting tasks, applying the catchup policy to
    /// occurrences at or before `now`. Runs in one transaction per routine:
    /// the inserts and the `materialized_until` advance commit together, so a
    /// crash never leaves the watermark ahead of the tasks it represents.
    fn materialize_one_routine(
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

        let device_id = self.device_id;
        let routine = routine.clone();
        let clock = self.clock.clone();
        let rng = self.rng.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            ensure_stream_row(
                tx,
                &routine.template.stream_id,
                now_ms,
                None,
                false,
                false,
                false,
            )?;
            for (key, at, title_override) in &jobs {
                let task = build_routine_task(&routine, key, *at, title_override.clone(), now_ms);
                let inserted = insert_task_row_or_ignore(tx, &task)?;
                if !inserted {
                    continue;
                }
                insert_task_contexts(tx, &task)?;
                ftsr_upsert_task(tx, &task)?;
                let inner_op = encode_inner_op(&InnerOp::TaskCreate(task.clone()))
                    .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
                let seq = next_seq_tx(tx, task.stream_id.bytes())?;
                let mut rand = [0u8; 10];
                rng.fill_bytes(&mut rand);
                let op_id = *Ulid::from_timestamp_and_random(clock.now_ms(), rand).as_bytes();
                ops_insert(
                    tx,
                    &op_id,
                    task.stream_id.bytes(),
                    &device_id,
                    seq,
                    now_ms,
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

        let device_id = self.device_id;
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
            let inner_op = encode_inner_op(&InnerOp::TaskDelete(t.id))?;
            let seq = next_seq(db, t.stream_id.bytes())?;
            let t_clone = t.clone();
            db.with_tx(|tx| -> rusqlite::Result<()> {
                update_task_row(tx, &t_clone)?;
                ftsr_delete_task(tx, &t_clone.id)?;
                ops_insert(
                    tx,
                    &op_id,
                    t_clone.stream_id.bytes(),
                    &device_id,
                    seq,
                    now_ms,
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

    // ---- query handlers ----

    fn query_routines(&self, db: &Db) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Routines(read_routines(db.conn())?))
    }

    fn query_today(
        &self,
        db: &Db,
        now_ms: u64,
        _contexts: &[EntityRef],
    ) -> Result<QueryResult, EngineError> {
        // Today = (a) tasks scheduled within today's local-day window OR
        //         (b) tasks due_at <= now + 24h, AND not done/deleted.
        // For v1 we use a simple rolling 24h window forward from `now_ms`.
        let window_end = now_ms.saturating_add(24 * 60 * 60 * 1000);
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE deleted = 0 AND archived = 0
               AND state != 'done' AND state != 'cancelled'
               AND ((scheduled_at_ms IS NOT NULL AND scheduled_at_ms <= ?)
                 OR (due_at_ms IS NOT NULL AND due_at_ms <= ?))
             ORDER BY COALESCE(scheduled_at_ms, due_at_ms) ASC",
        )?;
        let ids = stmt
            .query_map(params![window_end, window_end], |row| {
                let id: Vec<u8> = row.get(0)?;
                Ok(id)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if let Some(t) = read_task(db.conn(), &bytes)? {
                tasks.push(t);
            }
        }
        Ok(QueryResult::Tasks(tasks))
    }

    fn query_stream_tasks(&self, db: &Db, stream: &EntityRef) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE stream_id = ? AND deleted = 0
             ORDER BY COALESCE(scheduled_at_ms, due_at_ms) ASC, id ASC",
        )?;
        let stream_blob: Vec<u8> = stream.bytes().to_vec();
        let ids = stmt
            .query_map(params![stream_blob], |row| {
                let id: Vec<u8> = row.get(0)?;
                Ok(id)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if let Some(t) = read_task(db.conn(), &bytes)? {
                tasks.push(t);
            }
        }
        Ok(QueryResult::StreamTasks(tasks))
    }

    fn query_entity(&self, db: &Db, r: EntityRef) -> Result<QueryResult, EngineError> {
        match r.kind() {
            EntityKind::Task => {
                let t = read_task(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("task {r}")))?;
                Ok(QueryResult::Task(Box::new(t)))
            }
            EntityKind::Stream => {
                let s = read_stream(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("stream {r}")))?;
                Ok(QueryResult::Stream(Box::new(s)))
            }
            EntityKind::Routine => {
                let rt = read_routine(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("routine {r}")))?;
                Ok(QueryResult::Routine(Box::new(rt)))
            }
            _ => Err(EngineError::Invalid(format!(
                "EntityById not supported for kind {:?} in v1",
                r.kind()
            ))),
        }
    }

    fn query_device_list(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT device_id, nickname, platform, revoked_at_ms IS NOT NULL FROM devices",
        )?;
        let rows = stmt.query_map([], |row| {
            let blob: Vec<u8> = row.get(0)?;
            let mut a = [0u8; 16];
            let take = blob.len().min(16);
            a[..take].copy_from_slice(&blob[..take]);
            Ok(DeviceRow {
                device_id: a,
                nickname: row.get(1)?,
                platform: row.get(2)?,
                revoked: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(QueryResult::Devices(out))
    }

    fn query_stream_list(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let inbox = inbox_stream_ref();
        let inbox_blob: Vec<u8> = inbox.bytes().to_vec();

        // Synthetic Inbox row: count of open tasks parked in the inbox stream.
        let inbox_open: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM tasks
             WHERE stream_id = ? AND deleted = 0
               AND state IN ('todo', 'in_progress')",
            params![inbox_blob.clone()],
            |r| r.get(0),
        )?;

        let mut rows = Vec::new();
        rows.push(StreamRow {
            id: inbox,
            name: "Inbox".to_string(),
            color: StreamColor::Slate,
            open_task_count: u64::try_from(inbox_open).unwrap_or(0),
            archived: false,
        });

        // Real streams (exclude deleted and the synthetic inbox row, which
        // `ensure_stream_row` may have materialized with an empty name).
        // Ordered case-insensitively by name.
        let mut stmt = db.conn().prepare(
            "SELECT s.stream_id, s.name, s.color, s.archived,
                    (SELECT COUNT(*) FROM tasks t
                     WHERE t.stream_id = s.stream_id AND t.deleted = 0
                       AND t.state IN ('todo', 'in_progress')) AS open_count
             FROM streams s
             WHERE s.deleted = 0 AND s.stream_id != ?
             ORDER BY s.name COLLATE NOCASE ASC, s.stream_id ASC",
        )?;
        let mapped = stmt.query_map(params![inbox_blob], |row| {
            let id_blob: Vec<u8> = row.get(0)?;
            let name: String = row.get(1)?;
            let color_str: String = row.get(2)?;
            let archived: i64 = row.get(3)?;
            let open_count: i64 = row.get(4)?;
            let mut a = [0u8; 16];
            let take = id_blob.len().min(16);
            a[..take].copy_from_slice(&id_blob[..take]);
            Ok(StreamRow {
                id: EntityRef::new(EntityKind::Stream, a),
                name,
                color: StreamColor::from_str_lossy(&color_str),
                open_task_count: u64::try_from(open_count).unwrap_or(0),
                archived: archived != 0,
            })
        })?;
        for r in mapped {
            rows.push(r?);
        }
        Ok(QueryResult::Streams(rows))
    }

    fn query_search(&self, db: &Db, text: &str, limit: u32) -> Result<QueryResult, EngineError> {
        let match_expr = sanitize_fts_query(text);
        // Empty/whitespace-only input yields no results (never touch FTS).
        if match_expr.is_empty() {
            return Ok(QueryResult::Tasks(Vec::new()));
        }
        let mut stmt = db.conn().prepare(
            "SELECT id FROM search_idx
             WHERE search_idx MATCH ? AND kind = 'task'
             ORDER BY bm25(search_idx)
             LIMIT ?",
        )?;
        let ids = stmt
            .query_map(params![match_expr, i64::from(limit)], |row| {
                row.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            let mut bytes = [0u8; 16];
            let take = raw.len().min(16);
            bytes[..take].copy_from_slice(&raw[..take]);
            if let Some(t) = read_task(db.conn(), &bytes)? {
                if !t.deleted {
                    tasks.push(t);
                }
            }
        }
        Ok(QueryResult::Tasks(tasks))
    }

    // ---- id helpers ----

    fn fresh_id(&self, kind: EntityKind, now_ms: u64) -> EntityRef {
        let mut rand = [0u8; 10];
        self.rng.fill_bytes(&mut rand);
        let ulid = Ulid::from_timestamp_and_random(now_ms, rand);
        EntityRef::from_ulid(kind, ulid)
    }

    fn fresh_op_id(&self, now_ms: u64) -> [u8; 16] {
        let mut rand = [0u8; 10];
        self.rng.fill_bytes(&mut rand);
        *Ulid::from_timestamp_and_random(now_ms, rand).as_bytes()
    }
}

// ---- inner-op CBOR ----

#[derive(Debug, serde::Serialize)]
enum InnerOp {
    TaskCreate(Task),
    TaskUpdate(Task),
    TaskDelete(EntityRef),
    StreamCreate(Stream),
    StreamUpdate(Stream),
    StreamDelete(EntityRef),
    RoutineCreate(Box<Routine>),
    RoutineUpdate(Box<Routine>),
    RoutineDelete(EntityRef),
}

fn encode_inner_op(op: &InnerOp) -> Result<Vec<u8>, EngineError> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(op, &mut buf).map_err(|e| EngineError::Cbor(e.to_string()))?;
    Ok(buf)
}

// ---- table operations ----

#[allow(clippy::too_many_arguments)]
fn ops_insert(
    tx: &Transaction<'_>,
    op_id: &[u8; 16],
    stream_id: &[u8; 16],
    device_id: &[u8; 16],
    seq: u64,
    ts_ms: u64,
    envelope: &[u8],
    inner_kind: &str,
    target_kind: &str,
    target_id: Option<&[u8; 16]>,
    applied_at_ms: Option<u64>,
    received_from: Option<&[u8; 16]>,
    received_at_ms: u64,
    deps: &[[u8; 16]],
) -> rusqlite::Result<()> {
    if let Err(e) = OpLog::insert(
        tx,
        op_id,
        stream_id,
        device_id,
        seq,
        ts_ms,
        envelope,
        inner_kind,
        target_kind,
        target_id,
        applied_at_ms,
        received_from,
        received_at_ms,
        deps,
    ) {
        return match e {
            sunrise_storage::OpLogError::Sqlite(s) => Err(s),
            sunrise_storage::OpLogError::Db(_) => Err(rusqlite::Error::ExecuteReturnedResults),
        };
    }
    Ok(())
}

fn next_seq(db: &Db, stream_id: &[u8; 16]) -> Result<u64, EngineError> {
    let stream_blob: Vec<u8> = stream_id.to_vec();
    let max: Option<i64> = db
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM ops WHERE stream_id = ?",
            params![stream_blob],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let next = max.map_or(1, |v| v.saturating_add(1));
    Ok(u64::try_from(next).unwrap_or(1))
}

fn ensure_stream_row(
    tx: &Transaction<'_>,
    stream: &EntityRef,
    now_ms: u64,
    parent: Option<&EntityRef>,
    archived: bool,
    deleted: bool,
    overwrite: bool,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = stream.bytes().to_vec();
    let exists: i64 = tx.query_row(
        "SELECT count(*) FROM streams WHERE stream_id = ?",
        params![&id_blob],
        |r| r.get(0),
    )?;
    if exists > 0 && !overwrite {
        return Ok(());
    }
    if exists > 0 {
        return Ok(());
    }
    let parent_blob: Option<Vec<u8>> = parent.map(|p| p.bytes().to_vec());
    tx.execute(
        "INSERT INTO streams
         (stream_id, doc_blob, doc_blob_v, head_root, last_op_seq,
          parent_id, archived, deleted, created_at_ms, updated_at_ms)
         VALUES (?, ?, 1, ?, 0, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            Vec::<u8>::new(),
            vec![0u8; 32],
            parent_blob,
            archived as i64,
            deleted as i64,
            now_ms,
            now_ms,
        ],
    )?;
    Ok(())
}

fn insert_stream_row(tx: &Transaction<'_>, s: &Stream) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = s.id.bytes().to_vec();
    let parent_blob: Option<Vec<u8>> = s.parent_id.map(|p| p.bytes().to_vec());
    tx.execute(
        "INSERT INTO streams
         (stream_id, doc_blob, doc_blob_v, head_root, last_op_seq,
          parent_id, archived, deleted, created_at_ms, updated_at_ms, name, color)
         VALUES (?, ?, 1, ?, 0, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            Vec::<u8>::new(),
            vec![0u8; 32],
            parent_blob,
            s.archived as i64,
            s.deleted as i64,
            s.created_at.as_millisecond(),
            s.updated_at.as_millisecond(),
            s.name,
            s.color.as_str(),
        ],
    )?;
    Ok(())
}

fn update_stream_row(tx: &Transaction<'_>, s: &Stream) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = s.id.bytes().to_vec();
    let parent_blob: Option<Vec<u8>> = s.parent_id.map(|p| p.bytes().to_vec());
    tx.execute(
        "UPDATE streams
         SET parent_id = ?, archived = ?, deleted = ?, updated_at_ms = ?,
             name = ?, color = ?
         WHERE stream_id = ?",
        params![
            parent_blob,
            s.archived as i64,
            s.deleted as i64,
            s.updated_at.as_millisecond(),
            s.name,
            s.color.as_str(),
            id_blob,
        ],
    )?;
    Ok(())
}

fn read_stream(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<Option<Stream>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let row = conn
        .query_row(
            "SELECT parent_id, archived, deleted, created_at_ms, updated_at_ms, name, color
             FROM streams WHERE stream_id = ?",
            params![id_blob],
            |r| {
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((parent_raw, archived, deleted, created_ms, updated_ms, name, color_str)) = row else {
        return Ok(None);
    };
    let parent = parent_raw.map(|b| {
        let mut a = [0u8; 16];
        let take = b.len().min(16);
        a[..take].copy_from_slice(&b[..take]);
        EntityRef::new(EntityKind::Stream, a)
    });
    let stream = Stream {
        id: EntityRef::new(EntityKind::Stream, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        name,
        description: None,
        // Unknown/forward-compatible color strings fall back to Slate.
        color: StreamColor::from_str_lossy(&color_str),
        icon: None,
        parent_id: parent,
        sort_order: String::from("a0"),
        archived: archived != 0,
        paused: false,
        paused_until: None,
        review_cadence: StreamReviewCadence::Weekly,
        default_context: None,
        deleted: deleted != 0,
    };
    Ok(Some(stream))
}

fn insert_task_row(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "INSERT INTO tasks
         (id, stream_id, title, state, priority, energy, estimated_min,
          scheduled_at_ms, due_at_ms, completed_at_ms, deferred_count,
          routine_id, routine_occurrence, archived, deleted, body,
          scheduling_constraints, extra, head_root)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL)",
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
            t.scheduled_at.map(|d| d.as_millisecond()),
            t.due_at.map(|d| d.as_millisecond()),
            t.completed_at.map(|d| d.as_millisecond()),
            t.deferred_count,
            t.routine_id.as_ref().map(|r| r.bytes().to_vec()),
            t.routine_occurrence.map(|d| d.as_millisecond()),
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
        ],
    )?;
    Ok(())
}

fn update_task_row(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "UPDATE tasks SET
            stream_id = ?, title = ?, state = ?, priority = ?,
            energy = ?, estimated_min = ?, scheduled_at_ms = ?, due_at_ms = ?,
            completed_at_ms = ?, deferred_count = ?, archived = ?, deleted = ?,
            body = ?, scheduling_constraints = ?
         WHERE id = ?",
        params![
            stream_blob,
            t.title,
            task_state_str(t.state),
            t.priority.map(i64::from),
            t.energy.map(energy_str),
            t.estimated_duration_s
                .and_then(|s| i64::try_from(s / 60).ok()),
            t.scheduled_at.map(|d| d.as_millisecond()),
            t.due_at.map(|d| d.as_millisecond()),
            t.completed_at.map(|d| d.as_millisecond()),
            t.deferred_count,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            id_blob,
        ],
    )?;
    Ok(())
}

/// Encode a scheduling-constraint list to a canonical-CBOR blob for the
/// `scheduling_constraints` column. An empty list stores `NULL`.
fn encode_constraints(list: &[ScheduleConstraint]) -> rusqlite::Result<Option<Vec<u8>>> {
    if list.is_empty() {
        return Ok(None);
    }
    let v = list.to_vec();
    let bytes = sunrise_cbor::encode_canonical(&v)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(Some(bytes))
}

/// Decode the `scheduling_constraints` column blob (NULL == empty list).
fn decode_constraints(blob: Option<Vec<u8>>) -> Result<Vec<ScheduleConstraint>, EngineError> {
    match blob {
        None => Ok(Vec::new()),
        Some(bytes) => {
            sunrise_cbor::decode_canonical(&bytes).map_err(|e| EngineError::Cbor(e.to_string()))
        }
    }
}

fn insert_task_contexts(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    for c in &t.contexts {
        tx.execute(
            "INSERT OR IGNORE INTO task_contexts (task_id, context_id) VALUES (?, ?)",
            params![t.id.bytes().to_vec(), c.bytes().to_vec()],
        )?;
    }
    Ok(())
}

fn replace_task_contexts(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM task_contexts WHERE task_id = ?",
        params![t.id.bytes().to_vec()],
    )?;
    insert_task_contexts(tx, t)
}

fn ftsr_upsert_task(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<()> {
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

fn ftsr_delete_task(tx: &Transaction<'_>, id: &EntityRef) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = id.bytes().to_vec();
    tx.execute(
        "DELETE FROM search_idx WHERE kind = 'task' AND id = ?",
        params![id_blob],
    )?;
    Ok(())
}

fn read_task(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<Option<Task>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let row = conn
        .query_row(
            "SELECT stream_id, title, state, priority, energy, estimated_min,
                    scheduled_at_ms, due_at_ms, completed_at_ms, deferred_count,
                    archived, deleted, body, scheduling_constraints,
                    routine_id, routine_occurrence
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
        id: EntityRef::new(EntityKind::Task, *id),
        created_at: ms_to_ts(0),
        updated_at: ms_to_ts(0),
        title: t.1,
        body: t.12.map(NoteBody),
        stream_id: EntityRef::new(EntityKind::Stream, stream_bytes),
        contexts,
        state: parse_task_state(&t.2)?,
        priority: t.3.and_then(|v| u8::try_from(v).ok()),
        energy: t.4.as_deref().and_then(parse_energy),
        estimated_duration_s: t.5.and_then(|m| {
            let secs = m.checked_mul(60)?;
            u64::try_from(secs).ok()
        }),
        scheduled_at: t.6.map(|m| ms_to_ts(m.max(0))),
        due_at: t.7.map(|m| ms_to_ts(m.max(0))),
        scheduling_constraints: decode_constraints(t.13)?,
        completed_at: t.8.map(|m| ms_to_ts(m.max(0))),
        deferred_count: t.9,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
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
    };
    Ok(Some(task))
}

// ---- routine table operations ----

/// INSERT OR IGNORE a task row (materialization dedup by deterministic id).
/// Returns `true` when a new row was actually inserted.
fn insert_task_row_or_ignore(tx: &Transaction<'_>, t: &Task) -> rusqlite::Result<bool> {
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    let changed = tx.execute(
        "INSERT OR IGNORE INTO tasks
         (id, stream_id, title, state, priority, energy, estimated_min,
          scheduled_at_ms, due_at_ms, completed_at_ms, deferred_count,
          routine_id, routine_occurrence, archived, deleted, body,
          scheduling_constraints, extra, head_root)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL)",
        params![
            id_blob,
            stream_blob,
            t.title,
            task_state_str(t.state),
            t.priority.map(i64::from),
            t.energy.map(energy_str),
            t.estimated_duration_s
                .and_then(|s| i64::try_from(s / 60).ok()),
            t.scheduled_at.map(|d| d.as_millisecond()),
            t.due_at.map(|d| d.as_millisecond()),
            t.completed_at.map(|d| d.as_millisecond()),
            t.deferred_count,
            t.routine_id.as_ref().map(|r| r.bytes().to_vec()),
            t.routine_occurrence.map(|d| d.as_millisecond()),
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
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
        scheduled_at: Some(at),
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
    }
}

fn next_seq_tx(tx: &Transaction<'_>, stream_id: &[u8; 16]) -> rusqlite::Result<u64> {
    let stream_blob: Vec<u8> = stream_id.to_vec();
    let max: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM ops WHERE stream_id = ?",
        params![stream_blob],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(max.saturating_add(1)).unwrap_or(1))
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

fn insert_routine_row(tx: &Transaction<'_>, r: &Routine, now_ms: u64) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    tx.execute(
        "INSERT INTO routines
         (id, stream_id, rrule, rrule_text, timezone, starts_at_ms, ends_at_ms,
          streak_counter, paused, archived, deleted, scheduling_constraints,
          template, skip_dates, skipped_keys, catchup_policy,
          last_completed_at_ms, paused_until_ms, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            rrule_text,
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
        ],
    )?;
    Ok(())
}

fn update_routine_row(tx: &Transaction<'_>, r: &Routine) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    tx.execute(
        "UPDATE routines SET
            stream_id = ?, rrule = ?, rrule_text = ?, timezone = ?,
            starts_at_ms = ?, ends_at_ms = ?, streak_counter = ?, paused = ?,
            archived = ?, deleted = ?, scheduling_constraints = ?, template = ?,
            skip_dates = ?, skipped_keys = ?, catchup_policy = ?,
            last_completed_at_ms = ?, paused_until_ms = ?, updated_at_ms = ?
         WHERE id = ?",
        params![
            stream_blob,
            rrule_text,
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
) -> Result<Routine, EngineError> {
    let rrule = sunrise_domain::RRule::parse(rrule_text)
        .map_err(|e| EngineError::Invalid(format!("stored rrule: {e}")))?;
    let template: TaskTemplate = match template {
        Some(b) => {
            sunrise_cbor::decode_canonical(&b).map_err(|e| EngineError::Cbor(e.to_string()))?
        }
        None => return Err(EngineError::Invalid("routine missing template".into())),
    };
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
        paused: paused != 0,
        paused_until: paused_until_ms.map(|m| ms_to_ts(m.max(0))),
        archived: archived != 0,
        deleted: deleted != 0,
    })
}

const ROUTINE_COLUMNS: &str = "rrule_text, timezone, starts_at_ms, ends_at_ms,
     streak_counter, paused, archived, deleted, scheduling_constraints,
     template, skip_dates, skipped_keys, catchup_policy, last_completed_at_ms,
     paused_until_ms, created_at_ms, updated_at_ms";

fn read_routine(
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
            ))
        })
        .optional()?;
    let Some(v) = raw else {
        return Ok(None);
    };
    let routine = routine_from_row(
        id, &v.0, v.1, v.2, v.3, v.4, v.5, v.6, v.7, v.8, v.9, v.10, v.11, &v.12, v.13, v.14, v.15,
        v.16,
    )?;
    Ok(Some(routine))
}

fn read_routines(conn: &rusqlite::Connection) -> Result<Vec<Routine>, EngineError> {
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
            v.16, v.17,
        )?);
    }
    Ok(out)
}

// ---- helpers ----

fn require_kind(r: EntityRef, k: EntityKind) -> Result<(), EngineError> {
    if r.kind() != k {
        return Err(EngineError::Invalid(format!(
            "expected {k:?}, got {:?}",
            r.kind()
        )));
    }
    Ok(())
}

fn task_state_str(s: TaskState) -> &'static str {
    match s {
        TaskState::Todo => "todo",
        TaskState::InProgress => "in_progress",
        TaskState::Done => "done",
        TaskState::Cancelled => "cancelled",
    }
}

fn parse_task_state(s: &str) -> Result<TaskState, EngineError> {
    match s {
        "todo" => Ok(TaskState::Todo),
        "in_progress" => Ok(TaskState::InProgress),
        "done" => Ok(TaskState::Done),
        "cancelled" => Ok(TaskState::Cancelled),
        other => Err(EngineError::Invalid(format!("unknown task state: {other}"))),
    }
}

fn energy_str(e: sunrise_domain::Energy) -> &'static str {
    match e {
        sunrise_domain::Energy::Low => "low",
        sunrise_domain::Energy::Med => "med",
        sunrise_domain::Energy::High => "high",
    }
}

fn parse_energy(s: &str) -> Option<sunrise_domain::Energy> {
    match s {
        "low" => Some(sunrise_domain::Energy::Low),
        "med" => Some(sunrise_domain::Energy::Med),
        "high" => Some(sunrise_domain::Energy::High),
        _ => None,
    }
}

/// Turn arbitrary user input into a safe FTS5 MATCH expression.
///
/// FTS5's query grammar treats characters like `"`, `*`, `:`, `-`, `(`, `)`,
/// and bare-word operators (`AND`/`OR`/`NOT`/`NEAR`) as syntax; hostile or
/// accidental input can otherwise produce a MATCH syntax error. We defuse it
/// completely: split on whitespace, strip embedded double quotes and control
/// characters (a NUL would truncate the C string SQLite receives) from each
/// token, wrap each token in double quotes (making it a literal phrase — no
/// operator or special character survives), and join with spaces (implicit
/// AND). An empty result means "no query".
fn sanitize_fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| {
            let cleaned: String = tok
                .chars()
                .filter(|&c| c != '"' && !c.is_control())
                .collect();
            cleaned
        })
        .filter(|tok| !tok.is_empty())
        .map(|tok| format!("\"{tok}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ms_to_ts(ms: i64) -> jiff::Timestamp {
    // Determinism: never read a wall clock on failure. Out-of-range epoch-ms
    // (corrupt row) clamps to the Unix epoch rather than `Timestamp::now()`.
    jiff::Timestamp::from_millisecond(ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

fn hex_short(b: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for byte in b.iter().take(4) {
        use core::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SystemRng;
    use parking_lot::Mutex as PLMutex;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_storage::Db;

    #[derive(Debug)]
    struct FakeClock(PLMutex<u64>);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock()
        }
    }

    fn engine() -> Engine {
        Engine::new(
            Arc::new(FakeClock(PLMutex::new(1_700_000_000_000))),
            Arc::new(SystemRng),
            [9u8; 16],
        )
    }

    fn db() -> Db {
        Db::open_memory(&VaultRootKey::from_bytes([0xab; 32])).unwrap()
    }

    #[test]
    fn create_task_round_trips() {
        let mut db = db();
        let e = engine();
        let cmd = Command::CreateTask(TaskDraft {
            title: "draft a memo".into(),
            ..Default::default()
        });
        let res = e.apply(&mut db, cmd).unwrap();
        assert_eq!(res.entity.kind(), EntityKind::Task);
        assert_eq!(res.state, Some(TaskState::Todo));
        let t = match e.query(&db, Query::EntityById(res.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!("wrong query result variant"),
        };
        assert_eq!(t.title, "draft a memo");
        assert_eq!(t.stream_id, inbox_stream_ref());
    }

    #[test]
    fn complete_task_sets_done() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        e.apply(&mut db, Command::CompleteTask(r.entity)).unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.state, TaskState::Done);
        assert!(t.completed_at.is_some());
    }

    #[test]
    fn defer_increments_count() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        e.apply(
            &mut db,
            Command::DeferTask {
                id: r.entity,
                to_ms: 1_700_000_000_000 + 86_400_000,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.deferred_count, 1);
        assert!(t.scheduled_at.is_some());
    }

    #[test]
    fn delete_task_marks_deleted_and_hides_from_inbox() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        e.apply(&mut db, Command::DeleteTask(r.entity)).unwrap();
        let res = e.query(&db, Query::Inbox).unwrap();
        let tasks = match res {
            QueryResult::StreamTasks(v) => v,
            _ => panic!(),
        };
        assert!(tasks.is_empty());
    }

    #[test]
    fn create_stream_round_trips() {
        let mut db = db();
        let e = engine();
        let s = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    color: Some(StreamColor::Sky),
                    ..Default::default()
                }),
            )
            .unwrap();
        let st = match e.query(&db, Query::EntityById(s.entity)).unwrap() {
            QueryResult::Stream(s) => *s,
            _ => panic!(),
        };
        assert_eq!(st.archived, false);
        // Name and color must round-trip through storage (regression: these
        // used to come back as "" / Slate).
        assert_eq!(st.name, "Work");
        assert_eq!(st.color, StreamColor::Sky);
    }

    #[test]
    fn update_stream_persists_name_and_color() {
        let mut db = db();
        let e = engine();
        let s = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Work".into(),
                    color: Some(StreamColor::Sky),
                    ..Default::default()
                }),
            )
            .unwrap();
        let patch = StreamPatch {
            name: Some("Personal".into()),
            color: Some(StreamColor::Emerald),
            ..Default::default()
        };
        e.apply(
            &mut db,
            Command::UpdateStream {
                id: s.entity,
                patch,
            },
        )
        .unwrap();
        let st = match e.query(&db, Query::EntityById(s.entity)).unwrap() {
            QueryResult::Stream(s) => *s,
            _ => panic!(),
        };
        assert_eq!(st.name, "Personal");
        assert_eq!(st.color, StreamColor::Emerald);
    }

    #[test]
    fn cannot_delete_inbox_stream() {
        let mut db = db();
        let e = engine();
        let res = e.apply(&mut db, Command::DeleteStream(inbox_stream_ref()));
        assert!(matches!(res, Err(EngineError::Invalid(_))));
    }

    #[test]
    fn update_task_changes_state_and_records_op() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let mut p = TaskPatch::default();
        p.state = Some(TaskState::InProgress);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch: p,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.state, TaskState::InProgress);
        let count: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
            .unwrap();
        assert!(count >= 2);
    }

    #[test]
    fn today_query_returns_tasks_due_in_window() {
        let mut db = db();
        let e = engine();
        let now = 1_700_000_000_000u64;
        let due_soon = ms_to_ts((now + 3_600_000) as i64);
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "soon".into(),
                due_at: Some(due_soon),
                ..Default::default()
            }),
        )
        .unwrap();
        let far = ms_to_ts((now + 7 * 86_400_000) as i64);
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "far".into(),
                due_at: Some(far),
                ..Default::default()
            }),
        )
        .unwrap();
        let r = e
            .query(
                &db,
                Query::Today {
                    now_ms: now,
                    contexts: vec![],
                },
            )
            .unwrap();
        let tasks = match r {
            QueryResult::Tasks(v) => v,
            _ => panic!(),
        };
        let titles: Vec<_> = tasks.iter().map(|t| t.title.clone()).collect();
        assert!(titles.contains(&"soon".to_string()));
        assert!(!titles.contains(&"far".to_string()));
    }

    #[test]
    fn invalid_state_transition_rejected() {
        let mut db = db();
        let e = engine();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        // todo can transition to in_progress, done, cancelled all directly,
        // so we pick a forbidden non-self transition.
        // todo → todo is allowed. Force a sequence: todo → done → in_progress
        // is allowed; then in_progress → done allowed. Try patching done →
        // explicitly back to todo (allowed). What's NOT allowed? Per the
        // current `can_transition_to`, every non-equal pair across the four
        // states is allowed. The matrix is essentially fully connected. So
        // invalidity is exposed by patching wrong_kind; test that path.
        let bogus = EntityRef::new(EntityKind::Stream, *r.entity.bytes());
        let res = e.apply(
            &mut db,
            Command::UpdateTask {
                id: bogus,
                patch: TaskPatch::default(),
            },
        );
        assert!(matches!(res, Err(EngineError::Invalid(_))));
    }

    #[test]
    fn stream_list_counts_open_tasks_inbox_first_and_ordering() {
        let mut db = db();
        let e = engine();

        // Two real streams. Deliberately create "beta" before "alpha" so we
        // exercise case-insensitive name ordering rather than insertion order.
        let beta = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Beta".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let alpha = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "alpha".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        let gone = e
            .apply(
                &mut db,
                Command::CreateStream(StreamDraft {
                    name: "Deleted".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::DeleteStream(gone)).unwrap();

        // Inbox: 1 open + 1 done (done not counted).
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "inbox open".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        let inbox_done = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "inbox done".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::CompleteTask(inbox_done)).unwrap();

        // alpha: 2 open (one todo, one in-progress), 1 cancelled, 1 deleted.
        for _ in 0..2 {
            e.apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "a open".into(),
                    stream_id: Some(alpha),
                    ..Default::default()
                }),
            )
            .unwrap();
        }
        let a_ip = db_last_task(&db, alpha);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: a_ip,
                patch: TaskPatch {
                    state: Some(TaskState::InProgress),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let a_cancel = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "a cancel".into(),
                    stream_id: Some(alpha),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: a_cancel,
                patch: TaskPatch {
                    state: Some(TaskState::Cancelled),
                    ..Default::default()
                },
            },
        )
        .unwrap();
        let a_del = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "a del".into(),
                    stream_id: Some(alpha),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::DeleteTask(a_del)).unwrap();

        // beta: 0 open tasks.
        let rows = match e.query(&db, Query::StreamList).unwrap() {
            QueryResult::Streams(v) => v,
            _ => panic!("wrong variant"),
        };

        // Inbox is always first.
        assert_eq!(rows[0].id, inbox_stream_ref());
        assert_eq!(rows[0].name, "Inbox");
        assert_eq!(rows[0].open_task_count, 1);

        // Then real (non-deleted) streams ordered case-insensitively by name:
        // "alpha" < "Beta". Deleted stream excluded entirely.
        let names: Vec<_> = rows.iter().map(|r| r.name.clone()).collect();
        assert_eq!(names, vec!["Inbox", "alpha", "Beta"]);
        assert!(!names.contains(&"Deleted".to_string()));

        let alpha_row = rows.iter().find(|r| r.id == alpha).unwrap();
        assert_eq!(alpha_row.open_task_count, 2);
        let beta_row = rows.iter().find(|r| r.id == beta).unwrap();
        assert_eq!(beta_row.open_task_count, 0);
    }

    #[test]
    fn search_matches_title_and_body_excludes_deleted_and_respects_limit() {
        let mut db = db();
        let e = engine();

        // Title hit.
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "quarterly kangaroo report".into(),
                ..Default::default()
            }),
        )
        .unwrap();
        // Body hit (unique term only in the body).
        e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "misc notes".into(),
                body: Some(NoteBody(b"remember the kangaroo".to_vec())),
                ..Default::default()
            }),
        )
        .unwrap();
        // Deleted task that would otherwise match.
        let del = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "kangaroo obsolete".into(),
                    ..Default::default()
                }),
            )
            .unwrap()
            .entity;
        e.apply(&mut db, Command::DeleteTask(del)).unwrap();

        let hits = match e
            .query(
                &db,
                Query::Search {
                    text: "kangaroo".into(),
                    limit: 10,
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(v) => v,
            _ => panic!("wrong variant"),
        };
        let titles: Vec<_> = hits.iter().map(|t| t.title.clone()).collect();
        assert_eq!(
            hits.len(),
            2,
            "title + body hits, deleted excluded: {titles:?}"
        );
        assert!(titles.contains(&"quarterly kangaroo report".to_string()));
        assert!(titles.contains(&"misc notes".to_string()));
        assert!(!titles.contains(&"kangaroo obsolete".to_string()));

        // Limit is honored.
        let limited = match e
            .query(
                &db,
                Query::Search {
                    text: "kangaroo".into(),
                    limit: 1,
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(v) => v,
            _ => panic!(),
        };
        assert_eq!(limited.len(), 1);

        // Empty / whitespace-only input returns nothing without touching FTS.
        let empty = match e
            .query(
                &db,
                Query::Search {
                    text: "   ".into(),
                    limit: 10,
                },
            )
            .unwrap()
        {
            QueryResult::Tasks(v) => v,
            _ => panic!(),
        };
        assert!(empty.is_empty());
    }

    // Fetch the most-recently-created task id in a stream (highest rowid
    // proxy via id ordering is unstable, so use created ordering by title
    // being unavailable — instead read the last inserted via the ops table
    // is overkill; query tasks table directly).
    fn db_last_task(db: &Db, stream: EntityRef) -> EntityRef {
        let blob: Vec<u8> = stream.bytes().to_vec();
        let raw: Vec<u8> = db
            .conn()
            .query_row(
                "SELECT id FROM tasks WHERE stream_id = ? ORDER BY id DESC LIMIT 1",
                params![blob],
                |r| r.get(0),
            )
            .unwrap();
        let mut a = [0u8; 16];
        let take = raw.len().min(16);
        a[..take].copy_from_slice(&raw[..take]);
        EntityRef::new(EntityKind::Task, a)
    }

    fn sample_constraint() -> ScheduleConstraint {
        ScheduleConstraint {
            time_of_day: Some(sunrise_domain::TimeOfDayRange {
                start: jiff::civil::time(9, 0, 0, 0),
                end: jiff::civil::time(17, 0, 0, 0),
            }),
            days_of_week: sunrise_domain::WeekdaySet::new(),
            date_range: None,
            severity: sunrise_domain::ConstraintSeverity::Hard,
        }
    }

    #[test]
    fn create_task_with_constraints_round_trips() {
        let mut db = db();
        let e = engine();
        let c = sample_constraint();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "with constraint".into(),
                    scheduling_constraints: vec![c],
                    ..Default::default()
                }),
            )
            .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.scheduling_constraints, vec![c]);
    }

    #[test]
    fn update_task_replaces_whole_constraint_list() {
        let mut db = db();
        let e = engine();
        let c1 = sample_constraint();
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    scheduling_constraints: vec![c1],
                    ..Default::default()
                }),
            )
            .unwrap();
        // Replace with a different, single-element list.
        let c2 = ScheduleConstraint {
            time_of_day: None,
            days_of_week: sunrise_domain::WeekdaySet::from_days([sunrise_domain::Weekday::Sa]),
            date_range: None,
            severity: sunrise_domain::ConstraintSeverity::Soft,
        };
        let patch = TaskPatch {
            scheduling_constraints: Some(vec![c2]),
            ..Default::default()
        };
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert_eq!(t.scheduling_constraints, vec![c2]);

        // Clearing to empty persists as NULL / empty list.
        let clear = TaskPatch {
            scheduling_constraints: Some(vec![]),
            ..Default::default()
        };
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch: clear,
            },
        )
        .unwrap();
        let t = match e.query(&db, Query::EntityById(r.entity)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert!(t.scheduling_constraints.is_empty());
    }

    #[test]
    fn patch_due_before_scheduled_is_rejected() {
        let mut db = db();
        let e = engine();
        let now = 1_700_000_000_000i64;
        let scheduled = ms_to_ts(now + 2 * 3_600_000);
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "x".into(),
                    scheduled_at: Some(scheduled),
                    ..Default::default()
                }),
            )
            .unwrap();
        // Patch sets ONLY due_at, earlier than the existing scheduled_at. This
        // used to slip through silently; the invariant re-check must reject it.
        let earlier = ms_to_ts(now + 3_600_000);
        let patch = TaskPatch {
            due_at: Some(Some(earlier)),
            ..Default::default()
        };
        let res = e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch,
            },
        );
        assert!(matches!(
            res,
            Err(EngineError::Validation(
                sunrise_domain::ValidationError::DueBeforeScheduled
            ))
        ));
    }

    #[test]
    fn invalid_constraint_list_rejected_on_create_and_update() {
        let mut db = db();
        let e = engine();
        // An empty (no-dimension) constraint is invalid.
        let bad = ScheduleConstraint {
            time_of_day: None,
            days_of_week: sunrise_domain::WeekdaySet::new(),
            date_range: None,
            severity: sunrise_domain::ConstraintSeverity::Hard,
        };
        let create = e.apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "x".into(),
                scheduling_constraints: vec![bad],
                ..Default::default()
            }),
        );
        assert!(matches!(create, Err(EngineError::Validation(_))));

        // A valid task, then an update that introduces the invalid list.
        let r = e
            .apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "ok".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
        let patch = TaskPatch {
            scheduling_constraints: Some(vec![bad]),
            ..Default::default()
        };
        let update = e.apply(
            &mut db,
            Command::UpdateTask {
                id: r.entity,
                patch,
            },
        );
        assert!(matches!(update, Err(EngineError::Validation(_))));
    }

    proptest::proptest! {
        /// `Query::Search` must never surface an FTS5 syntax error, no matter
        /// how hostile the input (quotes, colons, stars, unbalanced quotes,
        /// `NEAR`, hyphens, parentheses).
        #[test]
        fn search_never_errors_on_hostile_input(s in ".*") {
            let mut db = db();
            let e = engine();
            e.apply(
                &mut db,
                Command::CreateTask(TaskDraft {
                    title: "haystack".into(),
                    ..Default::default()
                }),
            )
            .unwrap();
            let res = e.query(
                &db,
                Query::Search {
                    text: s,
                    limit: 20,
                },
            );
            proptest::prop_assert!(res.is_ok());
        }

        /// Any epoch-ms in a sane range survives the `ms_to_ts` ->
        /// `as_millisecond` round-trip that bridges SQLite INTEGER storage
        /// and `jiff::Timestamp`.
        #[test]
        fn ms_timestamp_round_trip(ms in -10_000_000_000_000i64..=10_000_000_000_000i64) {
            let ts = ms_to_ts(ms);
            proptest::prop_assert_eq!(ts.as_millisecond(), ms);
        }
    }

    // ---- routine materialization tests ----

    use sunrise_domain::{RRule, Routine, RoutineCatchupPolicy, RoutineDraft, TaskTemplate};

    const NOW: i64 = 1_700_000_000_000;
    const DAY_MS: i64 = 86_400_000;

    fn stream_ref(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [b; 16])
    }

    fn routine_draft(
        stream: EntityRef,
        rrule: &str,
        starts_ms: i64,
        policy: RoutineCatchupPolicy,
        constraints: Vec<ScheduleConstraint>,
    ) -> RoutineDraft {
        RoutineDraft {
            template: TaskTemplate {
                title: "Water plants".into(),
                stream_id: stream,
                contexts: Vec::new(),
                energy: None,
                priority: None,
                estimated_duration_s: None,
                body: None,
            },
            rrule: RRule::parse(rrule).unwrap(),
            timezone: "UTC".into(),
            starts_at: ms_to_ts(starts_ms),
            ends_at: None,
            scheduling_constraints: constraints,
            catchup_policy: policy,
        }
    }

    fn op_count(db: &Db) -> i64 {
        db.conn()
            .query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
            .unwrap()
    }

    fn live_task_ids(db: &Db) -> BTreeSet<[u8; 16]> {
        let mut stmt = db
            .conn()
            .prepare("SELECT id FROM tasks WHERE deleted = 0")
            .unwrap();
        let out = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|raw| {
                let raw = raw.unwrap();
                let mut a = [0u8; 16];
                let take = raw.len().min(16);
                a[..take].copy_from_slice(&raw[..take]);
                a
            })
            .collect();
        out
    }

    fn past_task_count(db: &Db, rid: EntityRef) -> i64 {
        db.conn()
            .query_row(
                "SELECT count(*) FROM tasks
                 WHERE routine_id = ? AND deleted = 0 AND scheduled_at_ms <= ?",
                params![rid.bytes().to_vec(), NOW],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn create_routine_materializes_future_tasks_with_linkage() {
        let mut db = db();
        let e = engine();
        let stream = stream_ref(5);
        let c = sample_constraint();
        let draft = routine_draft(
            stream,
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            vec![c],
        );
        let res = e.apply(&mut db, Command::CreateRoutine(draft)).unwrap();
        let rid = res.entity;
        assert_eq!(rid.kind(), EntityKind::Routine);

        // Routine is queryable.
        let routines = match e.query(&db, Query::Routines).unwrap() {
            QueryResult::Routines(v) => v,
            _ => panic!(),
        };
        assert_eq!(routines.len(), 1);
        let routine = routines.into_iter().next().unwrap();
        assert_eq!(routine.id, rid);

        // Daily occurrences within the 14-day horizon are all materialized.
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 14 * DAY_MS));
        let occ = routine.occurrences_in(window).unwrap();
        assert!(occ.len() >= 13, "daily horizon materializes ~14 days");

        // Each occurrence has a deterministic task with linkage + copied
        // constraints + scheduled_at == occurrence instant.
        for o in &occ {
            let tid = occurrence_task_id(&rid, &o.key);
            let t = match e.query(&db, Query::EntityById(tid)).unwrap() {
                QueryResult::Task(t) => *t,
                _ => panic!("task {tid} missing for occurrence {}", o.key),
            };
            assert_eq!(t.routine_id, Some(rid));
            assert_eq!(t.routine_occurrence, Some(o.at));
            assert_eq!(t.scheduled_at, Some(o.at));
            assert_eq!(t.scheduling_constraints, vec![c]);
        }
    }

    #[test]
    fn rematerialize_is_idempotent() {
        let mut db = db();
        let e = engine();
        let draft = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        e.apply(&mut db, Command::CreateRoutine(draft)).unwrap();
        let ids_before = live_task_ids(&db);
        let ops_before = op_count(&db);

        // Re-run materialization at the same clock: no new tasks, no new ops.
        e.apply(&mut db, Command::MaterializeRoutines { now_ms: NOW as u64 })
            .unwrap();
        assert_eq!(live_task_ids(&db), ids_before, "no duplicate tasks");
        assert_eq!(op_count(&db), ops_before, "no new ops on re-materialize");
    }

    #[test]
    fn two_engines_produce_identical_task_ids() {
        // Same routine content (same id) on two independent DBs => identical
        // deterministic task-id sets, even though op-ids differ (rng).
        fn fixed_routine() -> Routine {
            Routine {
                id: EntityRef::new(EntityKind::Routine, [0x33; 16]),
                created_at: ms_to_ts(NOW),
                updated_at: ms_to_ts(NOW),
                template: TaskTemplate {
                    title: "R".into(),
                    stream_id: stream_ref(7),
                    contexts: Vec::new(),
                    energy: None,
                    priority: None,
                    estimated_duration_s: None,
                    body: None,
                },
                rrule: RRule::parse("FREQ=DAILY").unwrap(),
                timezone: "UTC".into(),
                starts_at: ms_to_ts(NOW + 3_600_000),
                ends_at: None,
                scheduling_constraints: Vec::new(),
                skip_dates: Vec::new(),
                skipped_keys: Vec::new(),
                catchup_policy: RoutineCatchupPolicy::Skip,
                streak_counter: 0,
                last_completed_at: None,
                paused: false,
                paused_until: None,
                archived: false,
                deleted: false,
            }
        }

        fn run() -> BTreeSet<[u8; 16]> {
            let mut db = db();
            let e = engine();
            let r = fixed_routine();
            db.with_tx(|tx| {
                ensure_stream_row(
                    tx,
                    &r.template.stream_id,
                    NOW as u64,
                    None,
                    false,
                    false,
                    false,
                )?;
                insert_routine_row(tx, &r, NOW as u64)
            })
            .unwrap();
            e.materialize_one_routine(&mut db, &r, NOW as u64).unwrap();
            live_task_ids(&db)
        }

        let a = run();
        let b = run();
        assert!(!a.is_empty());
        assert_eq!(a, b, "task-id sets converge across devices");
    }

    #[test]
    fn catchup_policies_skip_queue_merge() {
        let start = NOW - (5 * DAY_MS + 3_600_000); // 6 missed daily occurrences
                                                    // Skip: no past tasks.
        {
            let mut db = db();
            let e = engine();
            let d = routine_draft(
                stream_ref(5),
                "FREQ=DAILY",
                start,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            );
            let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
            assert_eq!(past_task_count(&db, rid), 0, "skip drops missed");
        }
        // Queue: one task per missed occurrence.
        {
            let mut db = db();
            let e = engine();
            let d = routine_draft(
                stream_ref(5),
                "FREQ=DAILY",
                start,
                RoutineCatchupPolicy::Queue,
                Vec::new(),
            );
            let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
            assert_eq!(past_task_count(&db, rid), 6, "queue keeps every missed");
        }
        // Merge: exactly one past task, titled with the catch-up suffix.
        {
            let mut db = db();
            let e = engine();
            let d = routine_draft(
                stream_ref(5),
                "FREQ=DAILY",
                start,
                RoutineCatchupPolicy::Merge,
                Vec::new(),
            );
            let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
            assert_eq!(
                past_task_count(&db, rid),
                1,
                "merge collapses missed to one"
            );
            let title: String = db
                .conn()
                .query_row(
                    "SELECT title FROM tasks
                     WHERE routine_id = ? AND deleted = 0 AND scheduled_at_ms <= ?",
                    params![rid.bytes().to_vec(), NOW],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(title.contains("(x6 catch-up)"), "merge title: {title}");
        }
    }

    #[test]
    fn skip_occurrence_prevents_task() {
        let mut db = db();
        let e = engine();
        let d = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
        let routine = read_routine(db.conn(), rid.bytes()).unwrap().unwrap();
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
        let occ = routine.occurrences_in(window).unwrap();
        let target = occ[2].key.clone();
        let tid = occurrence_task_id(&rid, &target);

        // Task exists pre-skip.
        assert!(matches!(
            e.query(&db, Query::EntityById(tid)).unwrap(),
            QueryResult::Task(_)
        ));

        e.apply(
            &mut db,
            Command::SkipRoutineOccurrence {
                id: rid,
                occurrence_key: target.clone(),
            },
        )
        .unwrap();

        // The existing task is tombstoned...
        let t = match e.query(&db, Query::EntityById(tid)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert!(t.deleted, "skipped occurrence's task is tombstoned");

        // ...and re-materialization does not resurrect it.
        e.apply(&mut db, Command::MaterializeRoutines { now_ms: NOW as u64 })
            .unwrap();
        let t2 = match e.query(&db, Query::EntityById(tid)).unwrap() {
            QueryResult::Task(t) => *t,
            _ => panic!(),
        };
        assert!(t2.deleted, "skip persists across materialize");
    }

    #[test]
    fn update_routine_regenerates_future_but_keeps_started() {
        let mut db = db();
        let e = engine();
        let d = routine_draft(
            stream_ref(5),
            "FREQ=DAILY",
            NOW + 3_600_000,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        );
        let rid = e.apply(&mut db, Command::CreateRoutine(d)).unwrap().entity;
        let routine = read_routine(db.conn(), rid.bytes()).unwrap().unwrap();
        let window = (ms_to_ts(NOW), ms_to_ts(NOW + 20 * DAY_MS));
        let daily = routine.occurrences_in(window).unwrap();
        // occ[0] is the anchor weekday (kept by a weekly rule); occ[2] is a
        // different weekday (dropped); mark occ[3] in-progress so it survives.
        let keep_anchor = occurrence_task_id(&rid, &daily[0].key);
        let drop_id = occurrence_task_id(&rid, &daily[2].key);
        let started_id = occurrence_task_id(&rid, &daily[3].key);
        e.apply(
            &mut db,
            Command::UpdateTask {
                id: started_id,
                patch: TaskPatch {
                    state: Some(TaskState::InProgress),
                    ..Default::default()
                },
            },
        )
        .unwrap();

        // Change to weekly (only the anchor weekday recurs).
        let patch = RoutinePatch {
            rrule: Some(RRule::parse("FREQ=WEEKLY").unwrap()),
            ..Default::default()
        };
        e.apply(&mut db, Command::UpdateRoutine { id: rid, patch })
            .unwrap();

        let deleted = |id: EntityRef| -> bool {
            match e.query(&db, Query::EntityById(id)).unwrap() {
                QueryResult::Task(t) => t.deleted,
                _ => panic!(),
            }
        };
        assert!(!deleted(keep_anchor), "anchor-weekday occurrence stays");
        assert!(deleted(drop_id), "off-cadence future task regenerated away");
        assert!(!deleted(started_id), "started task is preserved");
    }
}
