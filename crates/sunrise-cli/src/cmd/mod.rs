//! Every subcommand that runs against an open vault, and what they share.
//!
//! [`dispatch`] is the one match over subcommand names; each family lives in
//! a module of its own. A command reads through [`Ctx::core`] and writes
//! through [`Ctx::submit`], which records the write in this vault's undo
//! history (`sunrise undo`), so no command reaches `Core::submit` directly
//! unless it is a write with nothing to undo (a revocation, a focus session).
//!
//! stdout is the contract a script reads and stderr is for the person, so
//! printing is allowed in this module tree and nowhere in the library.

#![allow(clippy::print_stdout, clippy::print_stderr)]

mod account;
mod blocks;
mod conflicts;
mod day_schedule;
mod devices;
mod files;
mod order;
mod organize;
mod plumbing;
mod reminders;
mod reports;
mod search;
mod settings;
mod tasks;
mod triage;
mod upcoming;
mod views;

pub(crate) use account::recover;

use std::cell::Cell;
use std::path::Path;

use sunrise_cli::i18n::strings;
use sunrise_client_core::history::UndoHistory;
use sunrise_core::{Command, CommandResult, Core, CoreError, Query, QueryResult};
use sunrise_id::{EntityKind, EntityRef};

/// A boxed error, as every subcommand returns one; `main` prints its
/// `Display`.
pub(crate) type CmdResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// What a subcommand runs against: the open vault, its directory, and the
/// undo history every write is recorded in.
pub(crate) struct Ctx<'a> {
    /// The open vault.
    pub(crate) core: &'a Core,
    /// The same vault, shared: what a signed relay call outside the sync
    /// driver binds to (`identity rotate`, `identity new-code`).
    pub(crate) shared: &'a std::sync::Arc<Core>,
    /// Its directory.
    pub(crate) vault_dir: &'a Path,
    /// This vault's undo history, read at open and written back at exit.
    pub(crate) history: UndoHistory,
    /// Whether anything was recorded, so a read-only command never writes
    /// the history file.
    pub(crate) dirty: Cell<bool>,
    /// `--json`: a query prints one JSON document instead of its table.
    pub(crate) json: bool,
    /// Where the reader's zone came from, for `sunrise tz`.
    pub(crate) zone_source: Cell<sunrise_cli::zone::ZoneSource>,
}

impl Ctx<'_> {
    /// Submit `cmd` and record how to reverse it under `label`.
    ///
    /// A command with no inverse is still submitted; it leaves nothing to
    /// undo, which is the client layer's rule, and a note on stderr says so.
    /// Without it, the next `undo` would reverse the step *before* this one
    /// with nothing to tell the user it was not this one.
    pub(crate) async fn submit(
        &self,
        cmd: Command,
        label: String,
    ) -> Result<CommandResult, CoreError> {
        let (res, not_undoable) = self.history.submit(self.core, cmd, label.clone()).await?;
        self.dirty.set(true);
        if not_undoable.is_some() {
            eprintln!(
                "{}",
                strings::general::note(&strings::undo::not_undoable(&label))
            );
        }
        Ok(res)
    }

    /// The zone every query reads in (`docs/11-adr/0066-times-survive-travel.md`
    /// §5): the core's, never the host's.
    pub(crate) fn reader(&self) -> jiff::tz::TimeZone {
        self.core.reader_zone()
    }

    /// "Now", from the core's injected clock.
    pub(crate) fn now(&self) -> jiff::Timestamp {
        sunrise_client_core::vocab::instant_at(self.core.now_ms())
    }
}

