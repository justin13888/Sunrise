//! Capture, triage and the task views: everything whose subject is a task.

use sunrise_cli::i18n::strings;
use sunrise_cli::when::{self, When};
use sunrise_core::commands::FocusStartDraft;
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::{NoteBlock, NoteBody, Task, TaskPatch};
use sunrise_id::EntityRef;

use super::{
    context_names, has, named, positionals, read_task, split_task_ids, stream_names, task_id,
    unresolved_note, Archived, CmdResult, Ctx,
};

/// Print a task list, one `[x] id  title` row each.
pub(super) fn list(ctx: &Ctx<'_>, r: QueryResult) {
    let (QueryResult::Tasks(tasks) | QueryResult::StreamTasks(tasks)) = r else {
        return;
    };
    if ctx.json {
        sunrise_cli::json::print(&sunrise_cli::json::tasks(&tasks, &ctx.reader()));
        return;
    }
    for t in tasks {
        let mark = if t.state == sunrise_domain::TaskState::Done {
            "x"
        } else {
            " "
        };
        println!("[{mark}] {}  {}", t.id.to_str(), t.title);
    }
}

/// Say so on stderr when the reader's zone is a stand-in: a time the user
/// types is then read in UTC (`docs/11-adr/0066-times-survive-travel.md` §3).
fn note_unknown_zone(ctx: &Ctx<'_>) {
    if ctx.core.zone_unknown() {
        eprintln!("{}", strings::general::note(&strings::tz::unknown_note()));
    }
}

/// `capture <text>...` — parse one line with the capture grammar and commit
/// it.
pub(super) async fn capture(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let text = rest.join(" ");
    if text.trim().is_empty() {
        return Err(strings::capture::needs_text().into());
    }
    note_unknown_zone(ctx);
    // The reader's zone, so `^tomorrow 9am` means the user's 9am.
    let parsed = ctx.core.capture(&text, &ctx.reader()).await?;
    for u in &parsed.unresolved {
        // Warnings go to stderr so stdout stays parseable.
        eprintln!("{}", strings::general::note(&unresolved_note(u)));
    }
    let title = parsed.draft.title.clone();
    let res = ctx
        .submit(
            Command::CreateTask(parsed.draft),
            strings::undo::capture(&title),
        )
        .await?;
    println!("{}  {title}", res.entity.to_str());
    Ok(())
}

/// `today` — Today, in the reader's zone.
pub(super) async fn today(ctx: &Ctx<'_>) -> CmdResult {
    let q = Query::Today {
        now_ms: ctx.core.now_ms(),
        contexts: vec![],
    };
    list(ctx, ctx.core.query(q).await?);
    Ok(())
}

/// `search <query>...` — full-text search.
pub(super) async fn search(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let text = rest.join(" ");
    if text.trim().is_empty() {
        return Err(strings::search::needs_query().into());
    }
    list(
        ctx,
        ctx.core.query(Query::Search { text, limit: 100 }).await?,
    );
    Ok(())
}

/// `show <id>` — one task, every field a triage pass reads.
///
/// The row labels are what a script greps for, so they are not translated;
/// the values are the stored ones, each time with its kind (ADR-0066 §4).
pub(super) async fn show(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let [raw] = rest else {
        return Err(strings::show::usage().into());
    };
    let t = read_task(ctx.core, task_id(raw)?).await?;
    let reader = ctx.reader();
    if ctx.json {
        sunrise_cli::json::print(&sunrise_cli::json::task(&t, &reader));
        return Ok(());
    }
    let streams = stream_names(ctx.core, Archived::Include).await?;
    let contexts = context_names(ctx.core, Archived::Include).await?;
    let name_of = |rows: &[(EntityRef, String)], id: EntityRef| {
        rows.iter()
            .find(|(r, _)| *r == id)
            .map_or_else(|| id.to_str(), |(_, n)| n.clone())
    };
    let time = |v: Option<&sunrise_domain::SunriseTime>| {
        v.map_or_else(|| "-".to_owned(), |v| when::describe(v, &reader))
    };
    println!("{}  {}", t.id.to_str(), t.title);
    println!("state      {}", t.state.as_str());
    println!("stream     {}", name_of(&streams, t.stream_id));
    let ctx_names: Vec<String> = t
        .contexts
        .iter()
        .map(|c| format!("@{}", name_of(&contexts, *c)))
        .collect();
    println!(
        "contexts   {}",
        if ctx_names.is_empty() {
            "-".to_owned()
        } else {
            ctx_names.join(" ")
        }
    );
    println!(
        "priority   {}",
        t.priority.map_or_else(|| "-".to_owned(), |p| p.to_string())
    );
    println!(
        "energy     {}",
        t.energy
            .as_ref()
            .map_or("-", sunrise_domain::Energy::as_str)
    );
    println!(
        "estimate   {}",
        t.estimated_duration_s
            .map_or_else(|| "-".to_owned(), sunrise_domain::phrase::short_duration)
    );
    println!("scheduled  {}", time(t.scheduled_at.as_ref()));
    println!("due        {}", time(t.due_at.as_ref()));
    println!("deferred   {}", t.deferred_count);
    if let Some(body) = note_text(&t) {
        println!();
        println!("{body}");
    }
    Ok(())
}

