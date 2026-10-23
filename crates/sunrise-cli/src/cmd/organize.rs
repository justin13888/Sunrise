//! Streams, contexts and routines: the vault's shape, listed and edited.
//!
//! Each plural is the listing and owns the verbs that change the set
//! (`streams create`, `contexts archive`, `routines skip`), the way
//! `streams move` always has. The singular (`stream <name>`) lists what is
//! *in* one, and a verb there would collide with a stream called `create`.

use sunrise_cli::i18n::strings;
use sunrise_client_core::editor::{routine_form, routine_patch, RoutineFields};
use sunrise_core::{Command, Query, QueryResult};
use sunrise_domain::{
    routine_rows, ContextDraft, ContextPatch, RRule, Routine, RoutineCatchupPolicy, RoutineDraft,
    StreamColor, StreamDraft, StreamPatch, TaskTemplate,
};
use sunrise_id::{EntityKind, EntityRef};

use super::{flag_value, positionals, resolve_context, resolve_stream, Archived, CmdResult, Ctx};

/// `streams [move|create|edit|archive|unarchive|delete …]`.
pub(super) async fn streams(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    match rest.first().map(String::as_str) {
        None => list_streams(ctx).await,
        Some("move") => move_stream(ctx, &rest[1..]).await,
        Some("create") => create_stream(ctx, &rest[1..]).await,
        Some("edit") => edit_stream(ctx, &rest[1..]).await,
        Some(verb @ ("archive" | "unarchive")) => {
            let id = resolve_stream(ctx.core, &rest[1..].join(" "), Archived::Include).await?;
            refuse_inbox(id)?;
            let archived = verb == "archive";
            let patch = StreamPatch {
                archived: Some(archived),
                ..Default::default()
            };
            ctx.submit(
                Command::UpdateStream { id, patch },
                strings::undo::archive_stream(&id.to_str()),
            )
            .await?;
            println!("{}  {verb}d", id.to_str());
            Ok(())
        }
        Some("delete") => {
            let id = resolve_stream(ctx.core, &rest[1..].join(" "), Archived::Include).await?;
            refuse_inbox(id)?;
            ctx.submit(
                Command::DeleteStream(id),
                strings::undo::delete(&id.to_str()),
            )
            .await?;
            println!("deleted  {}", id.to_str());
            Ok(())
        }
        Some(other) => Err(strings::streams::unknown_verb(other).into()),
    }
}

/// The Inbox is synthetic: it has no Stream entity to write to.
fn refuse_inbox(id: EntityRef) -> CmdResult {
    if id == sunrise_domain::inbox_stream_ref() {
        return Err(strings::streams::inbox_fixed().into());
    }
    Ok(())
}

async fn list_streams(ctx: &Ctx<'_>) -> CmdResult {
    if let QueryResult::Streams(rows) = ctx.core.query(Query::StreamList).await? {
        for s in rows {
            let mark = if s.archived { " [archived]" } else { "" };
            println!(
                "{}  {:<24} {} open{mark}",
                s.id.to_str(),
                s.name,
                s.open_task_count
            );
        }
    }
    Ok(())
}

/// A colour name, refusing one this build does not know rather than writing
/// it through as an unknown variant.
fn color(raw: &str) -> CmdResult<StreamColor> {
    let c = StreamColor::from_raw(&raw.trim().to_ascii_lowercase());
    if c.is_unknown() {
        return Err(strings::streams::unknown_color(raw).into());
    }
    Ok(c)
}

/// A note-shaped description from one argument.
fn description(text: &str) -> sunrise_domain::NoteBody {
    sunrise_domain::encode_note_body(&[sunrise_domain::NoteBlock::Paragraph {
        inline: vec![sunrise_domain::Inline::text(text.trim())],
    }])
}

/// `streams create <name>... [--color c] [--parent <stream>] [--icon i]`.
async fn create_stream(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    const FLAGS: &[&str] = &["--color", "--parent", "--icon", "--description"];
    let name = positionals(args, FLAGS, &[]).join(" ");
    if name.trim().is_empty() {
        return Err(strings::streams::create_usage().into());
    }
    let parent_id = match flag_value(args, "--parent") {
        Some(p) => Some(resolve_stream(ctx.core, &p, Archived::Exclude).await?),
        None => None,
    };
    let draft = StreamDraft {
        name: name.trim().to_owned(),
        color: flag_value(args, "--color")
            .as_deref()
            .map(color)
            .transpose()?,
        parent_id,
        icon: flag_value(args, "--icon"),
        description: flag_value(args, "--description").map(|d| description(&d)),
        ..Default::default()
    };
    let res = ctx
        .submit(Command::CreateStream(draft), strings::undo::create(&name))
        .await?;
    println!("{}  {}", res.entity.to_str(), name.trim());
    Ok(())
}

