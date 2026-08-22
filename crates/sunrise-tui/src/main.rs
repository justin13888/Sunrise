//! `sunrise-tui` binary entrypoint.
//!
//! v1 scope: stand up the terminal, open a Core against a vault directory
//! (env: `SUNRISE_VAULT`, default `~/.sunrise/vault`), and render the
//! current view. Quits on `q` / Esc / `:q`.
//!
//! The event loop selects over terminal input *and* `Core::changes()`, so ops
//! arriving over live sync repaint immediately rather than waiting for the
//! next keystroke. Everything a keypress does lives in
//! `sunrise_tui::runtime::apply_action`; this file only performs the I/O that
//! reducer asks for.

#![allow(
    clippy::print_stderr,
    clippy::missing_docs_in_private_items,
    clippy::match_same_arms,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::manual_let_else,
    clippy::needless_pass_by_value,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::too_many_lines
)]

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::{NoteBody, TaskPatch};
use sunrise_tui::livesync;
use sunrise_tui::runtime::{drain_changes, CHANGE_DEBOUNCE};
use sunrise_tui::{
    apply_action, editor, keymap, render, routine_rows, Outcome, SyncIndicator, View, ViewState,
};
use tokio::sync::broadcast::error::RecvError;

type Tty = Terminal<CrosstermBackend<Stdout>>;

/// Fixed dev / self-host vault root shared by every instance. Because both TUI
/// instances derive per-stream keys from the **same** root, each can decrypt
/// the other's op envelopes — this is what makes the shared-root two-terminal
/// sync demo converge. Device identities still differ (the keychain seeds a
/// fresh id per vault dir). Production derives this from a passphrase or a
/// completed pairing flow instead of a constant.
const DEV_ROOT: [u8; 32] = [7u8; 32];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(sub) = args.first() {
        return cli::run(sub, &args[1..]).await;
    }

    let vault_dir = vault_dir();
    std::fs::create_dir_all(&vault_dir).ok();

    // Dev/demo live-sync wiring (see `livesync`): env-driven, with cert files
    // for device trust. All three env vars are optional; unset ⇒ offline as
    // before. Documented env vars:
    //   SUNRISE_SYNC_URL          ws://127.0.0.1:8443/sync   (relay endpoint)
    //   SUNRISE_EXPORT_CERT_FILE  path to write this device's cert on startup
    //   SUNRISE_TRUST_CERT_FILE   path to a peer cert to trust on startup
    let plan = livesync::plan_from_env(&livesync::SyncEnv::from_process_env());
    let sync_on = !plan.is_off();
    let (core, startup_log) = livesync::open_with_plan(
        vault_dir.clone(),
        env!("CARGO_PKG_VERSION"),
        DEV_ROOT,
        &plan,
    )
    .await?;
    for line in &startup_log {
        eprintln!("sunrise-tui: {line}");
    }

    let mut term = setup()?;
    let result = run(&mut term, &core, sync_on).await;
    teardown(&mut term)?;
    // Non-consuming shutdown: `core` is an `Arc<Core>`, so we can't call the
    // consuming `close()`. `shutdown()` stops the driver and releases the
    // driver's transient strong ref so the lock drops when this Arc drops.
    core.shutdown().await;
    result
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

fn setup() -> Result<Tty, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn teardown(term: &mut Tty) -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    term.backend_mut().execute(LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

/// Re-enter the alternate screen after an external program (the `$EDITOR`
/// suspend) has had the terminal. Repaints from scratch: the editor left the
/// real screen in an unknown state, and Ratatui's diffing buffer no longer
/// describes it.
fn resume(term: &mut Tty) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    term.backend_mut().execute(EnterAlternateScreen)?;
    term.hide_cursor()?;
    term.clear()?;
    Ok(())
}

