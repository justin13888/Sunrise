//! The `contexts` row, and the purge of a tombstoned Context from every
//! Task's membership rows: the Context projection's writers.
//!
//! The purge is the only projection write that reaches into another entity's
//! rows, because a Task's contexts live in a join table the Task does not own
//! (ADR-0044 §5). The other is the search text of the tasks carrying a
//! context, which holds its name ([`index_context`]).

use super::search::{index_context, reindex_task_row};
use crate::engine::ids::{blob16, encode_unknowns, extra_over_opaque, ExtraTable};
use crate::engine::lww::LwwStamp;
use rusqlite::{params, Transaction};
use sunrise_domain::Context;

pub(in crate::engine::merge) fn insert_context_row(
    tx: &Transaction<'_>,
    c: &Context,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra_blob = encode_unknowns(&c.unknown)?;
    tx.execute(
        "INSERT INTO contexts
         (id, name, description, archived, deleted, created_at_ms, updated_at_ms, extra,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            c.id.bytes().to_vec(),
            c.name,
            c.description,
            c.archived as i64,
            c.deleted as i64,
            c.created_at.as_millisecond(),
            c.updated_at.as_millisecond(),
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    index_context(tx, c)
}

pub(in crate::engine::merge) fn update_context_row(
    tx: &Transaction<'_>,
    c: &Context,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra_blob = extra_over_opaque(
        tx,
        ExtraTable::Contexts,
        &c.id.bytes()[..],
        encode_unknowns(&c.unknown)?,
    )?;
    tx.execute(
        "UPDATE contexts
         SET name = ?, description = ?, archived = ?, deleted = ?, updated_at_ms = ?,
             extra = ?,
             lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            c.name,
            c.description,
            c.archived as i64,
            c.deleted as i64,
            c.updated_at.as_millisecond(),
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            c.id.bytes().to_vec(),
        ],
    )?;
    index_context(tx, c)
}

/// Drop `ctx` from every Task carrying it and rewrite those Tasks' search
/// rows, so its name stops matching them. Returns the number of memberships
/// removed. Idempotent.
pub(in crate::engine::merge) fn purge_context_from_tasks(
    tx: &Transaction<'_>,
    ctx: &[u8; 16],
) -> rusqlite::Result<usize> {
    let task_ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare("SELECT task_id FROM task_contexts WHERE context_id = ?")?;
        let rows = stmt.query_map(params![&ctx[..]], |r| r.get::<_, Vec<u8>>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // Only the rows go: each task's `contexts` set keeps the context, so a
    // restore of it shows it again (ADR-0044 §5).
    tx.execute(
        "DELETE FROM task_contexts WHERE context_id = ?",
        params![&ctx[..]],
    )?;
    for tid in &task_ids {
        reindex_task_row(tx, &blob16(tid))?;
    }
    Ok(task_ids.len())
}