/// `done <id>...` — complete tasks by id.
pub(super) async fn done(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    if rest.is_empty() {
        return Err(strings::done::needs_id().into());
    }
    for raw in rest {
        let id = task_id(raw)?;
        let title = read_task(ctx.core, id).await?.title;
        ctx.submit(Command::CompleteTask(id), strings::undo::complete(&title))
            .await?;
        println!("done  {raw}");
    }
    Ok(())
}

/// `edit <id>... <tokens>...` — change a task's facets with the annotate
/// grammar.
///
/// The grammar is [`sunrise_domain::annotate`]'s, unchanged and unextended:
/// `#stream @ctx @-ctx !N %energy ~30m ^when due:when`, with `-` clearing any
/// of them. It is the same vocabulary the app's edit line uses, which is the
/// point — `docs/07-clients/overview.md` §"What clients share" puts the
/// annotate grammar in `sunrise-domain` so two surfaces cannot disagree about
/// what `^next friday` means.
///
/// **Nothing is written unless the whole line parses.** Capture's rule is the
/// opposite — an unrecognised token stays in the title, because a capture line
/// *is* a title — but an edit has no title to fall back into, and a script
/// that mistyped `!9` and got four of its five changes applied is worse off
/// than one that got a non-zero exit. So the errors are reported first and the
/// command exits without touching the vault.
///
/// A `#stream` token becomes [`Command::PromoteToStream`] rather than a patch
/// field: moving between Streams re-keys the task's storage, and `TaskPatch`
/// deliberately does not carry it.
pub(super) async fn edit(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, tail) = split_task_ids(rest);
    if ids.is_empty() {
        return Err(strings::edit::usage().into());
    }
    let line = tail.join(" ");
    if line.trim().is_empty() {
        return Err(strings::edit::needs_change().into());
    }

    // Live rows only: an archived Stream or Context stays on the tasks that
    // carry it but must never be the target of new input.
    let streams = stream_names(ctx.core, Archived::Exclude).await?;
    let contexts = context_names(ctx.core, Archived::Exclude).await?;
    let streams = named(&streams);
    let contexts = named(&contexts);
    note_unknown_zone(ctx);
    // The reader's zone, so `^tomorrow 9am` means the user's 9am, exactly as
    // in `capture`.
    let tz = ctx.reader();
    let edit = sunrise_domain::parse_annotate(
        &line,
        &streams,
        &contexts,
        sunrise_domain::epoch_ms::from_u64(ctx.core.now_ms()),
        &tz,
    );
    if !edit.errors.is_empty() {
        for e in &edit.errors {
            eprintln!("{}", strings::general::note(&e.describe()));
        }
        return Err(strings::edit::nothing_changed().into());
    }
    if edit.is_empty() {
        return Err(strings::edit::needs_change().into());
    }

    let preview = edit.preview(&streams, &contexts, &tz);
    for id in ids {
        let task = read_task(ctx.core, id).await?;
        let current: Vec<EntityRef> = task.contexts.iter().copied().collect();
        let patch = edit.patch_for(&current);
        ctx.submit(
            Command::UpdateTask { id, patch },
            strings::undo::edit(&task.title),
        )
        .await?;
        if let Some(stream) = edit.stream() {
            ctx.submit(
                Command::PromoteToStream { id, stream },
                strings::undo::move_task(&task.title),
            )
            .await?;
        }
        println!("{}  {preview}", id.to_str());
    }
    Ok(())
}

