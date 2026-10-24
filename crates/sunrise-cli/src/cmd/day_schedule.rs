//! `day-schedule`: when the user usually wakes and sleeps
//! (`docs/02-domain/day-schedule.md`), which bounds the planner day Today,
//! Upcoming and the briefs read.
//!
//! Every entry is its own vault preference (`day_schedule.weekday.MO`,
//! `day_schedule.month_day.-1`, `day_schedule.date.2026-12-25`), so a write
//! here is a `SetPreference` of one entry and two devices editing two entries
//! both keep their edit. `sunrise prefs set day_schedule.date.… 07:00-23:00`
//! is the same write; this command only spells the keys for you. Like
//! `prefs set`, it records nothing to undo: the client layer has no inverse
//! for a preference write.

use jiff::civil::Date;
use sunrise_cli::i18n::strings;
use sunrise_cli::prefs_text;
use sunrise_core::{Command, PlannerDay, Query, QueryResult};
use sunrise_domain::day_schedule::WEEKDAYS;
use sunrise_domain::{
    pref_spec, DayOverride, DayScheduleEntry, DayWindow, PrefTarget, PrefValue, Weekday,
};

use super::{CmdResult, Ctx};

/// `day-schedule [show [<date>]] | set … | clear …`.
pub(super) async fn day_schedule(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    match rest.first().map(String::as_str) {
        None | Some("show") => show(ctx, rest.get(1).map(String::as_str)).await,
        Some("set") => set(ctx, rest.get(1..).unwrap_or_default()).await,
        Some("clear") => clear(ctx, rest.get(1..).unwrap_or_default()).await,
        Some(other) => Err(strings::day_schedule::unknown_verb(other).into()),
    }
}

