//! Reviews and stats (`docs/08-features/reviews-and-stats.md`).
//!
//! Everything in this module follows one shape: **the query fetches, a pure fold
//! in `sunrise-domain` computes**. Nothing here does arithmetic on a review
//! number, which is why the review, the trends, the timeline and the export can
//! never disagree with one another — they are four renderings of two folds.
//!
//! It is also the only read path that re-opens and decrypts op history rather
//! than reading materialized rows, which is what [`MAX_TREND_WEEKS`] bounds.

use super::ids::{blob16, ms_to_ts};
use super::lww::LwwStamp;
use super::merge::{check_patch, set_field, FieldOp};
use super::routine::read_routines;
use super::stream::{paused_streams, stream_member_tasks};
use super::task::{actionable_scan, read_task};
use super::{Engine, EngineError, MAX_TREND_WEEKS, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::{decode_inner_op, InnerOp, PatchPayload};
use crate::queries::QueryResult;
use rusqlite::{params, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_crypto::op_envelope::open_envelope;
use sunrise_crypto::{decode_envelope, DeviceCert};
use sunrise_domain::Unknowns;
use sunrise_domain::{
    activity_for_entities, activity_table, build_daily_review, build_weekly_review, focus_table,
    fold_activity, fold_trends, inbox_stream_ref, routine_drift, streaks_table, trends_table,
    ActivityEvent, ExportDataset, ExportFormat, OpPayload, OpRecord, ReviewSnapshot,
    ReviewSnapshotDraft, ReviewStream, ReviewWindow, RoutineDrift, StreakRow, Task, Trends,
    WeekGrid, WeeklyReview, WeeklyReviewInput, DEFAULT_DRIFT_THRESHOLD, DRIFT_WINDOW_WEEKS,
    TREND_WEEKS,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    /// Record a completed weekly review as an append-only `rvw_` record.
    pub(super) fn save_review_snapshot(
        &self,
        db: &mut Db,
        d: ReviewSnapshotDraft,
    ) -> Result<CommandResult, EngineError> {
        let now_ms = self.clock.now_ms();
        let id = self.fresh_id(EntityKind::ReviewSnapshot, now_ms);
        let snapshot = ReviewSnapshot {
            id,
            created_at: ms_to_ts(now_ms as i64),
            window_start: sunrise_domain::epoch_ms::from_u64(d.window_start_ms),
            window_end: sunrise_domain::epoch_ms::from_u64(d.window_end_ms.max(d.window_start_ms)),
            totals: d.totals,
            streams: d.streams,
            streaks: d.streaks,
            note: d.note,
            unknown: Unknowns::new(),
        };
        // A review spans the whole vault, so it routes to the meta log the way
        // Stream and Routine lifecycle ops do.
        let op = InnerOp::ReviewSnapshotCreate(Box::new(snapshot));
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// The device signing keys this vault can open envelopes with: this device
    /// plus every trusted peer, **including revoked ones**.
    ///
    /// A revoked device's past ops still happened, and a history that silently
    /// loses them the moment a laptop is de-authorized would be worse than no
    /// history. Nothing filters on revocation anywhere on this path — not here
    /// and not in [`Self::lookup_device_cert`], whose own doc explains why
    /// membership is not a fact about which key verifies a signature.
    pub(super) fn device_signing_keys(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<BTreeMap<[u8; 16], [u8; 32]>, EngineError> {
        let mut keys = BTreeMap::new();
        keys.insert(
            self.keychain.device_id(),
            self.keychain.device_signing_pub(),
        );
        let mut stmt = conn.prepare("SELECT device_id, cert_blob FROM devices")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, blob) in rows {
            let Ok(cert) = DeviceCert::from_cbor(&blob) else {
                continue;
            };
            let mut a = [0u8; 16];
            let take = id.len().min(16);
            a[..take].copy_from_slice(&id[..take]);
            keys.insert(a, cert.body.d_s_pub);
        }
        Ok(keys)
    }

    /// Decode op-log rows into the domain's [`OpRecord`] form.
    ///
    /// A row that cannot be opened (an envelope from a device whose cert this
    /// replica never stored) is **skipped, not an error**: a partial history is
    /// still a useful history, and one unreadable op must not blank a review.
    ///
    /// A task `Patch` carries only the fields it wrote, and the folds read
    /// whole tasks. So each one is replayed onto the task as the record
    /// before it left it ([`replay_task_patch`]), or, when the history read
    /// holds no earlier record of that task (compacted, or outside the
    /// window), onto the task as it projects now. It then reads as the
    /// `TaskCreated`, `TaskUpdated` or `TaskDeleted` of that whole task, and
    /// `fold_activity` and `fold_trends` read it as any other.
    fn decode_op_records(
        &self,
        conn: &rusqlite::Connection,
        keys: &BTreeMap<[u8; 16], [u8; 32]>,
        rows: Vec<OpRow>,
    ) -> Vec<OpRecord> {
        let mut tasks: BTreeMap<EntityRef, Task> = BTreeMap::new();
        rows.into_iter()
            .filter_map(|row| {
                let inner = self.open_logged_op(keys, &row.envelope)?;
                let target = inner.target_ref();
                let (payload, after) = match inner {
                    InnerOp::Patch(p) if target.kind() == EntityKind::Task => {
                        // A create starts from nothing, never from the task
                        // as it projects now, which already holds every later
                        // write.
                        let base = match tasks.get(&target) {
                            Some(t) => Some(t.clone()),
                            None if p.create => None,
                            None => read_task(conn, target.bytes()).ok().flatten(),
                        };
                        replay_task_patch(base, &p, row.ts_ms)
                    }
                    InnerOp::Patch(p) if target.kind() == EntityKind::Stream => {
                        (stream_patch_payload(&p, row.ts_ms), None)
                    }
                    InnerOp::TaskCreate(t) => {
                        let after = Some(t.clone());
                        (OpPayload::TaskCreated(Box::new(t)), after)
                    }
                    InnerOp::TaskUpdate(t) => {
                        let after = Some(t.clone());
                        (OpPayload::TaskUpdated(Box::new(t)), after)
                    }
                    InnerOp::TaskDelete(t) => (OpPayload::TaskDeleted, Some(t)),
                    other => (op_payload(other), None),
                };
                if let Some(t) = after {
                    tasks.insert(target, t);
                }
                Some(OpRecord {
                    op_id: row.op_id,
                    at_ms: row.ts_ms,
                    device: row.device,
                    target,
                    payload,
                })
            })
            .collect()
    }

    /// Verify + decrypt one stored envelope back to its [`InnerOp`].
    ///
    /// Unlike [`Keychain::open_op`](crate::keychain::Keychain::open_op) this accepts envelopes signed by *any*
    /// known device, which is what makes a merged vault's timeline show the
    /// other device's edits instead of only this one's.
    fn open_logged_op(
        &self,
        keys: &BTreeMap<[u8; 16], [u8; 32]>,
        envelope: &[u8],
    ) -> Option<InnerOp> {
        let env = decode_envelope(envelope).ok()?;
        let d_s_pub = keys.get(&env.device_id)?;
        let inner = self
            .keychain
            .stream_keys_at(&env.stream_id, env.epoch)
            .iter()
            .find_map(|k| open_envelope(&env, d_s_pub, Some(k)).ok())?;
        decode_inner_op(&inner).ok()
    }

    /// Every task op for the tasks touched since `since_ms`, in log order.
    ///
    /// The subselect is the whole trick: a trend needs each task's **full**
    /// history (to tell a transition from a rewrite of the same state), but it
    /// only cares about tasks something happened to inside the window. So the
    /// window picks the tasks and the outer query takes their whole history.
    fn task_history_ops(&self, db: &Db, since_ms: u64) -> Result<Vec<OpRecord>, EngineError> {
        let keys = self.device_signing_keys(db.conn())?;
        let since = i64::try_from(since_ms).unwrap_or(i64::MAX);
        let mut stmt = db.conn().prepare(
            // Filtered on `target_kind`, not on `inner_kind`: a vault's own
            // defers from before ADR-0065 PR 3 are logged as `task.defer`,
            // though each arrived at a peer as `task.update` (both are a
            // full-state `TaskUpdate`), so an inner-kind list would quietly
            // lose one device's defers.
            //
            // Ordered by `(ts_ms, device_id, seq)`, **not** by `(ts_ms,
            // op_id)`. `fold_activity` is a state machine over successive
            // full-state snapshots, so the order it reads them in *is* the
            // history it reports. `op_id` is a ULID whose low bits are random,
            // so two ops a device wrote in the same millisecond used to sort
            // arbitrarily — and a pair read backwards does not just come out
            // reordered, it fabricates a transition that never happened
            // (defer-then-complete read backwards reports a *reopen*).
            // `(device_id, seq)` is the device's own causal order, which is
            // exactly what the fold needs; `op_id` stays as a final tiebreak
            // so the ordering is still total.
            "SELECT op_id, ts_ms, device_id, envelope FROM ops
             WHERE target_kind = 'task'
               AND target_id IN (
                   SELECT DISTINCT target_id FROM ops
                   WHERE target_kind = 'task'
                     AND ts_ms >= ?1 AND target_id IS NOT NULL)
             ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC",
        )?;
        let rows = stmt
            .query_map(params![since], op_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(self.decode_op_records(db.conn(), &keys, rows))
    }

    /// The ops that make up one entity's activity feed.
    ///
    /// For a Task: its own ops plus the ops of every focus session recorded
    /// against it. For a Stream: its own lifecycle ops plus the ops of the
    /// Tasks that currently live in it, so the Stream's feed reads as "what
    /// happened in this stream".
    fn entity_timeline_ops(
        &self,
        db: &Db,
        entity: EntityRef,
    ) -> Result<Vec<OpRecord>, EngineError> {
        let keys = self.device_signing_keys(db.conn())?;
        let id: Vec<u8> = entity.bytes().to_vec();
        let sql = match entity.kind() {
            EntityKind::Task => {
                "SELECT op_id, ts_ms, device_id, envelope FROM ops
                 WHERE target_id = ?1
                    OR target_id IN (SELECT id FROM focus_sessions WHERE task_id = ?1)
                 ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC"
            }
            EntityKind::Stream => {
                "SELECT op_id, ts_ms, device_id, envelope FROM ops
                 WHERE target_id = ?1
                    OR target_id IN (SELECT id FROM tasks WHERE stream_id = ?1)
                    OR target_id IN (SELECT id FROM focus_sessions WHERE stream_id = ?1)
                 ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC"
            }
            _ => {
                return Err(EngineError::Invalid(
                    "activity timelines exist for tasks and streams only".into(),
                ))
            }
        };
        let mut stmt = db.conn().prepare(sql)?;
        let rows = stmt
            .query_map(params![id], op_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(self.decode_op_records(db.conn(), &keys, rows))
    }

    /// The device-local week grid, each week beginning on the `week_start`
    /// preference (`docs/10-cross-cutting/time.md` §4).
    ///
    /// The timezone comes from the injected clock — the same seam scheduling
    /// constraints use — so a week boundary is never read from ambient state.
    fn week_grid(&self, db: &Db, now_ms: u64, weeks: u32) -> Result<WeekGrid, EngineError> {
        let tz = self.device_zone();
        let week_start = self.week_start(db)?;
        WeekGrid::trailing(now_ms, weeks.clamp(1, MAX_TREND_WEEKS), &tz, &week_start)
            .map_err(|e| EngineError::Invalid(format!("week grid: {e}")))
    }

    pub(super) fn query_stream_trends(
        &self,
        db: &Db,
        weeks: u32,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Trends(Box::new(
            self.trends(db, weeks, now_ms)?,
        )))
    }

    fn trends(&self, db: &Db, weeks: u32, now_ms: u64) -> Result<Trends, EngineError> {
        let grid = self.week_grid(db, now_ms, weeks)?;
        let ops = self.task_history_ops(db, grid.starts().first().copied().unwrap_or(0))?;
        Ok(fold_trends(&ops, &grid))
    }

    pub(super) fn query_activity_timeline(
        &self,
        db: &Db,
        entity: EntityRef,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        let ops = self.entity_timeline_ops(db, entity)?;
        let events = fold_activity(&ops);
        // A Stream's feed is its own events plus its members'; a Task's feed is
        // just its own (its focus sessions are already attributed to it by the
        // fold).
        let mut subjects = BTreeSet::from([entity]);
        if entity.kind() == EntityKind::Stream {
            subjects.extend(stream_member_tasks(db.conn(), entity.bytes())?);
        }
        Ok(QueryResult::Activity(activity_for_entities(
            &events,
            &subjects,
            limit as usize,
        )))
    }

    /// Assemble the weekly review.
    pub(super) fn query_weekly_review(
        &self,
        db: &Db,
        week_start_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::WeeklyReview(Box::new(self.weekly_review(
            db,
            week_start_ms,
            now_ms,
        )?)))
    }

    fn weekly_review(
        &self,
        db: &Db,
        week_start_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<WeeklyReview, EngineError> {
        let grid = self.week_grid(db, now_ms, TREND_WEEKS)?;
        let start = week_start_ms.unwrap_or_else(|| grid.last_start_ms());
        let end = grid
            .starts()
            .iter()
            .find(|s| **s > start)
            .copied()
            .unwrap_or_else(|| grid.end_ms());
        let window = ReviewWindow::new(start, end);

        // One op scan feeds both folds: the activity feed the review's counts
        // come from, and the trend line each Stream shows.
        let ops = self.task_history_ops(db, grid.starts().first().copied().unwrap_or(0))?;
        let mut activity = fold_activity(&ops);
        activity.extend(self.focus_activity(db, window)?);
        activity.sort_by_key(|e| (e.at_ms, e.op_id));
        let trends = fold_trends(&ops, &grid);

        let QueryResult::FocusStats(focus) =
            self.query_focus_stats(db, None, Some(window.start_ms), now_ms)?
        else {
            return Err(EngineError::Invalid("focus stats shape".into()));
        };

        let QueryResult::Streams(stream_rows) = self.query_stream_list(db)? else {
            return Err(EngineError::Invalid("stream list shape".into()));
        };
        // `StreamRow` carries no `paused` flag and is constructed in several
        // crates, so the pause state is read straight from the projection here
        // rather than widened onto a shared struct.
        let paused = paused_streams(db.conn())?;
        let streams: Vec<ReviewStream> = stream_rows
            .into_iter()
            .map(|r| ReviewStream {
                paused: paused.contains(&r.id),
                id: r.id,
                name: r.name,
                archived: r.archived,
            })
            .collect();

        let tasks = read_live_tasks(db.conn())?;
        let routines: Vec<_> = read_routines(db.conn())?
            .into_iter()
            .map(|r| self.streak_now(r, now_ms))
            .collect();
        let drift_window = drift_window(window, DRIFT_WINDOW_WEEKS)?;
        let drift: Vec<RoutineDrift> = routines
            .iter()
            .filter(|r| !r.deleted && !r.archived)
            .filter_map(|r| {
                routine_drift(
                    r,
                    drift_window,
                    DEFAULT_DRIFT_THRESHOLD,
                    &self.device_zone(),
                )
                .ok()
            })
            .collect();

        Ok(build_weekly_review(WeeklyReviewInput {
            window,
            streams,
            tasks,
            activity,
            routines,
            drift,
            focus: *focus,
            trends,
            zone: self.device_zone(),
        }))
    }

    /// Focus start/end events inside `window`, folded from the op log.
    ///
    /// Kept separate from [`Self::task_history_ops`] because the trend fold has
    /// no use for them and scanning the whole session log for a 12-week trend
    /// would be waste.
    fn focus_activity(
        &self,
        db: &Db,
        window: ReviewWindow,
    ) -> Result<Vec<ActivityEvent>, EngineError> {
        let keys = self.device_signing_keys(db.conn())?;
        let since = i64::try_from(window.start_ms).unwrap_or(i64::MAX);
        let until = i64::try_from(window.end_ms).unwrap_or(i64::MAX);
        let mut stmt = db.conn().prepare(
            "SELECT op_id, ts_ms, device_id, envelope FROM ops
             WHERE target_kind = 'focus_session'
               AND ts_ms >= ?1 AND ts_ms < ?2
             ORDER BY ts_ms ASC, device_id ASC, seq ASC, op_id ASC",
        )?;
        let rows = stmt
            .query_map(params![since, until], op_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(fold_activity(&self.decode_op_records(
            db.conn(),
            &keys,
            rows,
        )))
    }

    pub(super) fn query_daily_review(
        &self,
        db: &Db,
        since_ms: u64,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let QueryResult::StreamTasks(inbox) = self.query_inbox(db)? else {
            return Err(EngineError::Invalid("inbox shape".into()));
        };
        let QueryResult::Tasks(today) = self.query_today(db, now_ms, &[])? else {
            return Err(EngineError::Invalid("today shape".into()));
        };
        // The derived-blocked set, straight off the dependency index — the same
        // walk `Query::Actionable` uses, so "blocked" means one thing.
        let blocked: BTreeSet<EntityRef> = actionable_scan(db.conn(), None, u32::MAX, false)?
            .into_iter()
            .filter(|(_, open, _)| *open > 0)
            .map(|(bytes, _, _)| EntityRef::new(EntityKind::Task, bytes))
            .collect();
        Ok(QueryResult::DailyReview(Box::new(build_daily_review(
            ReviewWindow::new(since_ms, now_ms.max(since_ms)),
            inbox,
            today,
            &blocked,
        ))))
    }

    pub(super) fn query_review_history(
        &self,
        db: &Db,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT body FROM review_snapshots
             ORDER BY window_start_ms DESC, created_at_ms DESC, id ASC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![limit], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let out = rows
            .into_iter()
            .filter_map(|blob| ciborium::de::from_reader::<ReviewSnapshot, _>(&blob[..]).ok())
            .collect();
        Ok(QueryResult::ReviewSnapshots(out))
    }

    pub(super) fn query_export(
        &self,
        db: &Db,
        dataset: ExportDataset,
        format: ExportFormat,
        weeks: u32,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let QueryResult::Streams(rows) = self.query_stream_list(db)? else {
            return Err(EngineError::Invalid("stream list shape".into()));
        };
        let names: BTreeMap<EntityRef, String> = rows.into_iter().map(|r| (r.id, r.name)).collect();
        let table = match dataset {
            ExportDataset::Trends => trends_table(&self.trends(db, weeks, now_ms)?, &names),
            ExportDataset::Activity => {
                let grid = self.week_grid(db, now_ms, weeks)?;
                let start = grid.starts().first().copied().unwrap_or(0);
                let ops = self.task_history_ops(db, start)?;
                let mut events = fold_activity(&ops);
                events.extend(self.focus_activity(db, ReviewWindow::new(start, grid.end_ms()))?);
                events.retain(|e| e.at_ms >= start);
                events.sort_by_key(|e| (e.at_ms, e.op_id));
                activity_table(&events)
            }
            ExportDataset::Focus => {
                let grid = self.week_grid(db, now_ms, weeks)?;
                let QueryResult::FocusStats(stats) =
                    self.query_focus_stats(db, None, grid.starts().first().copied(), now_ms)?
                else {
                    return Err(EngineError::Invalid("focus stats shape".into()));
                };
                focus_table(&stats, &names)
            }
            ExportDataset::Streaks => {
                let rows: Vec<StreakRow> = read_routines(db.conn())?
                    .into_iter()
                    .filter(|r| !r.deleted && !r.archived)
                    .map(|r| self.streak_now(r, now_ms))
                    .map(|r| StreakRow {
                        routine: r.id,
                        title: r.template.title,
                        streak: r.streak_counter,
                        last_completed_at_ms: r
                            .last_completed_at
                            .and_then(|t| t.resolve_in(&jiff::tz::TimeZone::UTC))
                            .and_then(|t| u64::try_from(t.as_millisecond()).ok()),
                        unknown: sunrise_domain::Unknowns::new(),
                    })
                    .collect();
                streaks_table(&rows)
            }
        };
        Ok(QueryResult::Export(table.render(format)))
    }
}

// ---- table operations ----

/// One op-log row as the history readers see it.
#[derive(Debug)]
struct OpRow {
    op_id: [u8; 16],
    ts_ms: u64,
    device: [u8; 16],
    envelope: Vec<u8>,
}

/// `rusqlite` row mapper for [`OpRow`].
fn op_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OpRow> {
    Ok(OpRow {
        op_id: blob16(&r.get::<_, Vec<u8>>(0)?),
        ts_ms: u64::try_from(r.get::<_, i64>(1)?.max(0)).unwrap_or(0),
        device: blob16(&r.get::<_, Vec<u8>>(2)?),
        envelope: r.get::<_, Vec<u8>>(3)?,
    })
}

/// Project an [`InnerOp`] onto the timeline's payload vocabulary.
///
/// Everything the spec's exclusion list names — routine bookkeeping, context
/// edits, interruption logs, snapshot ops — lands on `Ignored`, so the filter
/// lives in one exhaustive `match` that a new op variant cannot slip past.
fn op_payload(inner: InnerOp) -> OpPayload {
    match inner {
        InnerOp::TaskCreate(t) => OpPayload::TaskCreated(Box::new(t)),
        InnerOp::TaskUpdate(t) => OpPayload::TaskUpdated(Box::new(t)),
        InnerOp::TaskDelete(_) => OpPayload::TaskDeleted,
        InnerOp::StreamCreate(s) => OpPayload::StreamCreated(Box::new(s)),
        InnerOp::StreamDelete(_) => OpPayload::StreamDeleted,
        InnerOp::FocusStart(f) => OpPayload::FocusStarted(f),
        InnerOp::FocusEnd(f) => OpPayload::FocusEnded(f),
        InnerOp::StreamUpdate(_)
        | InnerOp::ContextCreate(_)
        | InnerOp::ContextUpdate(_)
        | InnerOp::ContextDelete(_)
        | InnerOp::RoutineCreate(_)
        | InnerOp::RoutineUpdate(_)
        | InnerOp::RoutineDelete(_)
        // Calendar bookkeeping. A Block records WHEN work is planned, never
        // that any happened, so it does not belong in a Task's or a Stream's
        // activity feed.
        | InnerOp::BlockCreate(_)
        | InnerOp::BlockUpdate(_)
        | InnerOp::BlockDelete(_)
        // Attachment metadata. The timeline is about what happened to the
        // work, and `docs/08-features/reviews-and-stats.md` keeps bookkeeping
        // ops out of it.
        | InnerOp::AttachmentCreate(_)
        | InnerOp::AttachmentDelete(_)
        | InnerOp::FocusInterrupt(_)
        // Control ops carry key material and trust, not work. A rotation is
        // not something that happened to a Task.
        | InnerOp::KeyEnvelope(_)
        | InnerOp::DeviceRevoke(_)
        | InnerOp::DeviceCertPublish(_)
        | InnerOp::IdentityTransition(_)
        | InnerOp::VaultRequires(_)
        | InnerOp::DeviceFeatures(_)
        // A digest says what a replica holds, not what anyone did.
        | InnerOp::StreamDigest(_)
        // A field-level write of a task is replayed onto the task before
        // it by the history reader (`replay_task_patch`), which never hands
        // one here; one of any other entity is bookkeeping, as the full-state
        // ops of those entities are.
        | InnerOp::Patch(_)
        | InnerOp::ReviewSnapshotCreate(_) => OpPayload::Ignored,
    }
}

/// What a task `Patch` stamped at `at_ms` reads as in the history: its
/// register, set and counter writes replayed onto `base`, the task as the
/// record before it left it, as a whole task (ADR-0065 PR 7).
///
/// A create starts from the defaults a `Patch` create projects with when
/// there is no earlier record. `deleted` set to `true` reads as
/// `TaskDeleted`, a create as `TaskCreated`, anything else as
/// `TaskUpdated`. A `Patch` this build cannot read, or whose result is not
/// a whole task, reads as `Ignored`, as it did before.
pub(super) fn replay_task_patch(
    base: Option<Task>,
    p: &PatchPayload,
    at_ms: u64,
) -> (OpPayload, Option<Task>) {
    use ciborium::value::Value;
    let Ok(ops) = check_patch(p) else {
        return (OpPayload::Ignored, None);
    };
    let stamp = Value::serialized(&ms_to_ts(i64::try_from(at_ms).unwrap_or(i64::MAX)))
        .unwrap_or(Value::Null);
    let mut entries: Vec<(Value, Value)> = match base.as_ref().map(Value::serialized) {
        Some(Ok(Value::Map(m))) => m,
        None if p.create => {
            let text = |s: &str| Value::Text(s.to_owned());
            vec![
                (
                    text("id"),
                    Value::serialized(&p.target).unwrap_or(Value::Null),
                ),
                (text("created_at"), stamp.clone()),
                (text("title"), text("")),
                (text("state"), text("todo")),
                (
                    text("stream_id"),
                    Value::serialized(&inbox_stream_ref()).unwrap_or(Value::Null),
                ),
            ]
        }
        _ => return (OpPayload::Ignored, None),
    };
    let deletes = replay_field_ops(EntityKind::Task, &mut entries, ops);
    set_field(EntityKind::Task, &mut entries, "updated_at", stamp);
    let Ok(task) = Value::Map(entries).deserialized::<Task>() else {
        return (OpPayload::Ignored, None);
    };
    let payload = if p.create {
        OpPayload::TaskCreated(Box::new(task.clone()))
    } else if deletes {
        OpPayload::TaskDeleted
    } else {
        OpPayload::TaskUpdated(Box::new(task.clone()))
    };
    (payload, Some(task))
}

/// What a stream `Patch` stamped at `at_ms` reads as in the history, as its
/// full-state op would: a create is `StreamCreated` with the stream it
/// writes, a `deleted` set to `true` is `StreamDeleted`, and anything else is
/// bookkeeping, as a `StreamUpdate` is.
fn stream_patch_payload(p: &PatchPayload, at_ms: u64) -> OpPayload {
    use ciborium::value::Value;
    let Ok(ops) = check_patch(p) else {
        return OpPayload::Ignored;
    };
    let stamp = Value::serialized(&ms_to_ts(i64::try_from(at_ms).unwrap_or(i64::MAX)))
        .unwrap_or(Value::Null);
    let mut entries = vec![
        (
            Value::Text("id".into()),
            Value::serialized(&p.target).unwrap_or(Value::Null),
        ),
        (Value::Text("created_at".into()), stamp.clone()),
        (Value::Text("updated_at".into()), stamp),
    ];
    let deletes = replay_field_ops(EntityKind::Stream, &mut entries, ops);
    if p.create {
        return Value::Map(entries)
            .deserialized::<sunrise_domain::Stream>()
            .map_or(OpPayload::Ignored, |s| {
                OpPayload::StreamCreated(Box::new(s))
            });
    }
    if deletes {
        OpPayload::StreamDeleted
    } else {
        OpPayload::Ignored
    }
}

/// Apply a `Patch`'s field ops to `entries`, a whole entity of `kind` as
/// CBOR: a register write, an OR-set edit and a counter delta each as the
/// fold reads it for one writer. Returns whether it sets `deleted` to `true`.
fn replay_field_ops(
    kind: EntityKind,
    entries: &mut Vec<(ciborium::value::Value, ciborium::value::Value)>,
    ops: Vec<(String, FieldOp)>,
) -> bool {
    use ciborium::value::Value;
    let mut deletes = false;
    for (name, op) in ops {
        match op {
            FieldOp::Set(v) => {
                if name == "deleted" {
                    deletes = v == Value::Bool(true);
                }
                set_field(kind, entries, &name, v);
            }
            FieldOp::Edit { add, remove } => {
                let mut held: Vec<Value> = entries
                    .iter()
                    .find(|(k, _)| k.as_text() == Some(name.as_str()))
                    .and_then(|(_, v)| v.as_array().cloned())
                    .unwrap_or_default();
                held.retain(|e| !remove.iter().any(|(r, _)| r == e));
                for e in add {
                    if !held.contains(&e) {
                        held.push(e);
                    }
                }
                set_field(kind, entries, &name, Value::Array(held));
            }
            FieldOp::Inc(n) => {
                let held = entries
                    .iter()
                    .find(|(k, _)| k.as_text() == Some(name.as_str()))
                    .and_then(|(_, v)| v.as_integer())
                    .and_then(|i| i64::try_from(i).ok())
                    .unwrap_or(0);
                let v = Value::Integer(held.saturating_add(n).into());
                set_field(kind, entries, &name, v);
            }
            // Neither a task nor a stream has a map field.
            FieldOp::Map(_) => {}
        }
    }
    deletes
}

/// The routine-tuning window: the `weeks` civil weeks ending with the reviewed
/// one. Approximated in whole weeks from the review window, which is exact
/// because a review window *is* one grid week.
fn drift_window(
    window: ReviewWindow,
    weeks: u32,
) -> Result<(jiff::Timestamp, jiff::Timestamp), EngineError> {
    let span = window.end_ms.saturating_sub(window.start_ms);
    let start = window
        .start_ms
        .saturating_sub(span.saturating_mul(u64::from(weeks.saturating_sub(1))));
    let to_ts = |ms: u64| {
        i64::try_from(ms)
            .ok()
            .and_then(|v| jiff::Timestamp::from_millisecond(v).ok())
            .ok_or_else(|| EngineError::Invalid(format!("timestamp out of range: {ms}")))
    };
    Ok((to_ts(start)?, to_ts(window.end_ms)?))
}

/// Every live Task, for the review's per-Stream lists.
pub(super) fn read_live_tasks(conn: &rusqlite::Connection) -> Result<Vec<Task>, EngineError> {
    let mut stmt = conn.prepare("SELECT id FROM tasks WHERE deleted = 0 ORDER BY id ASC")?;
    let ids = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for raw in ids {
        if let Some(t) = read_task(conn, &blob16(&raw))? {
            out.push(t);
        }
    }
    Ok(out)
}

/// Write the snapshot record. Idempotent on the snapshot id, which is what
/// makes re-delivery a no-op and lets two devices' reviews of one week coexist.
pub(super) fn insert_review_snapshot_row(
    tx: &Transaction<'_>,
    s: &ReviewSnapshot,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let mut body = Vec::new();
    ciborium::ser::into_writer(s, &mut body)
        .map_err(|_| rusqlite::Error::ExecuteReturnedResults)?;
    tx.execute(
        "INSERT OR IGNORE INTO review_snapshots
         (id, created_at_ms, window_start_ms, window_end_ms,
          completed, deferred, dropped, created, reopened, body,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &s.id.bytes()[..],
            s.created_at.as_millisecond(),
            s.window_start_ms() as i64,
            s.window_end_ms() as i64,
            i64::from(s.totals.completed),
            i64::from(s.totals.deferred),
            i64::from(s.totals.dropped),
            i64::from(s.totals.created),
            i64::from(s.totals.reopened),
            body,
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}