/// How long the loop waits with no input before repainting anyway. Keeps the
/// status-line sync indicator ticking without busy-looping.
const IDLE_REDRAW: Duration = Duration::from_millis(200);
/// How long the input reader waits for a key before releasing the input gate.
/// Bounds how long the `$EDITOR` suspend waits to take stdin; the wake itself
/// costs less than the 200 ms idle repaint the loop already does.
const INPUT_POLL: Duration = Duration::from_millis(100);
/// How long the input reader sleeps between checks while stdin is handed to a
/// child process. Only ever runs during an `$EDITOR` session.
const PAUSED_BACKOFF: Duration = Duration::from_millis(50);
/// What woke the event loop.
#[derive(Debug)]
enum Wake {
    /// A terminal event arrived.
    Input(Event),
    /// Terminal input is gone (reader thread ended); shut down.
    InputClosed,
    /// One or more domain events arrived (already debounced+drained).
    Changed,
    /// Idle timeout — repaint to refresh the sync indicator.
    Idle,
}

async fn run(term: &mut Tty, core: &Core, sync_on: bool) -> Result<(), Box<dyn std::error::Error>> {
    // The user's zone, resolved once: relative capture/schedule dates
    // (`^tomorrow`, `9am`) mean the user's day, not UTC. Held on the state so
    // the reducer stays pure and tests can pin it.
    let mut state = ViewState {
        tz: jiff::tz::TimeZone::system(),
        ..Default::default()
    };
    // `~/.config/sunrise/keys.toml` (docs/07-clients/tui.md). Absent is the
    // normal case and silent; malformed is loud and then falls back to the
    // defaults, because a typo in a config file must never cost the user a
    // working keyboard.
    let (map, key_warnings) = keymap::load_keymap(keymap::keys_config_path().as_deref());
    for w in &key_warnings {
        eprintln!("sunrise-tui: {w}");
    }
    state.keymap = map;
    // Image-preview state (`:preview <path>`). Owned here because Picker and
    // the protocol state are not Clone; render fns borrow them per frame.
    // init_picker queries the terminal, so this runs after entering the
    // alternate screen but before the event loop reads input.
    #[cfg(feature = "images")]
    let mut picker = sunrise_tui::images::init_picker();
    #[cfg(feature = "images")]
    let mut preview: Option<sunrise_tui::images::Preview> = None;

    // Terminal input is read on a dedicated OS thread and forwarded over an
    // mpsc channel. Chosen over crossterm's async `EventStream` because that
    // pulls in a `futures` + `mio` dependency this crate otherwise doesn't
    // need; the point is that input is one arm of a `select!`, so
    // `Core::changes()` can drive a repaint without the user touching the
    // keyboard.
    //
    // The thread polls rather than blocking forever in `event::read()` so that
    // stdin can be *handed over*: running `$EDITOR` means another process owns
    // the terminal, and a reader parked inside `read()` would steal its
    // keystrokes with no way to be called off. See [`InputGate`].
    let gate = Arc::new(InputGate::default());
    let reader_gate = Arc::clone(&gate);
    let (key_tx, mut key_rx) = tokio::sync::mpsc::channel::<Event>(64);
    std::thread::spawn(move || loop {
        match reader_gate.read_one() {
            // Send outside the gate: a full channel must not pin stdin.
            Read::Event(ev) => {
                if key_tx.blocking_send(ev).is_err() {
                    break;
                }
            }
            Read::Idle => {}
            Read::Closed => break,
        }
    });

    // Live repaint: every locally-applied *and* remotely-received op is
    // published here, so an inbound sync batch now redraws immediately.
    let mut changes = core.changes();
    // Cleared if the broadcast sender ever goes away, so the select! arm stops
    // firing instead of spinning on a closed channel.
    let mut changes_open = true;

    refresh(core, &mut state).await;

    loop {
        // Refresh the status-line sync indicator every iteration so it tracks
        // the background driver in real time (independent of view refreshes).
        poll_sync(core, &mut state, sync_on).await;
        term.draw(|f| {
            let area = f.area();
            #[cfg(feature = "images")]
            render(f, area, &state, preview.as_mut());
            #[cfg(not(feature = "images"))]
            render(f, area, &state);
        })?;

        let wake = tokio::select! {
            ev = key_rx.recv() => match ev {
                Some(ev) => Wake::Input(ev),
                None => Wake::InputClosed,
            },
            res = changes.recv(), if changes_open => {
                // `Lagged` means the broadcast buffer overflowed — the exact
                // events are lost but "something changed" still holds, so a
                // full refresh is the correct response.
                if matches!(res, Err(RecvError::Closed)) {
                    changes_open = false;
                    Wake::Idle
                } else {
                    // Coalesce the rest of the burst: a catch-up batch of N
                    // ops repaints once, not N times.
                    drain_changes(&mut changes, CHANGE_DEBOUNCE).await;
                    Wake::Changed
                }
            }
            () = tokio::time::sleep(IDLE_REDRAW) => Wake::Idle,
        };

        let ev = match wake {
            Wake::Idle => continue,
            Wake::InputClosed => break,
            Wake::Changed => {
                refresh(core, &mut state).await;
                continue;
            }
            Wake::Input(ev) => ev,
        };

        let Event::Key(k) = ev else {
            // Resize (and everything else) just falls through to the redraw
            // at the top of the loop; the layout is derived from `f.area()`.
            continue;
        };
        // Windows and kitty-protocol terminals also report key *release*;
        // acting on both would double every keystroke.
        if k.kind != KeyEventKind::Press {
            continue;
        }
        let Some(action) = state
            .keymap
            .dispatch(k.code, state.mode, state.vim_mode, state.view)
        else {
            continue;
        };
        match apply_action(action, &mut state, core.now_ms()) {
            Outcome::Quit => break,
            Outcome::None => {}
            Outcome::Refresh => refresh(core, &mut state).await,
            Outcome::Submit(cmd) => {
                if let Err(e) = core.submit(*cmd).await {
                    state.status = format!("error: {e}");
                }
                refresh(core, &mut state).await;
            }
            Outcome::SubmitMany(cmds) => {
                // One refresh for the whole batch. Errors are reported but do
                // not abort the rest: a bulk op that silently stopped halfway
                // would be worse than one that finishes and says what failed.
                let mut failed = 0usize;
                for cmd in cmds {
                    if core.submit(cmd).await.is_err() {
                        failed += 1;
                    }
                }
                if failed > 0 {
                    state.status = format!("{failed} of the selected tasks failed");
                }
                refresh(core, &mut state).await;
            }
            Outcome::OpenStreamPicker { tasks, title } => {
                match core.query(Query::StreamList).await {
                    Ok(QueryResult::Streams(rows)) => {
                        state.open_stream_picker(tasks, title, rows);
                    }
                    _ => state.status = "could not load streams".into(),
                }
            }
            Outcome::EditBody { id, body } => match edit_in_editor(term, &gate, &body) {
                Ok(Some(new_body)) => {
                    let patch = TaskPatch {
                        body: Some((!new_body.is_empty()).then_some(NoteBody(new_body))),
                        ..Default::default()
                    };
                    if let Err(e) = core.submit(Command::UpdateTask { id, patch }).await {
                        state.status = format!("error: {e}");
                    } else {
                        state.status = "body saved".into();
                    }
                    refresh(core, &mut state).await;
                }
                Ok(None) => state.status = "body unchanged".into(),
                Err(e) => state.status = format!("editor: {e}"),
            },
            Outcome::OpenTask(id) => match core.query(Query::EntityById(id)).await {
                Ok(QueryResult::Task(t)) => {
                    state.focused_task = Some(*t);
                    if state.view != View::Focus {
                        state.prev_view = Some(state.view);
                        state.view = View::Focus;
                    }
                    refresh(core, &mut state).await;
                }
                _ => state.status = format!("no such task: {}", id.to_str()),
            },
            Outcome::ShowDevices => match core.query(Query::DeviceList).await {
                Ok(QueryResult::Devices(rows)) => state.show_devices(rows),
                _ => state.status = "could not load devices".into(),
            },
            Outcome::Preview(path) => {
                #[cfg(feature = "images")]
                match sunrise_tui::images::load_preview(&mut picker, &path) {
                    Ok(p) => {
                        preview = Some(p);
                        state.status = format!("preview: {}", path.display());
                    }
                    Err(e) => state.status = e,
                }
                // Defensive: apply_command only emits this effect when
                // the `images` feature is compiled in.
                #[cfg(not(feature = "images"))]
                {
                    let _ = path;
                    state.status = "images feature disabled".into();
                }
            }
        }
        // Closing Focus drops any loaded preview with it.
        #[cfg(feature = "images")]
        if state.view != View::Focus {
            preview = None;
        }
    }
    Ok(())
}