/// Dispatch a subcommand against an open vault.
pub(crate) async fn dispatch(ctx: &Ctx<'_>, sub: &str, rest: &[String]) -> CmdResult {
    match sub {
        "login" | "logout" | "whoami" | "bootstrap" => account::run(ctx, sub, rest).await,
        "devices" => devices::list(ctx).await,
        "device" => devices::device(ctx.core, rest).await,
        "identity" => devices::identity(ctx, rest).await,

        "capture" => tasks::capture(ctx, rest).await,
        "today" => tasks::today(ctx).await,
        "upcoming" => upcoming::upcoming(ctx, rest).await,
        "inbox" => {
            tasks::list(ctx, ctx.core.query(Query::Inbox).await?);
            Ok(())
        }
        "search" => search::search(ctx, rest).await,
        "show" => tasks::show(ctx, rest).await,
        "done" => tasks::done(ctx, rest).await,
        "edit" => tasks::edit(ctx, rest).await,
        "retitle" => tasks::retitle(ctx, rest).await,
        "defer" => tasks::defer(ctx, rest).await,
        "schedule" => tasks::schedule(ctx, rest).await,
        "drop" => tasks::drop_tasks(ctx, rest).await,
        "triage" => triage::triage(ctx, rest).await,
        "untriage" => tasks::triage(ctx, rest, false).await,
        "move" => order::move_tasks(ctx, rest).await,
        "restore" => conflicts::restore(ctx, rest).await,
        "conflicts" => conflicts::conflicts(ctx, rest).await,
        "note" => tasks::note(ctx, rest).await,
        "next" => tasks::next(ctx, false).await,
        "focus" => tasks::focus(ctx, rest).await,

        "streams" => organize::streams(ctx, rest).await,
        "stream" => organize::stream_tasks(ctx, rest).await,
        "contexts" => organize::contexts(ctx, rest).await,
        "context" => organize::context_tasks(ctx, rest).await,
        "routines" => organize::routines(ctx, rest).await,

        "blocks" => blocks::list(ctx, rest).await,
        "block" => blocks::block(ctx, rest).await,

        "attachments" => files::attachments(ctx, rest).await,
        "attachment" => files::attachment(ctx, rest).await,

        "review" => reports::review(ctx, rest).await,
        "brief" => reports::brief(ctx, rest).await,
        "reminders" => reminders::reminders(ctx, rest).await,
        "export" => reports::export(ctx.core, rest).await,
        "ical" => reports::ical(ctx.core, rest).await,

        "undo" => settings::undo(ctx, rest).await,
        "redo" => settings::redo(ctx, rest).await,
        "views" => views::views(ctx, rest).await,
        "prefs" => settings::prefs(ctx, rest).await,
        "tz" => settings::tz(ctx, rest),
        "day-schedule" => day_schedule::day_schedule(ctx, rest).await,

        "sync" => plumbing::sync(ctx, rest).await,
        "rebuild" => plumbing::rebuild(ctx.core, rest),
        other => Err(strings::general::unknown_subcommand(&format!("{other:?}")).into()),
    }
}

/// Lowercase hex of a 16-byte id, for the device and identity listings.
///
/// These ids have no `EntityRef` prefix — a device id is a BLAKE3 derivation
/// of `D_S_pub` and an identity id one of `ID_S_pub`, neither of them a ULID —
/// so hex is the only form they have ever had on screen or in a log.
pub(crate) fn hex16(bytes: &[u8; 16]) -> String {
    sunrise_client_core::ids::hex_id(bytes)
}

/// The value after `--name`, if the flag is present with one.
pub(crate) fn flag_value(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

/// `args` with every `--flag value` pair in `flags` (and every bare flag in
/// `switches`) taken out, so what is left is the positional words.
pub(crate) fn positionals(args: &[String], flags: &[&str], switches: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if flags.contains(&a.as_str()) {
            it.next();
        } else if !switches.contains(&a.as_str()) {
            out.push(a.clone());
        }
    }
    out
}

