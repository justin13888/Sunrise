//! `triage` — the triage queue and its four actions (ADR-0047 §3–§4, #335).
//!
//! `sunrise triage` (or `triage list`) prints the queue the core derives
//! (`Query::Triage`), hard-late first, then soft-late, then stale, oldest
//! first. `triage defer|done|drop|keep <id>...` applies one action to the
//! named tasks as one `Command::Triage`: every task is checked before any is
//! written, and one that cannot take the action refuses them all. A bare
//! `triage <id>...` still takes tasks out of the Inbox (`MarkTriaged`,
//! ADR-0046), as it did before the queue existed.

use sunrise_cli::i18n::strings;
use sunrise_cli::when::{self, When};
use sunrise_core::{Command, Query, QueryResult};
use sunrise_domain::{DeferTarget, SunriseTime, TriageAction};
use sunrise_id::EntityRef;

use super::{read_task, split_task_ids, CmdResult, Ctx};

/// `triage ...`.
pub(super) async fn triage(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    match rest.split_first() {
        None => list(ctx).await,
        Some((verb, tail)) => match verb.as_str() {
            "list" if tail.is_empty() => list(ctx).await,
            "defer" => defer(ctx, tail).await,
            "done" => done(ctx, tail).await,
            "drop" => drop_(ctx, tail).await,
            "keep" => keep(ctx, tail).await,
            // `triage <id>...`: the Inbox triage it always was.
            _ if split_task_ids(rest).1.is_empty() => super::tasks::triage(ctx, rest, true).await,
            _ => Err(strings::triage::usage().into()),
        },
    }
}

/// The queue, one `id  state  title` row each.
async fn list(ctx: &Ctx<'_>) -> CmdResult {
    let QueryResult::Triage(rows) = ctx.core.query(Query::Triage).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    let reader = ctx.reader();
    if ctx.json {
        let rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let mut v = sunrise_cli::json::task(&r.task, &reader);
                if let serde_json::Value::Object(map) = &mut v {
                    map.insert("lateness".into(), sunrise_cli::json::lateness(&r.lateness));
                }
                v
            })
            .collect();
        sunrise_cli::json::print(&serde_json::Value::Array(rows));
        return Ok(());
    }
    if rows.is_empty() {
        println!("{}", strings::triage::empty());
        return Ok(());
    }
    for r in &rows {
        println!(
            "{}  {:<9}  {}",
            r.task.id.to_str(),
            r.lateness.state.as_str(),
            r.task.title
        );
    }
    Ok(())
}

/// The ids at the head of `rest`, at least one, and what follows them.
fn ids(rest: &[String]) -> CmdResult<(Vec<EntityRef>, &[String])> {
    let (ids, tail) = split_task_ids(rest);
    if ids.is_empty() {
        return Err(strings::triage::usage().into());
    }
    Ok((ids, tail))
}

/// Submit `action` over `ids`, labelled with their titles.
async fn submit(
    ctx: &Ctx<'_>,
    ids: Vec<EntityRef>,
    action: TriageAction,
    label: fn(&str) -> String,
) -> CmdResult {
    let mut titles = Vec::with_capacity(ids.len());
    for id in &ids {
        titles.push(read_task(ctx.core, *id).await?.title);
    }
    ctx.submit(Command::Triage { ids, action }, label(&titles.join(", ")))
        .await?;
    Ok(())
}

/// `triage defer <id>... <when>|next`.
async fn defer(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, tail) = ids(rest)?;
    let phrase = tail.join(" ");
    let reader = ctx.reader();
    let to = match phrase.trim() {
        "" => return Err(strings::defer::needs_date().into()),
        "next" => DeferTarget::NextFreeSlot,
        p => match when::read(p, ctx.now(), &reader) {
            Some(When::At(t)) => DeferTarget::At(t),
            // "+3d" is three civil days from today, as a date.
            Some(When::Days(days)) => DeferTarget::At(
                sunrise_client_core::schedule::scheduled_on_day(days, ctx.now(), &reader)?,
            ),
            None => return Err(strings::capture::unreadable_date(p).into()),
        },
    };
    let shown = match &to {
        DeferTarget::At(t) => when::describe(t, &reader),
        DeferTarget::NextFreeSlot => strings::triage::next_free_slot(),
    };
    submit(
        ctx,
        ids.clone(),
        TriageAction::Defer { to },
        strings::undo::defer,
    )
    .await?;
    for id in ids {
        println!("{}", strings::triage::deferred(&id.to_str(), &shown));
    }
    Ok(())
}

/// `triage done <id>... [--on <when>]`: done now, or at the time or on the
/// day given.
async fn done(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, tail) = ids(rest)?;
    let reader = ctx.reader();
    let completed_at = match tail {
        [] => SunriseTime::instant(ctx.now()),
        [flag, phrase @ ..] if flag == "--on" && !phrase.is_empty() => {
            let p = phrase.join(" ");
            match when::read(&p, ctx.now(), &reader) {
                // A date with no time is the honest record of "done that day".
                Some(When::At(SunriseTime::AllDay { date })) => SunriseTime::all_day(date),
                Some(When::At(t)) => SunriseTime::instant(
                    t.resolve_in(&reader)
                        .ok_or_else(|| strings::capture::unreadable_date(&p))?,
                ),
                Some(When::Days(days)) => {
                    sunrise_client_core::schedule::scheduled_on_day(days, ctx.now(), &reader)?
                }
                None => return Err(strings::capture::unreadable_date(&p).into()),
            }
        }
        _ => return Err(strings::triage::usage().into()),
    };
    let shown = when::describe(&completed_at, &reader);
    submit(
        ctx,
        ids.clone(),
        TriageAction::AlreadyDone { completed_at },
        strings::undo::triage_done,
    )
    .await?;
    for id in ids {
        println!("{}", strings::triage::done(&id.to_str(), &shown));
    }
    Ok(())
}

/// `triage drop <id>... [--reason <text>]`.
async fn drop_(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, tail) = ids(rest)?;
    let reason = match tail {
        [] => None,
        [flag, words @ ..] if flag == "--reason" && !words.is_empty() => Some(words.join(" ")),
        _ => return Err(strings::triage::usage().into()),
    };
    submit(
        ctx,
        ids.clone(),
        TriageAction::Drop { reason },
        strings::undo::drop,
    )
    .await?;
    for id in ids {
        println!("{}", strings::triage::dropped(&id.to_str()));
    }
    Ok(())
}

/// `triage keep <id>...`.
async fn keep(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, tail) = ids(rest)?;
    if !tail.is_empty() {
        return Err(strings::triage::usage().into());
    }
    submit(
        ctx,
        ids.clone(),
        TriageAction::Keep,
        strings::undo::triage_keep,
    )
    .await?;
    for id in ids {
        println!("{}", strings::triage::kept(&id.to_str()));
    }
    Ok(())
}
