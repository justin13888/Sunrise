//! The Routine command path, materialization, and reads of the `routines` table.
//!
//! Routine ops route to the vault-meta log: a Routine is not owned by any one
//! Stream, and its occurrences are minted locally from the rule rather than
//! travelling as task ops.
//!
//! `streak_advance` lives here although the Task command path calls it. Its
//! whole body is routine bookkeeping — it is the only consumer of
//! `StreakStateProj` — so leaving it beside `complete_task` would drag
//! `routines`, `streak_state` and `materialized_until` into that module for one
//! function.
//!
//! Materialization writes no row either: each occurrence it generates is a
//! `task.create` appended and folded like any command's
//! (`Engine::append_and_fold`), and an occurrence this replica already holds
//! is skipped, which is what keeps a regenerated one from being an error.
//!
//! # Routine times (#506, ADR-0066)
//!
//! A routine's anchor is a wall-clock time: `floating` by default, `zoned`
//! when pinned. Its occurrences are expanded in civil space and placed on
//! the timeline in the anchor's zone or the reader's
//! (`docs/10-cross-cutting/time.md` §6), and each one is planned with the
//! anchor's kind. A routine op is written in the form a build before
//! `DOC_SCHEMA_V` 15 reads ([`Routine::legacy_wire_form`]) unless the vault
//! requires `routine.civil_times` ([`Engine::routine_op_form`]).

