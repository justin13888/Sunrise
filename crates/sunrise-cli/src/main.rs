//! `sunrise` — the command-line client.
//!
//! Every subcommand is one-shot: open the vault, do the thing, print the
//! result, exit. That makes the whole stack — `Core::open`, the vault lock,
//! SQLCipher persistence, the capture parser, the query path, the sync driver
//! — drivable and testable **headlessly**, with no client and no terminal,
//! which is what `tests/cli.rs` does against the real binary.
//!
//! ```text
//! sunrise capture 'Renew passport #travel ^next saturday !1 ~1h'
//! sunrise today
//! sunrise next
//! SUNRISE_SYNC_URL=ws://127.0.0.1:8443/sync sunrise sync --once
//! ```
//!
//! Deliberately hand-rolled rather than pulling in an arg parser: the surface
//! is a handful of positional subcommands, and the crate has no dependency
//! that is not already earning its place.
//!
//! # stdout is the contract; logs go to a file
//!
//! A CLI's stdout is what a script reads, so nothing else may be written
//! there: `sunrise export trends json | jq` has to work. Human-facing notes
//! (an unresolved `@context`, say) go to stderr, and every `tracing` record
//! goes to `$XDG_STATE_HOME/sunrise/log/sunrise-cli.ndjson` (default
//! `~/.local/state/sunrise/log/…`, override with `SUNRISE_LOG_FILE`), which
//! [`sunrise_log::init_file`] caps at 16 MiB with one kept generation.
//!
//! If that file cannot be opened the binary runs **with no logging at all**.
//! Falling back to stderr would put NDJSON in the middle of a user's output,
//! and a missing log is the better of the two failures.

#![allow(
    clippy::missing_docs_in_private_items,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::semicolon_if_nothing_returned,
    clippy::too_many_lines
)]

use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_cli::{livesync, login, vault};
use sunrise_core::commands::FocusStartDraft;
use sunrise_core::{Command, Core, Query, QueryResult, SystemRng};
use sunrise_domain::routine_rows;
use sunrise_id::{EntityKind, EntityRef};

const USAGE: &str = "\
sunrise — command-line client for Sunrise

USAGE:
  capture and triage
    sunrise capture <text>...    parse and commit one task, then exit
    sunrise edit <id>... <tokens>...
                                 change a task's fields (see EDIT SYNTAX)
    sunrise defer <id>... <when> push tasks out, counting the deferral
    sunrise done <id>...         complete one or more tasks
    sunrise drop <id>...         soft-delete one or more tasks
    sunrise today                list today's tasks
    sunrise inbox                list inbox tasks
    sunrise next                 the focus planner's top picks
    sunrise search <query>...    full-text search

  the vault's shape
    sunrise streams              list streams with open counts
    sunrise streams move <id|name> before <id|name>
    sunrise streams move <id|name> last
                                 reorder the stream list; syncs to every device
    sunrise stream <id|name>     list the tasks in one stream
    sunrise contexts             list contexts with task counts
    sunrise context <id|name>    list the tasks carrying one context
    sunrise routines             list routines with cadence and streak

  review and reporting
    sunrise review               print this week's review summary
    sunrise export <dataset> [json|csv] [path]
                                 trends | activity | focus | streaks

  calendar interchange (RFC 5545)
    sunrise ical import <path|-> [--stream <id>] [--source <name>]
                                 read an .ics file (or stdin) as time blocks
    sunrise ical export [today|week] [path]
                                 write .ics to stdout, or to a path

  account
    sunrise vaults               list this machine's vaults, marking the open one
    sunrise login                sign in via OIDC and store the token
    sunrise logout               forget the stored token
    sunrise whoami               report the stored token's state

  plumbing
    sunrise focus <id>           open a focus session on a task
    sunrise sync --once          drain the outbox and exit (cron / CI)
    sunrise help                 show this message

CAPTURE SYNTAX:
    #stream  @context  ^when  !priority(1-5)  ~duration  *due:when*

    sunrise capture 'Renew passport #travel ^next saturday !1 ~1h'

EDIT SYNTAX:
    #stream  @ctx  @-ctx  !priority  %energy  ~duration  ^when  due:when
    a trailing `-` clears a field:  !-  %-  ~-  ^-  due:-  @-

    sunrise edit tsk_01J… '#work !1 ^next friday'
    sunrise edit tsk_01J… tsk_01K… '@errands ~30m'

    Bare words are refused: an edit line is not a title, and the whole
    line is rejected if any token is, so nothing is half-applied.

