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
use sunrise_cli::{livesync, login};
use sunrise_core::commands::FocusStartDraft;
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::routine_rows;
use sunrise_id::{EntityKind, EntityRef};

/// Fixed dev / self-host vault root. Every instance derives its per-stream
/// keys from the **same** root, which is what lets two of them decrypt each
/// other's op envelopes in the two-terminal sync demo. Device identities still
/// differ (the keychain seeds a fresh id per vault dir). Production derives
/// this from a passphrase or a completed pairing flow instead of a constant.
const DEV_ROOT: [u8; 32] = [7u8; 32];

const USAGE: &str = "\
sunrise — command-line client for Sunrise

USAGE:
  capture and triage
    sunrise capture <text>...    parse and commit one task, then exit
    sunrise done <id>...         complete one or more tasks
    sunrise today                list today's tasks
    sunrise inbox                list inbox tasks
    sunrise next                 the focus planner's top picks
    sunrise search <query>...    full-text search

  the vault's shape
    sunrise streams              list streams with open counts
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

ENVIRONMENT:
    SUNRISE_VAULT             vault directory (default ~/.sunrise/vault)
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        return Ok(());
    };
    run(sub, &args[1..]).await
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
        _ => {}
    }

    let dir = vault_dir();
    std::fs::create_dir_all(&dir).ok();
    let dir_for_store = dir.clone();
    // Opened offline first, so the vault is available to price a stored
    // token against the core's clock rather than the host's — which the
    // workspace lint bans reading directly.
    let (core, _) = livesync::open_with_plan(
        dir,
        env!("CARGO_PKG_VERSION"),
        DEV_ROOT,
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
            let id = resolve_stream(core, &rest.join(" ")).await?;
            print_tasks(core.query(Query::StreamTasks(id)).await?);
            Ok(())
        }
        "context" => {
            let id = resolve_context(core, &rest.join(" ")).await?;
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
        "review" => review(core).await,
        "export" => export(core, rest).await,
        "ical" => ical(core, rest).await,
        "sync" => sync_once(core, rest).await,
        other => Err(format!("unknown subcommand {other:?}; try `sunrise help`").into()),
    }
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
async fn stream_names(core: &Core) -> Result<Vec<(EntityRef, String)>, Box<dyn std::error::Error>> {
    let QueryResult::Streams(rows) = core.query(Query::StreamList).await? else {
        return Err("unexpected query result".into());
    };
    Ok(rows.into_iter().map(|s| (s.id, s.name)).collect())
}

/// The Contexts as capture-parser candidates. See [`stream_names`].
async fn context_names(
    core: &Core,
) -> Result<Vec<(EntityRef, String)>, Box<dyn std::error::Error>> {
    let QueryResult::Contexts(rows) = core.query(Query::Contexts).await? else {
        return Err("unexpected query result".into());
    };
    Ok(rows.into_iter().map(|c| (c.id, c.name)).collect())
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
async fn resolve_stream(core: &Core, raw: &str) -> Result<EntityRef, Box<dyn std::error::Error>> {
    let typed = raw.trim().trim_start_matches('#').trim();
    if typed.is_empty() {
        return Err("usage: stream <id|name>; `sunrise streams` lists them".into());
    }
    if let Ok(id) = EntityRef::parse(typed, EntityKind::Stream) {
        return Ok(id);
    }
    let rows = stream_names(core).await?;
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

/// Turn `<id|name>` into a Context id. See [`resolve_stream`].
async fn resolve_context(core: &Core, raw: &str) -> Result<EntityRef, Box<dyn std::error::Error>> {
    let typed = raw.trim().trim_start_matches('@').trim();
    if typed.is_empty() {
        return Err("usage: context <id|name>; `sunrise contexts` lists them".into());
    }
    if let Ok(id) = EntityRef::parse(typed, EntityKind::Context) {
        return Ok(id);
    }
    let rows = context_names(core).await?;
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