/// `streams edit <id|name>... [--name N] [--color c] [--icon i|-]
/// [--description d|-]`.
async fn edit_stream(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    const FLAGS: &[&str] = &["--name", "--color", "--icon", "--description"];
    let target = positionals(args, FLAGS, &[]).join(" ");
    let id = resolve_stream(ctx.core, &target, Archived::Include).await?;
    refuse_inbox(id)?;
    let clearable = |v: Option<String>| v.map(|v| (v != "-").then_some(v));
    let patch = StreamPatch {
        name: flag_value(args, "--name"),
        color: flag_value(args, "--color")
            .as_deref()
            .map(color)
            .transpose()?,
        icon: clearable(flag_value(args, "--icon")),
        description: clearable(flag_value(args, "--description"))
            .map(|d| d.map(|d| description(&d))),
        ..Default::default()
    };
    if patch.name.is_none()
        && patch.color.is_none()
        && patch.icon.is_none()
        && patch.description.is_none()
    {
        return Err(strings::streams::edit_usage().into());
    }
    ctx.submit(
        Command::UpdateStream { id, patch },
        strings::undo::edit(&target),
    )
    .await?;
    println!("{}  {}", id.to_str(), strings::general::updated());
    Ok(())
}

/// `sunrise streams move <id|name> before <id|name>` /
/// `sunrise streams move <id|name> last`.
///
/// The reorder the sidebar does by dragging, as one shot. Both halves are
/// joined rather than taken positionally, so an unquoted multi-word name works
/// exactly as it does for `sunrise stream home renovation`; `before` and
/// `last` are what separate the two names, which is why a stream may not be
/// called either. That is a cheaper rule than a flag, and it reads aloud.
///
/// The Inbox is refused on both sides: it is synthetic, it has no Stream
/// entity to write a key to, and it is pinned to the top of every listing.
async fn move_stream(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let core = ctx.core;
    let sep = args
        .iter()
        .position(|a| a == "before" || a == "last")
        .ok_or_else(strings::streams::move_usage)?;
    let moved = args[..sep].join(" ");
    let target = args[sep + 1..].join(" ");
    let to_last = args[sep] == "last";
    if moved.trim().is_empty() || to_last != target.trim().is_empty() {
        return Err(strings::streams::move_usage().into());
    }

    let moved = resolve_stream(core, &moved, Archived::Include).await?;
    let target = if to_last {
        None
    } else {
        Some(resolve_stream(core, &target, Archived::Include).await?)
    };
    if moved == sunrise_domain::inbox_stream_ref() || target == Some(moved) {
        return Err(strings::streams::inbox_fixed().into());
    }

    let QueryResult::Streams(rows) = core.query(Query::StreamList).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    // Everything but the moved row and the synthetic Inbox, in list order.
    // Dropping the moved row first is what makes "move it one place down"
    // work: its own key must not be one of the bounds it lands between.
    let others: Vec<_> = rows
        .iter()
        .filter(|s| s.id != moved && s.id != sunrise_domain::inbox_stream_ref())
        .collect();
    let at = match target {
        None => others.len(),
        Some(t) => others
            .iter()
            .position(|s| s.id == t)
            .ok_or_else(strings::streams::not_in_list)?,
    };
    let after = at.checked_sub(1).map(|i| others[i].sort_order.as_str());
    let before = others.get(at).map(|s| s.sort_order.as_str());

    let key = sunrise_domain::sort_order::between(after, before)
        .map_err(|e| strings::streams::cannot_place(&e.to_string()))?;
    let patch = StreamPatch {
        sort_order: Some(key),
        ..Default::default()
    };
    ctx.submit(
        Command::UpdateStream { id: moved, patch },
        strings::undo::reorder(),
    )
    .await?;

    // Print the new order, because the whole point of the command is the
    // order and a silent success would have to be checked with a second one.
    if let QueryResult::Streams(rows) = core.query(Query::StreamList).await? {
        for s in rows {
            println!("{}  {}", s.id.to_str(), s.name);
        }
    }
    Ok(())
}

