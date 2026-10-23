//! The `contexts` row, and the purge of a tombstoned Context from every
//! Task's membership rows: the Context projection's writers.
//!
//! The purge is the only projection write that reaches into another entity's
//! rows, because a Task's contexts live in a join table the Task does not own
//! (ADR-0044 §5).

use crate::engine::ids::{blob16, encode_unknowns, extra_over_opaque, ExtraTable};
use crate::engine::lww::LwwStamp;
use crate::engine::merge::seed_tasks_before_purge;
use rusqlite::{params, Transaction};
use sunrise_domain::Context;
use sunrise_id::{EntityKind, EntityRef};

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
    Ok(())
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
    Ok(())
}

/// Drop `ctx` from every Task carrying it and refresh those Tasks' FTS rows.
/// Returns the number of memberships removed. Idempotent.
pub(in crate::engine::merge) fn purge_context_from_tasks(
    tx: &Transaction<'_>,
    ctx: &[u8; 16],
) -> rusqlite::Result<usize> {
    let task_ids: Vec<Vec<u8>> = {
        let mut stmt = tx.prepare("SELECT task_id FROM task_contexts WHERE context_id = ?")?;
        let rows = stmt.query_map(params![&ctx[..]], |r| r.get::<_, Vec<u8>>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    // The membership rows are what an unseeded task's field state is seeded
    // from. Fold them in while they still name the context, so a restore of
    // it can show it again (ADR-0044 §5).
    let tasks: Vec<[u8; 16]> = task_ids.iter().map(|raw| blob16(raw)).collect();
    seed_tasks_before_purge(tx, &tasks)?;
    tx.execute(
        "DELETE FROM task_contexts WHERE context_id = ?",
        params![&ctx[..]],
    )?;
    for tid in &task_ids {
        refresh_task_contexts_fts(tx, tid)?;
    }
    Ok(task_ids.len())
}

/// Rewrite a task's `search_idx.contexts` column from its surviving
/// `task_contexts` rows, so a purged context stops matching search.
fn refresh_task_contexts_fts(tx: &Transaction<'_>, task_id: &[u8]) -> rusqlite::Result<()> {
    let joined = {
        let mut stmt = tx.prepare(
            "SELECT context_id FROM task_contexts WHERE task_id = ? ORDER BY context_id",
        )?;
        let rows = stmt.query_map(params![task_id], |r| r.get::<_, Vec<u8>>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .map(|raw| {
                let mut a = [0u8; 16];
                let take = raw.len().min(16);
                a[..take].copy_from_slice(&raw[..take]);
                EntityRef::new(EntityKind::Context, a).to_string()
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    tx.execute(
        "UPDATE search_idx SET contexts = ? WHERE kind = 'task' AND id = ?",
        params![joined, task_id],
    )?;
    Ok(())
}
