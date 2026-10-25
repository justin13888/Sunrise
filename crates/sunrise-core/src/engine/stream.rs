//! The Stream command path and Stream reads. The `streams` row is written by
//! the fold (`merge::rows`), apart from the placeholder below.
//!
//! Stream lifecycle ops route to the vault-meta log rather than to the Stream's
//! own id: the op that creates a Stream cannot be sequenced under a stream that
//! does not exist yet, and the op that deletes one has to outlive it.
//!
//! `ensure_stream_row` is reached from the focus records, which
//! file a session under a Stream and have to know the Stream row is there
//! first, which is why it is `pub(super)` here rather than duplicated. It
//! writes no entity: the placeholder it leaves carries no op's stamp.

use super::block::read_block;
use super::ids::{blob16, decode_unknowns, domain_bytes, ms_to_ts, require_writable_stream};
use super::query::sort_listing;
use super::routine::read_routine;
use super::snapshot::in_tx;
use super::task::{read_task, TaskWrite};
use super::{Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::InnerOp;
use crate::queries::{QueryResult, StreamRow};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_domain::sort_order;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    names_no_stream, resolve_parent_cycles, Disposition, RootedStream, Stream, StreamColor,
    StreamDraft, StreamFate, StreamPatch, StreamReviewCadence, SunriseTime,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    pub(super) fn create_stream(
        &self,
        db: &mut Db,
        d: StreamDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        if let Some(parent) = d.parent_id {
            require_writable_stream(parent)?;
        }
        let now_ms = self.clock.now_ms();
        let stream_id = self.fresh_id(EntityKind::Stream, now_ms);
        let last_key = last_stream_sort_order(db.conn())?;
        let stream = Stream {
            reminder_lead_s: d.reminder_lead_s,
            id: stream_id,
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            name: d.name.trim().to_string(),
            description: d.description.clone(),
            color: d.color.unwrap_or(StreamColor::Slate),
            icon: d.icon.clone(),
            parent_id: d.parent_id,
            // "New streams default between the last and 'end'"
            // (`docs/02-domain/streams.md` §Sort order): one digit's worth of
            // work, and no sibling is rewritten to make room.
            sort_order: sort_order::append_after(last_key.as_deref()),
            archived: false,
            paused: false,
            paused_until: None,
            review_cadence: d.review_cadence.unwrap_or(StreamReviewCadence::Weekly),
            default_context: d.default_context,
            rehomed_to: None,
            deleted: false,
            unknown: Unknowns::new(),
        };

        // Stream lifecycle ops route to the vault-meta log, not the new Stream.
        let op = InnerOp::StreamCreate(stream);
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);

        Ok(CommandResult::new(stream_id, None, op_id, seq))
    }

    pub(super) fn update_stream(
        &self,
        db: &mut Db,
        id: EntityRef,
        patch: StreamPatch,
    ) -> Result<CommandResult, EngineError> {
        require_writable_stream(id)?;
        if let Some(Some(parent)) = patch.parent_id {
            require_writable_stream(parent)?;
        }
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
        if let Some(lead) = patch.reminder_lead_s {
            stream.reminder_lead_s = lead;
        }
        if let Some(rc) = patch.review_cadence {
            stream.review_cadence = rc;
        }
        if let Some(icon) = patch.icon {
            stream.icon = icon;
        }
        if let Some(ctx) = patch.default_context {
            stream.default_context = ctx;
        }
        // A reorder. Validated by `patch.validate()` above, so a key that
        // could not have come from `sort_order::between` never reaches the
        // column. Note what this is NOT doing: no sibling is read, and none is
        // rewritten. That is the fractional index paying for itself, and it is
        // also why a reorder merges as one entity-level LWW write — see
        // `Stream::sort_order`.
        if let Some(k) = patch.sort_order {
            stream.sort_order = k;
        }
        if let Some(a) = patch.archived {
            stream.archived = a;
        }
        if let Some(p) = patch.paused {
            stream.paused = p;
        }
        if let Some(pu) = patch.paused_until {
            // TODO(#506): a non-instant expiry needs a `stream.civil_times`
            // feature, as the routine's does: a build before `DOC_SCHEMA_V` 15
            // reads only the bare instant this field held.
            if pu
                .as_ref()
                .is_some_and(|t| !matches!(t, SunriseTime::Instant { .. }))
            {
                return Err(EngineError::Invalid(
                    "stream.paused_until: only an instant can be written yet".into(),
                ));
            }
            stream.paused_until = pu;
        }
        stream.updated_at = ms_to_ts(now_ms as i64);

        let op = InnerOp::StreamUpdate(stream);
        let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &META_STREAM, now_ms))?;
        let (op_id, seq) = (done.op_id, done.seq);

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    /// [`Command::DeleteStream`](crate::commands::Command::DeleteStream):
    /// tombstone the stream and re-home everything it held, as one group
    /// (ADR-0046 §4).
    ///
    /// Every live Task and Block that names the stream is written to the
    /// disposition's target, each sealed under the target's key domain;
    /// every live routine's template is re-pointed, in the vault-meta
    /// domain; every live child Stream becomes top-level; and the tombstone
    /// records the target as `rehomed_to`, so a dependent another device
    /// writes into the stream later reads as re-homed too
    /// ([`sunrise_domain::effective_stream`]). Nothing is deleted with it.
    pub(super) fn delete_stream(
        &self,
        db: &mut Db,
        id: EntityRef,
        disposition: Disposition,
    ) -> Result<CommandResult, EngineError> {
        // The vault-meta stream and the private domain are refused here, as
        // everywhere a stream is named: neither is a Stream to delete.
        require_writable_stream(id)?;
        let now_ms = self.clock.now_ms();
        let now = ms_to_ts(now_ms as i64);
        let mut stream = read_stream(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("stream {id}")))?;
        let target = match disposition {
            Disposition::Detach => None,
            Disposition::MoveTo(to) => {
                require_writable_stream(to)?;
                if to == id {
                    return Err(EngineError::Invalid(format!(
                        "stream {id} cannot be re-homed into itself"
                    )));
                }
                let live = read_stream(db.conn(), to.bytes())?.is_some_and(|t| !t.deleted);
                if !live {
                    return Err(EngineError::Invalid(format!(
                        "stream {to} is not a live stream to move into"
                    )));
                }
                Some(to)
            }
        };
        let conn = db.conn();
        let mut members: Vec<(InnerOp, [u8; 16])> = Vec::new();
        let mut tasks = Vec::new();
        for raw in live_ids_in(conn, "tasks", "id", "stream_id", id.bytes())? {
            if let Some(mut task) = read_task(conn, &raw)? {
                task.stream_id = target;
                task.updated_at = now;
                tasks.push(TaskWrite::plain(task));
            }
        }
        for raw in live_ids_in(conn, "blocks", "id", "stream_id", id.bytes())? {
            if let Some(mut block) = read_block(conn, &raw)? {
                block.stream_id = target;
                block.updated_at = now;
                let domain = domain_bytes(target);
                members.push((InnerOp::BlockUpdate(Box::new(block)), domain));
            }
        }
        let mut routines = Vec::new();
        for raw in live_ids_in(conn, "routines", "id", "stream_id", id.bytes())? {
            if let Some(mut routine) = read_routine(conn, &raw)? {
                routine.template.stream_id = target;
                routine.updated_at = now;
                routines.push(routine);
            }
        }
        for routine in routines {
            let form = self.routine_op_form(db, &routine)?;
            members.push((InnerOp::RoutineUpdate(Box::new(form)), META_STREAM));
        }
        let conn = db.conn();
        for raw in live_ids_in(conn, "streams", "stream_id", "parent_id", id.bytes())? {
            if let Some(mut child) = read_stream(conn, &raw)? {
                child.parent_id = None;
                child.updated_at = now;
                members.push((InnerOp::StreamUpdate(child), META_STREAM));
            }
        }
        stream.deleted = true;
        stream.rehomed_to = target;
        stream.updated_at = now;
        let done = in_tx(db, |tx| {
            let mode = self.write_mode(tx, now_ms)?;
            let mut all: Vec<(InnerOp, [u8; 16])> = Vec::with_capacity(members.len() + 1);
            // The tombstone first, so the command's own result is its op.
            all.push((InnerOp::StreamDelete(stream.clone()), META_STREAM));
            for w in &tasks {
                all.extend(w.op_in(self, tx, mode)?);
            }
            all.extend(members.iter().cloned());
            Ok(self.append_and_fold_group(tx, all, now_ms)?)
        })?;
        let done = done
            .first()
            .copied()
            .unwrap_or(super::write::Appended::NOTHING);
        Ok(CommandResult::new(id, None, done.op_id, done.seq))
    }

    /// [`Query::Inbox`](crate::queries::Query::Inbox): the open, live tasks
    /// that are untriaged (ADR-0046 §3), in manual order, as a stream's list
    /// is. Untriaged is structural: no stream, no plan, no deadline, no
    /// `triaged_at` ([`sunrise_domain::Task::in_inbox`]).
    pub(super) fn query_inbox(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE stream_id IS NULL AND scheduled_at_ms IS NULL AND due_at_ms IS NULL
               AND triaged_at_ms IS NULL AND deleted = 0 AND archived = 0",
        )?;
        let ids = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tasks = Vec::with_capacity(ids.len());
        for raw in ids {
            if let Some(t) = read_task(db.conn(), &blob16(&raw))? {
                if t.in_inbox() {
                    tasks.push(t);
                }
            }
        }
        sort_listing(&mut tasks, &self.device_zone());
        Ok(QueryResult::StreamTasks(tasks))
    }

    /// Live tasks in one Stream in manual order: the placed ones by their
    /// key, then the rest unscheduled first and by when each lands in the
    /// reader's zone ([`sort_listing`]). The legacy Inbox reference names
    /// the tasks that belong to no stream, triaged or not: the no-stream set
    /// a manual order is kept within (#340).
    pub(super) fn query_stream_tasks(
        &self,
        db: &Db,
        stream: &EntityRef,
    ) -> Result<QueryResult, EngineError> {
        let mut stmt = db.conn().prepare(
            "SELECT id FROM tasks
             WHERE stream_id IS ? AND deleted = 0",
        )?;
        let stream_blob: Option<Vec<u8>> =
            (!names_no_stream(stream)).then(|| stream.bytes().to_vec());
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
        sort_listing(&mut tasks, &self.device_zone());
        Ok(QueryResult::StreamTasks(tasks))
    }

    /// [`Query::StreamList`](crate::queries::Query::StreamList): the live
    /// Streams. The Inbox is not one (ADR-0046 §3): it is a view, and its
    /// count is that view's ([`Self::query_inbox`]).
    pub(super) fn query_stream_list(&self, db: &Db) -> Result<QueryResult, EngineError> {
        let mut rows = Vec::new();

        // Live streams. The private domain has no row, but a vault folded by
        // a build before ADR-0046 may still hold the Inbox sentinel's
        // placeholder until its first rebuild, so it is left out by id.
        //
        // Ordered by the fractional index, which is the user's hand-made
        // order and the reason `sort_order` exists. Sorting the *strings* is
        // sorting the numbers they encode — see `sunrise_domain::sort_order`
        // — so this needs no decoding and no index beyond the column itself.
        //
        // Name and id are the tiebreak, not the ordering. They matter in two
        // cases: a row still holding the `''` sentinel because nothing has
        // ever ordered it, which sorts first exactly as an empty name did
        // before this column existed; and two rows that ended up on the same
        // key, which entity-level LWW permits — a device can only lose a
        // reorder wholesale, so it can lose it onto a key a sibling already
        // holds. Ties break the same way on every replica, which is the
        // property that matters.
        let mut stmt = db.conn().prepare(
            "SELECT s.stream_id, s.name, s.color, s.archived, s.paused, s.sort_order,
                    (SELECT COUNT(*) FROM tasks t
                     WHERE t.stream_id = s.stream_id AND t.deleted = 0
                       AND t.state NOT IN ('done', 'cancelled')) AS open_count
             FROM streams s
             WHERE s.deleted = 0 AND s.stream_id != ?
             ORDER BY s.sort_order ASC, s.name COLLATE NOCASE ASC, s.stream_id ASC",
        )?;
        let private = sunrise_domain::PRIVATE_DOMAIN_BYTES.to_vec();
        let mapped = stmt.query_map(params![private], |row| {
            let id_blob: Vec<u8> = row.get(0)?;
            let name: String = row.get(1)?;
            let color_str: String = row.get(2)?;
            let archived: i64 = row.get(3)?;
            let paused: i64 = row.get(4)?;
            let sort_order: String = row.get(5)?;
            let open_count: i64 = row.get(6)?;
            let mut a = [0u8; 16];
            let take = id_blob.len().min(16);
            a[..take].copy_from_slice(&id_blob[..take]);
            Ok(StreamRow {
                id: EntityRef::new(EntityKind::Stream, a),
                name,
                // Lossless: an unknown color stays itself (ADR-0045 §6).
                color: StreamColor::from_raw(&color_str),
                open_task_count: u64::try_from(open_count).unwrap_or(0),
                archived: archived != 0,
                paused: paused != 0,
                sort_order,
            })
        })?;
        for r in mapped {
            rows.push(r?);
        }
        Ok(QueryResult::Streams(rows))
    }
}

