//! `triage` — the triage queue and its four actions (ADR-0047 §3–§4, #335).
//!
//! `sunrise triage` (or `triage list`) prints the queue the core derives
//! (`Query::Triage`): a task whose plan breaks a `hard` scheduling constraint
//! first, flagged `hard_violation`, then hard-late, soft-late and stale,
//! oldest first. `triage defer|done|drop|keep <id>...` applies one action to the
//! named tasks as one `Command::Triage`: every task is checked before any is
//! written, and one that cannot take the action refuses them all. A bare
//! `triage <id>...` still takes tasks out of the Inbox (`MarkTriaged`,
//! ADR-0046), as it did before the queue existed.

use sunrise_cli::i18n::strings;
use sunrise_cli::when::{self, When};
use sunrise_core::{Command, Query, QueryResult, TriageRow};
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

/// The queue, one `id  state  flag  title` row each.
async fn list(ctx: &Ctx<'_>) -> CmdResult {
    let QueryResult::Triage(rows) = ctx.core.query(Query::Triage).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    let reader = ctx.reader();
    if ctx.json {
        let rows: Vec<serde_json::Value> = rows.iter().map(|r| row_json(r, &reader)).collect();
        sunrise_cli::json::print(&serde_json::Value::Array(rows));
        return Ok(());
    }
    if rows.is_empty() {
        println!("{}", strings::triage::empty());
        return Ok(());
    }
    for r in &rows {
        println!("{}", row_line(r));
    }
    Ok(())
}

/// The token the flag column holds for a row whose plan breaks a `hard`
/// scheduling constraint for the reader now; `-` for any other row.
const HARD_VIOLATION: &str = "hard_violation";

/// A row as text: its id, its lateness state, `hard_violation` or `-`, and
/// its title. The flag is a column of its own because a row can be in the
/// queue only for a broken window, with its lateness `on_track`.
fn row_line(r: &TriageRow) -> String {
    let flag = if r.hard_violation {
        HARD_VIOLATION
    } else {
        "-"
    };
    format!(
        "{}  {:<9}  {:<width$}  {}",
        r.task.id.to_str(),
        r.lateness.state.as_str(),
        flag,
        r.task.title,
        width = HARD_VIOLATION.len()
    )
}

/// A row as JSON: the task, with its `lateness` and `hard_violation`.
fn row_json(r: &TriageRow, reader: &jiff::tz::TimeZone) -> serde_json::Value {
    let mut v = sunrise_cli::json::task(&r.task, reader);
    if let serde_json::Value::Object(map) = &mut v {
        map.insert("lateness".into(), sunrise_cli::json::lateness(&r.lateness));
        map.insert("hard_violation".into(), r.hard_violation.into());
    }
    v
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

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_domain::{Lateness, LatenessState, Task, TaskDraft};

    fn row(lateness: Lateness, hard_violation: bool) -> TriageRow {
        let id = EntityRef::new(sunrise_id::EntityKind::Task, [1; 16]);
        let draft = TaskDraft {
            title: "Pay the rent".into(),
            ..Default::default()
        };
        TriageRow {
            task: Task::from_draft(id, jiff::Timestamp::UNIX_EPOCH, draft),
            lateness: LatenessState {
                state: lateness,
                since: None,
            },
            hard_violation,
        }
    }

    /// A row in the queue only for a broken window is `on_track`, so the flag
    /// is what says why it is there (#1218).
    #[test]
    fn a_flagged_row_says_so_in_text_and_json() {
        let flagged = row(Lateness::OnTrack, true);
        let line = row_line(&flagged);
        let cols: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(cols[1..4], ["on_track", "hard_violation", "Pay"], "{line}");
        let json = row_json(&flagged, &jiff::tz::TimeZone::UTC);
        assert_eq!(json["hard_violation"], true);
        assert_eq!(json["lateness"]["state"], "on_track");

        let plain = row(Lateness::LateHard, false);
        let line = row_line(&plain);
        let cols: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(cols[1..4], ["late_hard", "-", "Pay"], "{line}");
        assert_eq!(
            line.find("Pay"),
            row_line(&flagged).find("Pay"),
            "the title starts in the same column"
        );
        assert_eq!(
            row_json(&plain, &jiff::tz::TimeZone::UTC)["hard_violation"],
            false
        );
    }
}
