//! The `tasks` row, its `task_contexts` and `task_blockers` side tables, and
//! its full-text rows: the Task projection's writers.

use super::search::{index_task, unindex};
use crate::engine::ids::{
    encode_unknowns, energy_str, extra_over_opaque, task_state_str, time_to_parts, ExtraTable,
};
use crate::engine::lww::LwwStamp;
use crate::engine::task::encode_constraints;
use rusqlite::{params, Transaction};
use sunrise_domain::search::SearchKind;
use sunrise_domain::Task;
use sunrise_id::EntityRef;

pub(in crate::engine::merge) fn insert_task_row(
    tx: &Transaction<'_>,
    t: &Task,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let (sched_ms, sched_kind, sched_tz, sched_civil) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz, due_civil) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz, done_civil) = time_to_parts(t.completed_at.as_ref());
    let extra_blob = encode_unknowns(&t.unknown)?;

    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "INSERT INTO tasks
         (id, stream_id, title, state, priority, energy, estimated_min,
          scheduled_at_ms, scheduled_at_kind, scheduled_at_tz,
          due_at_ms, due_at_kind, due_at_tz,
          completed_at_ms, completed_at_kind, completed_at_tz, deferred_count,
          routine_id, routine_occurrence, reminder_lead_s, archived, deleted, body,
          scheduling_constraints, extra, head_root,
          created_at_ms, updated_at_ms, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device,
          scheduled_at_civil, due_at_civil, completed_at_civil, sort_order)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL,
                 ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            t.title,
            task_state_str(&t.state),
            t.priority.map(i64::from),
            t.energy.as_ref().map(energy_str),
            t.estimated_duration_s.and_then(|s| {
                let m = s / 60;
                i64::try_from(m).ok()
            }),
            sched_ms,
            sched_kind,
            sched_tz,
            due_ms,
            due_kind,
            due_tz,
            done_ms,
            done_kind,
            done_tz,
            t.deferred_count,
            t.routine_id.as_ref().map(|r| r.bytes().to_vec()),
            t.routine_occurrence.map(|d| d.as_millisecond()),
            t.reminder_lead_s,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            extra_blob,
            t.created_at.as_millisecond(),
            t.updated_at.as_millisecond(),
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            sched_civil,
            due_civil,
            done_civil,
            t.sort_order,
        ],
    )?;
    Ok(())
}

pub(in crate::engine::merge) fn update_task_row(
    tx: &Transaction<'_>,
    t: &Task,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let (sched_ms, sched_kind, sched_tz, sched_civil) = time_to_parts(t.scheduled_at.as_ref());
    let (due_ms, due_kind, due_tz, due_civil) = time_to_parts(t.due_at.as_ref());
    let (done_ms, done_kind, done_tz, done_civil) = time_to_parts(t.completed_at.as_ref());
    let id_blob: Vec<u8> = t.id.bytes().to_vec();
    let extra_blob = extra_over_opaque(
        tx,
        ExtraTable::Tasks,
        &id_blob,
        encode_unknowns(&t.unknown)?,
    )?;
    let stream_blob: Vec<u8> = t.stream_id.bytes().to_vec();
    let body_blob: Option<Vec<u8>> = t.body.as_ref().map(|b| b.0.clone());
    let constraints_blob = encode_constraints(&t.scheduling_constraints)?;
    tx.execute(
        "UPDATE tasks SET
            stream_id = ?, title = ?, state = ?, priority = ?,
            energy = ?, estimated_min = ?,
            scheduled_at_ms = ?, scheduled_at_kind = ?, scheduled_at_tz = ?,
            due_at_ms = ?, due_at_kind = ?, due_at_tz = ?,
            completed_at_ms = ?, completed_at_kind = ?, completed_at_tz = ?,
            scheduled_at_civil = ?, due_at_civil = ?, completed_at_civil = ?,
            deferred_count = ?, reminder_lead_s = ?, archived = ?, deleted = ?,
            body = ?, scheduling_constraints = ?, extra = ?, sort_order = ?,
            updated_at_ms = ?, lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            stream_blob,
            t.title,
            task_state_str(&t.state),
            t.priority.map(i64::from),
            t.energy.as_ref().map(energy_str),
            t.estimated_duration_s
                .and_then(|s| i64::try_from(s / 60).ok()),
            sched_ms,
            sched_kind,
            sched_tz,
            due_ms,
            due_kind,
            due_tz,
            done_ms,
            done_kind,
            done_tz,
            sched_civil,
            due_civil,
            done_civil,
            t.deferred_count,
            t.reminder_lead_s,
            t.archived as i64,
            t.deleted as i64,
            body_blob,
            constraints_blob,
            extra_blob,
            t.sort_order,
            t.updated_at.as_millisecond(),
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    Ok(())
}

pub(in crate::engine::merge) fn insert_task_contexts(
    tx: &Transaction<'_>,
    t: &Task,
) -> rusqlite::Result<()> {
    for c in &t.contexts {
        tx.execute(
            "INSERT OR IGNORE INTO task_contexts (task_id, context_id) VALUES (?, ?)",
            params![t.id.bytes().to_vec(), c.bytes().to_vec()],
        )?;
    }
    Ok(())
}

/// Replace a task's rows in the dependency index with its current
/// `blocked_by` set.
///
/// The index is what makes both derived directions cheap — `blocked` (forward,
/// covered by the primary key) and `blocks_others` (reverse, covered by
/// `task_blockers_by_blocker`). Edges naming a blocker this replica has not
/// materialized yet are written unchanged: see `0008_task_blockers.sql` for why
/// the table carries no foreign keys.
pub(in crate::engine::merge) fn replace_task_blockers(
    tx: &Transaction<'_>,
    t: &Task,
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM task_blockers WHERE task_id = ?",
        params![t.id.bytes().to_vec()],
    )?;
    for b in &t.blocked_by {
        tx.execute(
            "INSERT OR IGNORE INTO task_blockers (task_id, blocker_id) VALUES (?, ?)",
            params![t.id.bytes().to_vec(), b.bytes().to_vec()],
        )?;
    }
    Ok(())
}

pub(in crate::engine::merge) fn replace_task_contexts(
    tx: &Transaction<'_>,
    t: &Task,
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM task_contexts WHERE task_id = ?",
        params![t.id.bytes().to_vec()],
    )?;
    insert_task_contexts(tx, t)
}

/// Write a live task's search rows: its title, its contexts' names, and its
/// note as plain text ([`super::search::doc_of_task`]).
pub(in crate::engine::merge) fn ftsr_upsert_task(
    tx: &Transaction<'_>,
    t: &Task,
) -> rusqlite::Result<()> {
    index_task(tx, t)
}

/// Remove a tombstoned task's search rows.
pub(in crate::engine::merge) fn ftsr_delete_task(
    tx: &Transaction<'_>,
    id: &EntityRef,
) -> rusqlite::Result<()> {
    unindex(tx, SearchKind::Task, id.bytes())
}