/// The planner day holding `at_ms` (or the one named by `date`), and every
/// entry of the schedule.
async fn show(ctx: &Ctx<'_>, date: Option<&str>) -> CmdResult {
    let QueryResult::DaySchedule(today) = ctx
        .core
        .query(Query::DaySchedule {
            at_ms: ctx.core.now_ms(),
        })
        .await?
    else {
        return Err(strings::general::unexpected_result().into());
    };
    let reader = ctx.reader();
    let day = match date {
        None => *today,
        Some(raw) => {
            let date: Date = raw
                .parse()
                .map_err(|_| strings::day_schedule::bad_date(raw))?;
            day_on(&today.schedule, date, &reader)?
        }
    };
    let lead_s = wind_down_lead_s(ctx).await?;
    if ctx.json {
        sunrise_cli::json::print(&sunrise_cli::json::day_schedule(&day, &reader, lead_s));
        return Ok(());
    }
    let local = |t: jiff::Timestamp| {
        t.to_zoned(reader.clone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string()
    };
    println!(
        "{}",
        strings::day_schedule::planner_day(
            &day.date.to_string(),
            &local(day.start),
            &local(day.end)
        )
    );
    match day.window {
        Some(w) => println!(
            "{}",
            strings::day_schedule::window(&prefs_text::hm(w.wake), &prefs_text::hm(w.sleep))
        ),
        None => println!("{}", strings::day_schedule::no_window()),
    }
    if day.schedule.is_empty() {
        println!();
        println!("{}", strings::day_schedule::empty());
        return Ok(());
    }
    println!();
    for (d, w) in WEEKDAYS.iter().zip(day.schedule.weekday.iter()) {
        if let Some(w) = w {
            println!("{:<12} {}", d.as_str(), span(*w));
        }
    }
    for (n, w) in &day.schedule.month_day {
        println!("{:<12} {}", format!("day {}", month_day_name(*n)), span(*w));
    }
    for (d, o) in &day.schedule.date {
        let text = match o {
            DayOverride::Window(w) => span(*w),
            DayOverride::None => "none".to_owned(),
        };
        println!("{:<12} {text}", d.to_string());
    }
    Ok(())
}

/// The planner day `date` of `schedule` in `reader`, shaped as the query
/// shapes today's.
fn day_on(
    schedule: &sunrise_domain::DaySchedule,
    date: Date,
    reader: &jiff::tz::TimeZone,
) -> CmdResult<PlannerDay> {
    let (start, end) = sunrise_domain::day_bounds(schedule, date, reader)?;
    Ok(PlannerDay {
        date,
        start,
        end,
        window: schedule.window_for(date),
        plannable: sunrise_domain::plannable(schedule, date, reader)?,
        schedule: schedule.clone(),
    })
}

async fn wind_down_lead_s(ctx: &Ctx<'_>) -> CmdResult<u32> {
    let prefs = super::settings::read_prefs(ctx).await?;
    Ok(prefs
        .iter()
        .find(|p| p.key == "notifications.wind_down.lead_s")
        .and_then(|p| match p.value {
            Some(PrefValue::Uint(s)) => u32::try_from(s).ok(),
            _ => None,
        })
        .unwrap_or(3600))
}

/// `prefs get <entry>` for one day-schedule entry, which `Query::Preferences`
/// does not list: its value in the spelling `prefs set` reads, or `-`.
pub(super) async fn get_entry(ctx: &Ctx<'_>, key: &str, entry: &DayScheduleEntry) -> CmdResult {
    let QueryResult::DaySchedule(day) = ctx
        .core
        .query(Query::DaySchedule {
            at_ms: ctx.core.now_ms(),
        })
        .await?
    else {
        return Err(strings::general::unexpected_result().into());
    };
    let s = &day.schedule;
    let window = |w: DayWindow| PrefValue::DayWindow {
        wake: w.wake,
        sleep: w.sleep,
    };
    let value = match entry {
        DayScheduleEntry::Weekday(d) => s.weekday_window(d).map(window),
        DayScheduleEntry::MonthDay(n) => s.month_day.get(n).copied().map(window),
        DayScheduleEntry::Date(d) => s.date.get(d).map(|o| match o {
            DayOverride::Window(w) => window(*w),
            DayOverride::None => PrefValue::NoSchedule,
        }),
    };
    let text = value.as_ref().map(prefs_text::render);
    if ctx.json {
        sunrise_cli::json::print(&serde_json::json!({
            "key": key,
            "value": text,
            "source": if value.is_some() { "vault" } else { "default" },
            "scope": "vault",
        }));
    } else {
        println!("{}", text.unwrap_or_else(|| "-".to_owned()));
    }
    Ok(())
}

fn span(w: DayWindow) -> String {
    format!("{}-{}", prefs_text::hm(w.wake), prefs_text::hm(w.sleep))
}

fn month_day_name(n: i8) -> String {
    if n == -1 {
        "last".to_owned()
    } else {
        n.to_string()
    }
}

/// The entries a `set` or `clear` names, and what is left after them:
/// `<days>` (`MO`, `mo,tu`, `weekdays`, `weekend`, `daily`), `day <1..31|last>`
/// or `date <YYYY-MM-DD>`.
fn entries(args: &[String]) -> CmdResult<(Vec<DayScheduleEntry>, &[String])> {
    let usage = || strings::day_schedule::usage();
    let (first, tail) = args.split_first().ok_or_else(usage)?;
    match first.as_str() {
        "day" => {
            let (n, tail) = tail.split_first().ok_or_else(usage)?;
            let day: i8 = match n.as_str() {
                "last" | "-1" => -1,
                other => other
                    .parse()
                    .ok()
                    .filter(|n| (1..=31).contains(n))
                    .ok_or_else(|| strings::day_schedule::bad_month_day(other))?,
            };
            Ok((vec![DayScheduleEntry::MonthDay(day)], tail))
        }
        "date" => {
            let (d, tail) = tail.split_first().ok_or_else(usage)?;
            let date: Date = d.parse().map_err(|_| strings::day_schedule::bad_date(d))?;
            Ok((vec![DayScheduleEntry::Date(date)], tail))
        }
        days => Ok((
            weekdays(days)?
                .into_iter()
                .map(DayScheduleEntry::Weekday)
                .collect(),
            tail,
        )),
    }
}

fn weekdays(spec: &str) -> CmdResult<Vec<Weekday>> {
    let bad = || strings::day_schedule::bad_days(spec);
    let mut out = Vec::new();
    for part in spec.split(',') {
        match part.trim().to_ascii_lowercase().as_str() {
            "daily" | "all" => out.extend(WEEKDAYS.iter().cloned()),
            "weekdays" => out.extend(WEEKDAYS.iter().take(5).cloned()),
            "weekend" => out.extend(WEEKDAYS.iter().skip(5).cloned()),
            other => out.push(Weekday::parse(&other.to_ascii_uppercase()).ok_or_else(bad)?),
        }
    }
    if out.is_empty() {
        return Err(bad().into());
    }
    out.dedup();
    Ok(out)
}

/// `set <entries> <wake>-<sleep>`, or `set date <d> none`.
async fn set(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let (targets, tail) = entries(args)?;
    let [value] = tail else {
        return Err(strings::day_schedule::usage().into());
    };
    for entry in targets {
        let key = entry.key();
        let spec =
            pref_spec(&key).ok_or_else(|| prefs_text::PrefTextError::UnknownKey(key.clone()))?;
        let value = prefs_text::parse_for(&key, spec, value)?;
        ctx.core
            .submit(Command::SetPreference {
                key: key.clone(),
                value: value.clone(),
                target: PrefTarget::Vault,
            })
            .await?;
        println!("{key} = {}", prefs_text::render(&value));
    }
    Ok(())
}

/// `clear <entries>`.
async fn clear(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let (targets, tail) = entries(args)?;
    if !tail.is_empty() {
        return Err(strings::day_schedule::usage().into());
    }
    for entry in targets {
        let key = entry.key();
        ctx.core
            .submit(Command::ClearPreference {
                key: key.clone(),
                target: PrefTarget::Vault,
            })
            .await?;
        println!("{key} cleared");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn each_entry_spelling_names_its_keys() {
        let keys = |s: &str| {
            let w = words(s);
            entries(&w)
                .unwrap()
                .0
                .iter()
                .map(DayScheduleEntry::key)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys("MO 07:00-23:00"), ["day_schedule.weekday.MO"]);
        assert_eq!(keys("weekend x").len(), 2);
        assert_eq!(keys("weekdays,sa x").len(), 6);
        assert_eq!(keys("daily x").len(), 7);
        assert_eq!(keys("day last x"), ["day_schedule.month_day.-1"]);
        assert_eq!(keys("day 1 x"), ["day_schedule.month_day.1"]);
        assert_eq!(
            keys("date 2026-12-25 none"),
            ["day_schedule.date.2026-12-25"]
        );
        for bad in [
            "XX 07:00-23:00",
            "day 0 x",
            "day 32 x",
            "date 2026-13-01 x",
            "day",
        ] {
            assert!(entries(&words(bad)).is_err(), "{bad}");
        }
    }
}
