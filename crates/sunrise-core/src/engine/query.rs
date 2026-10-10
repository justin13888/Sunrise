//! Read-only queries that belong to no single entity.
//!
//! `query_entity` and `query_search` fan out across every table by
//! construction — one resolves an `EntityRef` of any kind, the other searches
//! all of them — so in any entity module they would force it to import every
//! other. `query_today`, `query_actionable` and `query_unblock_cascade` are
//! cross-entity folds for the same reason: what is actionable depends on
//! streams, contexts, blockers and blocks at once.

use super::attachment::read_attachment;
use super::block::{block_row, read_block};
use super::context::read_context;
use super::ids::{ms_to_ts, require_kind};
use super::routine::read_routine;
use super::stream::read_stream;
use super::task::{actionable_scan, read_task, ref_of};
use super::{Engine, EngineError};
use crate::queries::{ActionableTask, DeviceRow, IdentityStatus, ParkedOpCount, QueryResult};
use rusqlite::params;
use std::collections::BTreeMap;
use sunrise_domain::time::{SunriseTime, PREFILTER_SLACK_MS};
use sunrise_domain::{effective_state, unblock_cascade, DependencyGraph};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::{Db, OpLog};

/// Where [`sort_by_landing`] puts a task with neither `scheduled_at` nor
/// `due_at`.
#[derive(Clone, Copy)]
pub(super) enum Unlanded {
    /// Before every task that has a time: a Stream or Context listing, which
    /// leads with what is still unplanned.
    First,
    /// After every task that has a time: Today, which leads with what is due.
    Last,
}

/// Order a task list by when each task lands (`scheduled_at`, else
/// `due_at`) as a reader in `tz` sees it, with [`SunriseTime::cmp_in`]
/// (`docs/10-cross-cutting/time.md` §2 rule 3), then by id.
///
/// The one ordering every user-visible task list uses. The storage index
/// cannot stand in for it: a floating or all-day value is indexed in UTC, so
/// `ORDER BY` on it puts a floating 09:00 and a New York 09:00 in an order
/// that is wrong for every reader west of Greenwich.
pub(super) fn sort_by_landing(
    tasks: &mut [sunrise_domain::Task],
    tz: &jiff::tz::TimeZone,
    unlanded: Unlanded,
) {
    let lands = |t: &sunrise_domain::Task| t.scheduled_at.clone().or_else(|| t.due_at.clone());
    tasks.sort_by(|a, b| {
        match (lands(a), lands(b)) {
            (Some(x), Some(y)) => x.cmp_in(&y, tz),
            (x, y) => match unlanded {
                Unlanded::First => y.is_none().cmp(&x.is_none()),
                Unlanded::Last => x.is_none().cmp(&y.is_none()),
            },
        }
        .then_with(|| a.id.cmp(&b.id))
    });
}

