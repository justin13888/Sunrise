//! The Context command path, Context reads, and reads of the `contexts` table.
//!
//! Context lifecycle ops route to the vault-meta log (same as Streams and
//! routines): a Context is not owned by any one Stream.
//!
//! Deleting one is the only entity delete that reaches into another entity's
//! rows, because a Task's contexts live in a join table the Task does not own.
//! The fold does that part (`merge::rows`), on this replica as on every other.

use super::ids::{decode_unknowns, ms_to_ts, require_kind};
use super::query::sort_listing;
use super::task::read_task;
use super::{Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::InnerOp;
use crate::queries::{ContextRow, QueryResult};
use rusqlite::{params, OptionalExtension};
use sunrise_domain::Unknowns;
use sunrise_domain::{Context, ContextDraft, ContextPatch};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    pub(super) fn create_context(
        &self,
        db: &mut Db,
        d: ContextDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let name = Context::validate_name(&d.name)?;
        let description = Context::validate_description(d.description.as_deref())?;
        // Names are the handle `@name` capture resolves against, so a second
        // live Context with the same name (case-insensitively) would make every
        // such mention permanently ambiguous. Reject it at the source.
        if let Some(clash) = find_context_by_name(db.conn(), &name, None)? {
            return Err(EngineError::Invalid(format!(
                "context name {name:?} is already used by {clash}"
            )));
        }
        let now_ms = self.clock.now_ms();
        let ctx_id = self.fresh_id(EntityKind::Context, now_ms);
        let ctx = Context {
            id: ctx_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            name,
            description,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        };

        let op = InnerOp::ContextCreate(ctx);
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);

        Ok(CommandResult::new(ctx_id, None, op_id, seq))
    }

    pub(super) fn update_context(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: ContextPatch,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Context)?;
        patch.validate()?;
        let now_ms = self.clock.now_ms();
        let mut ctx = read_context(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("context {id}")))?;
        if let Some(n) = patch.name {
            let name = Context::validate_name(&n)?;
            if let Some(clash) = find_context_by_name(db.conn(), &name, Some(id.bytes()))? {
                return Err(EngineError::Invalid(format!(
                    "context name {name:?} is already used by {clash}"
                )));
            }
            ctx.name = name;
        }
        if let Some(desc) = patch.description {
            ctx.description = Context::validate_description(desc.as_deref())?;
        }
        // Archiving deliberately leaves `task_contexts` alone: per the spec it
        // only hides the Context from pickers, it does not strip it from Tasks.
        if let Some(a) = patch.archived {
            ctx.archived = a;
        }
        ctx.updated_at = ms_to_ts(now_ms as i64);

        let op = InnerOp::ContextUpdate(ctx);
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Soft-delete a Context and strip it from every Task carrying it.
    ///
    /// Per `docs/02-domain/contexts-and-tags.md` the tombstone and the
    /// membership removal are one transaction: a crash between them would leave
    /// tasks pointing at a context no picker can show. The fold of the
    /// `ContextDelete` op does the purge, here and on every replica that
    /// applies it, so the two sides stay in step without shipping one op per
    /// affected Task.
    pub(super) fn delete_context(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Context)?;
        let now_ms = self.clock.now_ms();
        let mut ctx = read_context(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("context {id}")))?;
        ctx.deleted = true;
        ctx.updated_at = ms_to_ts(now_ms as i64);

        let op = InnerOp::ContextDelete(ctx);
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// Tasks carrying one Context in manual order: the placed ones by their
    /// key, then the rest unscheduled first and by when each lands in the
    /// reader's zone ([`sort_listing`]).
    ///
    /// Deleted tasks are excluded but *done* ones are not: a Context listing
    /// is "what is tagged this", and hiding completed work would make the
    /// count in the picker disagree with the list it opens.
    pub(super) fn query_context_tasks(
        &self,
        db: &Db,
        context: &EntityRef,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT t.id FROM tasks t
             JOIN task_contexts tc ON tc.task_id = t.id
             WHERE tc.context_id = ? AND t.deleted = 0",
        )?;
        let blob: Vec<u8> = context.bytes().to_vec();
        let ids = stmt
            .query_map(params![blob], |row| row.get::<_, Vec<u8>>(0))?
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
        self.apply_catchup(db.conn(), &mut tasks, self.clock.now_ms())?;
        sort_listing(&mut tasks, &self.device_zone());
        Ok(QueryResult::StreamTasks(tasks))
    }

    /// All live contexts with the number of live tasks carrying each.
    ///
    /// Archived contexts are still listed (with `archived = true`) — archiving
    /// hides a Context from pickers, and that is the caller's filter to apply;
    /// only the tombstone removes it from the list.
    pub(super) fn query_contexts(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT c.id, c.name, c.description, c.archived,
                    (SELECT COUNT(*) FROM task_contexts tc
                     JOIN tasks t ON t.id = tc.task_id
                     WHERE tc.context_id = c.id AND t.deleted = 0) AS task_count
             FROM contexts c
             WHERE c.deleted = 0
             ORDER BY c.name COLLATE NOCASE ASC, c.id ASC",
        )?;
        let mapped = stmt.query_map([], |row| {
            let id_blob: Vec<u8> = row.get(0)?;
            let mut a = [0u8; 16];
            let take = id_blob.len().min(16);
            a[..take].copy_from_slice(&id_blob[..take]);
            let archived: i64 = row.get(3)?;
            let count: i64 = row.get(4)?;
            Ok(ContextRow {
                id: EntityRef::new(EntityKind::Context, a),
                name: row.get(1)?,
                description: row.get(2)?,
                archived: archived != 0,
                task_count: u64::try_from(count).unwrap_or(0),
            })
        })?;
        let mut rows = Vec::new();
        for r in mapped {
            rows.push(r?);
        }
        Ok(QueryResult::Contexts(rows))
    }
}

// ---- table operations ----

pub(super) fn read_context(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Context>, EngineError> {
    let row = conn
        .query_row(
            "SELECT name, description, archived, deleted, created_at_ms, updated_at_ms, extra
             FROM contexts WHERE id = ?",
            params![&id[..]],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<Vec<u8>>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((name, description, archived, deleted, created_ms, updated_ms, extra)) = row else {
        return Ok(None);
    };
    Ok(Some(Context {
        id: EntityRef::new(EntityKind::Context, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        name,
        description,
        archived: archived != 0,
        deleted: deleted != 0,
        unknown: decode_unknowns(extra),
    }))
}

/// Find a live Context whose name matches `name` after
/// [`Context::normalize_name`], excluding `except`.
///
/// Comparison happens in Rust rather than via SQL `COLLATE NOCASE`, which only
/// case-folds ASCII — `@Ärger` and `@ärger` are one name to a user and must be
/// one name here too.
fn find_context_by_name(
    conn: &rusqlite::Connection,
    name: &str,
    except: Option<&[u8; 16]>,
) -> Result<Option<EntityRef>, EngineError> {
    let wanted = Context::normalize_name(name);
    let mut stmt = conn.prepare("SELECT id, name FROM contexts WHERE deleted = 0")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id_blob, existing) = row?;
        let mut a = [0u8; 16];
        let take = id_blob.len().min(16);
        a[..take].copy_from_slice(&id_blob[..take]);
        if except == Some(&a) {
            continue;
        }
        if Context::normalize_name(&existing) == wanted {
            return Ok(Some(EntityRef::new(EntityKind::Context, a)));
        }
    }
    Ok(None)
}
