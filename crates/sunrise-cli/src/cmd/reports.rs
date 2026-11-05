//! Reviews, briefs and the two interchange formats.
//!
//! Each report is one core query; what a client decides around it — the daily
//! glance's window, a brief's sections, the reminder schedule's cap and order —
//! is the client layer's ([`sunrise_client_core::review`],
//! [`sunrise_client_core::brief`], [`sunrise_client_core::reminders`]), so the
//! CLI says the same thing about the same morning as the apps do.

use sunrise_cli::i18n::strings;
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::Task;
use sunrise_id::{EntityKind, EntityRef};

use super::{CmdResult, Ctx};

/// `review [daily|save [note...]]`.
pub(super) async fn review(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    match rest.first().map(String::as_str) {
        None => weekly(ctx).await,
        Some("daily") => daily(ctx).await,
        Some("save") => save(ctx, &rest[1..]).await,
        Some(other) => Err(strings::review::unknown_mode(other).into()),
    }
}

async fn weekly_review(core: &Core) -> CmdResult<sunrise_domain::WeeklyReview> {
    let q = Query::WeeklyReview {
        week_start_ms: None,
        now_ms: core.now_ms(),
    };
    match core.query(q).await? {
        QueryResult::WeeklyReview(w) => Ok(*w),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// `review` — this week's counts, the same fold the Review view renders.
async fn weekly(ctx: &Ctx<'_>) -> CmdResult {
    let w = weekly_review(ctx.core).await?;
    if ctx.json {
        let reader = ctx.reader();
        sunrise_cli::json::print(&serde_json::json!({
            "window": {"start_ms": w.window.start_ms, "end_ms": w.window.end_ms},
            "totals": {
                "completed": w.totals.completed,
                "deferred": w.totals.deferred,
                "dropped": w.totals.dropped,
                "created": w.totals.created,
                "reopened": w.totals.reopened,
            },
            "streams": w.streams.iter().map(|s| serde_json::json!({
                "id": s.stream.to_str(),
                "name": s.name,
                "completed": s.completed.len(),
                "deferred": s.deferred.len(),
                "untouched": s.created_untouched.len(),
            })).collect::<Vec<_>>(),
            "slipped": sunrise_cli::json::tasks(&w.slipped, &reader),
        }));
        return Ok(());
    }
    println!(
        "completed {}  deferred {}  dropped {}  created {}  reopened {}",
        w.totals.completed,
        w.totals.deferred,
        w.totals.dropped,
        w.totals.created,
        w.totals.reopened
    );
    for s in &w.streams {
        println!(
            "  {:<24} {} done · {} deferred · {} untouched",
            s.name,
            s.completed.len(),
            s.deferred.len(),
            s.created_untouched.len()
        );
    }
    if !w.slipped.is_empty() {
        println!("slipped:");
        for t in &w.slipped {
            println!("  {}  {}", t.id.to_str(), t.title);
        }
    }
    Ok(())
}

/// One section of rows, headed, with its empty message when it has none.
fn section(title: &str, empty: &str, tasks: &[Task]) {
    println!("{title}");
    if tasks.is_empty() {
        println!("  {empty}");
    }
    for t in tasks {
        println!("  {}  {}", t.id.to_str(), t.title);
    }
}

/// `review daily` — the 60-second glance, from the start of yesterday in the
/// reader's zone ([`sunrise_client_core::review::daily_review_since_ms`]),
/// never `now − 24 h`.
async fn daily(ctx: &Ctx<'_>) -> CmdResult {
    let now_ms = ctx.core.now_ms();
    let since_ms = sunrise_client_core::review::daily_review_since_ms(now_ms, &ctx.reader());
    let QueryResult::DailyReview(d) = ctx
        .core
        .query(Query::DailyReview { since_ms, now_ms })
        .await?
    else {
        return Err(strings::general::unexpected_result().into());
    };
    if ctx.json {
        let reader = ctx.reader();
        sunrise_cli::json::print(&serde_json::json!({
            "since_ms": since_ms,
            "inbox": sunrise_cli::json::tasks(&d.inbox, &reader),
            "today": sunrise_cli::json::tasks(&d.today, &reader),
            "blocked": sunrise_cli::json::tasks(&d.blocked, &reader),
        }));
        return Ok(());
    }
    section(
        &strings::review::daily_inbox(),
        &strings::review::daily_inbox_empty(),
        &d.inbox,
    );
    section(
        &strings::review::daily_today(),
        &strings::review::daily_today_empty(),
        &d.today,
    );
    section(
        &strings::review::daily_blocked(),
        &strings::review::daily_blocked_empty(),
        &d.blocked,
    );
    Ok(())
}

/// `review save [note...]` — record that this week was reviewed, with the
/// counts on screen ([`sunrise_client_core::review::snapshot_draft_of`]).
async fn save(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let w = weekly_review(ctx.core).await?;
    let note = rest.join(" ");
    let draft = sunrise_client_core::review::snapshot_draft_of(&w, Some(&note));
    // Append-only and with no inverse, so not recorded for undo.
    let res = ctx.core.submit(Command::SaveReviewSnapshot(draft)).await?;
    println!("{}  {}", res.entity.to_str(), strings::review::saved());
    Ok(())
}

/// `brief morning|evening` — the two daily briefs, sectioned and headlined
/// by the client layer.
pub(super) async fn brief(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    use sunrise_client_core::brief;
    let now_ms = ctx.core.now_ms();
    let (which, headline, sections) = match rest.first().map(String::as_str) {
        Some("morning") | None => {
            let QueryResult::MorningSummary(s) =
                ctx.core.query(Query::MorningSummary { now_ms }).await?
            else {
                return Err(strings::general::unexpected_result().into());
            };
            (
                "morning",
                brief::morning_headline(Some(&s)),
                brief::morning_sections(&s, &ctx.reader()),
            )
        }
        Some("evening") => {
            let QueryResult::EndOfDayPlan(p) =
                ctx.core.query(Query::EndOfDayPlan { now_ms }).await?
            else {
                return Err(strings::general::unexpected_result().into());
            };
            (
                "evening",
                brief::evening_headline(Some(&p)),
                brief::evening_sections(&p),
            )
        }
        Some(other) => return Err(strings::brief::usage(other).into()),
    };
    if ctx.json {
        let reader = ctx.reader();
        sunrise_cli::json::print(&serde_json::json!({
            "brief": which,
            "headline": headline,
            "sections": sections.iter().map(|s| serde_json::json!({
                "id": s.id.as_str(),
                "title": s.title,
                "tasks": sunrise_cli::json::tasks(&s.tasks, &reader),
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    println!("{headline}");
    for s in sections {
        println!();
        section(s.title, s.empty_message, &s.tasks);
    }
    Ok(())
}

/// `export all <dir> [--no-fetch]` — everything, decrypted, in the open
/// format of `docs/04-storage/export-format.md`.
///
/// The writer is the client layer's (`sunrise_client_core::export`), so this
/// writes the same bytes the apps do. stdout gets the directory, for a script
/// to pick up; the count and each attachment left out go to stderr.
async fn export_all(core: &Core, rest: &[String]) -> CmdResult {
    use sunrise_client_core::export::{export_all, ExportOptions};
    use sunrise_core::export::MissingReason;
    let words = super::positionals(rest, &[], &["--no-fetch"]);
    let [dir] = words.as_slice() else {
        return Err(strings::export::all_usage().into());
    };
    let opts = ExportOptions {
        fetch: !super::has(rest, "--no-fetch"),
    };
    let manifest = export_all(core, std::path::Path::new(dir), opts).await?;
    for a in &manifest.attachments {
        let line = match a.missing_reason {
            None => continue,
            Some(MissingReason::NotFetched) => {
                strings::export::missing_not_fetched(&a.id, &a.filename)
            }
            Some(MissingReason::NeverUploaded) => {
                strings::export::missing_never_uploaded(&a.id, &a.filename)
            }
            Some(MissingReason::FetchFailed) => {
                strings::export::missing_fetch_failed(&a.id, &a.filename)
            }
        };
        eprintln!("{}", strings::general::note(&line));
    }
    eprintln!(
        "{}",
        strings::export::all_written(
            // The manifest is a file too, and is not in its own list.
            i64::try_from(manifest.files.len() + 1).unwrap_or(i64::MAX),
            dir
        )
    );
    println!("{dir}");
    Ok(())
}

/// `export <dataset> [json|csv] [path]`.
///
/// With no path the document goes to **stdout**: a CLI's stdout is its
/// contract, so `sunrise export trends json | jq` has to work.
pub(super) async fn export(core: &Core, rest: &[String]) -> CmdResult {
    use sunrise_domain::{ExportDataset, ExportFormat};
    let Some(name) = rest.first() else {
        return Err(strings::export::usage().into());
    };
    if name == "all" {
        return export_all(core, &rest[1..]).await;
    }
    let dataset = match name.as_str() {
        "trends" | "trend" => ExportDataset::Trends,
        "activity" | "timeline" => ExportDataset::Activity,
        "focus" => ExportDataset::Focus,
        "streaks" | "streak" => ExportDataset::Streaks,
        other => return Err(strings::export::unknown_dataset(other).into()),
    };
    let mut format = ExportFormat::Csv;
    let mut path: Option<String> = None;
    for w in &rest[1..] {
        match w.as_str() {
            "json" => format = ExportFormat::Json,
            "csv" => format = ExportFormat::Csv,
            other if path.is_none() => path = Some(other.to_string()),
            other => return Err(strings::general::unexpected_argument(other).into()),
        }
    }
    let q = Query::ExportStats {
        dataset,
        format,
        weeks: 12,
        now_ms: core.now_ms(),
    };
    let QueryResult::Export(body) = core.query(q).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    match path {
        Some(p) => {
            std::fs::write(&p, body.as_bytes())?;
            println!("{p}");
        }
        None => print!("{body}"),
    }
    Ok(())
}

/// `ical import …` / `ical export …` — RFC 5545 interchange.
///
/// One subcommand with two modes rather than a top-level `import` and a second
/// meaning for `export`: `sunrise export` already names the *stats* datasets,
/// and overloading it with a format that produces a calendar rather than a
/// table would make `sunrise export ics json` a question with no answer.
pub(super) async fn ical(core: &Core, rest: &[String]) -> CmdResult {
    match rest.first().map(String::as_str) {
        Some("import") => ical_import(core, &rest[1..]).await,
        Some("export") => ical_export(core, &rest[1..]).await,
        Some(other) => Err(strings::ical::unknown_mode(&format!("{other:?}")).into()),
        None => Err(strings::ical::usage().into()),
    }
}

/// `ical import <path|-> [--stream <id>] [--source <name>]`.
///
/// Idempotent by construction: the Block an event lands on is derived from
/// `(source, UID)`, so running this twice on the same file updates the same
/// Blocks instead of making a second copy of the calendar. `--source` is what
/// a user reaches for when two calendars really do share a UID and they want
/// both.
///
/// The Block ids go to **stdout**, one per line with the title, so a script
/// can pipe them onward. Everything the import could not carry goes to stderr,
/// where it is visible without corrupting that contract.
async fn ical_import(core: &Core, rest: &[String]) -> CmdResult {
    use sunrise_integrations::ical_vault::{import, ICS_SOURCE};

    let mut path: Option<&str> = None;
    // No stream unless `--stream` names one (ADR-0046).
    let mut stream = None;
    let mut source = ICS_SOURCE.to_string();
    let mut args = rest.iter();
    while let Some(w) = args.next() {
        match w.as_str() {
            "--stream" => {
                let v = args.next().ok_or_else(strings::ical::stream_needs_id)?;
                stream = sunrise_domain::home_stream(Some(
                    EntityRef::parse(v, EntityKind::Stream)
                        .map_err(|e| strings::general::not_a_stream_id(v, &e.to_string()))?,
                ));
            }
            "--source" => {
                let v = args.next().ok_or_else(strings::ical::source_needs_name)?;
                if v.trim().is_empty() {
                    return Err(strings::ical::source_needs_text().into());
                }
                source = v.trim().to_string();
            }
            other if path.is_none() => path = Some(other),
            other => return Err(strings::general::unexpected_argument(other).into()),
        }
    }
    let path = path.ok_or_else(strings::ical::import_usage)?;

    // `-` is the conventional spelling of stdin, and it is what makes
    // `curl … | sunrise ical import -` work without a temporary file.
    let text = if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?
    };

    let report = import(core, &text, stream, &source).await?;
    for n in &report.notices {
        let uid = n.uid.as_deref().unwrap_or("-");
        eprintln!(
            "{}",
            strings::general::note(&format!("[{}] {uid}: {}", n.code.as_str(), n.detail))
        );
    }
    for b in &report.blocks {
        let verb = if b.created { "new" } else { "upd" };
        println!("{verb}  {}  {}", b.block.to_str(), b.title);
    }
    eprintln!("{}", report.summary_line());
    Ok(())
}

/// `ical export [today|day|week] [path]`.
///
/// With no path the document goes to stdout, exactly as `sunrise export`
/// does — `sunrise ical export week | pbcopy` has to work.
async fn ical_export(core: &Core, rest: &[String]) -> CmdResult {
    use sunrise_integrations::ical_vault::{export, ExportWindow};

    let mut window = ExportWindow::Day;
    let mut path: Option<&str> = None;
    for w in rest {
        match w.as_str() {
            "today" | "day" => window = ExportWindow::Day,
            "week" => window = ExportWindow::Week,
            other if path.is_none() => path = Some(other),
            other => return Err(strings::general::unexpected_argument(other).into()),
        }
    }
    let body = export(core, window, core.now_ms()).await?;
    match path {
        Some(p) => {
            std::fs::write(p, body.as_bytes())?;
            println!("{p}");
        }
        None => print!("{body}"),
    }
    Ok(())
}