// ---- table operations ----

/// Materialize a placeholder `streams` row for `stream` if none exists yet.
///
/// The placeholder exists only so that an op naming a stream this device
/// never saw created has a row to hang off, and it must never overwrite a row
/// that already carries real data. Its times are the earliest `now_ms` any op
/// naming it was folded at, so a replica that received those ops in another
/// order, and a rebuild that refolds them in stamp order, hold the same row.
pub(super) fn ensure_stream_row(
    tx: &Transaction<'_>,
    stream: &EntityRef,
    now_ms: u64,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = stream.bytes().to_vec();
    let exists: i64 = tx.query_row(
        "SELECT count(*) FROM streams WHERE stream_id = ?",
        params![&id_blob],
        |r| r.get(0),
    )?;
    if exists > 0 {
        tx.execute(
            "UPDATE streams SET created_at_ms = MIN(created_at_ms, ?2),
                                updated_at_ms = MIN(updated_at_ms, ?2)
             WHERE stream_id = ?1 AND lww_device IS NULL",
            params![&id_blob, now_ms],
        )?;
        return Ok(());
    }
    tx.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq,
          parent_id, archived, deleted, created_at_ms, updated_at_ms)
         VALUES (?, ?, 0, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            vec![0u8; 32],
            None::<Vec<u8>>,
            0_i64,
            0_i64,
            now_ms,
            now_ms,
        ],
    )?;
    Ok(())
}

