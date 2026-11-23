//! Context shifts (ADR-0073 §7): `shift start`, `shift end` and
//! `shift status`.
//!
//! A shift is one synced `shf_` entity. Starting one writes it; ending one
//! writes its `ends_at` and nothing else, because nothing the shift covered
//! was rewritten (§4), so there is no prior state to restore. Which shift is
//! in force, and what accrued during it, are the core's reads
//! (`Query::ActiveShift`, `Query::AwaySummary`); this module only words them.

use sunrise_cli::i18n::strings;
use sunrise_core::{Command, Query, QueryResult};
use sunrise_domain::{AwaySummary, Routine, Shift, ShiftDraft, SunriseTime};
use sunrise_id::{EntityKind, EntityRef};

use super::{flag_value, id_of, positionals, read_task, CmdResult, Ctx};

/// `shift start|end|status`.
pub(super) async fn shift(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let args = rest.get(1..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        Some("start") => start(ctx, args).await,
        Some("end") if args.is_empty() => end(ctx).await,
        Some("status") if args.is_empty() => status(ctx).await,
        None | Some("end" | "status") => Err(strings::shift::usage().into()),
        Some(other) => Err(strings::shift::unknown_verb(other).into()),
    }
}

/// The shift in force now, if any.
async fn active(ctx: &Ctx<'_>) -> CmdResult<Option<Shift>> {
    match ctx.core.query(Query::ActiveShift).await? {
        QueryResult::ActiveShift(s) => Ok(s),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// What accrued during `id`'s window, read now.
async fn away(ctx: &Ctx<'_>, id: EntityRef) -> CmdResult<AwaySummary> {
    match ctx.core.query(Query::AwaySummary(id)).await? {
        QueryResult::AwaySummary(s) => Ok(s),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// `shift start <name>... [--zone Z] [--place plc_…]`.
///
/// The shift starts now and runs until ended. A zone the tzdb does not know
/// is refused here, by the name typed, before the core would refuse it.
async fn start(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let name = positionals(args, &["--zone", "--place"], &[]).join(" ");
    let name = name.trim();
    if name.is_empty() || args.last().is_some_and(|a| a == "--zone" || a == "--place") {
        return Err(strings::shift::start_usage().into());
    }
    let zone = flag_value(args, "--zone");
    if let Some(z) = &zone {
        sunrise_client_core::civil::reader_zone(z)?;
    }
    let place = flag_value(args, "--place")
        .map(|raw| id_of(&raw, EntityKind::Place))
        .transpose()?;
    let previous = active(ctx).await?;
    let draft = ShiftDraft {
        name: name.to_owned(),
        starts_at: None,
        ends_at: None,
        zone,
        place,
        primary_streams: std::collections::BTreeSet::new(),
        primary_contexts: std::collections::BTreeSet::new(),
    };
    let res = ctx
        .submit(
            Command::CreateShift(draft),
            strings::undo::start_shift(name),
        )
        .await?;
    println!("{}  {}", res.entity.to_str(), strings::shift::started(name));
    if let Some(previous) = previous {
        eprintln!(
            "{}",
            strings::general::note(&strings::shift::overlaps(&previous.name, name))
        );
    }
    Ok(())
}

/// `shift end`: end the shift in force now, then print what accrued while it
/// ran.
async fn end(ctx: &Ctx<'_>) -> CmdResult {
    let shift = active(ctx)
        .await?
        .ok_or_else(strings::shift::none_in_force)?;
    ctx.submit(
        Command::EndShift {
            id: shift.id,
            at: None,
        },
        strings::undo::end_shift(&shift.name),
    )
    .await?;
    println!(
        "{}  {}",
        shift.id.to_str(),
        strings::shift::ended(&shift.name)
    );
    let summary = away(ctx, shift.id).await?;
    print_away(ctx, &summary).await
}

/// `shift status`: the shift in force and what it has gathered so far.
async fn status(ctx: &Ctx<'_>) -> CmdResult {
    let shift = active(ctx).await?;
    let summary = match &shift {
        Some(s) => Some(away(ctx, s.id).await?),
        None => None,
    };
    if ctx.json {
        sunrise_cli::json::print(&sunrise_cli::json::shift_status(
            shift.as_ref(),
            summary.as_ref(),
        ));
        return Ok(());
    }
    let (Some(shift), Some(summary)) = (shift, summary) else {
        println!("{}", strings::shift::none());
        return Ok(());
    };
    let reader = ctx.reader();
    let instant = |at| sunrise_cli::when::describe(&SunriseTime::instant(at), &reader);
    println!("{}  {}", shift.id.to_str(), shift.name);
    println!("since   {}", instant(shift.starts_at));
    println!(
        "until   {}",
        shift
            .ends_at
            .map_or_else(strings::shift::until_ended, instant)
    );
    println!("zone    {}", shift.zone.as_deref().unwrap_or("-"));
    println!(
        "place   {}",
        shift.place.map_or_else(|| "-".to_owned(), |p| p.to_str())
    );
    print_away(ctx, &summary).await
}

/// The "While you were away" summary (ADR-0073 §6): each group under its
/// heading, a missed occurrence by its routine's title and when it was due,
/// a task by its id and title.
async fn print_away(ctx: &Ctx<'_>, summary: &AwaySummary) -> CmdResult {
    println!();
    println!("{}", strings::shift::away());
    if summary.missed.is_empty()
        && summary.deadlines_passed.is_empty()
        && summary.captured.is_empty()
    {
        println!("  {}", strings::shift::away_empty());
        return Ok(());
    }
    let reader = ctx.reader();
    if !summary.missed.is_empty() {
        let QueryResult::Routines(routines) = ctx.core.query(Query::Routines).await? else {
            return Err(strings::general::unexpected_result().into());
        };
        let title = |id: EntityRef| {
            routines
                .iter()
                .find(|r: &&Routine| r.id == id)
                .map_or_else(|| id.to_str(), |r| r.template.title.clone())
        };
        println!("  {}", strings::shift::away_missed());
        for m in &summary.missed {
            println!(
                "    {}  {}  {}",
                m.routine.to_str(),
                sunrise_cli::when::describe(&SunriseTime::instant(m.at), &reader),
                title(m.routine)
            );
        }
    }
    for (heading, ids) in [
        (strings::shift::away_deadlines(), &summary.deadlines_passed),
        (strings::shift::away_captured(), &summary.captured),
    ] {
        if ids.is_empty() {
            continue;
        }
        println!("  {heading}");
        for id in ids {
            let task = read_task(ctx.core, *id).await?;
            println!("    {}  {}", id.to_str(), task.title);
        }
    }
    Ok(())
}