/// Open `body` in the user's editor and return the edited bytes, or `None` if
/// nothing changed.
///
/// `docs/07-clients/tui.md`: "Long-form note editing: `e` opens the note body
/// in `$EDITOR` (vim/helix/nano), saves on exit." Bound to `E` here so the
/// already-tested inline title edit keeps `e`.
///
/// The terminal is handed over completely — alternate screen left, raw mode
/// off — and taken back on return. `gate` is held for the whole handover so the
/// input reader cannot race the editor for stdin.
///
/// Nothing is written back when the editor exits non-zero or leaves the file
/// byte-identical: an aborted edit (`:cq`, `:q!`) must not overwrite a body.
fn edit_in_editor(
    term: &mut Tty,
    gate: &Arc<InputGate>,
    body: &[u8],
) -> Result<Option<Vec<u8>>, String> {
    let _held = gate.suspend();
    teardown(term).map_err(|e| e.to_string())?;
    let result = run_editor_on(body);
    let resumed = resume(term).map_err(|e| e.to_string());
    // Restoring the screen matters more than the edit: report a failure to come
    // back before a failure to edit, because the former leaves the UI unusable.
    resumed?;
    result
}

/// Arbitrates ownership of stdin between the input reader thread and a
/// suspended-terminal child process (`$EDITOR`).
///
/// Two parts, both needed:
///
/// * the `Mutex` is held across the reader's `poll` + `read` pair, so a
///   suspend that acquires it knows no read is in flight and cannot lose a
///   keystroke to a half-finished one;
/// * the `paused` flag makes the handover starvation-free. Without it the
///   reader — which releases the mutex only for the instant between polls —
///   could re-acquire it ahead of a waiting suspend indefinitely. With it the
///   reader stops contending entirely, so the suspend waits at most one poll
///   interval.
#[derive(Debug, Default)]
struct InputGate {
    stdin: Mutex<()>,
    paused: std::sync::atomic::AtomicBool,
}