/// The largest `sort_order` among live streams, or `None` for an empty vault.
///
/// Restricted to keys this build can compute against — `A`..=`Z` and nothing
/// else. Two kinds of row would otherwise poison every subsequent create:
/// a placeholder from [`ensure_stream_row`], which holds the `''` sentinel,
/// and a key from a peer running a schema this build does not model. Neither
/// admits a key after it, so neither is allowed to be the anchor; the new
/// stream lands after the last key that *is* well-formed instead of failing.
fn last_stream_sort_order(conn: &rusqlite::Connection) -> Result<Option<String>, EngineError> {
    let key: Option<String> = conn.query_row(
        "SELECT MAX(sort_order) FROM streams
         WHERE deleted = 0
           AND sort_order GLOB '[A-Z]*'
           AND sort_order NOT GLOB '*[^A-Z]*'",
        [],
        |r| r.get(0),
    )?;
    Ok(key)
}

pub(super) fn read_stream(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Stream>, EngineError> {
    let id_blob: Vec<u8> = id.to_vec();
    let row = conn
        .query_row(
            "SELECT parent_id, archived, deleted, created_at_ms, updated_at_ms, name, color,
                    paused, paused_until_ms, review_cadence, icon, reminder_lead_s, sort_order,
                    extra, description, default_context,
                    paused_until_kind, paused_until_tz, paused_until_civil, rehomed_to
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
                    r.get::<_, i64>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, Option<u32>>(11)?,
                    r.get::<_, String>(12)?,
                    r.get::<_, Option<Vec<u8>>>(13)?,
                    r.get::<_, Option<Vec<u8>>>(14)?,
                    r.get::<_, Option<Vec<u8>>>(15)?,
                    (
                        r.get::<_, Option<String>>(16)?,
                        r.get::<_, Option<String>>(17)?,
                        r.get::<_, Option<String>>(18)?,
                    ),
                    r.get::<_, Option<Vec<u8>>>(19)?,
                ))
            },
        )
        .optional()?;
    let Some((
        parent_raw,
        archived,
        deleted,
        created_ms,
        updated_ms,
        name,
        color_str,
        paused,
        paused_until_ms,
        cadence_str,
        icon,
        reminder_lead_s,
        sort_order,
        extra,
        description_raw,
        default_ctx_raw,
        (pause_kind, pause_tz, pause_civil),
        rehomed_raw,
    )) = row
    else {
        return Ok(None);
    };
    let parent = parent_raw.map(|b| {
        let mut a = [0u8; 16];
        let take = b.len().min(16);
        a[..take].copy_from_slice(&b[..take]);
        EntityRef::new(EntityKind::Stream, a)
    });
    // A stream the cycle rule reads as a root has no parent here, whatever
    // its register holds (`rooted_streams`), and neither has one whose parent
    // is a tombstone: a deletion promotes its children to the top level
    // (ADR-0046 §4), including one nested under it on another device later.
    let parent = match parent {
        Some(_) if rooted_streams(conn)?.iter().any(|r| r.stream.bytes() == id) => None,
        Some(p)
            if matches!(
                stream_fate(conn, p.bytes())?,
                Some(StreamFate::Deleted { .. })
            ) =>
        {
            None
        }
        other => other,
    };
    let stream = Stream {
        reminder_lead_s,
        id: EntityRef::new(EntityKind::Stream, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        name,
        description: description_raw.map(sunrise_domain::NoteBody),
        // Lossless: an unknown color is kept verbatim and written back as it
        // arrived; it reads as Slate wherever it has to be drawn.
        color: StreamColor::from_raw(&color_str),
        icon,
        parent_id: parent,
        sort_order,
        archived: archived != 0,
        paused: paused != 0,
        paused_until: super::ids::time_from_parts(
            paused_until_ms,
            pause_kind.as_deref(),
            pause_tz.as_deref(),
            pause_civil.as_deref(),
        ),
        review_cadence: parse_cadence(&cadence_str),
        default_context: default_ctx_raw.map(|b| {
            let mut a = [0u8; 16];
            let take = b.len().min(16);
            a[..take].copy_from_slice(&b[..take]);
            EntityRef::new(EntityKind::Context, a)
        }),
        deleted: deleted != 0,
        rehomed_to: rehomed_raw.map(|b| EntityRef::new(EntityKind::Stream, blob16(&b))),
        unknown: decode_unknowns(extra),
    };
    Ok(Some(stream))
}

