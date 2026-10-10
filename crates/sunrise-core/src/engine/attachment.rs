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

use super::ids::{
    blob16, blob32, decode_unknowns, encode_unknowns, extra_over_opaque, ms_to_ts, require_kind,
    ExtraTable,
};
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

/// The feature an attachment's thumbnail and dimensions are (ADR-0053 §2).
pub(crate) const ATTACHMENT_THUMBNAIL_FEATURE: &str = "attachment.thumbnail";

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
        // The thumbnail and the dimensions are the `attachment.thumbnail`
        // feature, which the vault must require before the first op that
        // carries them. While another device has not said it supports the
        // feature, the attachment is written without them: the file is what
        // the user asked to attach, the thumbnail is a convenience, and an
        // older device that cannot represent the fields must not be locked out
        // of writing attachments by one it never asked for. Write-once, so the
        // attachment then has no thumbnail for good (ADR-0053 §2).
        let wants_preview = d.thumbnail.is_some() || d.width.is_some() || d.height.is_some();
        let keep_preview = if wants_preview {
            match self.require_features(db, &[ATTACHMENT_THUMBNAIL_FEATURE], false) {
                Ok(_) => true,
                Err(EngineError::FeatureUnsupportedByDevices { devices, .. }) => {
                    tracing::info!(
                        ev = "core.attachment.thumbnail_dropped",
                        n_devices = devices.len() as u64,
                        "an attachment was written without its thumbnail, because a paired \
                         device does not support attachment.thumbnail yet"
                    );
                    false
                }
                Err(e) => return Err(e),
            }
        } else {
            false
        };
        let mut att = Attachment {
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
            ciphertext_hash: d.ciphertext_hash,
            width: None,
            height: None,
            thumbnail_blob_id: None,
            thumbnail_blob_key: None,
            thumbnail_mime: None,
            thumbnail_size_bytes: None,
            thumbnail_content_hash: None,
            thumbnail_ciphertext_hash: None,
            deleted: false,
            unknown: Unknowns::new(),
        };
        if keep_preview {
            att.set_preview(d.width, d.height, d.thumbnail.as_ref());
        }
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
    let extra_blob = extra_over_opaque(
        tx,
        ExtraTable::Attachments,
        &a.id.bytes()[..],
        encode_unknowns(&a.unknown)?,
    )?;
    tx.execute(
        "INSERT INTO attachments
         (id, parent_kind, parent_id, filename, mime_type, size_bytes,
          blob_key, blob_id, chunk_count, content_hash, ciphertext_hash,
          deleted, extra,
          created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device,
          width, height, thumbnail_blob_id, thumbnail_blob_key, thumbnail_mime,
          thumbnail_size_bytes, thumbnail_content_hash, thumbnail_ciphertext_hash)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                 ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            width = excluded.width,
            height = excluded.height,
            thumbnail_blob_id = excluded.thumbnail_blob_id,
            thumbnail_blob_key = excluded.thumbnail_blob_key,
            thumbnail_mime = excluded.thumbnail_mime,
            thumbnail_size_bytes = excluded.thumbnail_size_bytes,
            thumbnail_content_hash = excluded.thumbnail_content_hash,
            thumbnail_ciphertext_hash = excluded.thumbnail_ciphertext_hash,
            filename = excluded.filename,
            mime_type = excluded.mime_type,
            size_bytes = excluded.size_bytes,
            blob_key = excluded.blob_key,
            blob_id = excluded.blob_id,
            chunk_count = excluded.chunk_count,
            content_hash = excluded.content_hash,
            ciphertext_hash = excluded.ciphertext_hash,
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
            &a.ciphertext_hash[..],
            a.deleted as i64,
            extra_blob,
            a.created_at.as_millisecond(),
            a.updated_at.as_millisecond(),
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
            a.width,
            a.height,
            a.thumbnail_blob_id.as_ref().map(|b| &b[..]),
            a.thumbnail_blob_key.as_ref().map(|b| &b[..]),
            a.thumbnail_mime,
            a.thumbnail_size_bytes,
            a.thumbnail_content_hash.as_ref().map(|b| &b[..]),
            a.thumbnail_ciphertext_hash.as_ref().map(|b| &b[..]),
        ],
    )?;
    Ok(())
}

/// The eight `attachment.thumbnail` columns of one row, as stored.
type PreviewColumns = (
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<String>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
);