impl InputGate {
    /// Reader side: poll for one event, yielding stdin while paused.
    fn read_one(&self) -> Read {
        if self.paused.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(PAUSED_BACKOFF);
            return Read::Idle;
        }
        let _held = self
            .stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match event::poll(INPUT_POLL) {
            Ok(true) => event::read().map_or(Read::Closed, Read::Event),
            Ok(false) => Read::Idle,
            Err(_) => Read::Closed,
        }
    }

    /// Suspend side: take stdin until the returned guard drops.
    fn suspend(&self) -> InputGateGuard<'_> {
        self.paused
            .store(true, std::sync::atomic::Ordering::Release);
        let held = self
            .stdin
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        InputGateGuard {
            gate: self,
            _held: held,
        }
    }
}

/// One turn of the input reader loop.
enum Read {
    /// A terminal event to forward.
    Event(Event),
    /// Nothing arrived within the poll window (or stdin is paused).
    Idle,
    /// The terminal is gone; the reader should stop.
    Closed,
}

/// Holds stdin for the duration of a terminal suspend; releases it on drop
/// (including on an early `?` return, which is why this is a guard and not a
/// pair of calls).
struct InputGateGuard<'a> {
    gate: &'a InputGate,
    _held: std::sync::MutexGuard<'a, ()>,
}