/// What this replica holds of stream `id`, for the re-homing rule
/// ([`sunrise_domain::effective_stream`]): `None` when it has no row,
/// [`StreamFate::Live`] for a live row (a placeholder included), and the
/// tombstone's `rehomed_to` otherwise.
pub(super) fn stream_fate(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> rusqlite::Result<Option<StreamFate>> {
    conn.query_row(
        "SELECT deleted, rehomed_to FROM streams WHERE stream_id = ?",
        params![&id[..]],
        |r| {
            let deleted: i64 = r.get(0)?;
            let rehomed: Option<Vec<u8>> = r.get(1)?;
            Ok(if deleted == 0 {
                StreamFate::Live
            } else {
                StreamFate::Deleted {
                    rehomed_to: rehomed.map(|b| EntityRef::new(EntityKind::Stream, blob16(&b))),
                }
            })
        },
    )
    .optional()
}

/// The live rows of `table` whose `column` names stream `stream`, by key.
fn live_ids_in(
    conn: &rusqlite::Connection,
    table: &str,
    key: &str,
    column: &str,
    stream: &[u8; 16],
) -> rusqlite::Result<Vec<[u8; 16]>> {
    let sql =
        format!("SELECT {key} FROM {table} WHERE {column} = ? AND deleted = 0 ORDER BY {key}");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![&stream[..]], |r| r.get::<_, Vec<u8>>(0))?;
    rows.map(|r| r.map(|raw| blob16(&raw))).collect()
}

