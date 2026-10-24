//! The `attachments` row: the Attachment projection's writer.

use super::search::index_attachment;
use crate::engine::attachment::attachment_parent_kind;
use crate::engine::ids::{encode_unknowns, extra_over_opaque, ExtraTable};
use crate::engine::lww::LwwStamp;
use rusqlite::{params, Transaction};
use sunrise_domain::Attachment;

/// INSERT-or-REPLACE an attachment row, stamping the LWW columns.
pub(in crate::engine::merge) fn upsert_attachment_row(
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
    index_attachment(tx, a)
}