ENVIRONMENT:
    SUNRISE_VAULT             vault directory (default ~/.sunrise/vault).
                              Each one is a separate account: the first open
                              mints a random 32-byte root for it
    SUNRISE_KEYSTORE          where those roots are kept, mode 0600 and one
                              file per vault (default
                              $XDG_DATA_HOME/sunrise/keys). Deliberately not
                              inside the vault: a vault directory copied on
                              its own must stay ciphertext. Back both up
    SUNRISE_VAULT_ROOT        open with this root (64 hex chars) and touch no
                              keystore — how two vaults share one account
                              until pairing lands, and how to open a vault
                              made before per-vault keys
    SUNRISE_SYNC_URL          relay endpoint; unset means fully offline
    SUNRISE_SYNC_TOKEN        OIDC bearer for the relay; overrides a stored
                              login. Unset, with no stored login, only works
                              against a self-host relay
    SUNRISE_OIDC_ISSUER       OIDC issuer URL, for `sunrise login`
    SUNRISE_OIDC_CLIENT_ID    OIDC client id, for `sunrise login`
    SUNRISE_EXPORT_CERT_FILE  write this device's cert here on startup
    SUNRISE_TRUST_CERT_FILE   trust the peer cert at this path on startup
    SUNRISE_LOG_FILE          override the NDJSON log destination
";

/// Returns [`std::process::ExitCode`] rather than a `Result`, because the
/// `Termination` impl for a `Result` prints the error's **`Debug`** form.
/// `VaultError` spends most of its length explaining what a user should do
/// next, and `RootMissing { id: "07a5…", keystore: "/var/…" }` is not that
/// explanation — it is the struct the explanation was written on. Printing
/// `Display` ourselves is the whole difference between a typed error and a
/// typed error a user can act on, and it drops the `Error: "…"` quoting that
/// the default handler put around every message this binary already had.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    // First statement in the process. `init_file` is fallible and its failure
    // is deliberately discarded: an unwritable state directory must not stop
    // the command, and the alternative destination — stderr — would put
    // NDJSON in the middle of the user's output.
    let _ = sunrise_log::init_file("sunrise-cli");
    // Same shape as `sunrise-server`'s `srv.start`: the protocol versions are
    // reported once per process rather than on every record.
    let proto = sunrise_log::ProtoVersions::new(WIRE_PROTO_V, DOC_SCHEMA_V, CRYPTO_SUITE_V);
    tracing::info!(
        ev = "ui.start",
        app_v = env!("CARGO_PKG_VERSION"),
        wire_v = u64::from(proto.wire),
        doc_v = u64::from(proto.doc),
        crypto_v = u64::from(proto.crypto),
        "sunrise-cli starting"
    );
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(sub) = args.first() else {
        #[allow(clippy::print_stdout)]
        {
            print!("{USAGE}");
        }
        return std::process::ExitCode::SUCCESS;
    };
    match run(sub, &args[1..]).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // stderr, because stdout is the contract a script reads.
            #[allow(clippy::print_stderr)]
            {
                eprintln!("error: {e}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn vault_dir() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SUNRISE_VAULT") {
        return std::path::PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".sunrise")
        .join("vault")
}

/// `vaults` — every vault this machine holds a key for, marking the open one.
///
/// Multi-account is only a capability if a user can find their accounts. The
/// macOS app answers this with a picker in Settings; the CLI answers it with a
/// listing, because a `SUNRISE_VAULT` nobody can enumerate is a feature you
/// have to already know about to use.
///
/// A vault opened only with `SUNRISE_VAULT_ROOT` never touches the keystore
/// and so is deliberately absent: this lists what is *keyed here*, which is
/// the question a user asking "what have I got" is really asking.
fn vaults() {
    #![allow(clippy::print_stdout)]
    let keystore = vault::keystore_dir();
    let current = vault_dir();
    let rows = vault::registered(&keystore);
    if rows.is_empty() {
        // stdout stays empty so `sunrise vaults | wc -l` is honest; the reason
        // there is nothing to list is a note for the human.
        #[allow(clippy::print_stderr)]
        {
            eprintln!("no vaults keyed in {}", keystore.display());
        }
        return;
    }
    for v in rows {
        let mark = if v.path == current { "  [current]" } else { "" };
        println!("{}  {}{mark}", v.id, v.path.display());
    }
}

