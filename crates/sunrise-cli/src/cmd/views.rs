//! `views` — saved views, a synced entity of the vault (#341).
//!
//! A view saved here exists on every device of the account, and names its
//! contexts by id, so renaming a context keeps it working. The verbs read
//! and write the entity through the core; nothing writes `views.toml` any
//! more. A device's old file is imported once per vault, the first time
//! `views` runs against it ([`import_once`]), and a marker beside the vault
//! records that it was.

use std::collections::BTreeMap;

use sunrise_cli::i18n::strings;
use sunrise_client_core::views::{
    describe_entity, parse_view_kind, plan_import, primary_view, SavedViewStore, View,
    IMPORT_MARKER,
};
use sunrise_core::{Command, Query, QueryResult};
use sunrise_domain::{SavedView, SavedViewDraft, SavedViewPatch, ViewFilter, ViewSort};
use sunrise_id::EntityRef;

use super::{
    context_names, flag_value, positionals, resolve_context, resolve_stream, stream_names,
    Archived, CmdResult, Ctx,
};

/// The live views, in the list's order.
async fn live_views(ctx: &Ctx<'_>) -> CmdResult<Vec<SavedView>> {
    match ctx.core.query(Query::SavedViews).await? {
        QueryResult::SavedViews(v) => Ok(v),
        _ => Err(strings::general::unexpected_result().into()),
    }
}