/// The streams the `parent_id` cycle rule reads as roots (ADR-0065 §5), with
/// the parent each one's register names and the loop it closes.
///
/// Every stream's `parent_id` is its own register, so two devices that each
/// nest one stream under the other merge into a loop. In each loop the
/// stream whose `parent_id` write has the greatest stamp reads as a root
/// ([`sunrise_domain::resolve_parent_cycles`]). Applied when a stream is
/// read, never written, so every replica holding the same registers reads
/// the same forest. A forest, the usual case, needs no stamp lookup.
pub(super) fn rooted_streams(
    conn: &rusqlite::Connection,
) -> Result<Vec<RootedStream<Option<super::merge::Stamp>>>, EngineError> {
    let mut stmt =
        conn.prepare("SELECT stream_id, parent_id FROM streams WHERE parent_id IS NOT NULL")?;
    let edges: Vec<(EntityRef, EntityRef)> = stmt
        .query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(c, p)| {
            (
                EntityRef::new(EntityKind::Stream, blob16(&c)),
                EntityRef::new(EntityKind::Stream, blob16(&p)),
            )
        })
        .collect();
    let (_, unstamped) = resolve_parent_cycles(edges.iter().map(|(c, p)| (*c, *p, ())));
    if unstamped.is_empty() {
        return Ok(Vec::new());
    }
    let stamps: BTreeMap<[u8; 16], super::merge::Stamp> =
        super::merge::register_winners(conn, "parent_id")?
            .into_iter()
            .map(|(id, _, at)| (id, at))
            .collect();
    let (_, rooted) = resolve_parent_cycles(
        edges
            .into_iter()
            .map(|(c, p)| (c, p, stamps.get(c.bytes()).copied())),
    );
    Ok(rooted)
}