/// `retitle <id> <text>...` — give one task a new title.
///
/// **Its own verb rather than a token in the annotate grammar, and rather than
/// a flag on `edit`.** [`sunrise_domain::annotate`] states the rule this
/// preserves: an edit line is not a title, so a bare word there is refused and
/// one bad token rejects the whole line. A title is free text containing
/// spaces and, sooner or later, a `#` or a `!`; putting it inside that grammar
/// would make a bare word mean "title text" in one reading and "malformed
/// token" in another. A `--title` flag has the same defect one level up — it
/// has to swallow the rest of the line, so `edit <id> --title fix the typo !1`
/// has no answer for where the title stops.
///
/// **One task, not a list.** A title is the field that tells two tasks apart,
/// so applying one to five is a mistake worth refusing rather than a bulk
/// operation worth offering.
pub(super) async fn retitle(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, tail) = split_task_ids(rest);
    let [id] = ids[..] else {
        if ids.is_empty() {
            return Err(strings::retitle::usage().into());
        }
        return Err(strings::retitle::one_task().into());
    };
    // A title is the one field where empty is not a value to store but a line
    // to refuse — `!-` clears a priority, and nothing clears a title.
    let title = sunrise_domain::validation::validate_title(
        &tail.join(" "),
        "task.title",
        sunrise_domain::MAX_TASK_TITLE_LEN,
    )
    .map_err(|e| strings::retitle::bad_title(&e.to_string()))?;

    // Read first, so a mistyped id fails before anything is written and the
    // old title can be shown beside the new one.
    let before = read_task(ctx.core, id).await?;
    let patch = TaskPatch {
        title: Some(title.clone()),
        ..Default::default()
    };
    ctx.submit(
        Command::UpdateTask { id, patch },
        strings::undo::retitle(&before.title),
    )
    .await?;
    // Both titles, because the whole point of the command is that the old one
    // was wrong and a silent success would have to be checked with a `search`.
    println!("{}  {title}  (was {:?})", id.to_str(), before.title);
    Ok(())
}

/// The ids and the date phrase of `defer` and `schedule`.
fn ids_and_phrase(
    rest: &[String],
    usage: fn() -> String,
    needs_date: fn() -> String,
) -> CmdResult<(Vec<EntityRef>, String)> {
    let (ids, tail) = split_task_ids(rest);
    if ids.is_empty() {
        return Err(usage().into());
    }
    let phrase = tail.join(" ");
    if phrase.trim().is_empty() {
        return Err(needs_date().into());
    }
    Ok((ids, phrase))
}

/// `defer <id>... <when>` — push tasks out to a new date.
///
/// Separate from `edit ^when`, which merely sets `scheduled_at`. This is
/// [`Command::DeferTaskTo`], which also bumps the task's `deferred_count`, and
/// that counter is what `sunrise review` reports as "deferred" — a task moved
/// four times is the signal the weekly review exists to surface.
///
/// A day (`tomorrow`, `next friday`, `+3d`) goes through the client layer's
/// [`sunrise_client_core::schedule::defer_command`]: civil days in the
/// reader's zone, and the plan keeps its kind and time of day, so a floating
/// 09:00 deferred to tomorrow is 09:00 tomorrow wherever the user is
/// (`docs/11-adr/0066-times-survive-travel.md` §4). A phrase that names a time
/// as well (`tomorrow 3pm`) is the user choosing the time, and is written as
/// typed. Never an instant computed as `now + N × 24 h`.
pub(super) async fn defer(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, phrase) = ids_and_phrase(rest, strings::defer::usage, strings::defer::needs_date)?;
    note_unknown_zone(ctx);
    let reader = ctx.reader();
    let now = ctx.now();
    let plan = when::read(&phrase, now, &reader)
        .ok_or_else(|| strings::capture::unreadable_date(&phrase))?;
    for id in ids {
        let task = read_task(ctx.core, id).await?;
        let cmd = match &plan {
            When::Days(days) => sunrise_client_core::schedule::defer_command(
                id,
                task.scheduled_at.as_ref(),
                *days,
                now,
                &reader,
            )?,
            When::At(to) => Command::DeferTaskTo { id, to: to.clone() },
        };
        let Command::DeferTaskTo { to, .. } = &cmd else {
            unreachable!("both arms build a DeferTaskTo");
        };
        let shown = when::describe(to, &reader);
        ctx.submit(cmd, strings::undo::defer(&task.title)).await?;
        println!("{}  deferred to {shown}", id.to_str());
    }
    Ok(())
}