/// `stream <id|name>` — the tasks in one stream.
///
/// Singular lists the tasks *in* one; plural lists the rows. The argument is
/// joined rather than taken as `rest[0]` so an unquoted multi-word name
/// (`sunrise stream home renovation`) works, exactly as `capture` and
/// `search` already join theirs.
pub(super) async fn stream_tasks(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let id = resolve_stream(ctx.core, &rest.join(" "), Archived::Include).await?;
    super::tasks::list(ctx.core.query(Query::StreamTasks(id)).await?);
    Ok(())
}

/// `context <id|name>` — the tasks carrying one context.
pub(super) async fn context_tasks(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let id = resolve_context(ctx.core, &rest.join(" "), Archived::Include).await?;
    super::tasks::list(ctx.core.query(Query::ContextTasks(id)).await?);
    Ok(())
}

/// `contexts [create|edit|archive|unarchive|delete …]`.
pub(super) async fn contexts(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let args = rest.get(1..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        None => {
            if let QueryResult::Contexts(rows) = ctx.core.query(Query::Contexts).await? {
                for c in rows {
                    let mark = if c.archived { " [archived]" } else { "" };
                    println!(
                        "{}  @{:<20} {} tasks{mark}",
                        c.id.to_str(),
                        c.name,
                        c.task_count
                    );
                }
            }
            Ok(())
        }
        Some("create") => {
            let name = positionals(args, &["--description"], &[]).join(" ");
            let name = name.trim().trim_start_matches('@').to_owned();
            if name.is_empty() {
                return Err(strings::contexts::create_usage().into());
            }
            let draft = ContextDraft {
                name: name.clone(),
                description: flag_value(args, "--description"),
            };
            let res = ctx
                .submit(Command::CreateContext(draft), strings::undo::create(&name))
                .await?;
            println!("{}  @{name}", res.entity.to_str());
            Ok(())
        }
        Some("edit") => {
            let target = positionals(args, &["--name", "--description"], &[]).join(" ");
            let id = resolve_context(ctx.core, &target, Archived::Include).await?;
            let patch = ContextPatch {
                name: flag_value(args, "--name").map(|n| n.trim_start_matches('@').to_owned()),
                description: flag_value(args, "--description").map(|d| (d != "-").then_some(d)),
                archived: None,
            };
            if patch.name.is_none() && patch.description.is_none() {
                return Err(strings::contexts::edit_usage().into());
            }
            ctx.submit(
                Command::UpdateContext { id, patch },
                strings::undo::edit(&target),
            )
            .await?;
            println!("{}  {}", id.to_str(), strings::general::updated());
            Ok(())
        }
        Some(verb @ ("archive" | "unarchive")) => {
            let id = resolve_context(ctx.core, &args.join(" "), Archived::Include).await?;
            let patch = ContextPatch {
                archived: Some(verb == "archive"),
                ..Default::default()
            };
            ctx.submit(
                Command::UpdateContext { id, patch },
                strings::undo::archive_context(&id.to_str()),
            )
            .await?;
            println!("{}  {verb}d", id.to_str());
            Ok(())
        }
        Some("delete") => {
            let id = resolve_context(ctx.core, &args.join(" "), Archived::Include).await?;
            ctx.submit(
                Command::DeleteContext(id),
                strings::undo::delete(&id.to_str()),
            )
            .await?;
            println!("deleted  {}", id.to_str());
            Ok(())
        }
        Some(other) => Err(strings::contexts::unknown_verb(other).into()),
    }
}

/// Every routine in the vault.
async fn all_routines(ctx: &Ctx<'_>) -> CmdResult<Vec<Routine>> {
    match ctx.core.query(Query::Routines).await? {
        QueryResult::Routines(rs) => Ok(rs),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// A routine by `rtn_` id, or by an exact or uniquely-prefixing title.
async fn resolve_routine(ctx: &Ctx<'_>, raw: &str) -> CmdResult<Routine> {
    let typed = raw.trim();
    if typed.is_empty() {
        return Err(strings::routines::needs_target().into());
    }
    let all = all_routines(ctx).await?;
    if let Ok(id) = EntityRef::parse(typed, EntityKind::Routine) {
        return all
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| strings::routines::no_match(typed).into());
    }
    let lower = typed.to_lowercase();
    if let Some(r) = all
        .iter()
        .find(|r| r.template.title.to_lowercase() == lower)
    {
        return Ok(r.clone());
    }
    let hits: Vec<&Routine> = all
        .iter()
        .filter(|r| r.template.title.to_lowercase().starts_with(&lower))
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(strings::routines::no_match(typed).into()),
        _ => Err(strings::routines::ambiguous(typed).into()),
    }
}