/// A Stream's review cadence in its storage string form (matches the serde
/// representation, so the column and the op agree).
pub(super) fn cadence_str(c: &StreamReviewCadence) -> &str {
    c.as_str()
}

/// Parse a stored cadence. Lossless (ADR-0045 §6): an unknown value is kept
/// and written back verbatim, and reads as `Weekly` wherever logic acts on it,
/// so a vault written by a newer binary still loads.
fn parse_cadence(s: &str) -> StreamReviewCadence {
    StreamReviewCadence::from_raw(s)
}

/// Streams currently paused — the review skips them
/// (`docs/08-features/reviews-and-stats.md` §Weekly review step 1).
pub(super) fn paused_streams(
    conn: &rusqlite::Connection,
) -> Result<BTreeSet<EntityRef>, EngineError> {
    let mut stmt = conn.prepare("SELECT stream_id FROM streams WHERE paused != 0")?;
    let out = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Stream, blob16(&raw)))
        .collect();
    Ok(out)
}

/// The Tasks currently filed under one Stream, for its activity feed.
pub(super) fn stream_member_tasks(
    conn: &rusqlite::Connection,
    stream: &[u8; 16],
) -> Result<Vec<EntityRef>, EngineError> {
    let mut stmt = conn.prepare("SELECT id FROM tasks WHERE stream_id = ?")?;
    let out = stmt
        .query_map(params![&stream[..]], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|raw| EntityRef::new(EntityKind::Task, blob16(&raw)))
        .collect();
    Ok(out)
}