/// Dispatch a subcommand. Returns `Ok` on success; the process exit code
/// is non-zero only on a real failure, so scripts can branch on it.
async fn run(sub: &str, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    // A CLI's whole job is writing to stdout; the workspace-wide ban on
    // print_stdout exists to keep it out of *library* code.
    #![allow(clippy::print_stdout)]
    match sub {
        "help" | "--help" | "-h" => {
            print!("{USAGE}");
            return Ok(());
        }
        "--version" | "-V" => {
            println!("sunrise {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // Answered without opening anything. It is the subcommand a user
        // reaches for when a vault will *not* open, so it must not need one to
        // have opened — the same reason the macOS registry lives in
        // `UserDefaults` rather than in the vault.
        "vaults" => {
            vaults();
            return Ok(());
        }
        _ => {}
    }

    let dir = vault_dir();
    std::fs::create_dir_all(&dir).ok();
    let dir_for_store = dir.clone();
    // This vault's own root — see `sunrise_cli::vault`. Minted on the first
    // open of a directory and kept in the keystore from then on, so two
    // `SUNRISE_VAULT` values are two accounts and not two folders sharing one
    // key. `SystemRng` is the injected CSPRNG the workspace requires; nothing
    // here reaches for `rand` directly.
    let root = vault::resolve(&dir, &SystemRng)?;
    // Opened offline first, so the vault is available to price a stored
    // token against the core's clock rather than the host's — which the
    // workspace lint bans reading directly.
    let (core, _) = livesync::open_with_plan(
        dir,
        env!("CARGO_PKG_VERSION"),
        root,
        &livesync::SyncPlan::default(),
    )
    .await?;

    // Then the real plan. Every subcommand honours the cert-file vars —
    // `SUNRISE_EXPORT_CERT_FILE` is documented as acting "on startup" — but
    // only `sync` starts a driver: opening one for a command that exits
    // milliseconds later would just churn the relay.
    let mut env = livesync::SyncEnv::from_process_env();
    if sub != "sync" {
        env.url = None;
    }
    let plan =
        livesync::plan_from_env(&env.with_stored(&login::store_for(&dir_for_store), core.now_ms()));
    // The startup banner names the cert files it touched, which is what the
    // two-replica walkthrough needs to see. stderr, because stdout is the
    // contract a script reads — and not a log record, because those strings
    // carry filesystem paths (`livesync::apply_plan`, "Two outputs").
    #[allow(clippy::print_stderr)]
    for line in livesync::apply_plan(&core, &plan).await {
        eprintln!("{line}");
    }

    let result = dispatch(&core, &dir_for_store, sub, rest).await;
    core.shutdown().await;
    result
}

async fn dispatch(
    core: &Core,
    vault_dir: &std::path::Path,
    sub: &str,
    rest: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    match sub {
        "login" => {
            let cfg = login::LoginConfig::from_env().map_err(|e| {
                format!(
                    "{e}; set {} and {} first",
                    login::ENV_ISSUER,
                    login::ENV_CLIENT_ID
                )
            })?;
            let store = login::store_for(vault_dir);
            let device_id = login::device_id_hex(core);
            let mut announce = |line: &str| println!("{line}");
            println!("Opening your browser to sign in. If it does not open, visit:");
            let creds =
                login::login(&cfg, &device_id, &store, core.now_ms(), &mut announce).await?;
            let secs = creds.expires_at_ms.saturating_sub(core.now_ms()) / 1000;
            println!("Signed in. Access token valid for {secs}s.");
            return Ok(());
        }
        "logout" => {
            login::logout(&login::store_for(vault_dir))?;
            println!("Signed out.");
            return Ok(());
        }
        "whoami" => {
            println!(
                "{}",
                login::status_line(&login::store_for(vault_dir), core.now_ms())
            );
            return Ok(());
        }
        _ => {}
    }
    match sub {
        "capture" => {
            let text = rest.join(" ");
            if text.trim().is_empty() {
                return Err("capture needs some text; see `sunrise help`".into());
            }
            // System zone, so `^tomorrow 9am` means the user's 9am.
            let tz = jiff::tz::TimeZone::system();
            let parsed = core.capture(&text, &tz).await?;
            for u in &parsed.unresolved {
                // Warnings go to stderr so stdout stays parseable. This is
                // CLI output for the human running the command, not a log
                // record — the subcommand path never enters the alternate
                // screen, so stderr is safe here and only here.
                #[allow(clippy::print_stderr)]
                {
                    eprintln!("note: {}", unresolved_note(u));
                }
            }
            let title = parsed.draft.title.clone();
            let res = core.submit(Command::CreateTask(parsed.draft)).await?;
            println!("{}  {title}", res.entity.to_str());
            Ok(())
        }
        "today" => {
            let q = Query::Today {
                now_ms: core.now_ms(),
                contexts: vec![],
            };
            print_tasks(core.query(q).await?);
            Ok(())
        }
        "inbox" => {
            print_tasks(core.query(Query::Inbox).await?);
            Ok(())
        }
        // The listing, and the one mutation that belongs to the list rather
        // than to any one row. `move` is a subcommand here instead of a verb
        // of its own because it is about the *order of the plural*, which is
        // what `streams` names; a top-level `sunrise move` would read as
        // moving a task between streams, which `edit #stream` already does.
        "streams" if rest.first().map(String::as_str) == Some("move") => {
            move_stream(core, &rest[1..]).await
        }
        "streams" => {
            if let QueryResult::Streams(rows) = core.query(Query::StreamList).await? {
                for s in rows {
                    println!(
                        "{}  {:<24} {} open",
                        s.id.to_str(),
                        s.name,
                        s.open_task_count
                    );
                }
            }
            Ok(())
        }
        // Singular lists the tasks *in* one; plural lists the rows. Chosen
        // over `streams <id>` and over a `--stream` flag: this CLI's other
        // targeted verbs are bare positionals (`done <id>`, `focus <id>`), and
        // `streams <id>` would read as "list streams, filtered" rather than
        // "list that stream's tasks". One subcommand, one shape of output.
        //
        // The argument is joined rather than taken as `rest[0]` so an unquoted
        // multi-word name (`sunrise stream home renovation`) works, exactly as
        // `capture` and `search` already join theirs.
        "stream" => {
            let id = resolve_stream(core, &rest.join(" "), Archived::Include).await?;
            print_tasks(core.query(Query::StreamTasks(id)).await?);
            Ok(())
        }
        "context" => {
            let id = resolve_context(core, &rest.join(" "), Archived::Include).await?;
            print_tasks(core.query(Query::ContextTasks(id)).await?);
            Ok(())
        }
        "search" => {
            let text = rest.join(" ");
            if text.trim().is_empty() {
                return Err("search needs a query".into());
            }
            let q = Query::Search { text, limit: 100 };
            print_tasks(core.query(q).await?);
            Ok(())
        }
        "done" => done(core, rest).await,
        "contexts" => {
            if let QueryResult::Contexts(rows) = core.query(Query::Contexts).await? {
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
        "routines" => {
            if let QueryResult::Routines(rs) = core.query(Query::Routines).await? {
                let now =
                    jiff::Timestamp::from_millisecond(i64::try_from(core.now_ms()).unwrap_or(0))
                        .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
                for r in routine_rows(&rs, now) {
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
            }
            Ok(())
        }
        // `docs/07-clients/tui.md` §Capture from anywhere: "sunrise focus
        // next — picks next Today task and enters focus".
        "next" => next(core, false).await,
        "focus" => match rest.first().map(String::as_str) {
            Some("next") | None => next(core, true).await,
            Some(id) => focus_on(core, id).await,
        },
        "edit" => edit(core, rest).await,
        "defer" => defer(core, rest).await,
        "drop" => drop_tasks(core, rest).await,
        "review" => review(core).await,
        "export" => export(core, rest).await,
        "ical" => ical(core, rest).await,
        "sync" => sync_once(core, rest).await,
        other => Err(format!("unknown subcommand {other:?}; try `sunrise help`").into()),
    }
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
enum Archived {
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
fn unresolved_note(u: &sunrise_domain::capture::Unresolved) -> String {
    use sunrise_domain::capture::Unresolved as U;
    match u {
        U::UnknownStream(t) => format!("no stream matches \"{t}\" (try `sunrise streams`)"),
        U::UnknownContext(t) => format!("no context matches \"{t}\" (try `sunrise contexts`)"),
        U::AmbiguousStream { typed, candidates } | U::AmbiguousContext { typed, candidates } => {
            format!("\"{typed}\" matches {}", candidates.join(", "))
        }
        U::UnparseableDate(t) => format!("could not read the date \"{t}\""),
        U::PriorityOutOfRange(t) => format!("priority \"{t}\" is not 1-5"),
        U::UnparseableDuration(t) => format!("could not read the duration \"{t}\""),
    }
}

/// The live (or, for a listing, every) Stream as capture-parser candidates.
///
/// Returned as owned pairs because [`sunrise_domain::NamedRef`] borrows its
/// name and the query result is a temporary.
async fn stream_names(
    core: &Core,
    archived: Archived,
) -> Result<Vec<(EntityRef, String)>, Box<dyn std::error::Error>> {
    let QueryResult::Streams(rows) = core.query(Query::StreamList).await? else {
        return Err("unexpected query result".into());
    };
    Ok(rows
        .into_iter()
        .filter(|s| archived == Archived::Include || !s.archived)
        .map(|s| (s.id, s.name))
        .collect())
}

/// The Contexts as capture-parser candidates. See [`stream_names`].
async fn context_names(
    core: &Core,
    archived: Archived,
) -> Result<Vec<(EntityRef, String)>, Box<dyn std::error::Error>> {
    let QueryResult::Contexts(rows) = core.query(Query::Contexts).await? else {
        return Err("unexpected query result".into());
    };
    Ok(rows
        .into_iter()
        .filter(|c| archived == Archived::Include || !c.archived)
        .map(|c| (c.id, c.name))
        .collect())
}

fn named(rows: &[(EntityRef, String)]) -> Vec<sunrise_domain::NamedRef<'_>> {
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
async fn resolve_stream(
    core: &Core,
    raw: &str,
    archived: Archived,
) -> Result<EntityRef, Box<dyn std::error::Error>> {
    let typed = raw.trim().trim_start_matches('#').trim();
    if typed.is_empty() {
        return Err("usage: stream <id|name>; `sunrise streams` lists them".into());
    }
    if let Ok(id) = EntityRef::parse(typed, EntityKind::Stream) {
        return Ok(id);
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
            .map_or_else(|| format!("no stream matches \"{typed}\""), unresolved_note)
            .into()
    })
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
async fn move_stream(core: &Core, args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    const USAGE: &str = "usage: streams move <id|name> before <id|name>\n\
                                streams move <id|name> last";

    let sep = args
        .iter()
        .position(|a| a == "before" || a == "last")
        .ok_or(USAGE)?;
    let moved = args[..sep].join(" ");
    let target = args[sep + 1..].join(" ");
    let to_last = args[sep] == "last";
    if moved.trim().is_empty() || to_last != target.trim().is_empty() {
        return Err(USAGE.into());
    }

    let moved = resolve_stream(core, &moved, Archived::Include).await?;
    let target = if to_last {
        None
    } else {
        Some(resolve_stream(core, &target, Archived::Include).await?)
    };
    if moved == sunrise_domain::inbox_stream_ref() || target == Some(moved) {
        return Err("the Inbox is not a stream and cannot be reordered".into());
    }

    let QueryResult::Streams(rows) = core.query(Query::StreamList).await? else {
        return Err("unexpected query result".into());
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
            .ok_or("that stream is not in the list")?,
    };
    let after = at.checked_sub(1).map(|i| others[i].sort_order.as_str());
    let before = others.get(at).map(|s| s.sort_order.as_str());

    let key = sunrise_domain::sort_order::between(after, before)
        .map_err(|e| format!("cannot place that stream: {e}"))?;
    let patch = sunrise_domain::StreamPatch {
        sort_order: Some(key),
        ..Default::default()
    };
    core.submit(Command::UpdateStream { id: moved, patch })
        .await?;

    // Print the new order, because the whole point of the command is the
    // order and a silent success would have to be checked with a second one.
    #[allow(clippy::print_stdout)]
    if let QueryResult::Streams(rows) = core.query(Query::StreamList).await? {
        for s in rows {
            println!("{}  {}", s.id.to_str(), s.name);
        }
    }
    Ok(())
}

/// Turn `<id|name>` into a Context id. See [`resolve_stream`].
async fn resolve_context(
    core: &Core,
    raw: &str,
    archived: Archived,
) -> Result<EntityRef, Box<dyn std::error::Error>> {
    let typed = raw.trim().trim_start_matches('@').trim();
    if typed.is_empty() {
        return Err("usage: context <id|name>; `sunrise contexts` lists them".into());
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
            .map_or_else(
                || format!("no context matches \"{typed}\""),
                unresolved_note,
            )
            .into()
    })
}

/// `done <id>...` — complete tasks by id.
async fn done(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    if rest.is_empty() {
        return Err("done needs at least one task id".into());
    }
    for raw in rest {
        let id = EntityRef::parse(raw, EntityKind::Task)
            .map_err(|e| format!("not a task id: {raw} ({e})"))?;
        core.submit(Command::CompleteTask(id)).await?;
        println!("done  {raw}");
    }
    Ok(())
}

/// Split `<id>... <rest>...` at the first argument that is not a task id.
///
/// Lets every mutating verb take a list of tasks and then its own tail —
/// `edit tsk_a tsk_b '!1 ^tomorrow'`, `defer tsk_a tsk_b tomorrow` — with the
/// same shape `done <id>...` already has. An argument that is not a task id
/// ends the list rather than failing, because the tail is a legitimate part of
/// the line; a *leading* argument that is not an id yields an empty list, and
/// the caller refuses that.
fn split_task_ids(rest: &[String]) -> (Vec<EntityRef>, &[String]) {
    let mut ids = Vec::new();
    for (i, raw) in rest.iter().enumerate() {
        match EntityRef::parse(raw, EntityKind::Task) {
            Ok(id) => ids.push(id),
            Err(_) => return (ids, &rest[i..]),
        }
    }
    (ids, &[])
}

/// One task's current contexts, which an annotate line needs because contexts
/// are a *replace* field: "add `@home`" is only expressible as the union of
/// `@home` with what that particular task already carries.
async fn task_contexts(
    core: &Core,
    id: EntityRef,
) -> Result<Vec<EntityRef>, Box<dyn std::error::Error>> {
    let QueryResult::Task(t) = core.query(Query::EntityById(id)).await? else {
        return Err(format!("no such task: {}", id.to_str()).into());
    };
    Ok(t.contexts.iter().copied().collect())
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
async fn edit(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout, clippy::print_stderr)]
    let (ids, tail) = split_task_ids(rest);
    if ids.is_empty() {
        return Err("usage: edit <id>... <tokens>...; see `sunrise help`".into());
    }
    let line = tail.join(" ");
    if line.trim().is_empty() {
        return Err("edit needs something to change, e.g. `!1 ^tomorrow #work`".into());
    }

    // Live rows only: an archived Stream or Context stays on the tasks that
    // carry it but must never be the target of new input.
    let streams = stream_names(core, Archived::Exclude).await?;
    let contexts = context_names(core, Archived::Exclude).await?;
    let streams = named(&streams);
    let contexts = named(&contexts);
    // System zone, so `^tomorrow 9am` means the user's 9am, exactly as in
    // `capture`.
    let tz = jiff::tz::TimeZone::system();
    let edit = sunrise_domain::parse_annotate(
        &line,
        &streams,
        &contexts,
        sunrise_domain::now_ts(core.now_ms()),
        &tz,
    );
    if !edit.errors.is_empty() {
        for e in &edit.errors {
            eprintln!("note: {}", e.describe());
        }
        return Err("nothing was changed; fix the line and run it again".into());
    }
    if edit.is_empty() {
        return Err("edit needs something to change, e.g. `!1 ^tomorrow #work`".into());
    }

    let preview = edit.preview(&streams, &contexts, &tz);
    for id in ids {
        let patch = edit.patch_for(&task_contexts(core, id).await?);
        core.submit(Command::UpdateTask { id, patch }).await?;
        if let Some(stream) = edit.stream() {
            core.submit(Command::PromoteToStream { id, stream }).await?;
        }
        println!("{}  {preview}", id.to_str());
    }
    Ok(())
}

/// `defer <id>... <when>` — push tasks out to a new date.
///
/// Separate from `edit ^when`, which merely sets `scheduled_at`. This is
/// [`Command::DeferTask`], which also bumps the task's `deferred_count`, and
/// that counter is what `sunrise review` reports as "deferred" — a task moved
/// four times is the signal the weekly review exists to surface. Spelling a
/// real defer as a schedule change would quietly zero that signal out.
async fn defer(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    let (ids, tail) = split_task_ids(rest);
    if ids.is_empty() {
        return Err("usage: defer <id>... <when>; see `sunrise help`".into());
    }
    let phrase = tail.join(" ");
    if phrase.trim().is_empty() {
        return Err("defer needs a date, e.g. `tomorrow`, `next friday`, `+3d`".into());
    }
    let tz = jiff::tz::TimeZone::system();
    let now = sunrise_domain::now_ts(core.now_ms());
    let to = sunrise_domain::capture::parse_when(&phrase, now, &tz)
        .ok_or_else(|| format!("could not read the date \"{phrase}\""))?;
    let to_ms = u64::try_from(to.as_millisecond()).map_err(|_| "that date is before the epoch")?;
    for id in ids {
        core.submit(Command::DeferTask { id, to_ms }).await?;
        println!("{}  deferred to {}", id.to_str(), stamp(to, &tz));
    }
    Ok(())
}

/// `drop <id>...` — soft-delete tasks.
///
/// Named for the review's own vocabulary: `sunrise review` counts "dropped",
/// and a triage surface that can only say yes (`done`) and not no is half a
/// surface. Soft, like every other client's delete — the op is a tombstone,
/// not an erase.
async fn drop_tasks(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    if rest.is_empty() {
        return Err("drop needs at least one task id".into());
    }
    for raw in rest {
        let id = EntityRef::parse(raw, EntityKind::Task)
            .map_err(|e| format!("not a task id: {raw} ({e})"))?;
        core.submit(Command::DeleteTask(id)).await?;
        println!("dropped  {raw}");
    }
    Ok(())
}

/// `YYYY-MM-DD HH:MM` in `tz`, the same shape the annotate preview uses.
fn stamp(ts: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> String {
    let dt = ts.to_zoned(tz.clone()).datetime();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

/// `next` / `focus next` — the planner's ranked picks, optionally opening
/// a session on the top one.
///
/// The ranking is the core's (`Query::FocusPlan`), not a re-sort here:
/// "what should I do next" must give the same answer here and in the app,
/// or one of them is lying.
async fn next(core: &Core, start: bool) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    /// Enough to choose from without becoming a list.
    const PICKS: u32 = 5;
    let q = Query::FocusPlan {
        stream: None,
        energy: None,
        length: sunrise_domain::SessionLength::OnePomodoro,
        limit: PICKS,
    };
    let QueryResult::FocusPlan(rows) = core.query(q).await? else {
        return Err("unexpected query result".into());
    };
    if rows.is_empty() {
        println!("nothing actionable — everything is blocked, done, or unscheduled");
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
        println!("focus started on {}", top.task.title);
    }
    Ok(())
}

/// `focus <id>` — open a session on one task.
async fn focus_on(core: &Core, raw: &str) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    let id = EntityRef::parse(raw, EntityKind::Task)
        .map_err(|e| format!("not a task id: {raw} ({e})"))?;
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

/// `review` — this week's counts, the same fold the Review view renders.
async fn review(core: &Core) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    let q = Query::WeeklyReview {
        week_start_ms: None,
        now_ms: core.now_ms(),
    };
    let QueryResult::WeeklyReview(w) = core.query(q).await? else {
        return Err("unexpected query result".into());
    };
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

/// `export <dataset> [json|csv] [path]`.
///
/// With no path the document goes to **stdout**: a CLI's stdout is its
/// contract, so `sunrise export trends json | jq` has to work.
async fn export(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    use sunrise_domain::{ExportDataset, ExportFormat};
    let Some(name) = rest.first() else {
        return Err("usage: export <trends|activity|focus|streaks> [json|csv] [path]".into());
    };
    let dataset = match name.as_str() {
        "trends" | "trend" => ExportDataset::Trends,
        "activity" | "timeline" => ExportDataset::Activity,
        "focus" => ExportDataset::Focus,
        "streaks" | "streak" => ExportDataset::Streaks,
        other => return Err(format!("unknown dataset: {other}").into()),
    };
    let mut format = ExportFormat::Csv;
    let mut path: Option<String> = None;
    for w in &rest[1..] {
        match w.as_str() {
            "json" => format = ExportFormat::Json,
            "csv" => format = ExportFormat::Csv,
            other if path.is_none() => path = Some(other.to_string()),
            other => return Err(format!("unexpected argument: {other}").into()),
        }
    }
    let q = Query::ExportStats {
        dataset,
        format,
        weeks: 12,
        now_ms: core.now_ms(),
    };
    let QueryResult::Export(body) = core.query(q).await? else {
        return Err("unexpected query result".into());
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
async fn ical(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match rest.first().map(String::as_str) {
        Some("import") => ical_import(core, &rest[1..]).await,
        Some("export") => ical_export(core, &rest[1..]).await,
        Some(other) => Err(format!("unknown ical mode {other:?}; try import or export").into()),
        None => Err("usage: ical import <path|-> | ical export [today|week] [path]".into()),
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
async fn ical_import(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout, clippy::print_stderr)]
    use sunrise_integrations::ical_vault::{import, ICS_SOURCE};

    let mut path: Option<&str> = None;
    let mut stream = sunrise_domain::inbox_stream_ref();
    let mut source = ICS_SOURCE.to_string();
    let mut args = rest.iter();
    while let Some(w) = args.next() {
        match w.as_str() {
            "--stream" => {
                let v = args.next().ok_or("--stream needs a stream id")?;
                stream = EntityRef::parse(v, EntityKind::Stream)
                    .map_err(|e| format!("not a stream id: {v} ({e})"))?;
            }
            "--source" => {
                let v = args.next().ok_or("--source needs a name")?;
                if v.trim().is_empty() {
                    return Err("--source needs a non-empty name".into());
                }
                source = v.trim().to_string();
            }
            other if path.is_none() => path = Some(other),
            other => return Err(format!("unexpected argument: {other}").into()),
        }
    }
    let path = path.ok_or("usage: ical import <path|-> [--stream <id>] [--source <name>]")?;

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
        eprintln!("note: [{}] {uid}: {}", n.code.as_str(), n.detail);
    }
    for b in &report.blocks {
        let verb = if b.created { "new" } else { "upd" };
        println!("{verb}  {}  {}", b.block.to_str(), b.title);
    }
    eprintln!("{}", report.summary_line());
    Ok(())
}

/// `ical export [today|week] [path]`.
///
/// With no path the document goes to stdout, exactly as `sunrise export`
/// does — `sunrise ical export week | pbcopy` has to work.
async fn ical_export(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    use sunrise_integrations::ical_vault::{export, ExportWindow};

    let mut window = ExportWindow::Day;
    let mut path: Option<&str> = None;
    for w in rest {
        match w.as_str() {
            "today" | "day" => window = ExportWindow::Day,
            "week" => window = ExportWindow::Week,
            other if path.is_none() => path = Some(other),
            other => return Err(format!("unexpected argument: {other}").into()),
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

/// `sync --once` — bring the relay up, drain the outbox, exit.
///
/// Named for cron and CI in `docs/07-clients/overview.md`. It is a *bounded*
/// wait, not a loop: a job that hangs forever because the relay is down is
/// worse than one that fails, since nothing downstream ever runs.
async fn sync_once(core: &Core, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    #![allow(clippy::print_stdout)]
    /// How long to wait for the outbox to empty before giving up.
    const DEADLINE_MS: u64 = 30_000;
    /// How often to re-read the outbox depth.
    const POLL: std::time::Duration = std::time::Duration::from_millis(200);

    if !rest.is_empty() && rest[0] != "--once" {
        return Err("usage: sync --once".into());
    }
    if std::env::var("SUNRISE_SYNC_URL").is_err() {
        return Err("sync needs SUNRISE_SYNC_URL".into());
    }
    // Timed against the core's injected clock rather than `Instant`, which
    // the workspace lint bans so that time is never read from two sources.
    let deadline = core.now_ms().saturating_add(DEADLINE_MS);
    let mut last = u32::MAX;
    while core.now_ms() < deadline {
        let QueryResult::SyncStatus(s) = core.query(Query::SyncStatus).await? else {
            return Err("unexpected query result".into());
        };
        if s.outbox_pending == 0 && s.state == sunrise_sync::SyncState::Live {
            println!("sync: live, outbox empty");
            return Ok(());
        }
        if s.outbox_pending != last {
            last = s.outbox_pending;
            println!("sync: {} pending ({:?})", s.outbox_pending, s.state);
        }
        tokio::time::sleep(POLL).await;
    }
    Err(format!("sync: outbox did not drain within {}s", DEADLINE_MS / 1000).into())
}

fn print_tasks(r: QueryResult) {
    #![allow(clippy::print_stdout)]
    let (QueryResult::Tasks(tasks) | QueryResult::StreamTasks(tasks)) = r else {
        return;
    };
    for t in tasks {
        let mark = if t.state == sunrise_domain::TaskState::Done {
            "x"
        } else {
            " "
        };
        println!("[{mark}] {}  {}", t.id.to_str(), t.title);
    }
}
