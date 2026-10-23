//! Calendar blocks: the day and week grids, and the edits a calendar makes.
//!
//! Every write goes through the client layer's calendar
//! ([`sunrise_client_core::calendar`]): a new block's bounds are written in
//! the kind the calendar writes ([`sunrise_client_core::calendar::NEW_BLOCK_KIND`]
//! unless `--kind` says otherwise), a move keeps each bound's kind and zone,
//! and a merge is the Resolve menu's three writes in the order that leaves the
//! vault readable if one fails.

use sunrise_cli::i18n::strings;
use sunrise_cli::when;
use sunrise_client_core::calendar::{merge_blocks, moved_block_patch, new_block_draft};
use sunrise_client_core::civil::{seed_ms, TimeKind};
use sunrise_core::{Command, Query, QueryResult};
use sunrise_domain::Block;
use sunrise_id::{EntityKind, EntityRef};

use super::{flag_value, id_of, positionals, resolve_stream, task_id, Archived, CmdResult, Ctx};

/// A `--from` / `--to` phrase as epoch milliseconds, read in the reader's
/// zone.
fn moment(ctx: &Ctx<'_>, phrase: &str) -> CmdResult<i64> {
    sunrise_domain::capture::parse_when(phrase, ctx.now(), &ctx.reader())
        .map(jiff::Timestamp::as_millisecond)
        .ok_or_else(|| strings::capture::unreadable_date(phrase).into())
}

/// A `--for` length: `45m`, `1h30m`, `2h`.
fn length_ms(raw: &str) -> CmdResult<i64> {
    let d: jiff::SignedDuration = raw
        .trim()
        .parse()
        .map_err(|_| strings::capture::unreadable_duration(raw))?;
    let ms = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
    if ms <= 0 {
        return Err(strings::capture::unreadable_duration(raw).into());
    }
    Ok(ms)
}

fn kind(raw: &str) -> CmdResult<TimeKind> {
    Ok(match raw {
        "zoned" => TimeKind::Zoned,
        "floating" => TimeKind::Floating,
        "instant" => TimeKind::Instant,
        other => return Err(strings::blocks::bad_kind(other).into()),
    })
}

/// One block, read from the vault.
async fn read_block(ctx: &Ctx<'_>, id: EntityRef) -> CmdResult<Block> {
    match ctx.core.query(Query::EntityById(id)).await? {
        QueryResult::Blocks(mut rows) if rows.len() == 1 => Ok(rows
            .pop()
            .map(|r| r.block)
            .ok_or_else(|| strings::blocks::no_such(&id.to_str()))?),
        _ => Err(strings::blocks::no_such(&id.to_str()).into()),
    }
}

