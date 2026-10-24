//! `move` — put tasks in a hand-arranged order that syncs (#340).
//!
//! The order is one per task, within its stream (`Task.sort_order`,
//! `docs/02-domain/tasks.md`). This reads the stream's list as the core
//! shows it, lets the client layer turn the drop into the one
//! `MoveTasks` it is ([`sunrise_client_core::task_order::plan_move`]), and
//! submits it. The core computes the keys.

use sunrise_cli::i18n::strings;
use sunrise_client_core::task_order::{plan_move_in, Drop};
use sunrise_core::{Query, QueryResult};
use sunrise_id::EntityRef;

use super::{read_task, split_task_ids, task_id, CmdResult, Ctx};

/// Where the words after the ids put the tasks, and which task's stream the
/// list is.
fn drop_of(tail: &[String]) -> CmdResult<Drop> {
    let word = |s: &String| s.trim_start_matches("--").to_owned();
    let words: Vec<String> = tail.iter().map(word).collect();
    match words.as_slice() {
        [w, id] if w == "before" => Ok(Drop::Before(task_id(id)?)),
        [w, id] if w == "after" => Ok(Drop::After(task_id(id)?)),
        [w] if w == "first" || w == "top" => Ok(Drop::Top),
        [w] if w == "last" || w == "bottom" => Ok(Drop::Bottom),
        _ => Err(strings::reorder::usage().into()),
    }
}

/// `move <id>... before|after <id>` and `move <id>... first|last`.
///
/// The tasks land in the order they are listed in their stream, as a
/// multi-row drag does. Every one must be in the anchor's stream: the order
/// is kept per stream, and a task in another one has no place in this list.
/// With `--json`, prints the stream's tasks in their new order.
pub(super) async fn move_tasks(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, tail) = split_task_ids(rest);
    if ids.is_empty() {
        return Err(strings::reorder::usage().into());
    }
    let to = drop_of(tail)?;
    let anchor = match to {
        Drop::Before(a) | Drop::After(a) => a,
        Drop::Top | Drop::Bottom => ids[0],
    };
    let stream = read_task(ctx.core, anchor).await?.stream_id;
    let mut titles = Vec::with_capacity(ids.len());
    for id in &ids {
        let t = read_task(ctx.core, *id).await?;
        if t.stream_id != stream {
            return Err(strings::reorder::other_stream(&id.to_str(), &anchor.to_str()).into());
        }
        titles.push(t.title);
    }
    let list = stream_tasks(ctx, stream).await?;
    let Some(plan) = plan_move_in(&list, &ids, to) else {
        if !ctx.json {
            println!("{}", strings::reorder::unchanged());
        }
        return print_list(ctx, stream).await;
    };
    let label = strings::undo::reorder_tasks(&titles.join(", "));
    ctx.submit(plan.command(), label).await?;
    if !ctx.json {
        for id in &ids {
            println!("moved  {}", id.to_str());
        }
    }
    print_list(ctx, stream).await
}

async fn stream_tasks(ctx: &Ctx<'_>, stream: EntityRef) -> CmdResult<Vec<sunrise_domain::Task>> {
    match ctx.core.query(Query::StreamTasks(stream)).await? {
        QueryResult::StreamTasks(t) | QueryResult::Tasks(t) => Ok(t),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// The stream's tasks in manual order: the table, or with `--json` the list.
async fn print_list(ctx: &Ctx<'_>, stream: EntityRef) -> CmdResult {
    let tasks = stream_tasks(ctx, stream).await?;
    super::tasks::list(ctx, QueryResult::StreamTasks(tasks));
    Ok(())
}