impl Engine {
    /// What completing `task` released — the mid-session unblock cascade.
    ///
    /// Recomputes the frontier from the dependency index against the blockers'
    /// *current* states, exactly like every other derived-`blocked` read, so it
    /// is right whether the completion happened locally or merged in.
    pub(super) fn query_unblock_cascade(
        &self,
        db: &Db,
        task: EntityRef,
    ) -> Result<QueryResult, EngineError> {
        require_kind(task, EntityKind::Task)?;
        // Dependents that are still open, with their *remaining* open-blocker
        // count. An unknown blocker counts as open, matching `query_actionable`.
        let mut stmt = db.conn().prepare(
            "SELECT d.id,
                    (SELECT COUNT(*) FROM task_blockers tb2
                       LEFT JOIN tasks b ON b.id = tb2.blocker_id
                      WHERE tb2.task_id = d.id
                        AND (b.id IS NULL
                             OR (b.deleted = 0
                                 AND b.state NOT IN ('done', 'cancelled')))) AS open_blockers
             FROM task_blockers tb
               JOIN tasks d ON d.id = tb.task_id
             WHERE tb.blocker_id = ?1
               AND d.deleted = 0 AND d.archived = 0
               AND d.state NOT IN ('done', 'cancelled')
             ORDER BY d.id ASC",
        )?;
        let rows = stmt
            .query_map(params![task.bytes().to_vec()], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut graph = DependencyGraph::new();
        let mut open_after: BTreeMap<EntityRef, u32> = BTreeMap::new();
        for (raw, open) in rows {
            let dep = ref_of(EntityKind::Task, &raw);
            graph.add_edge(dep, task);
            open_after.insert(dep, u32::try_from(open.max(0)).unwrap_or(u32::MAX));
        }
        Ok(QueryResult::UnblockCascade(Box::new(unblock_cascade(
            &graph,
            task,
            &open_after,
        ))))
    }

    /// Today: every open task scheduled or due on or before the reader's
    /// civil day (`docs/10-cross-cutting/time.md` §4).
    ///
    /// Today is the civil day containing `now_ms` in the device zone, not a
    /// rolling 24 hours, and a task belongs to it by the date the reader sees
    /// its time on ([`SunriseTime::day_in`]): an all-day value occupies its
    /// whole day, so a task due "today" is here until midnight. Overdue and
    /// slipped tasks stay, as before; `today_section` sorts them out.
    ///
    /// The SQL reads the storage index up to the end of the day plus
    /// [`PREFILTER_SLACK_MS`], and membership is then decided in Rust on the
    /// resolved value (§2 rule 2). Rows come back by their resolved time
    /// (`scheduled_at`, else `due_at`) with [`SunriseTime::cmp_in`], then id.
    /// With a day schedule (#338) the day's bounds become its planner day's.
    pub(super) fn query_today(
        &self,
        db: &Db,
        now_ms: u64,
        contexts: &[EntityRef],
    ) -> Result<QueryResult, EngineError> {
        let tz = self.device_zone();
        let today = ms_to_ts(i64::try_from(now_ms).unwrap_or(i64::MAX))
            .to_zoned(tz.clone())
            .date();
        let day_end_ms = today
            .tomorrow()
            .ok()
            .and_then(|d| {
                tz.to_zoned(d.to_datetime(jiff::civil::Time::midnight()))
                    .ok()
            })
            .map_or(i64::MAX, |z| z.timestamp().as_millisecond());
        let window_end = day_end_ms.saturating_add(PREFILTER_SLACK_MS);

        // `contexts` is an OR-set filter per docs/02-domain/contexts-and-tags.md:
        // an empty slice means "no filter", and a non-empty one keeps tasks
        // carrying at least one of the named contexts. The placeholders are
        // generated from the slice length and every value is still bound, so
        // no caller-controlled bytes reach the SQL text.
        let ctx_clause = if contexts.is_empty() {
            String::new()
        } else {
            let placeholders = std::iter::repeat_n("?", contexts.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                " AND EXISTS (SELECT 1 FROM task_contexts tc
                              WHERE tc.task_id = tasks.id
                                AND tc.context_id IN ({placeholders}))"
            )
        };
        let sql = format!(
            "SELECT id FROM tasks
             WHERE deleted = 0 AND archived = 0
               AND state != 'done' AND state != 'cancelled'
               AND ((scheduled_at_ms IS NOT NULL AND scheduled_at_ms <= ?)
                 OR (due_at_ms IS NOT NULL AND due_at_ms <= ?)){ctx_clause}"
        );
        let mut stmt = db.conn().prepare(&sql)?;

        let mut bound: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(window_end), Box::new(window_end)];
        for c in contexts {
            bound.push(Box::new(c.bytes().to_vec()));
        }
        let ids = stmt
            .query_map(rusqlite::params_from_iter(bound.iter()), |row| {
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
        let on_or_before_today =
            |v: Option<&SunriseTime>| v.and_then(|v| v.day_in(&tz)).is_some_and(|d| d <= today);
        tasks.retain(|t| {
            on_or_before_today(t.scheduled_at.as_ref()) || on_or_before_today(t.due_at.as_ref())
        });
        sort_by_landing(&mut tasks, &tz, Unlanded::Last);
        Ok(QueryResult::Tasks(tasks))
    }

    /// What moving the reader from `from` to `to` at `now_ms` changes, for
    /// [`crate::Core::on_time_zone_changed`] (`docs/10-cross-cutting/time.md`
    /// §7). Reads only; nothing stored is rewritten.
    ///
    /// Counts the open tasks whose `scheduled_at` or `due_at` resolves to a
    /// different instant in the two zones, or whose deadline is overdue in
    /// one and not the other, and reports whether the
    /// `notifications.timezone_changed.enabled` preference asks for a
    /// notification.
    pub(crate) fn time_zone_impact(
        &self,
        db: &Db,
        from: &jiff::tz::TimeZone,
        to: &jiff::tz::TimeZone,
        now_ms: u64,
    ) -> Result<(u32, bool), EngineError> {
        let moved =
            |v: Option<&SunriseTime>| v.is_some_and(|v| v.resolve_in(from) != v.resolve_in(to));
        let late_moved = |v: Option<&SunriseTime>| {
            v.is_some_and(|v| {
                sunrise_domain::is_overdue(v, now_ms, from)
                    != sunrise_domain::is_overdue(v, now_ms, to)
            })
        };
        let affected = super::review::read_live_tasks(db.conn())?
            .iter()
            .filter(|t| {
                !t.archived
                    && matches!(
                        t.state.effective(),
                        sunrise_domain::TaskState::Todo | sunrise_domain::TaskState::InProgress
                    )
            })
            .filter(|t| {
                moved(t.scheduled_at.as_ref())
                    || moved(t.due_at.as_ref())
                    || late_moved(t.due_at.as_ref())
            })
            .count();
        let notify = matches!(
            self.resolved_preference(db, "notifications.timezone_changed.enabled")?,
            Some(sunrise_domain::PrefValue::Bool(true))
        );
        Ok((u32::try_from(affected).unwrap_or(u32::MAX), notify))
    }

    /// Open tasks with their derived dependency state, ranked for a planner.
    ///
    /// Neither `blocked` nor `blocks_others` is stored on a Task — both are
    /// recomputed here against the blockers' *current* states, which is what
    /// makes "completing a blocker flips its dependents to actionable" true
    /// with no repair pass and no extra op, locally or after a merge.
    ///
    /// Ordering is the read path a Focus Planner wants: actionable first, then
    /// by how many open dependents finishing the task would release, then by id
    /// so the result is deterministic.
    pub(super) fn query_actionable(
        &self,
        db: &Db,
        stream: Option<EntityRef>,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
        if let Some(s) = stream {
            require_kind(s, EntityKind::Stream)?;
        }
        let rows = actionable_scan(db.conn(), stream, limit, false)?;
        let mut out = Vec::with_capacity(rows.len());
        for (bytes, open, unblocks) in rows {
            let Some(task) = read_task(db.conn(), &bytes)? else {
                continue;
            };
            out.push(ActionableTask {
                effective_state: effective_state(&task.state, open),
                open_blockers: open,
                unblocks,
                task,
            });
        }
        Ok(QueryResult::Actionable(out))
    }

    pub(super) fn query_entity(&self, db: &Db, r: EntityRef) -> Result<QueryResult, EngineError> {
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
            EntityKind::Context => {
                let c = read_context(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("context {r}")))?;
                Ok(QueryResult::Context(Box::new(c)))
            }
            EntityKind::Routine => {
                let rt = read_routine(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("routine {r}")))?;
                Ok(QueryResult::Routine(Box::new(rt)))
            }
            EntityKind::Block => {
                let b = read_block(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("block {r}")))?;
                Ok(QueryResult::Blocks(vec![block_row(db.conn(), b)?]))
            }
            // A single-row `Attachments`, not a variant of its own: the
            // attachment byte path reads one row by id before it can find the
            // blob, and inventing a second result shape for the same record
            // would make the two reads disagree the first time one gained a
            // field.
            EntityKind::Attachment => {
                let a = read_attachment(db.conn(), r.bytes())?
                    .ok_or_else(|| EngineError::NotFound(format!("attachment {r}")))?;
                Ok(QueryResult::Attachments(vec![a]))
            }
            _ => Err(EngineError::Invalid(format!(
                "EntityById not supported for kind {:?}",
                r.kind()
            ))),
        }
    }

    /// The count of parked ops by reason (ADR-0045 §4, "Visibility").
    pub(super) fn query_parked_ops_summary(db: &Db) -> Result<QueryResult, EngineError> {
        let counts = OpLog::parked_counts_by_reason(db)?
            .into_iter()
            .map(|c| ParkedOpCount {
                reason: c.reason,
                count: c.count,
            })
            .collect();
        Ok(QueryResult::ParkedOps(counts))
    }

    pub(super) fn query_device_list(&self, db: &Db) -> Result<QueryResult, EngineError> {
        // `current` is computed against the chain head rather than stored,
        // on the same rule every other membership test in this file follows:
        // it is a fact about the op set, so a row that cached it would go stale
        // the moment a transition landed.
        //
        // Both revocation tables are joined, because since ADR-0041 they can
        // disagree and the disagreement is the thing worth showing. `revoked`
        // is the derived register, which an unwind can take a row back out of;
        // `read_bounded` is the ratchet the four key-distribution sites read
        // (`0028_device_read_bounds.sql`), which nothing takes a row out of. A
        // device reading `revoked: false, read_bounded: true` is one the fold
        // stopped believing a revocation of while the keys stay cut off — an
        // ordinary outcome, not an exotic one, and invisible on a list that
        // showed only the register.
        let head = self.current_identity(db.conn())?;
        let mut stmt = db.conn().prepare(
            "SELECT d.device_id, d.nickname, d.platform, r.device_id IS NOT NULL,
                    b.device_id IS NOT NULL,
                    d.identity_id IS NOT NULL AND d.identity_id = ?1,
                    d.admitted_after_revocation
             FROM devices d
             LEFT JOIN device_revocations r ON r.device_id = d.device_id
             LEFT JOIN device_read_bounds b ON b.device_id = d.device_id",
        )?;
        let rows = stmt.query_map(params![&head.identity_id[..]], |row| {
            let blob: Vec<u8> = row.get(0)?;
            let mut a = [0u8; 16];
            let take = blob.len().min(16);
            a[..take].copy_from_slice(&blob[..take]);
            Ok(DeviceRow {
                device_id: a,
                nickname: row.get(1)?,
                platform: row.get(2)?,
                revoked: row.get(3)?,
                read_bounded: row.get(4)?,
                current: row.get(5)?,
                admitted_after_revocation: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(QueryResult::Devices(out))
    }

    /// The identity chain, folded fresh. Nothing here is cached: a status that
    /// could be stale is worse than no status, because the one moment a user
    /// looks at it is the moment something has just changed.
    pub(super) fn query_identity_status(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let chain = self.chain_identities(db.conn())?;
        let genesis = chain
            .first()
            .expect("chain_identities always returns at least the genesis link")
            .0;
        let current = self.current_identity(db.conn())?.identity_id;
        Ok(QueryResult::Identity(Box::new(IdentityStatus {
            genesis_identity_id: genesis,
            current_identity_id: current,
            transitions: chain.len() - 1,
            this_device_is_current: self.keychain.identity_id() == current,
            holds_recovery_key: self.keychain.holds_only_copy_of_identity_key(),
            can_sponsor: self.keychain.can_rotate_identity(),
        })))
    }

    pub(super) fn query_search(
        &self,
        db: &Db,
        text: &str,
        limit: u32,
    ) -> Result<QueryResult, EngineError> {
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
}

// ---- helpers ----

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