/// Fill `a`'s `attachment.thumbnail` fields from their columns, and from its
/// unknowns for any column that is NULL.
///
/// The second source is a row a build without these columns projected from
/// an op that carried them: it kept them in `extra`, and migration 0038 could
/// not move them, because SQL cannot read CBOR. Taking them out of the
/// unknowns is also what stops them being written twice, once as a field and
/// once as an unknown key, when the row is next serialized.
fn read_preview(a: &mut Attachment, cols: PreviewColumns) {
    fn take<T: serde::de::DeserializeOwned>(a: &mut Attachment, key: &str) -> Option<T> {
        let v = a.unknown.remove(key)?;
        v.0.deserialized().ok()
    }
    fn take_bytes<const N: usize>(a: &mut Attachment, key: &str) -> Option<[u8; N]> {
        match a.unknown.remove(key)?.0 {
            ciborium::Value::Bytes(b) => b.try_into().ok(),
            _ => None,
        }
    }
    fn exact<const N: usize>(b: Option<Vec<u8>>) -> Option<[u8; N]> {
        b.and_then(|b| b.try_into().ok())
    }
    let (w, h, bid, bkey, mime, size, chash, xhash) = cols;
    let from_extra_w = take::<u32>(a, "width");
    let from_extra_h = take::<u32>(a, "height");
    let from_extra_bid = take_bytes::<16>(a, "thumbnail_blob_id");
    let from_extra_bkey = take_bytes::<32>(a, "thumbnail_blob_key");
    let from_extra_mime = take::<String>(a, "thumbnail_mime");
    let from_extra_size = take::<u32>(a, "thumbnail_size_bytes");
    let from_extra_chash = take_bytes::<32>(a, "thumbnail_content_hash");
    let from_extra_xhash = take_bytes::<32>(a, "thumbnail_ciphertext_hash");
    a.width = w.and_then(|v| u32::try_from(v).ok()).or(from_extra_w);
    a.height = h.and_then(|v| u32::try_from(v).ok()).or(from_extra_h);
    a.thumbnail_blob_id = exact(bid).or(from_extra_bid);
    a.thumbnail_blob_key = exact(bkey).or(from_extra_bkey);
    a.thumbnail_mime = mime.or(from_extra_mime);
    a.thumbnail_size_bytes = size.and_then(|v| u32::try_from(v).ok()).or(from_extra_size);
    a.thumbnail_content_hash = exact(chash).or(from_extra_chash);
    a.thumbnail_ciphertext_hash = exact(xhash).or(from_extra_xhash);
}

/// The `parent_kind` discriminator. The core validates Task-only parents on the
/// command path; a remote op from a build that widened it still stores its own
/// kind rather than being coerced to `task`.
fn attachment_parent_kind(parent: EntityRef) -> &'static str {
    match parent.kind() {
        EntityKind::Stream => "stream",
        EntityKind::Note => "note",
        _ => "task",
    }
}

pub(crate) fn read_attachment(
    conn: &rusqlite::Connection,
    id: &[u8; 16],
) -> Result<Option<Attachment>, EngineError> {
    let row = conn
        .query_row(
            "SELECT parent_kind, parent_id, filename, mime_type, size_bytes,
                    blob_key, blob_id, chunk_count, content_hash, ciphertext_hash,
                    deleted, extra,
                    created_at_ms, updated_at_ms,
                    width, height, thumbnail_blob_id, thumbnail_blob_key, thumbnail_mime,
                    thumbnail_size_bytes, thumbnail_content_hash, thumbnail_ciphertext_hash
             FROM attachments WHERE id = ?",
            params![&id[..]],
            |r| {
                let preview: PreviewColumns = (
                    r.get(14)?,
                    r.get(15)?,
                    r.get(16)?,
                    r.get(17)?,
                    r.get(18)?,
                    r.get(19)?,
                    r.get(20)?,
                    r.get(21)?,
                );
                Ok((
                    preview,
                    (
                        r.get::<_, String>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Vec<u8>>(5)?,
                        r.get::<_, Vec<u8>>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, Vec<u8>>(8)?,
                        r.get::<_, Vec<u8>>(9)?,
                        r.get::<_, i64>(10)?,
                        r.get::<_, Option<Vec<u8>>>(11)?,
                        r.get::<_, i64>(12)?,
                        r.get::<_, i64>(13)?,
                    ),
                ))
            },
        )
        .optional()?;
    let Some((preview, a)) = row else {
        return Ok(None);
    };
    let parent_kind = match a.0.as_str() {
        "stream" => EntityKind::Stream,
        "note" => EntityKind::Note,
        _ => EntityKind::Task,
    };
    let mut att = Attachment {
        id: EntityRef::new(EntityKind::Attachment, *id),
        created_at: ms_to_ts(a.12.max(0)),
        updated_at: ms_to_ts(a.13.max(0)),
        parent: EntityRef::new(parent_kind, blob16(&a.1)),
        filename: a.2,
        mime_type: a.3,
        size_bytes: u64::try_from(a.4.max(0)).unwrap_or(0),
        blob_key: blob32(&a.5),
        blob_id: blob16(&a.6),
        chunk_count: u32::try_from(a.7.max(0)).unwrap_or(0),
        content_hash: blob32(&a.8),
        // `blob32` left-pads, so the zero-width `X''` that 0022 backfilled onto
        // pre-existing rows reads back as the all-zero hash — which is exactly
        // `Attachment::is_fetchable`'s "this blob has no name on the relay".
        ciphertext_hash: blob32(&a.9),
        width: None,
        height: None,
        thumbnail_blob_id: None,
        thumbnail_blob_key: None,
        thumbnail_mime: None,
        thumbnail_size_bytes: None,
        thumbnail_content_hash: None,
        thumbnail_ciphertext_hash: None,
        deleted: a.10 != 0,
        unknown: decode_unknowns(a.11),
    };
    read_preview(&mut att, preview);
    Ok(Some(att))
}