/// Whether the bare switch `name` is present.
pub(crate) fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Whether a resolver may land on an archived Stream / Context.
///
/// The domain rule (`docs/02-domain/contexts-and-tags.md`, and
/// [`sunrise_domain::annotate`]'s module docs) is that an archived entity stays
/// on the Tasks that carry it but must never be a target for **new input**.
/// Listing is not new input — `sunrise contexts` prints archived rows, and a
/// row you can see but cannot open is a dead end — so the two cases differ and
/// the caller says which one it is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Archived {
    /// Listing (`sunrise stream`, `sunrise context`): archived rows count.
    Include,
    /// New input (`sunrise edit`'s `#stream` / `@context`): live rows only.
    Exclude,
}

/// Readable phrasing for an unresolved capture / annotate token.
///
/// The parser reports these as data so each surface can word them; this is the
/// CLI's wording, and it is the *only* one, so `capture`'s note and a failed
/// `sunrise stream <name>` do not describe the same condition two ways.
pub(crate) fn unresolved_note(u: &sunrise_domain::capture::Unresolved) -> String {
    use sunrise_domain::capture::Unresolved as U;
    match u {
        U::UnknownStream(t) => strings::capture::unknown_stream(t),
        U::UnknownContext(t) => strings::capture::unknown_context(t),
        U::AmbiguousStream { typed, candidates } | U::AmbiguousContext { typed, candidates } => {
            strings::capture::ambiguous(typed, &candidates.join(", "))
        }
        U::UnparseableDate(t) => strings::capture::unreadable_date(t),
        U::PriorityOutOfRange(t) => strings::capture::priority_out_of_range(t),
        U::UnparseableDuration(t) => strings::capture::unreadable_duration(t),
    }
}

/// The live (or, for a listing, every) Stream as capture-parser candidates.
///
/// Returned as owned pairs because [`sunrise_domain::NamedRef`] borrows its
/// name and the query result is a temporary.
pub(crate) async fn stream_names(
    core: &Core,
    archived: Archived,
) -> CmdResult<Vec<(EntityRef, String)>> {
    let QueryResult::Streams(rows) = core.query(Query::StreamList).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    let listed = rows
        .iter()
        .filter(|s| archived == Archived::Include || !s.archived)
        .map(|s| sunrise_domain::NamedRef {
            id: s.id,
            name: s.name.as_str(),
        })
        .collect();
    Ok(sunrise_domain::with_inbox_alias(listed)
        .into_iter()
        .map(|r| (r.id, r.name.to_owned()))
        .collect())
}

/// Whether `typed` is a word that names "no stream" where a command takes a
/// stream: `none`, `no-stream` or `-` (ADR-0046).
fn names_no_stream(typed: &str) -> bool {
    matches!(
        typed.to_ascii_lowercase().as_str(),
        "none" | "no-stream" | "nostream" | "-"
    )
}

/// [`resolve_stream`] for a command that gives a task, block or routine a
/// home: `None` for "no stream" (`none`, `no-stream`, `-`, or the Inbox's
/// old name), and the stream otherwise.
pub(crate) async fn resolve_home(
    core: &Core,
    raw: &str,
    archived: Archived,
) -> CmdResult<Option<EntityRef>> {
    Ok(sunrise_domain::home_stream(Some(
        resolve_stream(core, raw, archived).await?,
    )))
}

/// The Contexts as capture-parser candidates. See [`stream_names`].
pub(crate) async fn context_names(
    core: &Core,
    archived: Archived,
) -> CmdResult<Vec<(EntityRef, String)>> {
    let QueryResult::Contexts(rows) = core.query(Query::Contexts).await? else {
        return Err(strings::general::unexpected_result().into());
    };
    Ok(rows
        .into_iter()
        .filter(|c| archived == Archived::Include || !c.archived)
        .map(|c| (c.id, c.name))
        .collect())
}

/// Owned candidates as the borrowed rows the resolver reads.
pub(crate) fn named(rows: &[(EntityRef, String)]) -> Vec<sunrise_domain::NamedRef<'_>> {
    rows.iter()
        .map(|(id, name)| sunrise_domain::NamedRef {
            id: *id,
            name: name.as_str(),
        })
        .collect()
}

