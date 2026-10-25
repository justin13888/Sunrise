//! `upcoming [7|14|30]` — the Upcoming view (#351): the next 7, 14 or 30
//! civil days in the reader's zone, then each stream's undated tasks.
//!
//! The days, their bounds, what is on each and in what order are all the
//! core's (`Query::Upcoming`); this prints them. With no span, the device's
//! `views.upcoming.span_days` preference decides, as it does in the apps.

use sunrise_cli::i18n::strings;
use sunrise_cli::when;
use sunrise_core::{Query, QueryResult};
use sunrise_domain::{Task, TaskState};
use sunrise_id::EntityRef;

use super::{stream_names, Archived, CmdResult, Ctx};

/// `upcoming [7|14|30]`.
pub(super) async fn upcoming(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let span_days = match rest {
        [] => None,
        [raw] => Some(
            raw.parse::<u32>()
                .ok()
                .filter(|n| sunrise_domain::is_upcoming_span(*n))
                .ok_or_else(|| strings::upcoming::unknown_span(raw))?,
        ),
        _ => return Err(strings::upcoming::usage().into()),
    };
    let q = Query::Upcoming {
        now_ms: ctx.core.now_ms(),
        span_days,
        contexts: vec![],
    };
    let QueryResult::Upcoming(view) = ctx.core.query(q).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    let reader = ctx.reader();
    if ctx.json {
        sunrise_cli::json::print(&sunrise_cli::json::upcoming(&view, &reader));
        return Ok(());
    }

    let row = |t: &Task, at: Option<&sunrise_domain::SunriseTime>, mark_as: Option<String>| {
        let mark = if t.state == TaskState::Done { "x" } else { " " };
        let when = at.map_or_else(String::new, |v| {
            let text = when::describe(v, &reader);
            match &mark_as {
                Some(label) => format!("  {label} {text}"),
                None => format!("  {text}"),
            }
        });
        format!("[{mark}] {}  {}{when}", t.id.to_str(), t.title)
    };
    let due = || Some(strings::upcoming::due());
    let target = || Some(strings::upcoming::target());
    for day in &view.days {
        if day.blocks.is_empty()
            && day.planned.is_empty()
            && day.due.is_empty()
            && day.targets.is_empty()
        {
            println!("{}  {}", day.date, strings::upcoming::empty_day());
            continue;
        }
        println!("{}", day.date);
        for b in &day.blocks {
            println!(
                "  {}  {} -> {}  {}",
                b.block.id.to_str(),
                when::describe(&b.block.starts_at, &reader),
                when::describe(&b.block.ends_at, &reader),
                b.title.as_deref().unwrap_or("-"),
            );
        }
        for t in &day.planned {
            println!("  {}", row(t, t.planned_at.as_ref(), None));
        }
        for t in &day.due {
            println!("  {}", row(t, t.hard_due_at.as_ref(), due()));
        }
        for t in &day.targets {
            println!("  {}", row(t, t.target_at.as_ref(), target()));
        }
    }

    if view.no_date.is_empty() {
        return Ok(());
    }
    let names = stream_names(ctx.core, Archived::Include).await?;
    let name_of = |id: EntityRef| {
        names
            .iter()
            .find(|(r, _)| *r == id)
            .map_or_else(|| id.to_str(), |(_, n)| n.clone())
    };
    println!("{}", strings::upcoming::no_date());
    for g in &view.no_date {
        if g.no_stream {
            println!("  {}", strings::upcoming::no_stream());
        } else {
            println!("  #{}", name_of(g.stream));
        }
        for t in &g.tasks {
            let line = match (&t.target_at, &t.hard_due_at) {
                (Some(v), _) => row(t, Some(v), target()),
                (None, v) => row(t, v.as_ref(), due()),
            };
            println!("    {line}");
        }
        let shown = u32::try_from(g.tasks.len()).unwrap_or(u32::MAX);
        if g.total > shown {
            println!(
                "    {}",
                strings::upcoming::more(i64::from(g.total - shown))
            );
        }
    }
    Ok(())
}