/// `schedule <id>... <when>|-` — give tasks a plan without counting a
/// deferral.
///
/// A day is that day, all day ([`sunrise_client_core::schedule::scheduled_on_day`]):
/// "schedule for tomorrow" names no time of day, so none is invented. A phrase
/// with a time is written as typed, floating. `-` clears the plan.
pub(super) async fn schedule(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let (ids, phrase) = ids_and_phrase(
        rest,
        strings::schedule::usage,
        strings::schedule::needs_date,
    )?;
    note_unknown_zone(ctx);
    let reader = ctx.reader();
    let now = ctx.now();
    let value = if phrase.trim() == "-" {
        None
    } else {
        Some(
            match when::read(&phrase, now, &reader)
                .ok_or_else(|| strings::capture::unreadable_date(&phrase))?
            {
                When::Days(days) => {
                    sunrise_client_core::schedule::scheduled_on_day(days, now, &reader)?
                }
                When::At(v) => v,
            },
        )
    };
    for id in ids {
        let task = read_task(ctx.core, id).await?;
        let patch = TaskPatch {
            scheduled_at: Some(value.clone()),
            ..Default::default()
        };
        ctx.submit(
            Command::UpdateTask { id, patch },
            strings::undo::schedule(&task.title),
        )
        .await?;
        let shown = value
            .as_ref()
            .map_or_else(|| "-".to_owned(), |v| when::describe(v, &reader));
        println!("{}  scheduled {shown}", id.to_str());
    }
    Ok(())
}

/// `drop <id>...` — soft-delete tasks.
///
/// Named for the review's own vocabulary: `sunrise review` counts "dropped",
/// and a triage surface that can only say yes (`done`) and not no is half a
/// surface. Soft, like every other client's delete — the op is a tombstone,
/// not an erase, and like every delete it cannot be undone.
pub(super) async fn drop_tasks(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    if rest.is_empty() {
        return Err(strings::drop::needs_id().into());
    }
    for raw in rest {
        let id = task_id(raw)?;
        ctx.submit(Command::DeleteTask(id), strings::undo::drop(raw))
            .await?;
        println!("dropped  {raw}");
    }
    Ok(())
}

/// A task's note as plain text, `None` when it has none.
fn note_text(t: &Task) -> Option<String> {
    let body = t.body.as_ref().filter(|b| !b.is_empty())?;
    let doc = sunrise_domain::decode_note_body(body);
    Some(sunrise_domain::note_body_to_markdown(&doc.blocks))
}

/// Plain text as a note: one paragraph per blank-line-separated run.
fn note_from(text: &str) -> NoteBody {
    let blocks: Vec<NoteBlock> = text
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| NoteBlock::Paragraph {
            inline: vec![sunrise_domain::Inline::text(p)],
        })
        .collect();
    sunrise_domain::encode_note_body(&blocks)
}

/// `note <id> [<text>...|-|--file <path|->] [--force]` — read or replace a
/// task's note.
///
/// With nothing after the id it prints the note. Text replaces it, one
/// paragraph per blank-line-separated run; `-` clears it; `--file` reads the
/// text from a file or stdin. A note this build cannot write back exactly — a
/// newer build's block, say — is refused unless `--force` says losing the
/// rest of it is meant (`docs/02-domain/notes.md`).
pub(super) async fn note(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let words = positionals(rest, &["--file"], &["--force"]);
    let Some((raw, text)) = words.split_first() else {
        return Err(strings::note::usage().into());
    };
    let id = task_id(raw)?;
    let task = read_task(ctx.core, id).await?;
    let file = super::flag_value(rest, "--file");
    if text.is_empty() && file.is_none() {
        match note_text(&task) {
            Some(body) => println!("{body}"),
            None => eprintln!("{}", strings::note::empty()),
        }
        return Ok(());
    }
    if let Some(body) = &task.body {
        let exact = sunrise_domain::decode_note_body(body).is_exact();
        if !exact && !has(rest, "--force") {
            return Err(strings::note::not_exact().into());
        }
    }
    let new = match file.as_deref() {
        Some("-") => {
            use std::io::Read as _;
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Some(note_from(&s))
        }
        Some(path) => Some(note_from(
            &std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?,
        )),
        None if text == ["-"] => None,
        None => Some(note_from(&text.join(" "))),
    };
    let new = new.filter(|b| !b.is_empty());
    let cleared = new.is_none();
    let patch = TaskPatch {
        body: Some(new),
        ..Default::default()
    };
    ctx.submit(
        Command::UpdateTask { id, patch },
        strings::undo::note(&task.title),
    )
    .await?;
    if cleared {
        println!("{}  {}", id.to_str(), strings::note::cleared());
    } else {
        println!("{}  {}", id.to_str(), strings::note::saved());
    }
    Ok(())
}