/// `blocks [day|week] [<when>]` — the grid's blocks, one row each.
pub(super) async fn list(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (week, phrase) = match rest.first().map(String::as_str) {
        Some("week") => (true, rest[1..].join(" ")),
        Some("day") => (false, rest[1..].join(" ")),
        _ => (false, rest.join(" ")),
    };
    let at = if phrase.trim().is_empty() {
        ctx.core.now_ms()
    } else {
        u64::try_from(moment(ctx, &phrase)?).map_err(|_| strings::defer::before_epoch())?
    };
    let q = if week {
        Query::WeekBlocks { week_ms: at }
    } else {
        Query::DayBlocks { day_ms: at }
    };
    let QueryResult::Blocks(rows) = ctx.core.query(q).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    let reader = ctx.reader();
    for r in rows {
        println!(
            "{}  {} -> {}  {}  [{}]",
            r.block.id.to_str(),
            when::describe(&r.block.starts_at, &reader),
            when::describe(&r.block.ends_at, &reader),
            r.title.as_deref().unwrap_or("-"),
            r.block
                .tasks
                .iter()
                .map(EntityRef::to_str)
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    Ok(())
}

/// `block create|move|bind|unbind|merge|delete …`.
pub(super) async fn block(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let args = rest.get(1..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        Some("create") => create(ctx, args).await,
        Some("move") => move_block(ctx, args).await,
        Some(verb @ ("bind" | "unbind")) => {
            let [blk, tasks @ ..] = args else {
                return Err(strings::blocks::bind_usage().into());
            };
            if tasks.is_empty() {
                return Err(strings::blocks::bind_usage().into());
            }
            let block = id_of(blk, EntityKind::Block)?;
            for raw in tasks {
                let task = task_id(raw)?;
                let cmd = if verb == "bind" {
                    Command::BindTask { block, task }
                } else {
                    Command::UnbindTask { block, task }
                };
                ctx.submit(cmd, strings::undo::bind(&block.to_str()))
                    .await?;
                println!("{}  {verb} {}", block.to_str(), task.to_str());
            }
            Ok(())
        }
        Some("merge") => {
            let [a, b] = args else {
                return Err(strings::blocks::merge_usage().into());
            };
            let (a, b) = (id_of(a, EntityKind::Block)?, id_of(b, EntityKind::Block)?);
            let merged = merge_blocks(ctx.core, a, b, &ctx.reader()).await?;
            println!("{}  {}", merged.to_str(), strings::blocks::merged());
            Ok(())
        }
        Some("delete") => {
            let [raw] = args else {
                return Err(strings::blocks::usage().into());
            };
            let id = id_of(raw, EntityKind::Block)?;
            ctx.submit(Command::DeleteBlock(id), strings::undo::delete(raw))
                .await?;
            println!("deleted  {raw}");
            Ok(())
        }
        _ => Err(strings::blocks::usage().into()),
    }
}

/// `block create --from <when> (--to <when> | --for <length>) [--title T]
/// [--stream S] [--task <tsk>]... [--kind zoned|floating|instant]`.
async fn create(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let from = flag_value(args, "--from").ok_or_else(strings::blocks::create_usage)?;
    let from_ms = moment(ctx, &from)?;
    let to_ms = match (flag_value(args, "--to"), flag_value(args, "--for")) {
        (Some(to), None) => moment(ctx, &to)?,
        (None, Some(len)) => from_ms.saturating_add(length_ms(&len)?),
        _ => return Err(strings::blocks::create_usage().into()),
    };
    if to_ms <= from_ms {
        return Err(strings::blocks::end_before_start().into());
    }
    let stream = match flag_value(args, "--stream") {
        Some(s) => resolve_stream(ctx.core, &s, Archived::Exclude).await?,
        None => sunrise_domain::inbox_stream_ref(),
    };
    let mut tasks = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--task" {
            let raw = it.next().ok_or_else(strings::blocks::create_usage)?;
            tasks.push(task_id(raw)?);
        }
    }
    let kind = flag_value(args, "--kind")
        .as_deref()
        .map(kind)
        .transpose()?
        .unwrap_or(sunrise_client_core::calendar::NEW_BLOCK_KIND);
    let title = flag_value(args, "--title");
    let draft = new_block_draft(
        from_ms,
        to_ms,
        kind,
        stream,
        title.clone(),
        tasks,
        &ctx.reader(),
    )?;
    let label = title.unwrap_or_else(|| from.clone());
    let res = ctx
        .submit(Command::CreateBlock(draft), strings::undo::create(&label))
        .await?;
    println!("{}  {label}", res.entity.to_str());
    Ok(())
}

/// `block move <blk> --from <when> [--to <when>]` — a drag on the grid.
///
/// With no `--to` the block keeps its length. Each bound keeps its kind and,
/// for a zoned one, its own zone
/// ([`sunrise_client_core::calendar::moved_block_patch`]).
async fn move_block(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let words = positionals(args, &["--from", "--to"], &[]);
    let [raw] = words.as_slice() else {
        return Err(strings::blocks::move_usage().into());
    };
    let id = id_of(raw, EntityKind::Block)?;
    let from = flag_value(args, "--from").ok_or_else(strings::blocks::move_usage)?;
    let block = read_block(ctx, id).await?;
    let reader = ctx.reader();
    let start_ms = moment(ctx, &from)?;
    let end_ms = if let Some(to) = flag_value(args, "--to") {
        moment(ctx, &to)?
    } else {
        let len = seed_ms(&block.ends_at, &reader) - seed_ms(&block.starts_at, &reader);
        start_ms.saturating_add(len)
    };
    let patch = moved_block_patch(&block, start_ms, end_ms, &reader)?;
    ctx.submit(
        Command::UpdateBlock { id, patch },
        strings::undo::move_block(raw),
    )
    .await?;
    println!("{}  {}", id.to_str(), strings::general::updated());
    Ok(())
}