use super::ids::time_from_parts;
use super::ids::{decode_unknowns, domain_bytes, home_of, ms_to_ts, require_kind};
use super::snapshot::in_tx;
use super::task::{decode_constraints, read_task};
use super::write::{Appended, PatchBuilder, WriteMode};
use super::{Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::InnerOp;
use crate::queries::QueryResult;
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_domain::time::PREFILTER_SLACK_MS;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    materialization_horizon_days, occurrence_key_at, occurrence_task_id, parse_occurrence_key,
    Routine, RoutineCatchupPolicy, RoutineDraft, RoutinePatch, StreakOutcome, SunriseTime, Task,
    TaskState, TaskTemplate,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// The feature a routine op uses when it carries a routine time the wire form
/// before `DOC_SCHEMA_V` 15 cannot (#506): a `floating` anchor, an `all_day`
/// end, a `zoned` wall-clock time in a DST gap, a non-instant pause.
pub(crate) const ROUTINE_CIVIL_TIMES_FEATURE: &str = "routine.civil_times";

/// The routine write a completion's streak advance makes: the advanced
/// routine as a full-state op, and the keys of the occurrences it counted on
/// time.
pub(super) struct StreakWrite {
    /// The advanced routine, in the form [`Engine::routine_op_form`] chose.
    routine: Routine,
    /// The occurrence keys completed within their grace window, sorted.
    on_time: Vec<String>,
}

impl StreakWrite {
    /// The advance of `routine`, which counted the occurrences `on_time`
    /// within their grace window.
    pub(super) fn new(routine: Routine, mut on_time: Vec<String>) -> Self {
        on_time.sort();
        Self { routine, on_time }
    }

    /// The op it is written as in `mode`, if any.
    ///
    /// In [`WriteMode::Legacy`], the full-state `RoutineUpdate` carrying the
    /// streak the completion rule computed. In [`WriteMode::Patch`], the
    /// `Patch` that adds the on-time keys to `streak_keys`, from which every
    /// replica derives the streak (clarification D17); a key added by a
    /// concurrent completion of another occurrence on another device survives
    /// beside it, so both count (#331). A completion past its grace window
    /// adds no key, which is what makes it a miss, so it writes nothing
    /// (`docs/02-domain/routines-and-recurrence.md` §Streak).
    pub(super) fn op_in(
        &self,
        tx: &Transaction<'_>,
        mode: WriteMode,
    ) -> rusqlite::Result<Option<InnerOp>> {
        if mode == WriteMode::Legacy {
            return Ok(Some(InnerOp::RoutineUpdate(Box::new(self.routine.clone()))));
        }
        if self.on_time.is_empty() {
            return Ok(None);
        }
        let keys = self
            .on_time
            .iter()
            .map(|k| ciborium::value::Value::Text(k.clone()))
            .collect();
        PatchBuilder::new(self.routine.id)
            .edit("streak_keys", keys, Vec::new())
            .build(tx)
    }
}

/// The tombstone of a routine occurrence the routine engine itself drops: a
/// `Patch` of `deleted` with `"origin": "generated"` in [`WriteMode::Patch`],
/// so a concurrent user edit of the occurrence is never beaten by it
/// (ADR-0065 §3), and the full-state `TaskDelete` otherwise.
fn generated_tombstone(
    tx: &Transaction<'_>,
    mode: WriteMode,
    task: &Task,
) -> rusqlite::Result<Option<(InnerOp, [u8; 16])>> {
    let stream = domain_bytes(task.stream_id);
    if mode == WriteMode::Legacy {
        return Ok(Some((InnerOp::TaskDelete(task.clone()), stream)));
    }
    Ok(PatchBuilder::new(task.id)
        .generated()
        .set("deleted", ciborium::value::Value::Bool(true))
        .build(tx)?
        .map(|op| (op, stream)))
}

impl Engine {
    /// The form of `routine` its op is written in.
    ///
    /// `routine` itself when the vault requires `routine.civil_times`. Else
    /// its [`Routine::legacy_wire_form`] when it has one, which every build
    /// reads as this same routine. Else `routine` itself, after requiring the
    /// feature, which [`Engine::require_features`] refuses while a paired
    /// device has not said it supports it: such a device could not read the
    /// op, and would go on writing the routine in the old form.
    ///
    /// # Errors
    /// [`EngineError::FeatureUnsupportedByDevices`] as above; storage.
    pub(super) fn routine_op_form(
        &self,
        db: &mut Db,
        routine: &Routine,
    ) -> Result<Routine, EngineError> {
        if self.vault_requires(db.conn(), ROUTINE_CIVIL_TIMES_FEATURE)? {
            return Ok(routine.clone());
        }
        if let Some(legacy) = routine.legacy_wire_form() {
            return Ok(legacy);
        }
        self.require_features(db, &[ROUTINE_CIVIL_TIMES_FEATURE], false)?;
        Ok(routine.clone())
    }

    /// Advance the owning Routine's streak for a task that just went `done`.
    ///
    /// Returns the `RoutineUpdate` op that carries the advanced Routine, or
    /// `None` when there is nothing to emit: the task is not
    /// routine-generated, its routine is gone, it carries no occurrence
    /// instant, or the occurrence was already counted (the idempotency key set
    /// makes a `done -> todo -> done` round trip a no-op).
    ///
    /// The streak key is the occurrence's intended key, recovered from the
    /// task id ([`Routine::occurrence_key_of`]), so it is the key the task id
    /// and a skip use even on a day the DST rule moved the occurrence (#331).
    /// The grace window runs from the occurrence's due instant for the reader.
    pub(super) fn streak_advance(
        &self,
        db: &mut Db,
        task: &Task,
        now_ms: u64,
    ) -> Result<Option<StreakWrite>, EngineError> {
        let (Some(rid), Some(_)) = (task.routine_id, task.routine_occurrence) else {
            return Ok(None);
        };
        let Some(mut routine) = read_routine(db.conn(), rid.bytes())? else {
            return Ok(None);
        };
        let Some((key, outcome)) = self.advance_streak_of(&mut routine, task, now_ms) else {
            return Ok(None);
        };
        let on_time = (outcome == StreakOutcome::Incremented).then_some(key);
        let routine = self.routine_op_form(db, &routine)?;
        Ok(Some(StreakWrite::new(
            routine,
            on_time.into_iter().collect(),
        )))
    }

    /// Record `task`'s completion in `routine`'s streak, in memory: what
    /// [`Self::streak_advance`] writes, and what a bulk completion of several
    /// occurrences of one routine applies to it once per occurrence. Returns
    /// the occurrence's key and what the completion did, or `None` when the
    /// streak did not move: an occurrence already counted does not.
    pub(super) fn advance_streak_of(
        &self,
        routine: &mut Routine,
        task: &Task,
        now_ms: u64,
    ) -> Option<(String, StreakOutcome)> {
        let occurrence_at = task.routine_occurrence?;
        // A key no expansion near the occurrence has (the rule was edited
        // since) falls back to the occurrence's wall clock in the routine's
        // zone, the key this build counted before #331.
        let key = routine
            .occurrence_key_of(&task.id, occurrence_at)
            .or_else(|| occurrence_key_at(&routine.timezone, occurrence_at).ok())?;
        let reader = self.device_zone();
        let due = parse_occurrence_key(&key)
            .and_then(|c| routine.occurrence_instant(c, &reader))
            .unwrap_or(occurrence_at);
        let outcome = routine.record_occurrence_completion(&key, due, ms_to_ts(now_ms as i64));
        if outcome == StreakOutcome::Duplicate {
            return None;
        }
        routine.updated_at = ms_to_ts(now_ms as i64);
        Some((key, outcome))
    }

    pub(super) fn create_routine(
        &self,
        db: &mut Db,
        mut d: RoutineDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        d.template.stream_id = home_of(d.template.stream_id)?;
        // The routine op commits before its occurrences are materialized in
        // a second transaction, so a task lock is checked here, up front.
        self.ensure_unlocked(db.conn(), &[EntityKind::Routine, EntityKind::Task])?;
        let now_ms = self.clock.now_ms();
        let routine_id = self.fresh_id(EntityKind::Routine, now_ms);
        let mut routine = Routine {
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
        // An instant anchor, end or `UNTIL` in the draft means what a legacy
        // one does: zoned in the draft's zone.
        routine.normalize_legacy_times();
        let wire = match self.routine_op_form(db, &routine) {
            Ok(wire) => wire,
            // A floating routine in a vault a paired device cannot read one
            // in yet. The routine the user asked for is written, pinned in
            // its zone: the behaviour every device understands, which is
            // what a routine was before #506. One edit unpins it once the
            // devices have updated. An explicit edit to floating is refused
            // instead (`update_routine`), because that is the user asking.
            Err(EngineError::FeatureUnsupportedByDevices { devices, .. }) => {
                tracing::info!(
                    ev = "core.routine.pinned_for_older_devices",
                    n_devices = devices.len() as u64,
                    "a new routine was pinned to its zone rather than following the \
                     traveller, because a paired device does not support \
                     routine.civil_times yet"
                );
                routine.rezone_pinned();
                self.routine_op_form(db, &routine)?
            }
            Err(e) => return Err(e),
        };
        // Routine ops route to the vault-meta log.
        let op = InnerOp::RoutineCreate(Box::new(wire));
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);

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
        mut patch: RoutinePatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Routine)?;
        if let Some(t) = patch.template.as_mut() {
            t.stream_id = home_of(t.stream_id)?;
        }
        patch.validate()?;
        // As in `create_routine`: tasks are regenerated and materialized
        // after the routine op commits.
        self.ensure_unlocked(db.conn(), &[EntityKind::Routine, EntityKind::Task])?;
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
        // A zone change keeps every wall-clock time (ADR-0066 §1), so no key
        // changes and nothing is regenerated: the untouched occurrences
        // already planned in the old zone are re-pinned instead.
        let mut repinned_from = None;
        if let Some(tz) = patch.timezone {
            if routine.timezone != tz {
                repinned_from = Some(routine.timezone.clone());
                routine.rezone(tz);
            }
        }
        if let Some(s) = patch.starts_at {
            structural |= routine.starts_at != s;
            routine.starts_at = s;
        }
        if let Some(e) = patch.ends_at {
            structural |= routine.ends_at != e;
            routine.ends_at = e;
        }
        routine.normalize_legacy_times();
        routine.validate_times()?;
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

        let wire = self.routine_op_form(db, &routine)?;
        let op = InnerOp::RoutineUpdate(Box::new(wire));
        // A re-key tombstones the untouched occurrences the new rule no
        // longer has, in the routine op's transaction: in a field-ops vault
        // one group, so no replica shows the new rule beside the old
        // occurrences (ADR-0065 §6).
        let stale = if structural {
            self.stale_future_tasks(db, &routine, now_ms)?
        } else {
            Vec::new()
        };
        let done = db.with_tx(|tx| -> rusqlite::Result<Appended> {
            let mode = self.write_mode(tx, now_ms)?;
            let mut members = vec![(op.clone(), META_STREAM)];
            for t in &stale {
                members.extend(generated_tombstone(tx, mode, t)?);
            }
            let done = self.append_and_fold_together(tx, members, now_ms)?;
            Ok(done.first().copied().unwrap_or(Appended::NOTHING))
        })?;
        let (op_id, seq) = (done.op_id, done.seq);

        if let Some(old) = repinned_from {
            self.repin_future_tasks(db, &routine, &old, now_ms)?;
        }
        self.materialize_one_routine(db, &routine, now_ms)?;

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// After a zone change, move each untouched future occurrence still
    /// planned `zoned` in the old zone to the same wall-clock time in the new
    /// one (ADR-0066 §1): "09:00 Los Angeles" becomes "09:00 New York" for the
    /// occurrences already materialized too. A task the user started,
    /// deferred or re-planned is left as it is.
    fn repin_future_tasks(
        &self,
        db: &mut Db,
        routine: &Routine,
        old: &str,
        now_ms: u64,
    ) -> Result<(), EngineError> {
        let SunriseTime::Zoned { tz: new, .. } = &routine.starts_at else {
            return Ok(());
        };
        let now = ms_to_ts(now_ms as i64);
        for raw in routine_task_ids(db.conn(), routine.id.bytes())? {
            let Some(mut t) = read_task(db.conn(), &raw)? else {
                continue;
            };
            let Some(SunriseTime::Zoned { civil, tz }) = t.planned_at.clone() else {
                continue;
            };
            let future = t
                .planned_at
                .as_ref()
                .and_then(|s| s.resolve_in(&self.device_zone()));
            if tz != old || future.is_none_or(|at| at <= now) {
                continue;
            }
            t.planned_at = Some(SunriseTime::zoned(civil, new.clone()));
            t.updated_at = now;
            let stream = domain_bytes(t.stream_id);
            // The routine engine's own write: in a field-ops vault a
            // generated `Patch` of the plan, which a concurrent user edit of
            // the occurrence beats (ADR-0065 §3).
            in_tx(db, |tx| {
                let op = if self.write_mode(tx, now_ms)? == WriteMode::Patch {
                    let plan = ciborium::value::Value::serialized(&t.planned_at)
                        .map_err(|e| EngineError::Cbor(e.to_string()))?;
                    match PatchBuilder::new(t.id)
                        .generated()
                        .set("scheduled_at", plan)
                        .build(tx)?
                    {
                        Some(op) => op,
                        None => return Ok(()),
                    }
                } else {
                    InnerOp::TaskUpdate(t)
                };
                self.append_and_fold(tx, &op, &stream, now_ms)?;
                Ok(())
            })?;
        }
        Ok(())
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
        let op = InnerOp::RoutineDelete(Box::new(self.routine_op_form(db, &routine)?));
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);
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
                    && t.state.effective() == TaskState::Todo
                    && t.deferred_count == 0
                    && t.routine_id == Some(id)
            })
            .cloned();

        let op = InnerOp::RoutineUpdate(Box::new(self.routine_op_form(db, &routine)?));
        let done = db.with_tx(|tx| -> rusqlite::Result<Appended> {
            // Each its own op under its own stream, folded with its own
            // stamp: see the note on `Engine::lww_stamp`. In a field-ops
            // vault the skip and the tombstone are one group.
            let mut members = vec![(op.clone(), META_STREAM)];
            if let Some(mut t) = drop_task {
                t.deleted = true;
                t.updated_at = ms_to_ts(now_ms as i64);
                let stream = domain_bytes(t.stream_id);
                members.push((InnerOp::TaskDelete(t), stream));
            }
            let done = self.append_and_fold_together(tx, members, now_ms)?;
            Ok(done.first().copied().unwrap_or(Appended::NOTHING))
        })?;
        let (op_id, seq) = (done.op_id, done.seq);

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
    /// and generate each resulting task this replica does not hold, applying the catchup policy to
    /// occurrences at or before `now`. Runs in one transaction per routine:
    /// the inserts and the `materialized_until` advance commit together, so a
    /// crash never leaves the watermark ahead of the tasks it represents.
    ///
    /// Occurrences are placed on the timeline for this device's reader zone
    /// (a floating routine's at its wall clock here), and a pause that has
    /// run out generates again ([`Routine::occurrences_in`]). The window
    /// reopens [`PREFILTER_SLACK_MS`] below the watermark: a zone change (the
    /// reader's, for a floating routine, or the routine's own) moves every
    /// occurrence along the timeline by up to a day, and one that moved back
    /// across the watermark would otherwise never be generated. Its key, and
    /// so its task id, did not move, so one already generated is skipped.
    /// Only future occurrences are taken from the reopened span: what it held
    /// in the past was decided, catch-up and all, when it was first scanned.
    pub(super) fn materialize_one_routine(
        &self,
        db: &mut Db,
        routine: &Routine,
        now_ms: u64,
    ) -> Result<(), EngineError> {
        if routine.archived || routine.deleted || (routine.paused && routine.paused_until.is_none())
        {
            return Ok(());
        }
        let reader = self.device_zone();
        let mat_until = read_materialized_until(db.conn(), routine.id.bytes())?;
        let horizon_ms = u64::from(materialization_horizon_days(&routine.rrule.freq)) * 86_400_000;
        let window_end_ms = now_ms.saturating_add(horizon_ms);
        let starts_ms = routine
            .anchor_civil()
            .and_then(|c| routine.occurrence_instant(c, &reader))
            .map_or(0, |t| u64::try_from(t.as_millisecond().max(0)).unwrap_or(0));
        let reopened = if mat_until == 0 {
            0
        } else {
            mat_until.saturating_sub(PREFILTER_SLACK_MS as u64)
        };
        let window_start_ms = starts_ms.max(reopened);
        if window_end_ms <= window_start_ms {
            return Ok(());
        }
        let window = (
            ms_to_ts(window_start_ms as i64),
            ms_to_ts(window_end_ms as i64),
        );
        let occ = routine
            .occurrences_in(window, &reader)
            .map_err(|e| EngineError::Invalid(format!("routine expand: {e}")))?;
        let now_ts = ms_to_ts(now_ms as i64);
        let first_scan = ms_to_ts(i64::try_from(mat_until).unwrap_or(i64::MAX));

        // Partition into missed (<= now) and future (> now). A missed one in
        // the reopened span was already decided.
        let (missed, future): (Vec<_>, Vec<_>) = occ
            .into_iter()
            .filter(|o| o.at > now_ts || o.at >= first_scan)
            .partition(|o| o.at <= now_ts);

        // Build the (occurrence, optional-title-override) pairs to insert.
        let mut jobs: Vec<(&sunrise_domain::Occurrence, Option<String>)> = Vec::new();
        match routine.catchup_policy {
            // An unknown policy reads as `skip`, its fallback: it must not
            // materialize a backlog nobody asked for.
            RoutineCatchupPolicy::Skip | RoutineCatchupPolicy::Unknown(_) => {}
            RoutineCatchupPolicy::Queue => {
                for o in &missed {
                    jobs.push((o, None));
                }
            }
            RoutineCatchupPolicy::Merge => {
                let n = missed.len();
                if n >= 2 {
                    let latest = &missed[n - 1];
                    let title = format!("{} (x{n} catch-up)", routine.template.title);
                    jobs.push((latest, Some(title)));
                } else if let Some(o) = missed.first() {
                    jobs.push((o, None));
                }
            }
        }
        for o in &future {
            jobs.push((o, None));
        }

        let routine = routine.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            for (occ, title_override) in &jobs {
                let task = build_routine_task(&routine, occ, title_override.clone(), now_ms);
                // An occurrence is generated once: its id is a function of the
                // routine and the key, so one this replica already holds,
                // whoever generated it, is not generated again. Each one it
                // does generate is its OWN op, with its own seq and stamp.
                if occurrence_exists(tx, task.id.bytes())? {
                    continue;
                }
                let stream = domain_bytes(task.stream_id);
                self.append_and_fold(tx, &InnerOp::TaskCreate(task), &stream, now_ms)?;
            }
            set_materialized_until(tx, routine.id.bytes(), window_end_ms)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Regeneration policy (current): after a structural routine edit, delete future
    /// routine-materialized tasks that are still untouched (Todo, non-deferred)
    /// and whose deterministic id no longer matches any current future
    /// occurrence. Started, completed, deferred, or user-moved tasks are kept.
    ///
    /// Returns those tasks, each tombstoned, for the routine op's transaction
    /// to write.
    fn stale_future_tasks(
        &self,
        db: &Db,
        routine: &Routine,
        now_ms: u64,
    ) -> Result<Vec<Task>, EngineError> {
        let horizon_ms = u64::from(materialization_horizon_days(&routine.rrule.freq)) * 86_400_000;
        let now_ts = ms_to_ts(now_ms as i64);
        let window = (now_ts, ms_to_ts(now_ms.saturating_add(horizon_ms) as i64));
        let valid: BTreeSet<[u8; 16]> = routine
            .occurrences_in(window, &self.device_zone())
            .map_err(|e| EngineError::Invalid(format!("routine expand: {e}")))?
            .iter()
            .map(|o| *occurrence_task_id(&routine.id, &o.key).bytes())
            .collect();

        // Candidate future, not-started routine tasks for this routine.
        let rid_blob: Vec<u8> = routine.id.bytes().to_vec();
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE routine_id = ? AND deleted = 0
               -- `todo`, spelled so an unknown state reads as it (ADR-0045 §6).
               AND state NOT IN ('in_progress', 'done', 'cancelled')
               AND deferred_count = 0
               AND planned_at_ms IS NOT NULL AND planned_at_ms > ?",
        )?;
        // The index key prefilters, widened (time.md §2 rule 2); whether a
        // task is in the future is decided on its resolved time below.
        let floor = (now_ms as i64).saturating_sub(PREFILTER_SLACK_MS);
        let ids = stmt
            .query_map(params![rid_blob, floor], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let reader = self.device_zone();
        let mut out = Vec::new();
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
            let future = t
                .planned_at
                .as_ref()
                .and_then(|s| s.resolve_in(&reader))
                .is_some_and(|at| at > now_ts);
            if !future {
                continue;
            }
            t.deleted = true;
            t.updated_at = ms_to_ts(now_ms as i64);
            out.push(t);
        }
        Ok(out)
    }

    pub(super) fn query_routines(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let now_ms = self.clock.now_ms();
        Ok(QueryResult::Routines(
            read_routines(db.conn())?
                .into_iter()
                .map(|r| self.streak_now(r, now_ms))
                .collect(),
        ))
    }

    /// `routine` with its streak as a read shows it at `now_ms`: the stored
    /// streak, with the occurrences after the newest key that lapsed since
    /// applied as misses ([`Routine::streak_as_of`]). Stored streak fields
    /// are as of the newest key, a function of the data; "as of now" is a
    /// function of the clock, so it is applied here, at read time, and never
    /// written (ADR-0065 §5).
    pub(super) fn streak_now(&self, mut routine: Routine, now_ms: u64) -> Routine {
        let now = ms_to_ts(i64::try_from(now_ms).unwrap_or(i64::MAX));
        routine
            .streak_as_of(now, &self.device_zone())
            .apply_to(&mut routine);
        routine
    }
}

