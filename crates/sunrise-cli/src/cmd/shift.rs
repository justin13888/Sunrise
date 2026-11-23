//! Context shifts (ADR-0073 §7): `shift start`, `shift end` and
//! `shift status`.
//!
//! A shift is one synced `shf_` entity. Starting one writes it; ending one
//! writes its `ends_at` and nothing else, because nothing the shift covered
//! was rewritten (§4), so there is no prior state to restore. Which shift is
//! in force, and what accrued during it, are the core's reads
//! (`Query::ActiveShift`, `Query::AwaySummary`); this module only words them.

use std::collections::BTreeMap;

use jiff::Timestamp;
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

/// Print the "While you were away" summary: read the titles its lines name,
/// then [`away_lines`].
async fn print_away(ctx: &Ctx<'_>, summary: &AwaySummary) -> CmdResult {
    let mut titles = BTreeMap::new();
    if !summary.missed.is_empty() {
        let QueryResult::Routines(routines) = ctx.core.query(Query::Routines).await? else {
            return Err(strings::general::unexpected_result().into());
        };
        titles.extend(
            routines
                .into_iter()
                .map(|r: Routine| (r.id, r.template.title)),
        );
    }
    for id in summary.deadlines_passed.iter().chain(&summary.captured) {
        if !titles.contains_key(id) {
            titles.insert(*id, read_task(ctx.core, *id).await?.title);
        }
    }
    let reader = ctx.reader();
    let when = |at| sunrise_cli::when::describe(&SunriseTime::instant(at), &reader);
    for line in away_lines(summary, &titles, when) {
        println!("{line}");
    }
    Ok(())
}

/// The "While you were away" summary (ADR-0073 §6), one printed line each:
/// a blank line, the heading, then each non-empty group under its own
/// heading, a missed occurrence as `routine  when  title` and a task as
/// `id  title`. `titles` holds the routines' and tasks' titles by id; a
/// routine missing from it (archived since) prints its id as its title.
fn away_lines(
    summary: &AwaySummary,
    titles: &BTreeMap<EntityRef, String>,
    when: impl Fn(Timestamp) -> String,
) -> Vec<String> {
    let title = |id: &EntityRef| titles.get(id).cloned().unwrap_or_else(|| id.to_str());
    let mut lines = vec![String::new(), strings::shift::away()];
    if summary.missed.is_empty()
        && summary.deadlines_passed.is_empty()
        && summary.captured.is_empty()
    {
        lines.push(format!("  {}", strings::shift::away_empty()));
        return lines;
    }
    if !summary.missed.is_empty() {
        lines.push(format!("  {}", strings::shift::away_missed()));
        for m in &summary.missed {
            lines.push(format!(
                "    {}  {}  {}",
                m.routine.to_str(),
                when(m.at),
                title(&m.routine)
            ));
        }
    }
    for (heading, ids) in [
        (strings::shift::away_deadlines(), &summary.deadlines_passed),
        (strings::shift::away_captured(), &summary.captured),
    ] {
        if ids.is_empty() {
            continue;
        }
        lines.push(format!("  {heading}"));
        for id in ids {
            lines.push(format!("    {}  {}", id.to_str(), title(id)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_domain::MissedOccurrence;

    fn at(ms: i64) -> Timestamp {
        Timestamp::from_millisecond(ms).unwrap()
    }

    /// Every group prints under its heading in the documented order: a
    /// missed occurrence by routine id, when it was due and the routine's
    /// title (its id when the routine is gone), then each passed deadline and
    /// each capture by task id and title.
    #[test]
    fn the_away_summary_prints_each_group_under_its_heading() {
        let routine = EntityRef::new(EntityKind::Routine, [1; 16]);
        let gone = EntityRef::new(EntityKind::Routine, [2; 16]);
        let due = EntityRef::new(EntityKind::Task, [3; 16]);
        let captured = EntityRef::new(EntityKind::Task, [4; 16]);
        let summary = AwaySummary {
            window: Some((at(1_000), at(9_000))),
            missed: vec![
                MissedOccurrence {
                    routine,
                    key: "k1".into(),
                    at: at(2_000),
                },
                MissedOccurrence {
                    routine: gone,
                    key: "k2".into(),
                    at: at(3_000),
                },
            ],
            deadlines_passed: vec![due],
            captured: vec![captured],
        };
        let titles = BTreeMap::from([
            (routine, "Stretch".to_owned()),
            (due, "File the visa form".to_owned()),
            (captured, "Buy an adapter".to_owned()),
        ]);
        let lines = away_lines(&summary, &titles, |t| format!("@{}", t.as_millisecond()));
        assert_eq!(
            lines,
            [
                String::new(),
                "While you were away".to_owned(),
                "  Missed".to_owned(),
                format!("    {}  @2000  Stretch", routine.to_str()),
                format!("    {}  @3000  {}", gone.to_str(), gone.to_str()),
                "  Deadlines passed".to_owned(),
                format!("    {}  File the visa form", due.to_str()),
                "  Captured".to_owned(),
                format!("    {}  Buy an adapter", captured.to_str()),
            ]
        );
    }

    /// A summary with nothing in it says so, and prints no group heading.
    #[test]
    fn an_empty_away_summary_says_nothing_accrued() {
        let lines = away_lines(&AwaySummary::default(), &BTreeMap::new(), |_| {
            unreachable!("nothing to time")
        });
        assert_eq!(
            lines,
            [
                String::new(),
                "While you were away".to_owned(),
                "  Nothing was missed, passed or captured.".to_owned(),
            ]
        );
    }
}
