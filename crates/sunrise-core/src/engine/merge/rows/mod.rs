//! The entity rows' writers, private to the merge (ADR-0065 §5).
//!
//! A row is the projection of its entity's merge state, and
//! [`super::project::write_projection`] is the one thing that writes it. Each
//! writer here is `pub(in crate::engine::merge)`, so a command module that
//! tried to write a row instead of appending an op and folding it
//! ([`crate::engine::Engine::append_and_fold`]) would not compile.
//!
//! Reading a row stays with its entity's module (`read_task` and its
//! siblings), and so do the encoders a writer shares with a reader.

mod attachment;
mod block;
mod context;
mod preferences;
mod routine;
mod stream;
mod task;

pub(in crate::engine::merge) use attachment::upsert_attachment_row;
pub(in crate::engine::merge) use block::{replace_block_tasks, upsert_block_row};
pub(in crate::engine::merge) use context::{
    insert_context_row, purge_context_from_tasks, update_context_row,
};
pub(in crate::engine::merge) use preferences::upsert_preferences_row;
pub(in crate::engine::merge) use routine::{insert_routine_row, update_routine_row};
pub(in crate::engine::merge) use stream::{insert_stream_row, update_stream_row};
pub(in crate::engine::merge) use task::{
    ftsr_delete_task, ftsr_upsert_task, insert_task_contexts, insert_task_row,
    replace_task_blockers, replace_task_contexts, update_task_row,
};

/// The row writers, for a test that builds a vault older than the merge: one
/// whose rows were written with no field state behind them (migration 0033),
/// which the merge seeds from the row the first time an op touches it.
/// Nothing outside the tests writes a row this way.
#[cfg(test)]
pub(in crate::engine) mod legacy_rows {
    use crate::engine::lww::LwwStamp;
    use rusqlite::Transaction;
    use sunrise_domain::{Context, Routine, Stream, Task};

    pub(in crate::engine) fn insert_stream_row(
        tx: &Transaction<'_>,
        s: &Stream,
        lww: &LwwStamp,
    ) -> rusqlite::Result<()> {
        super::insert_stream_row(tx, s, lww)
    }

    pub(in crate::engine) fn update_stream_row(
        tx: &Transaction<'_>,
        s: &Stream,
        lww: &LwwStamp,
    ) -> rusqlite::Result<()> {
        super::update_stream_row(tx, s, lww)
    }

    pub(in crate::engine) fn insert_context_row(
        tx: &Transaction<'_>,
        c: &Context,
        lww: &LwwStamp,
    ) -> rusqlite::Result<()> {
        super::insert_context_row(tx, c, lww)
    }

    pub(in crate::engine) fn update_context_row(
        tx: &Transaction<'_>,
        c: &Context,
        lww: &LwwStamp,
    ) -> rusqlite::Result<()> {
        super::update_context_row(tx, c, lww)
    }

    pub(in crate::engine) fn insert_routine_row(
        tx: &Transaction<'_>,
        r: &Routine,
        now_ms: u64,
        lww: &LwwStamp,
    ) -> rusqlite::Result<()> {
        super::insert_routine_row(tx, r, now_ms, lww)
    }

    pub(in crate::engine) fn update_routine_row(
        tx: &Transaction<'_>,
        r: &Routine,
        lww: &LwwStamp,
    ) -> rusqlite::Result<()> {
        super::update_routine_row(tx, r, lww)
    }

    pub(in crate::engine) fn update_task_row(
        tx: &Transaction<'_>,
        t: &Task,
        lww: &LwwStamp,
    ) -> rusqlite::Result<()> {
        super::update_task_row(tx, t, lww)
    }
}