// ---- table operations ----

/// Whether the occurrence task `id` is already on this replica: projected to
/// a row, or created in the merge state and waiting for a field it lacks. A
/// replica that holds only a later edit of it (a `Patch` that overtook its
/// create) still generates the create.
fn occurrence_exists(tx: &Transaction<'_>, id: &[u8; 16]) -> rusqlite::Result<bool> {
    tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM tasks WHERE id = ?1)
             OR EXISTS (SELECT 1 FROM merge_entities WHERE entity_id = ?1 AND created = 1)",
        params![&id[..]],
        |r| r.get(0),
    )
}

/// Every live task this routine generated.
fn routine_task_ids(
    conn: &rusqlite::Connection,
    rid: &[u8; 16],
) -> rusqlite::Result<Vec<[u8; 16]>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM tasks
         WHERE routine_id = ? AND deleted = 0
           -- `todo`, spelled so an unknown state reads as it (ADR-0045 §6).
           AND state NOT IN ('in_progress', 'done', 'cancelled')
           AND deferred_count = 0
         ORDER BY id",
    )?;
    let ids = stmt
        .query_map(params![&rid[..]], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids.iter().map(|raw| super::ids::blob16(raw)).collect())
}

/// Build a materialized routine task for one occurrence. `title_override`
/// carries the merge suffix when a catch-up collapses ≥2 missed occurrences.
///
/// The task is planned at the occurrence's civil key, of the anchor's kind
/// ([`Routine::occurrence_time`]): never an instant (ADR-0066 §1).
/// `routine_occurrence` records when it was due for this device's reader.
fn build_routine_task(
    routine: &Routine,
    occ: &sunrise_domain::Occurrence,
    title_override: Option<String>,
    now_ms: u64,
) -> Task {
    let key = occ.key.as_str();
    let at = occ.at;
    let draft = routine
        .template
        .to_draft(Some(routine.occurrence_time(occ.civil)));
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
        planned_at: draft.planned_at,
        target_at: None,
        hard_due_at: None,
        // Routine constraints are copied verbatim onto each occurrence.
        scheduling_constraints: routine.scheduling_constraints.clone(),
        completed_at: None,
        deferred_count: 0,
        blocks: BTreeSet::new(),
        blocked_by: BTreeSet::new(),
        assignee: None,
        routine_id: Some(routine.id),
        routine_occurrence: Some(at),
        sort_order: None,
        triaged_at: None,
        archived: false,
        deleted: false,
        unknown: Unknowns::new(),
    }
}