/// The one live view named `name`, or the error saying there is none or
/// more than one (two devices can each save one name while apart).
async fn view_named(ctx: &Ctx<'_>, name: &str) -> CmdResult<SavedView> {
    if let Ok(id) = EntityRef::parse(name, sunrise_id::EntityKind::SavedView) {
        if let Some(v) = live_views(ctx).await?.into_iter().find(|v| v.id == id) {
            return Ok(v);
        }
    }
    let mut found: Vec<SavedView> = live_views(ctx)
        .await?
        .into_iter()
        .filter(|v| v.name == name)
        .collect();
    match found.len() {
        0 => Err(strings::views::no_such(name).into()),
        1 => Ok(found.remove(0)),
        _ => Err(strings::views::ambiguous(
            name,
            &found
                .iter()
                .map(|v| v.id.to_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
        .into()),
    }
}

/// Every context's current name, by id.
async fn context_index(ctx: &Ctx<'_>) -> CmdResult<BTreeMap<EntityRef, String>> {
    Ok(context_names(ctx.core, Archived::Include)
        .await?
        .into_iter()
        .collect())
}

/// Import this device's `views.toml` into the open vault, once.
///
/// Context and stream names resolve to ids; one that does not is said on
/// stderr and kept on the view. A view the vault already has by name is
/// left alone, so a second device importing the same file adds nothing.
/// The marker is written only when every view was saved: an import a paired
/// device's older build refuses is tried again next time.
async fn import_once(ctx: &Ctx<'_>) -> CmdResult {
    let marker = ctx.vault_dir.join(IMPORT_MARKER);
    if marker.exists() {
        return Ok(());
    }
    let store = SavedViewStore::at(sunrise_client_core::views::config_path());
    let (file, warnings) = store.load();
    for w in warnings {
        eprintln!("{}", strings::general::note(&w));
    }
    if !file.is_empty() {
        let contexts = context_names(ctx.core, Archived::Include).await?;
        let streams = stream_names(ctx.core, Archived::Include).await?;
        let plan = plan_import(&file, &contexts, &streams, &live_views(ctx).await?);
        for (view, name) in &plan.unresolved {
            eprintln!(
                "{}",
                strings::general::note(&strings::views::import_unresolved(view, name))
            );
        }
        for d in plan.drafts {
            let name = d.name.clone();
            // Not an undo step: nothing the user did this run.
            if let Err(e) = ctx.core.submit(Command::CreateSavedView(d)).await {
                eprintln!(
                    "{}",
                    strings::general::note(&strings::views::import_failed(&name, &e.to_string()))
                );
                return Ok(());
            }
            eprintln!(
                "{}",
                strings::general::note(&strings::views::imported(&name))
            );
        }
    }
    std::fs::write(&marker, b"")?;
    Ok(())
}

/// `views [list|save|apply|rename|delete …]`.
pub(super) async fn views(ctx: &Ctx<'_>, rest: &[String]) -> CmdResult {
    import_once(ctx).await?;
    let args = rest.get(1..).unwrap_or_default();
    match rest.first().map(String::as_str) {
        None | Some("list") => list(ctx).await,
        Some("save") => save(ctx, args).await,
        Some("apply") => {
            let [name] = args else {
                return Err(strings::views::apply_usage().into());
            };
            apply(ctx, &view_named(ctx, name).await?).await
        }
        Some("rename") => {
            let [name, to] = args else {
                return Err(strings::views::rename_usage().into());
            };
            let v = view_named(ctx, name).await?;
            let patch = SavedViewPatch {
                name: Some(to.clone()),
                ..SavedViewPatch::default()
            };
            ctx.submit(
                Command::UpdateSavedView { id: v.id, patch },
                strings::undo::edit(&v.name),
            )
            .await?;
            println!("renamed  {name} -> {to}");
            Ok(())
        }
        Some("delete") => {
            let [name] = args else {
                return Err(strings::views::delete_usage().into());
            };
            let v = view_named(ctx, name).await?;
            ctx.submit(
                Command::DeleteSavedView(v.id),
                strings::undo::delete(&v.name),
            )
            .await?;
            println!("deleted  {name}");
            Ok(())
        }
        Some(other) => Err(strings::views::unknown_verb(other).into()),
    }
}

async fn list(ctx: &Ctx<'_>) -> CmdResult {
    let views = live_views(ctx).await?;
    let names = context_index(ctx).await?;
    let name_of = |id: EntityRef| names.get(&id).cloned();
    if ctx.json {
        sunrise_cli::json::print(
            &views
                .iter()
                .map(|v| sunrise_cli::json::saved_view(v, &name_of, &describe_entity(v, &name_of)))
                .collect(),
        );
        return Ok(());
    }
    for v in views {
        println!("{:<16} {}", v.name, describe_entity(&v, &name_of));
    }
    Ok(())
}

/// `views save <name> <view> [--query <text>] [--contexts a,b] [--stream s] [--sort k]`.
///
/// Saving a name the vault already has edits that view; otherwise it is a
/// new one. Contexts and streams are resolved now, so a typo is refused now
/// and not on recall. A Stream view's `--query` names its stream, as it did
/// when views lived in a file.
async fn save(ctx: &Ctx<'_>, args: &[String]) -> CmdResult {
    let words = positionals(args, &["--query", "--contexts", "--stream", "--sort"], &[]);
    let [name, view] = words.as_slice() else {
        return Err(strings::views::save_usage().into());
    };
    let kind = parse_view_kind(view)
        .ok_or_else(|| strings::views::bad_spec(&format!("unknown view {view:?}")))?;
    let mut filter = ViewFilter::default();
    if let Some(list) = flag_value(args, "--contexts") {
        for c in list.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            let id = resolve_context(ctx.core, c, Archived::Include).await?;
            if !filter.contexts.contains(&id) {
                filter.contexts.push(id);
            }
        }
    }
    let query = flag_value(args, "--query").unwrap_or_default();
    let stream = flag_value(args, "--stream").or_else(|| {
        (primary_view(&kind) == Some(View::Stream) && !query.trim().is_empty())
            .then(|| query.clone())
    });
    if let Some(s) = &stream {
        filter
            .streams
            .push(resolve_stream(ctx.core, s, Archived::Include).await?);
    }
    if flag_value(args, "--stream").is_some() || primary_view(&kind) != Some(View::Stream) {
        filter.text = query;
    }
    let sort = flag_value(args, "--sort").map_or(ViewSort::Default, |s| ViewSort::from_raw(&s));
    if sort.is_unknown() {
        return Err(strings::views::bad_spec(&format!("unknown sort {:?}", sort.as_str())).into());
    }
    let existing = live_views(ctx).await?.into_iter().find(|v| &v.name == name);
    let saved = if let Some(v) = existing {
        let patch = SavedViewPatch {
            view: Some(kind),
            filter: Some(ViewFilter {
                unknown: v.filter.unknown.clone(),
                ..filter
            }),
            sort: Some(sort),
            ..SavedViewPatch::default()
        };
        ctx.submit(
            Command::UpdateSavedView { id: v.id, patch },
            strings::undo::edit(name),
        )
        .await?
        .entity
    } else {
        let draft = SavedViewDraft {
            name: name.clone(),
            view: kind,
            filter,
            sort,
        };
        ctx.submit(Command::CreateSavedView(draft), strings::undo::create(name))
            .await?
            .entity
    };
    let names = context_index(ctx).await?;
    let name_of = |id: EntityRef| names.get(&id).cloned();
    if let Some(v) = live_views(ctx).await?.into_iter().find(|v| v.id == saved) {
        println!("{:<16} {}", v.name, describe_entity(&v, &name_of));
    }
    Ok(())
}

/// Recall a view: the core evaluates it now, and the tasks it matches are
/// listed. A view of a screen with no list (calendar, focus, …) runs that
/// screen. Contexts that no longer exist, and names an import could not
/// resolve, are said on stderr rather than silently filtering everything
/// away.
async fn apply(ctx: &Ctx<'_>, view: &SavedView) -> CmdResult {
    let QueryResult::SavedView(result) = ctx.core.query(Query::SavedView(view.id)).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    for c in &result.missing_contexts {
        eprintln!(
            "{}",
            strings::general::note(&strings::views::missing_context(&c.to_str()))
        );
    }
    for n in &result.unresolved_names {
        eprintln!(
            "{}",
            strings::general::note(&strings::views::unresolved_name(n))
        );
    }
    if !result.complete {
        eprintln!("{}", strings::general::note(&strings::views::incomplete()));
    }
    match primary_view(&view.view) {
        Some(View::Calendar) => super::blocks::list(ctx, &[]).await,
        Some(View::Focus) => super::tasks::next(ctx, false).await,
        Some(View::Routines) => super::organize::routines(ctx, &[]).await,
        Some(View::Review) => super::reports::review(ctx, &[]).await,
        Some(View::Morning) => super::reports::brief(ctx, &["morning".to_owned()]).await,
        Some(View::Evening) => super::reports::brief(ctx, &["evening".to_owned()]).await,
        _ => {
            super::tasks::list(ctx, QueryResult::Tasks(result.tasks));
            Ok(())
        }
    }
}