/// Turn `<id|name>` into a Stream id.
///
/// An `str_…` id wins outright; anything else is a name, resolved by
/// [`sunrise_domain::resolve_named`] — the same exact-then-unique-prefix rule
/// `#stream` obeys in a capture line, because a user who types `#work` in one
/// place and `sunrise stream work` in another means the same Stream.
pub(crate) async fn resolve_stream(
    core: &Core,
    raw: &str,
    archived: Archived,
) -> CmdResult<EntityRef> {
    let typed = raw.trim().trim_start_matches('#').trim();
    if typed.is_empty() {
        return Err(strings::streams::usage().into());
    }
    if let Ok(id) = EntityRef::parse(typed, EntityKind::Stream) {
        return Ok(id);
    }
    if names_no_stream(typed) {
        return Ok(sunrise_domain::inbox_stream_ref());
    }
    let rows = stream_names(core, archived).await?;
    let mut unresolved = Vec::new();
    sunrise_domain::resolve_named(
        typed,
        &named(&rows),
        &mut unresolved,
        sunrise_domain::NameKind::Stream,
    )
    .ok_or_else(|| {
        unresolved
            .first()
            .map_or_else(|| strings::streams::no_match(typed), unresolved_note)
            .into()
    })
}

/// Turn `<id|name>` into a Context id. See [`resolve_stream`].
pub(crate) async fn resolve_context(
    core: &Core,
    raw: &str,
    archived: Archived,
) -> CmdResult<EntityRef> {
    let typed = raw.trim().trim_start_matches('@').trim();
    if typed.is_empty() {
        return Err(strings::contexts::usage().into());
    }
    if let Ok(id) = EntityRef::parse(typed, EntityKind::Context) {
        return Ok(id);
    }
    let rows = context_names(core, archived).await?;
    let mut unresolved = Vec::new();
    sunrise_domain::resolve_named(
        typed,
        &named(&rows),
        &mut unresolved,
        sunrise_domain::NameKind::Context,
    )
    .ok_or_else(|| {
        unresolved
            .first()
            .map_or_else(|| strings::contexts::no_match(typed), unresolved_note)
            .into()
    })
}

/// A task id, or the error naming what was typed.
pub(crate) fn task_id(raw: &str) -> CmdResult<EntityRef> {
    EntityRef::parse(raw, EntityKind::Task)
        .map_err(|e| strings::general::not_a_task_id(raw, &e.to_string()).into())
}

/// An id of `kind`, or the general "not an id" error naming what was typed.
pub(crate) fn id_of(raw: &str, kind: EntityKind) -> CmdResult<EntityRef> {
    EntityRef::parse(raw, kind).map_err(|e| strings::general::not_an_id(raw, &e.to_string()).into())
}

/// One task, read from the vault.
pub(crate) async fn read_task(core: &Core, id: EntityRef) -> CmdResult<sunrise_domain::Task> {
    match core.query(Query::EntityById(id)).await? {
        QueryResult::Task(t) => Ok(*t),
        _ => Err(strings::general::no_such_task(&id.to_str()).into()),
    }
}

/// Split `<id>... <rest>...` at the first argument that is not a task id.
///
/// Lets every mutating verb take a list of tasks and then its own tail —
/// `edit tsk_a tsk_b '!1 ^tomorrow'`, `defer tsk_a tsk_b tomorrow` — with the
/// same shape `done <id>...` already has. An argument that is not a task id
/// ends the list rather than failing, because the tail is a legitimate part of
/// the line; a *leading* argument that is not an id yields an empty list, and
/// the caller refuses that.
pub(crate) fn split_task_ids(rest: &[String]) -> (Vec<EntityRef>, &[String]) {
    let mut ids = Vec::new();
    for (i, raw) in rest.iter().enumerate() {
        match EntityRef::parse(raw, EntityKind::Task) {
            Ok(id) => ids.push(id),
            Err(_) => return (ids, &rest[i..]),
        }
    }
    (ids, &[])
}
