//! The Stream command path, Stream reads, and the `streams` table.
//!
//! Stream lifecycle ops route to the vault-meta log rather than to the Stream's
//! own id: the op that creates a Stream cannot be sequenced under a stream that
//! does not exist yet, and the op that deletes one has to outlive it.
//!
//! `ensure_stream_row` is reached from six other modules — anything that
//! materializes a remote entity into a Stream has to know the Stream row is
//! there first — which is why it is `pub(super)` here rather than duplicated.

use super::ids::{blob16, decode_unknowns, encode_unknowns, ms_to_ts, require_kind};
use super::lww::LwwStamp;
use super::task::read_task;
use super::{Engine, EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::{encode_inner_op, InnerOp};
use crate::queries::{QueryResult, StreamRow};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::BTreeSet;
use sunrise_domain::sort_order;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    inbox_stream_ref, Stream, StreamColor, StreamDraft, StreamPatch, StreamReviewCadence,
    INBOX_STREAM_BYTES,
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
            deleted: false,
            unknown: Unknowns::new(),
        };

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamCreate(stream.clone()))?;
        // Stream lifecycle ops route to the vault-meta log, not the new Stream.
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            insert_stream_row(tx, &stream_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
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

        Ok(CommandResult::new(stream_id, None, op_id, seq))
    }

    pub(super) fn update_stream(
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
            stream.paused_until = pu;
        }
        stream.updated_at = ms_to_ts(now_ms as i64);

        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamUpdate(stream.clone()))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_stream_row(tx, &stream_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
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

        Ok(CommandResult::new(id, None, op_id, seq))
    }

    pub(super) fn delete_stream(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Stream)?;
        // Compared against the Inbox's own id, not against sixteen zero bytes:
        // those are the vault-meta stream now, and a guard that names the wrong
        // constant is a guard that protects the wrong thing.
        if id.bytes() == &INBOX_STREAM_BYTES || id.bytes() == &META_STREAM {
            return Err(EngineError::Invalid(
                "cannot delete the inbox or vault-meta stream".into(),
            ));
        }
        let now_ms = self.clock.now_ms();
        let mut stream = read_stream(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("stream {id}")))?;
        stream.deleted = true;
        stream.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::StreamDelete(stream.clone()))?;
        let seq = self.next_seq(db, &META_STREAM)?;
        let lww = self.lww_stamp(seq);
        let stream_clone = stream.clone();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            update_stream_row(tx, &stream_clone, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &META_STREAM,
                seq,
                lww.hlc,
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
        Ok(CommandResult::new(id, None, op_id, seq))
    }

    pub(super) fn query_stream_tasks(
        &self,
        db: &Db,
        stream: &EntityRef,
    ) -> Result<QueryResult, EngineError> {
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

    pub(super) fn query_stream_list(&self, db: &Db) -> Result<QueryResult, EngineError> {
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
            paused: false,
            // The Inbox is not a Stream entity and cannot be reordered: it is
            // pushed to the front of this list unconditionally, above. The
            // sentinel says "no position", which is the truth about it.
            sort_order: String::new(),
        });

        // Real streams (exclude deleted and the synthetic inbox row, which
        // `ensure_stream_row` may have materialized with an empty name).
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
                       AND t.state IN ('todo', 'in_progress')) AS open_count
             FROM streams s
             WHERE s.deleted = 0 AND s.stream_id != ?
             ORDER BY s.sort_order ASC, s.name COLLATE NOCASE ASC, s.stream_id ASC",
        )?;
        let mapped = stmt.query_map(params![inbox_blob], |row| {
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
                color: StreamColor::from_str_lossy(&color_str),
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
/// A no-op when the row is already there: the placeholder exists only so that
/// an op naming a stream this device never saw created has a row to hang off,
/// and it must never overwrite a row that already carries real data.
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

pub(super) fn insert_stream_row(
    tx: &Transaction<'_>,
    s: &Stream,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = s.id.bytes().to_vec();
    let parent_blob: Option<Vec<u8>> = s.parent_id.map(|p| p.bytes().to_vec());
    let extra_blob = encode_unknowns(&s.unknown)?;
    let description_blob: Option<Vec<u8>> = s.description.as_ref().map(|b| b.0.clone());
    let default_ctx_blob: Option<Vec<u8>> = s.default_context.map(|c| c.bytes().to_vec());
    tx.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq,
          parent_id, archived, deleted, created_at_ms, updated_at_ms, name, color, icon,
          paused, paused_until_ms, review_cadence, reminder_lead_s, sort_order, extra,
          description, default_context,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            vec![0u8; 32],
            parent_blob,
            s.archived as i64,
            s.deleted as i64,
            s.created_at.as_millisecond(),
            s.updated_at.as_millisecond(),
            s.name,
            s.color.as_str(),
            s.icon,
            s.paused as i64,
            s.paused_until.map(|t| t.as_millisecond()),
            cadence_str(s.review_cadence),
            s.reminder_lead_s,
            s.sort_order,
            extra_blob,
            description_blob,
            default_ctx_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

pub(super) fn update_stream_row(
    tx: &Transaction<'_>,
    s: &Stream,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = s.id.bytes().to_vec();
    let parent_blob: Option<Vec<u8>> = s.parent_id.map(|p| p.bytes().to_vec());
    let extra_blob = encode_unknowns(&s.unknown)?;
    let description_blob: Option<Vec<u8>> = s.description.as_ref().map(|b| b.0.clone());
    let default_ctx_blob: Option<Vec<u8>> = s.default_context.map(|c| c.bytes().to_vec());
    tx.execute(
        "UPDATE streams
         SET parent_id = ?, archived = ?, deleted = ?, updated_at_ms = ?,
             name = ?, color = ?, icon = ?, paused = ?, paused_until_ms = ?,
             review_cadence = ?, reminder_lead_s = ?, sort_order = ?, extra = ?,
             description = ?, default_context = ?,
             lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE stream_id = ?",
        params![
            parent_blob,
            s.archived as i64,
            s.deleted as i64,
            s.updated_at.as_millisecond(),
            s.name,
            s.color.as_str(),
            s.icon,
            s.paused as i64,
            s.paused_until.map(|t| t.as_millisecond()),
            cadence_str(s.review_cadence),
            s.reminder_lead_s,
            s.sort_order,
            extra_blob,
            description_blob,
            default_ctx_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    Ok(())
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
                    extra, description, default_context
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
    let stream = Stream {
        reminder_lead_s,
        id: EntityRef::new(EntityKind::Stream, *id),
        created_at: ms_to_ts(created_ms.max(0)),
        updated_at: ms_to_ts(updated_ms.max(0)),
        name,
        description: description_raw.map(sunrise_domain::NoteBody),
        // Unknown/forward-compatible color strings fall back to Slate.
        color: StreamColor::from_str_lossy(&color_str),
        icon,
        parent_id: parent,
        sort_order,
        archived: archived != 0,
        paused: paused != 0,
        paused_until: paused_until_ms.map(ms_to_ts),
        review_cadence: parse_cadence(&cadence_str),
        default_context: default_ctx_raw.map(|b| {
            let mut a = [0u8; 16];
            let take = b.len().min(16);
            a[..take].copy_from_slice(&b[..take]);
            EntityRef::new(EntityKind::Context, a)
        }),
        deleted: deleted != 0,
        unknown: decode_unknowns(extra),
    };
    Ok(Some(stream))
}

/// A Stream's review cadence in its storage string form (matches the serde
/// representation, so the column and the op agree).
fn cadence_str(c: StreamReviewCadence) -> &'static str {
    match c {
        StreamReviewCadence::Weekly => "weekly",
        StreamReviewCadence::Biweekly => "biweekly",
        StreamReviewCadence::Monthly => "monthly",
        StreamReviewCadence::None => "none",
    }
}

/// Parse a stored cadence. Unknown values fall back to `Weekly` so a vault
/// written by a newer binary still loads.
fn parse_cadence(s: &str) -> StreamReviewCadence {
    match s {
        "biweekly" => StreamReviewCadence::Biweekly,
        "monthly" => StreamReviewCadence::Monthly,
        "none" => StreamReviewCadence::None,
        _ => StreamReviewCadence::Weekly,
    }
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