/// A rule body, `FREQ=…;…`, with an optional `RRULE:` prefix.
fn rule(raw: &str) -> CmdResult<RRule> {
    let body = raw.trim();
    let body = body.strip_prefix("RRULE:").unwrap_or(body);
    RRule::parse(body).map_err(|e| strings::routines::bad_rule(&e.to_string()).into())
}

fn catchup(raw: &str) -> CmdResult<RoutineCatchupPolicy> {
    let p = RoutineCatchupPolicy::from_raw(&raw.trim().to_ascii_lowercase());
    if p.is_unknown() {
        return Err(strings::routines::bad_catchup(raw).into());
    }
    Ok(p)
}

/// A priority, or `-` for none.
fn priority(raw: &str) -> CmdResult<Option<u8>> {
    if raw == "-" {
        return Ok(None);
    }
    match raw.parse::<u8>() {
        Ok(p @ 1..=5) => Ok(Some(p)),
        _ => Err(strings::capture::priority_out_of_range(raw).into()),
    }
}

/// `routines [create|edit|archive|unarchive|delete|skip …]`.
pub(super) async fn routines(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    let args = rest.get(1..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        None => {
            let rs = all_routines(ctx).await?;
            for r in routine_rows(&rs, ctx.now()) {
                let next = r.next.map_or_else(|| "-".to_string(), |t| t.to_string());
                let paused = if r.paused { " [paused]" } else { "" };
                println!(
                    "{}  {:<28} {:<24} next {next} streak {}{paused}",
                    r.id.to_str(),
                    r.title,
                    r.rrule,
                    r.streak
                );
            }
            Ok(())
        }
        Some("create") => create_routine(ctx, args).await,
        Some("edit") => edit_routine(ctx, args).await,
        Some(verb @ ("archive" | "unarchive" | "pause" | "resume")) => {
            let r = resolve_routine(ctx, &args.join(" ")).await?;
            let mut patch = sunrise_domain::RoutinePatch::default();
            match verb {
                "archive" => patch.archived = Some(true),
                "unarchive" => patch.archived = Some(false),
                "pause" => patch.paused = Some(true),
                _ => patch.paused = Some(false),
            }
            ctx.submit(
                Command::UpdateRoutine { id: r.id, patch },
                strings::undo::edit(&r.template.title),
            )
            .await?;
            println!("{}  {verb}d", r.id.to_str());
            Ok(())
        }
        Some("delete") => {
            let r = resolve_routine(ctx, &args.join(" ")).await?;
            ctx.submit(
                Command::DeleteRoutine(r.id),
                strings::undo::delete(&r.template.title),
            )
            .await?;
            println!("deleted  {}", r.id.to_str());
            Ok(())
        }
        Some("skip") => skip_routine(ctx, args).await,
        Some(other) => Err(strings::routines::unknown_verb(other).into()),
    }
}

/// `routines create <title>... --rule <RRULE> [--stream S] [--zone Z]
/// [--starts <when>] [--catchup skip|merge|queue] [--priority N]`.
///
/// The routine's zone defaults to the reader's: "every day at 07:00" means
/// 07:00 where its owner is when they set it up, and a routine that follows a
/// place is the deliberate `--zone`.
async fn create_routine(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    const FLAGS: &[&str] = &[
        "--rule",
        "--stream",
        "--zone",
        "--starts",
        "--catchup",
        "--priority",
    ];
    let title = positionals(args, FLAGS, &[]).join(" ");
    let (Some(rule_text), false) = (flag_value(args, "--rule"), title.trim().is_empty()) else {
        return Err(strings::routines::create_usage().into());
    };
    let reader = ctx.reader();
    let timezone = match flag_value(args, "--zone") {
        Some(z) => {
            sunrise_client_core::civil::reader_zone(&z)?;
            z
        }
        None => reader
            .iana_name()
            .map(str::to_owned)
            .ok_or_else(strings::routines::needs_zone)?,
    };
    let stream_id = match flag_value(args, "--stream") {
        Some(s) => resolve_stream(ctx.core, &s, Archived::Exclude).await?,
        None => sunrise_domain::inbox_stream_ref(),
    };
    let starts_at = match flag_value(args, "--starts") {
        Some(w) => sunrise_domain::capture::parse_when(&w, ctx.now(), &reader)
            .ok_or_else(|| strings::capture::unreadable_date(&w))?,
        None => ctx.now(),
    };
    let draft = RoutineDraft {
        template: TaskTemplate {
            title: title.trim().to_owned(),
            stream_id,
            contexts: Vec::new(),
            energy: None,
            priority: flag_value(args, "--priority")
                .as_deref()
                .map(priority)
                .transpose()?
                .flatten(),
            estimated_duration_s: None,
            body: None,
            unknown: sunrise_domain::Unknowns::new(),
        },
        rrule: rule(&rule_text)?,
        timezone,
        starts_at,
        ends_at: None,
        scheduling_constraints: Vec::new(),
        catchup_policy: flag_value(args, "--catchup")
            .as_deref()
            .map(catchup)
            .transpose()?
            .unwrap_or(RoutineCatchupPolicy::Skip),
    };
    let res = ctx
        .submit(
            Command::CreateRoutine(draft),
            strings::undo::create(title.trim()),
        )
        .await?;
    println!("{}  {}", res.entity.to_str(), title.trim());
    Ok(())
}

