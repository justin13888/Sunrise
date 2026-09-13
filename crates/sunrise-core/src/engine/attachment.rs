//! Attachments (`docs/02-domain/attachments.md`).
//!
//! Attachment ops route under the parent Task's Stream, so an attachment travels
//! with the work it belongs to and inherits that Stream's key.
//!
//! Metadata is **write-once**: created, then only ever tombstoned. Every field
//! but `deleted` describes one specific run of ciphertext identified by
//! `content_hash`, so there is no update op — re-attaching an edited file is a
//! new attachment. Which is why this module has an upsert and a read and no
//! update-row at all.

use super::ids::{blob16, blob32, decode_unknowns, encode_unknowns, ms_to_ts, require_kind};
use super::lww::LwwStamp;
use super::task::read_task;
use super::{Engine, EngineError};
use crate::commands::CommandResult;
use crate::inner_op::{encode_inner_op, InnerOp};
use crate::queries::QueryResult;
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_domain::Unknowns;
use sunrise_domain::{inbox_stream_ref, Attachment, AttachmentDraft};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    pub(super) fn attach_file(
        &self,
        db: &mut Db,
        d: AttachmentDraft,
    ) -> Result<CommandResult, EngineError> {
        d.validate()?;
        let now_ms = self.clock.now_ms();
        // The parent has to exist locally: its Stream is the op's routing
        // stream, and an attachment on a task this replica has never seen is a
        // client bug rather than an out-of-order merge.
        let parent = read_task(db.conn(), d.parent.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("task {}", d.parent)))?;
        let att = Attachment {
            id: self.fresh_id(EntityKind::Attachment, now_ms),
            created_at: ms_to_ts(now_ms as i64),
            updated_at: ms_to_ts(now_ms as i64),
            parent: d.parent,
            filename: d.filename.trim().to_string(),
            mime_type: d.mime_type.trim().to_string(),
            size_bytes: d.size_bytes,
            blob_key: d.blob_key,
            blob_id: d.blob_id,
            chunk_count: d.chunk_count,
            content_hash: d.content_hash,
            deleted: false,
            unknown: Unknowns::new(),
        };
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::AttachmentCreate(Box::new(att.clone())))?;
        let seq = self.next_seq(db, parent.stream_id.bytes())?;
        let lww = self.lww_stamp(seq);
        let stream_bytes = *parent.stream_id.bytes();
        let id = att.id;
        db.with_tx(|tx| -> rusqlite::Result<()> {
            upsert_attachment_row(tx, &att, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner_op,
                "attachment.create",
                "attachment",
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

    /// Tombstone attachment metadata.
    ///
    /// This is step 1 of `docs/02-domain/attachments.md` §Deletion and all of
    /// it that belongs in the core: reclaiming the blob needs the device-cursor
    /// quorum and the 30-day grace period, which are the relay's job.
    pub(super) fn detach_file(
        &self,
        db: &mut Db,
        id: EntityRef,
    ) -> Result<CommandResult, EngineError> {
        require_kind(id, EntityKind::Attachment)?;
        let now_ms = self.clock.now_ms();
        let mut att = read_attachment(db.conn(), id.bytes())?
            .ok_or_else(|| EngineError::NotFound(format!("attachment {id}")))?;
        let stream = read_task(db.conn(), att.parent.bytes())?
            .map_or_else(inbox_stream_ref, |t| t.stream_id);
        // The op carries the attachment's whole state with `deleted` set, so a
        // delete that wins LWW replaces the row on every replica rather than
        // flipping one column on top of whatever that replica held.
        att.deleted = true;
        att.updated_at = ms_to_ts(now_ms as i64);
        let op_id = self.fresh_op_id(now_ms);
        let inner_op = encode_inner_op(&InnerOp::AttachmentDelete(Box::new(att.clone())))?;
        let seq = self.next_seq(db, stream.bytes())?;
        let lww = self.lww_stamp(seq);
        let stream_bytes = *stream.bytes();
        db.with_tx(|tx| -> rusqlite::Result<()> {
            upsert_attachment_row(tx, &att, &lww)?;
            self.ops_insert(
                tx,
                &op_id,
                &stream_bytes,
                seq,
                lww.hlc,
                &inner_op,
                "attachment.delete",
                "attachment",
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

    /// Live attachments on one Task, oldest first — the order they were added
    /// in, which is the order a preview pane shows them.
    pub(super) fn query_task_attachments(
        &self,
        db: &Db,
        task: EntityRef,
    ) -> Result<QueryResult, EngineError> {
        require_kind(task, EntityKind::Task)?;
        let mut stmt = db.conn().prepare(
            "SELECT id FROM attachments
             WHERE parent_kind = 'task' AND parent_id = ? AND deleted = 0
             ORDER BY created_at_ms ASC, id ASC",
        )?;
        let ids = stmt
            .query_map(params![&task.bytes()[..]], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for raw in ids {
            if let Some(a) = read_attachment(db.conn(), &blob16(&raw))? {
                out.push(a);
            }
        }
        Ok(QueryResult::Attachments(out))
    }
}

// ---- table operations ----

/// INSERT-or-REPLACE an attachment row, stamping the LWW columns.
pub(super) fn upsert_attachment_row(
    tx: &Transaction<'_>,
    a: &Attachment,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO attachments
         (id, parent_kind, parent_id, filename, mime_type, size_bytes,
          blob_key, blob_id, chunk_count, content_hash, deleted, extra,
          created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            filename = excluded.filename,
            mime_type = excluded.mime_type,
            size_bytes = excluded.size_bytes,
            blob_key = excluded.blob_key,
            blob_id = excluded.blob_id,
            chunk_count = excluded.chunk_count,
            content_hash = excluded.content_hash,
            deleted = excluded.deleted,
            extra = excluded.extra,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device",
        params![
            &a.id.bytes()[..],
            attachment_parent_kind(a.parent),
            &a.parent.bytes()[..],
            a.filename,
            a.mime_type,
            a.size_bytes as i64,
            &a.blob_key[..],
            &a.blob_id[..],
            a.chunk_count,
            &a.content_hash[..],
            a.deleted as i64,
            encode_unknowns(&a.unknown)?,
            a.created_at.as_millisecond(),
            a.updated_at.as_millisecond(),
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
        ],
    )?;
    Ok(())
}

/// The `parent_kind` discriminator. v1 validates Task-only parents on the
/// command path; a remote op from a build that widened it still stores its own
/// kind rather than being coerced to `task`.
fn attachment_parent_kind(parent: EntityRef) -> &'static str {
    match parent.kind() {
        EntityKind::Stream => "stream",
        EntityKind::Note => "note",
        _ => "task",
    }
}

pub(super) fn read_attachment(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Attachment>, EngineError> {
    let row = conn
        .query_row(
            "SELECT parent_kind, parent_id, filename, mime_type, size_bytes,
                    blob_key, blob_id, chunk_count, content_hash, deleted, extra,
                    created_at_ms, updated_at_ms
             FROM attachments WHERE id = ?",
            params![&id[..]],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Vec<u8>>(5)?,
                    r.get::<_, Vec<u8>>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, Vec<u8>>(8)?,
                    r.get::<_, i64>(9)?,
                    r.get::<_, Option<Vec<u8>>>(10)?,
                    r.get::<_, i64>(11)?,
                    r.get::<_, i64>(12)?,
                ))
            },
        )
        .optional()?;
    let Some(a) = row else {
        return Ok(None);
    };
    let parent_kind = match a.0.as_str() {
        "stream" => EntityKind::Stream,
        "note" => EntityKind::Note,
        _ => EntityKind::Task,
    };
    Ok(Some(Attachment {
        id: EntityRef::new(EntityKind::Attachment, *id),
        created_at: ms_to_ts(a.11.max(0)),
        updated_at: ms_to_ts(a.12.max(0)),
        parent: EntityRef::new(parent_kind, blob16(&a.1)),
        filename: a.2,
        mime_type: a.3,
        size_bytes: u64::try_from(a.4.max(0)).unwrap_or(0),
        blob_key: blob32(&a.5),
        blob_id: blob16(&a.6),
        chunk_count: u32::try_from(a.7.max(0)).unwrap_or(0),
        content_hash: blob32(&a.8),
        deleted: a.9 != 0,
        unknown: decode_unknowns(a.10),
    }))
}
