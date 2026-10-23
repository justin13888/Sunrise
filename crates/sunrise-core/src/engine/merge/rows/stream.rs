//! The `streams` row: the Stream projection's writers. The placeholder row
//! an op naming an unknown stream hangs off is
//! [`crate::engine::stream::ensure_stream_row`]'s, which writes no entity.

use crate::engine::ids::{encode_unknowns, extra_over_opaque, ExtraTable};
use crate::engine::lww::LwwStamp;
use crate::engine::stream::cadence_str;
use rusqlite::{params, Transaction};
use sunrise_domain::Stream;

pub(in crate::engine::merge) fn insert_stream_row(
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
            cadence_str(&s.review_cadence),
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

pub(in crate::engine::merge) fn update_stream_row(
    tx: &Transaction<'_>,
    s: &Stream,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let id_blob: Vec<u8> = s.id.bytes().to_vec();
    let parent_blob: Option<Vec<u8>> = s.parent_id.map(|p| p.bytes().to_vec());
    let extra_blob = extra_over_opaque(
        tx,
        ExtraTable::Streams,
        &id_blob,
        encode_unknowns(&s.unknown)?,
    )?;
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
            cadence_str(&s.review_cadence),
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