/// `routines edit <id|title>... [--title T] [--rule R] [--zone Z] [--stream S]
/// [--priority N|-] [--catchup p]`.
///
/// Through the client layer's routine editor
/// ([`sunrise_client_core::editor::routine_patch`]): the form starts as the
/// stored routine, the flags change what they name, and the patch carries only
/// what changed, so a template field a newer build wrote goes back untouched.
async fn edit_routine(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    const FLAGS: &[&str] = &[
        "--title",
        "--rule",
        "--zone",
        "--stream",
        "--priority",
        "--catchup",
    ];
    let target = positionals(args, FLAGS, &[]).join(" ");
    let r = resolve_routine(ctx, &target).await?;
    let stored = RoutineFields::from(&r);
    let mut form = routine_form(&stored);
    if let Some(t) = flag_value(args, "--title") {
        form.title = t;
    }
    if let Some(rr) = flag_value(args, "--rule") {
        form.rrule = rule(&rr)?;
    }
    if let Some(z) = flag_value(args, "--zone") {
        sunrise_client_core::civil::reader_zone(&z)?;
        form.timezone = z;
    }
    if let Some(s) = flag_value(args, "--stream") {
        form.stream_id = resolve_stream(ctx.core, &s, Archived::Exclude).await?;
    }
    if let Some(p) = flag_value(args, "--priority") {
        form.priority = priority(&p)?;
    }
    if let Some(c) = flag_value(args, "--catchup") {
        form.catchup_policy = catchup(&c)?;
    }
    let patch = routine_patch(&stored, &form)?;
    // The five fields the routine editor writes; nothing else is set here.
    if patch.template.is_none()
        && patch.rrule.is_none()
        && patch.timezone.is_none()
        && patch.catchup_policy.is_none()
        && patch.ends_at.is_none()
    {
        return Err(strings::routines::edit_usage().into());
    }
    ctx.submit(
        Command::UpdateRoutine { id: r.id, patch },
        strings::undo::edit(&r.template.title),
    )
    .await?;
    println!("{}  {}", r.id.to_str(), strings::general::updated());
    Ok(())
}

/// `routines skip <id|title>... [--at <YYYY-MM-DDTHH:MM>]` — skip one
/// occurrence: the next one, or the one keyed `--at` in the routine's zone.
///
/// The key is the client layer's
/// ([`sunrise_client_core::vocab::occurrence_key_at`]), read in the
/// routine's own zone, which is what every replica derives the same key in.
async fn skip_routine(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let target = positionals(args, &["--at"], &[]).join(" ");
    let r = resolve_routine(ctx, &target).await?;
    let key = if let Some(k) = flag_value(args, "--at") {
        k
    } else {
        let at = sunrise_client_core::vocab::next_occurrence_ms(r.clone(), ctx.core.now_ms())
            .ok_or_else(strings::routines::no_next)?;
        sunrise_client_core::vocab::occurrence_key_at(
            &r.timezone,
            sunrise_client_core::civil::instant(at)?,
        )?
    };
    // Not undoable: a skip has no inverse command, so it is submitted
    // directly rather than recorded as a step that would do nothing.
    ctx.core
        .submit(Command::SkipRoutineOccurrence {
            id: r.id,
            occurrence_key: key.clone(),
        })
        .await?;
    println!("{}  skipped {key}", r.id.to_str());
    Ok(())
}
