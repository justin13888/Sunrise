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
mod saved_view;
mod search;
mod stream;
mod task;

pub(in crate::engine::merge) use attachment::upsert_attachment_row;
pub(in crate::engine::merge) use block::{replace_block_tasks, upsert_block_row};
pub(in crate::engine::merge) use context::{
    insert_context_row, purge_context_from_tasks, update_context_row,
};
pub(in crate::engine::merge) use preferences::upsert_preferences_row;
pub(in crate::engine::merge) use routine::{insert_routine_row, update_routine_row};
pub(in crate::engine::merge) use saved_view::upsert_saved_view_row;
pub(in crate::engine::merge) use stream::{insert_stream_row, update_stream_row};
// The full-text rows are written by the row writers above, and rebuilt from
// the rows by `engine::search`, which is the one reader of the rest.
pub(in crate::engine) use search::{create_words_sql, rebuild_from_rows};
pub(in crate::engine::merge) use task::{
    ftsr_delete_task, ftsr_upsert_task, insert_task_contexts, insert_task_row,
    replace_task_blockers, replace_task_contexts, update_task_row,
};
