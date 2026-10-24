//! The `blocks` row and its `block_tasks` bindings: the Block projection's
//! writers. `Task.blocks` is read off the bindings, never written.

use super::search::index_block;
use crate::engine::ids::{encode_unknowns, extra_over_opaque, ExtraTable};
use crate::engine::lww::LwwStamp;
use rusqlite::{params, Transaction};
use sunrise_domain::Block;

/// INSERT-or-REPLACE a Block row, stamping the LWW columns.
pub(in crate::engine::merge) fn upsert_block_row(
    tx: &Transaction<'_>,
    b: &Block,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let (start_ms, start_kind, start_tz) = b.starts_at.to_parts();
    let (end_ms, end_kind, end_tz) = b.ends_at.to_parts();
    let extra_blob = extra_over_opaque(
        tx,
        ExtraTable::Blocks,
        &b.id.bytes()[..],
        encode_unknowns(&b.unknown)?,
    )?;
    tx.execute(
        "INSERT INTO blocks
         (id, stream_id, starts_at_ms, starts_at_kind, starts_at_tz,
          ends_at_ms, ends_at_kind, ends_at_tz, title, title_track_task,
          deleted, extra, created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device,
          starts_at_civil, ends_at_civil)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            stream_id = excluded.stream_id,
            starts_at_ms = excluded.starts_at_ms,
            starts_at_kind = excluded.starts_at_kind,
            starts_at_tz = excluded.starts_at_tz,
            starts_at_civil = excluded.starts_at_civil,
            ends_at_ms = excluded.ends_at_ms,
            ends_at_kind = excluded.ends_at_kind,
            ends_at_tz = excluded.ends_at_tz,
            ends_at_civil = excluded.ends_at_civil,
            title = excluded.title,
            title_track_task = excluded.title_track_task,
            deleted = excluded.deleted,
            extra = excluded.extra,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device",
        params![
            &b.id.bytes()[..],
            &b.stream_id.bytes()[..],
            start_ms,
            start_kind,
            start_tz,
            end_ms,
            end_kind,
            end_tz,
            b.title.as_deref(),
            b.title_track_task as i64,
            b.deleted as i64,
            extra_blob,
            b.created_at.as_millisecond(),
            b.updated_at.as_millisecond(),
            lww.hlc.physical_ms as i64,
            lww.hlc.logical,
            lww.seq as i64,
            &lww.device[..],
            b.starts_at.civil_part(),
            b.ends_at.civil_part(),
        ],
    )?;
    index_block(tx, b)
}

/// Replace a Block's bindings with the op's set. Full-state, like every other
/// entity op: the set on the winning op is the set.
pub(in crate::engine::merge) fn replace_block_tasks(
    tx: &Transaction<'_>,
    b: &Block,
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM block_tasks WHERE block_id = ?",
        params![&b.id.bytes()[..]],
    )?;
    for t in &b.tasks {
        tx.execute(
            "INSERT OR IGNORE INTO block_tasks (block_id, task_id) VALUES (?, ?)",
            params![&b.id.bytes()[..], &t.bytes()[..]],
        )?;
    }
    Ok(())
}