impl Drop for InputGateGuard<'_> {
    fn drop(&mut self) {
        self.gate
            .paused
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// The editor round trip itself, with the terminal already released.
///
/// The decision rules (seed the file, ignore an aborted or unchanged edit,
/// always clean up) live in `sunrise_tui::editor` where they are unit-tested;
/// this is only the process spawn.
fn run_editor_on(body: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let name = format!("sunrise-note-{}.md", std::process::id());
    editor::edit_bytes(&std::env::temp_dir(), &name, body, |path| {
        let (program, args) = editor::resolve_editor(
            std::env::var("EDITOR").ok().as_deref(),
            std::env::var("VISUAL").ok().as_deref(),
        );
        let status = std::process::Command::new(&program)
            .args(&args)
            .arg(path)
            .status()
            .map_err(|e| format!("could not run {program}: {e}"))?;
        Ok(if status.success() {
            editor::EditorExit::Ok
        } else {
            editor::EditorExit::Failed
        })
    })
}

/// Refresh the status-line sync indicator from the core's live sync status.
/// Shows `off` (with the DB outbox depth) when no `SUNRISE_SYNC_URL` was set,
/// otherwise the driver's live state.
async fn poll_sync(core: &Core, state: &mut ViewState, sync_on: bool) {
    if let Ok(QueryResult::SyncStatus(s)) = core.query(Query::SyncStatus).await {
        state.sync = Some(if sync_on {
            SyncIndicator::live(s.state, s.outbox_pending)
        } else {
            SyncIndicator::off(s.outbox_pending)
        });
    }
}

async fn refresh(core: &Core, state: &mut ViewState) {
    // Streams and contexts back `#stream` / `@context` resolution in capture,
    // the capture preview, and the Focus detail line, so both are loaded for
    // every view rather than only the ones that display them. Two indexed
    // queries per refresh.
    if let Ok(QueryResult::Streams(rows)) = core.query(Query::StreamList).await {
        state.streams = rows;
        state.after_streams_loaded();
    }
    if let Ok(QueryResult::Contexts(rows)) = core.query(Query::Contexts).await {
        state.contexts = rows;
    }
    // A triage pass always reads the Inbox, whatever the nominal view is.
    if state.triage {
        load_tasks(core, state, Query::Inbox).await;
        return;
    }
    match state.view {
        View::Today => {
            let q = Query::Today {
                now_ms: core.now_ms(),
                contexts: vec![],
            };
            load_tasks(core, state, q).await;
        }
        View::Inbox => load_tasks(core, state, Query::Inbox).await,
        View::Stream => match state.selected_stream_row().map(|r| r.id) {
            Some(id) => load_tasks(core, state, Query::StreamTasks(id)).await,
            None => {
                state.tasks.clear();
                state.after_tasks_loaded();
            }
        },
        View::Search => {
            let q = Query::Search {
                text: state.input.clone(),
                limit: 100,
            };
            load_tasks(core, state, q).await;
        }
        View::Routines => {
            if let Ok(QueryResult::Routines(rs)) = core.query(Query::Routines).await {
                let now =
                    jiff::Timestamp::from_millisecond(i64::try_from(core.now_ms()).unwrap_or(0))
                        .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
                state.routines = routine_rows(&rs, now);
                state.after_routines_loaded();
            }
        }
        View::Focus => {
            let id = state.focused_task.as_ref().map(|t| t.id);
            if let Some(id) = id {
                if let Ok(QueryResult::Task(t)) = core.query(Query::EntityById(id)).await {
                    state.focused_task = Some(*t);
                }
            }
        }
    }
}

async fn load_tasks(core: &Core, state: &mut ViewState, q: Query) {
    if let Ok(QueryResult::Tasks(tasks) | QueryResult::StreamTasks(tasks)) = core.query(q).await {
        state.tasks = tasks;
        state.after_tasks_loaded();
    }
}

/// Non-interactive subcommands.
///
/// `docs/08-features/inbox-and-capture.md` lists a TUI subcommand as a
/// first-class capture surface ("one-shot commit"), and
/// `docs/07-clients/parity-matrix.md` marks an OS automation surface as MUST
/// for the TUI. These also make the vault scriptable and testable without a
/// terminal, which the interactive loop is not.
///
/// Deliberately hand-rolled rather than pulling in an arg parser: the surface
/// is a handful of positional subcommands, and the crate currently has no
/// dependency that is not already earning its place.
mod cli {
    use super::{vault_dir, DEV_ROOT};
    use sunrise_core::{Command, Core, Query, QueryResult};
    use sunrise_tui::livesync;

    const USAGE: &str = "\
sunrise-tui — terminal client for Sunrise

USAGE:
    sunrise-tui                      launch the interactive TUI
    sunrise-tui capture <text>...    parse and commit one task, then exit
    sunrise-tui today                list today's tasks
    sunrise-tui inbox                list inbox tasks
    sunrise-tui streams              list streams with open counts
    sunrise-tui search <query>...    full-text search
    sunrise-tui help                 show this message

CAPTURE SYNTAX:
    #stream  @context  ^when  !priority(1-5)  ~duration  *due:when*

    sunrise-tui capture 'Renew passport #travel ^next saturday !1 ~1h'

ENVIRONMENT:
    SUNRISE_VAULT   vault directory (default ~/.sunrise/vault)

FILES:
    ~/.config/sunrise/keys.toml   optional key overrides, one `action = \"key\"`
                                  per line (e.g. capture = \"n\"). Honours
                                  XDG_CONFIG_HOME. Absent means defaults; a
                                  malformed file warns and uses defaults.
";

    /// Dispatch a subcommand. Returns `Ok` on success; the process exit code
    /// is non-zero only on a real failure, so scripts can branch on it.
    pub(crate) async fn run(sub: &str, rest: &[String]) -> Result<(), Box<dyn std::error::Error>> {
        // A CLI's whole job is writing to stdout; the workspace-wide ban on
        // print_stdout exists to keep it out of *library* code.
        #![allow(clippy::print_stdout)]
        match sub {
            "help" | "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!("sunrise-tui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => {}
        }

        let dir = vault_dir();
        std::fs::create_dir_all(&dir).ok();
        // Subcommands are one-shot and offline: opening a sync driver for a
        // command that exits milliseconds later would just churn the relay.
        let (core, _log) = livesync::open_with_plan(
            dir,
            env!("CARGO_PKG_VERSION"),
            DEV_ROOT,
            &livesync::SyncPlan::default(),
        )
        .await?;

        let result = dispatch(&core, sub, rest).await;
        core.shutdown().await;
        result
    }

    async fn dispatch(
        core: &Core,
        sub: &str,
        rest: &[String],
    ) -> Result<(), Box<dyn std::error::Error>> {
        #![allow(clippy::print_stdout)]
        match sub {
            "capture" => {
                let text = rest.join(" ");
                if text.trim().is_empty() {
                    return Err("capture needs some text; see `sunrise-tui help`".into());
                }
                // System zone, so `^tomorrow 9am` means the user's 9am.
                let tz = jiff::tz::TimeZone::system();
                let parsed = core.capture(&text, &tz).await?;
                for u in &parsed.unresolved {
                    // Warnings go to stderr so stdout stays parseable.
                    eprintln!("note: {u:?}");
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
            "search" => {
                let text = rest.join(" ");
                if text.trim().is_empty() {
                    return Err("search needs a query".into());
                }
                let q = Query::Search { text, limit: 100 };
                print_tasks(core.query(q).await?);
                Ok(())
            }
            other => Err(format!("unknown subcommand {other:?}; try `sunrise-tui help`").into()),
        }
    }

    fn print_tasks(r: QueryResult) {
        #![allow(clippy::print_stdout)]
        let tasks = match r {
            QueryResult::Tasks(t) | QueryResult::StreamTasks(t) => t,
            _ => return,
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
}
