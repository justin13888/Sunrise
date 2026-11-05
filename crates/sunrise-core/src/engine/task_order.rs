//! `Command::MoveTasks`: placing tasks in manual order (`Task.sort_order`,
//! `docs/02-domain/tasks.md`, issue #340).
//!
//! The core owns the key arithmetic. A client says which tasks land between
//! which two neighbours, and this computes keys strictly between the
//! neighbours' keys with [`sunrise_domain::sort_order::spread`], so a run of
//! rows keeps the order it is given and no sibling is rewritten.
//!
//! # What is written
//!
//! Each moved task gets one write of its new key: in a vault that requires
//! `core.field_ops` a `Patch` of `sort_order` alone (ADR-0065 §7), and in one
//! still in legacy mode a full-state op of the task as it projects (D9), like
//! every other task edit there. The key is one quiet register, so two devices
//! moving the same task concurrently converge on one key by stamp, and moves
//! of different tasks never touch each other.
//! A move of more than one task is one group (ADR-0065 §6), so a peer shows
//! the whole move or none of it.
//!
//! A move changes no time, state or blocker, so it clears none of the gates
//! an edit of those does (`plan_update`): a task that already breaks a
//! constraint can still be dragged.

use super::ids::require_kind;
use super::task::{read_task, TaskWrite};
use super::{Engine, EngineError};
use crate::commands::CommandResult;
use std::collections::BTreeSet;
use sunrise_domain::sort_order;
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

impl Engine {
    /// [`Command::MoveTasks`](crate::commands::Command::MoveTasks).
    pub(super) fn move_tasks(
        &self,
        db: &mut Db,
        ids: Vec<EntityRef>,
        after: Option<EntityRef>,
        before: Option<EntityRef>,
    ) -> Result<CommandResult, EngineError> {
        if ids.is_empty() {
            return Err(EngineError::Invalid(
                "a move needs at least one task".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for id in &ids {
            require_kind(*id, EntityKind::Task)?;
            if !seen.insert(*id) {
                return Err(EngineError::Invalid(format!(
                    "{id} appears twice in one move"
                )));
            }
        }
        for anchor in [after, before].into_iter().flatten() {
            require_kind(anchor, EntityKind::Task)?;
            if seen.contains(&anchor) {
                return Err(EngineError::Invalid(format!(
                    "{anchor} cannot be moved next to itself"
                )));
            }
        }
        let conn = db.conn();
        let lo = after.map(|a| anchor_key(conn, a)).transpose()?;
        let hi = before.map(|b| anchor_key(conn, b)).transpose()?;
        let keys = sort_order::spread(lo.as_deref(), hi.as_deref(), ids.len()).map_err(|_| {
            EngineError::Invalid(
                "the two anchors are not in increasing manual order; move the later one too".into(),
            )
        })?;
        let now_ms = self.clock.now_ms();
        let mut writes = Vec::with_capacity(ids.len());
        for (id, key) in ids.into_iter().zip(keys) {
            let mut task = live_task(conn, id)?;
            task.sort_order = Some(key);
            task.updated_at = super::ids::ms_to_ts(now_ms as i64);
            writes.push(TaskWrite::plain(task));
        }
        if let [write] = writes.as_slice() {
            let (op, stream) = write.op();
            let done = db.with_tx(|tx| self.append_and_fold(tx, &op, &stream, now_ms))?;
            let write = writes.pop().expect("one write");
            return Ok(write.result(done));
        }
        let members = writes.iter().map(TaskWrite::op).collect();
        let done = db.with_tx(|tx| self.append_and_fold_group(tx, members, now_ms))?;
        match (writes.into_iter().next(), done.first()) {
            (Some(write), Some(done)) => Ok(write.result(*done)),
            _ => Err(EngineError::Invalid("a move wrote nothing".into())),
        }
    }
}

/// A task this replica holds and has not deleted.
fn live_task(
    conn: &rusqlite::Connection,
    id: EntityRef,
) -> Result<sunrise_domain::Task, EngineError> {
    let task =
        read_task(conn, id.bytes())?.ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
    if task.deleted {
        return Err(EngineError::NotFound(format!("task {id} is deleted")));
    }
    Ok(task)
}

/// The key of an anchor, which must be placed: a list in manual order shows
/// its placed tasks first, so nothing can land beside an unplaced one without
/// placing it too.
fn anchor_key(conn: &rusqlite::Connection, id: EntityRef) -> Result<String, EngineError> {
    live_task(conn, id)?.sort_order.ok_or_else(|| {
        EngineError::Invalid(format!(
            "{id} has no place in manual order yet; move it with the others"
        ))
    })
}