/// The stored spelling of a catch-up policy; an unknown one's raw string.
pub(super) fn catchup_policy_str(p: &RoutineCatchupPolicy) -> &str {
    p.as_str()
}

/// Read a stored catch-up policy. Lossless (ADR-0045 §6): an unknown
/// spelling is kept, and reads as `skip` wherever logic acts on it.
fn parse_catchup_policy(s: &str) -> RoutineCatchupPolicy {
    RoutineCatchupPolicy::from_raw(s)
}

/// Encode a CBOR blob for a non-empty serializable value, or `None` when empty
/// (mirrors `encode_constraints`, keeping empty lists as SQL NULL).
pub(super) fn encode_blob_opt<T: serde::Serialize>(
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
pub(super) fn encode_streak_state(r: &Routine) -> rusqlite::Result<Option<Vec<u8>>> {
    let proj = StreakStateProj::from_routine(r);
    let empty = proj.is_default();
    encode_blob_opt(&proj, empty)
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

/// A routine's rule as canonical CBOR, for `routines.rrule_cbor` (migration
/// 0032).
///
/// The RFC 5545 text beside it cannot hold a raw `FREQ`/`BYDAY`/`WKST` value
/// that contains `;`, `,` or `=`, and those values are now kept verbatim
/// (ADR-0045 §6); the blob holds any string.
pub(super) fn encode_rrule(rule: &sunrise_domain::RRule) -> rusqlite::Result<Vec<u8>> {
    sunrise_cbor::encode_canonical(rule)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// Read a stored rule: the CBOR blob when the row has one, else the text, which
/// is all a row written before migration 0032 has — and which only ever held
/// values its writer knew, so it parses back exactly.
fn decode_rrule(
    rrule_blob: Option<&[u8]>,
    rrule_text: &str,
) -> Result<sunrise_domain::RRule, EngineError> {
    match rrule_blob {
        Some(b) => sunrise_cbor::decode_canonical(b).map_err(|e| EngineError::Cbor(e.to_string())),
        None => sunrise_domain::RRule::parse(rrule_text)
            .map_err(|e| EngineError::Invalid(format!("stored rrule: {e}"))),
    }
}

/// The columns a routine is read from, in the order [`RoutineRowRaw::read`]
/// reads them.
const ROUTINE_COLUMNS: &str = "rrule_text, timezone,
     starts_at_ms, starts_at_kind, starts_at_tz, starts_at_civil,
     ends_at_ms, ends_at_kind, ends_at_tz, ends_at_civil,
     streak_counter, paused, archived, deleted, scheduling_constraints,
     template, skip_dates, skipped_keys, catchup_policy,
     last_completed_at_ms, last_completed_at_kind, last_completed_at_tz, last_completed_at_civil,
     paused_until_ms, paused_until_kind, paused_until_tz, paused_until_civil,
     created_at_ms, updated_at_ms, streak_state, extra, rrule_cbor";

/// One `SunriseTime`'s four columns, starting at `at`.
fn time_at(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Option<SunriseTime>> {
    Ok(time_from_parts(
        r.get(at)?,
        r.get::<_, Option<String>>(at + 1)?.as_deref(),
        r.get::<_, Option<String>>(at + 2)?.as_deref(),
        r.get::<_, Option<String>>(at + 3)?.as_deref(),
    ))
}

/// The columns of [`ROUTINE_COLUMNS`] starting at `from`, as owned values,
/// decoded outside the row closure (which must return `rusqlite::Result`).
struct RoutineRowRaw {
    rrule_text: String,
    timezone: String,
    starts_at: Option<SunriseTime>,
    ends_at: Option<SunriseTime>,
    streak: i64,
    paused: i64,
    archived: i64,
    deleted: i64,
    constraints: Option<Vec<u8>>,
    template: Option<Vec<u8>>,
    skip_dates: Option<Vec<u8>>,
    skipped_keys: Option<Vec<u8>>,
    catchup: String,
    last_completed_at: Option<SunriseTime>,
    paused_until: Option<SunriseTime>,
    created_ms: i64,
    updated_ms: i64,
    streak_state: Option<Vec<u8>>,
    extra: Option<Vec<u8>>,
    rrule_blob: Option<Vec<u8>>,
}

impl RoutineRowRaw {
    fn read(r: &rusqlite::Row<'_>, from: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            rrule_text: r.get(from)?,
            timezone: r.get(from + 1)?,
            starts_at: time_at(r, from + 2)?,
            ends_at: time_at(r, from + 6)?,
            streak: r.get(from + 10)?,
            paused: r.get(from + 11)?,
            archived: r.get(from + 12)?,
            deleted: r.get(from + 13)?,
            constraints: r.get(from + 14)?,
            template: r.get(from + 15)?,
            skip_dates: r.get(from + 16)?,
            skipped_keys: r.get(from + 17)?,
            catchup: r.get(from + 18)?,
            last_completed_at: time_at(r, from + 19)?,
            paused_until: time_at(r, from + 23)?,
            created_ms: r.get(from + 27)?,
            updated_ms: r.get(from + 28)?,
            streak_state: r.get(from + 29)?,
            extra: r.get(from + 30)?,
            rrule_blob: r.get(from + 31)?,
        })
    }
}

/// Decode a routine row. The routine is read as the engine acts on it, its
/// legacy bare instants read as what they mean
/// ([`Routine::normalize_legacy_times`]).
fn routine_from_row(id: &[u8; 16], v: RoutineRowRaw) -> Result<Routine, EngineError> {
    let rrule = decode_rrule(v.rrule_blob.as_deref(), &v.rrule_text)?;
    let template: TaskTemplate = match v.template {
        Some(b) => {
            sunrise_cbor::decode_canonical(&b).map_err(|e| EngineError::Cbor(e.to_string()))?
        }
        None => return Err(EngineError::Invalid("routine missing template".into())),
    };
    // NULL (every pre-v9 row, and every routine never completed) decodes to the
    // all-default streak state.
    let streak_proj: StreakStateProj = decode_blob_opt(v.streak_state)?;
    let starts_at = v
        .starts_at
        .ok_or_else(|| EngineError::Invalid("routine missing starts_at".into()))?;
    let routine = Routine {
        id: EntityRef::new(EntityKind::Routine, *id),
        created_at: ms_to_ts(v.created_ms.max(0)),
        updated_at: ms_to_ts(v.updated_ms.max(0)),
        template,
        rrule,
        timezone: v.timezone,
        starts_at,
        ends_at: v.ends_at,
        scheduling_constraints: decode_constraints(v.constraints)?,
        skip_dates: decode_blob_opt(v.skip_dates)?,
        skipped_keys: decode_blob_opt(v.skipped_keys)?,
        catchup_policy: parse_catchup_policy(&v.catchup),
        streak_counter: v.streak,
        last_completed_at: v.last_completed_at,
        grace_window_s: streak_proj.grace_window_s,
        forgiveness_enabled: !streak_proj.forgiveness_disabled,
        streak_started_at: streak_proj.streak_started_at_ms.map(|m| ms_to_ts(m.max(0))),
        forgivenesses_in_window: streak_proj.forgivenesses_in_window,
        streak_keys: streak_proj.streak_keys,
        paused: v.paused != 0,
        paused_until: v.paused_until,
        archived: v.archived != 0,
        deleted: v.deleted != 0,
        unknown: decode_unknowns(v.extra),
    };
    Ok(routine.normalized())
}

pub(super) fn read_routine(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Routine>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let sql = format!("SELECT {ROUTINE_COLUMNS} FROM routines WHERE id = ?");
    let raw = conn
        .query_row(&sql, params![id_blob], |r| RoutineRowRaw::read(r, 0))
        .optional()?;
    raw.map(|v| routine_from_row(id, v)).transpose()
}

pub(super) fn read_routines(conn: &rusqlite::Connection) -> Result<Vec<Routine>, EngineError> {
    let sql =
        format!("SELECT id, {ROUTINE_COLUMNS} FROM routines WHERE deleted = 0 ORDER BY id ASC");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            let id_raw: Vec<u8> = r.get(0)?;
            Ok((id_raw, RoutineRowRaw::read(r, 1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (raw, v) in rows {
        let mut id = [0u8; 16];
        let take = raw.len().min(16);
        id[..take].copy_from_slice(&raw[..take]);
        out.push(routine_from_row(&id, v)?);
    }
    Ok(out)
}
