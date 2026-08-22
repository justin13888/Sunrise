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
use std::time::Duration;
use sunrise_core::{Core, Query, QueryResult};
use sunrise_tui::livesync;
use sunrise_tui::runtime::{drain_changes, CHANGE_DEBOUNCE};
use sunrise_tui::{
    apply_action, dispatch, render, routine_rows, Outcome, SyncIndicator, View, ViewState,
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

/// How long the loop waits with no input before repainting anyway. Keeps the
/// status-line sync indicator ticking without busy-looping.
const IDLE_REDRAW: Duration = Duration::from_millis(200);
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
    let mut state = ViewState::default();
    // Image-preview state (`:preview <path>`). Owned here because Picker and
    // the protocol state are not Clone; render fns borrow them per frame.
    // init_picker queries the terminal, so this runs after entering the
    // alternate screen but before the event loop reads input.
    #[cfg(feature = "images")]
    let mut picker = sunrise_tui::images::init_picker();
    #[cfg(feature = "images")]
    let mut preview: Option<sunrise_tui::images::Preview> = None;

    // Terminal input is read on a dedicated OS thread that blocks in
    // `event::read()` and forwards over an mpsc channel. Chosen over
    // crossterm's async `EventStream` because that pulls in a `futures` +
    // `mio` dependency this crate otherwise doesn't need, and because a
    // blocking read costs nothing while idle — the old `event::poll(200ms)`
    // loop woke 5x/second forever. Either way the point is the same: input
    // is now just one arm of a `select!`, so `Core::changes()` can drive a
    // repaint without the user touching the keyboard.
    let (key_tx, mut key_rx) = tokio::sync::mpsc::channel::<Event>(64);
    std::thread::spawn(move || {
        // Ends when `event::read()` errors or the receiver is dropped (app
        // exiting); the process tears down either way.
        while let Ok(ev) = event::read() {
            if key_tx.blocking_send(ev).is_err() {
                break;
            }
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
        let Some(action) = dispatch(k.code, state.mode, state.vim_mode, state.view) else {
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
            Outcome::OpenStreamPicker { task, title } => {
                match core.query(Query::StreamList).await {
                    Ok(QueryResult::Streams(rows)) => {
                        state.open_stream_picker(task, title, rows);
                    }
                    _ => state.status = "could not load streams".into(),
                }
            }
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
    match state.view {
        View::Today => {
            let q = Query::Today {
                now_ms: core.now_ms(),
                contexts: vec![],
            };
            load_tasks(core, state, q).await;
        }
        View::Inbox => load_tasks(core, state, Query::Inbox).await,
        View::Stream => {
            if let Ok(QueryResult::Streams(rows)) = core.query(Query::StreamList).await {
                state.streams = rows;
                state.after_streams_loaded();
            }
            match state.selected_stream_row().map(|r| r.id) {
                Some(id) => load_tasks(core, state, Query::StreamTasks(id)).await,
                None => {
                    state.tasks.clear();
                    state.after_tasks_loaded();
                }
            }
        }
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
                // Stream rows back the "stream: <name>" detail line.
                if let Ok(QueryResult::Streams(rows)) = core.query(Query::StreamList).await {
                    state.streams = rows;
                    state.after_streams_loaded();
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