/// `next` / `focus next` — the planner's ranked picks, optionally opening
/// a session on the top one.
///
/// The ranking is the core's (`Query::FocusPlan`), not a re-sort here:
/// "what should I do next" must give the same answer here and in the app,
/// or one of them is lying.
pub(super) async fn next(ctx: &Ctx<'_>, start: bool) -> CmdResult {
    /// Enough to choose from without becoming a list.
    const PICKS: u32 = 5;
    let core = ctx.core;
    let q = Query::FocusPlan {
        stream: None,
        energy: None,
        length: sunrise_domain::SessionLength::OnePomodoro,
        limit: PICKS,
    };
    let QueryResult::FocusPlan(rows) = core.query(q).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    if ctx.json && !start {
        let reader = ctx.reader();
        sunrise_cli::json::print(
            &rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "task": sunrise_cli::json::task(&r.task, &reader),
                        "unblocks": r.unblocks,
                    })
                })
                .collect(),
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("{}", strings::focus::nothing_actionable());
        return Ok(());
    }
    for (i, r) in rows.iter().enumerate() {
        let marker = if i == 0 && start { "▶" } else { " " };
        println!(
            "{marker} {}  {:<40} unblocks {}",
            r.task.id.to_str(),
            r.task.title,
            r.unblocks
        );
    }
    if start {
        let top = &rows[0];
        core.submit(Command::StartFocus(FocusStartDraft {
            task_id: top.task.id,
            kind: sunrise_domain::FocusKind::Work,
            length: sunrise_domain::SessionLength::OnePomodoro,
            energy: None,
        }))
        .await?;
        println!("{}", strings::focus::started_on(&top.task.title));
    }
    Ok(())
}

/// `focus [next|end [--done]|<id>]`.
///
/// `end` sits beside `next` because neither is a task id (`tsk_…` is the only
/// thing `focus <id>` accepts), so a bare word here is unambiguously a mode
/// and never a mistyped target. A focus session is append-only and has no
/// inverse, so none of these is recorded for undo.
pub(super) async fn focus(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let core = ctx.core;
    match rest.first().map(String::as_str) {
        Some("next") | None => next(ctx, true).await,
        Some("end") => end_focus(core, &rest[1..]).await,
        Some(raw) => {
            let id = task_id(raw)?;
            let res = core
                .submit(Command::StartFocus(FocusStartDraft {
                    task_id: id,
                    kind: sunrise_domain::FocusKind::Work,
                    length: sunrise_domain::SessionLength::OnePomodoro,
                    energy: None,
                }))
                .await?;
            println!("{}  focus session started", res.entity.to_str());
            Ok(())
        }
    }
}

/// `focus end [--done]` — close the running session(s).
///
/// The session id is looked up rather than typed: `focus <id>` and `next` both
/// take a **task**, and neither prints the `fcs_` id they minted, so requiring
/// one here would name a thing this surface never gave the user.
///
/// Every running session is closed, not just one. Two devices starting
/// concurrently mint different ids and both are valid
/// ([`Command::StartFocus`]), so "end my focus" means all of them.
///
/// `actual_focused_ms` is left `None`, which freezes the derived elapsed time.
/// A one-shot process tracked no pauses, so it has no smaller real figure to
/// offer and must not invent one.
async fn end_focus(core: &Core, rest: &[String]) -> CmdResult {
    let completed_task = match rest.first().map(String::as_str) {
        None => false,
        Some("--done") if rest.len() == 1 => true,
        Some(other) => return Err(strings::focus::end_usage(&format!("{other:?}")).into()),
    };
    let QueryResult::FocusSessions(rows) = core.query(Query::RunningFocusSessions).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    if rows.is_empty() {
        return Err(strings::focus::none_running().into());
    }
    for r in rows {
        let id = r.session.start.id;
        core.submit(Command::EndFocus {
            session: id,
            actual_focused_ms: None,
            completed_task,
        })
        .await?;
        // Minutes, because a focus session is reported in minutes everywhere
        // else and a millisecond count is not a thing anyone reads.
        println!("{}  focused {}m", id.to_str(), r.focused_ms / 60_000);
    }
    Ok(())
}
