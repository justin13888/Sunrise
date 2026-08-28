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
//!
//! # Logs go to a file, never to the terminal
//!
//! This binary owns the alternate screen. A single line written to stdout or
//! stderr while Ratatui holds it lands in the middle of the user's board and
//! the diffing renderer never repaints over it, so the corruption persists
//! until the next full clear. Every log record therefore goes to
//! `$XDG_STATE_HOME/sunrise/log/sunrise-tui.ndjson` (default
//! `~/.local/state/sunrise/log/…`, override with `SUNRISE_LOG_FILE`), which
//! [`sunrise_log::init_file`] caps at 16 MiB with one kept generation.
//!
//! If that file cannot be opened the binary runs **with no logging at all**.
//! Falling back to stderr would trade a missing log for a broken display, and
//! that is the wrong way round.

#![allow(
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
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sunrise_cbor::version::{CRYPTO_SUITE_V, DOC_SCHEMA_V, WIRE_PROTO_V};
use sunrise_cli::livesync;
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::{NoteBody, TaskPatch};
use sunrise_tui::runtime::{drain_changes, CHANGE_DEBOUNCE};
use sunrise_tui::{
    apply_action, editor, keymap, render, routine_rows, sort_today, BrowseTarget, CascadeReport,
    Outcome, SyncIndicator, View, ViewState,
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
    // First statement in the process. `init_file` is fallible and its failure
    // is deliberately swallowed: see the module docs — a TUI that cannot log
    // is a working TUI, a TUI that logs to stderr is a wrecked screen.
    // The `Result` is deliberately discarded rather than reported: an
    // unwritable state directory must not stop the app, and there is nowhere
    // to report it to that would not be worse than staying quiet.
    let _ = sunrise_log::init_file("sunrise-tui");
    // Same shape as `sunrise-server`'s `srv.start`: the protocol versions are
    // reported once per process rather than on every record.
    let proto = sunrise_log::ProtoVersions::new(WIRE_PROTO_V, DOC_SCHEMA_V, CRYPTO_SUITE_V);
    tracing::info!(
        ev = "ui.start",
        app_v = env!("CARGO_PKG_VERSION"),
        wire_v = u64::from(proto.wire),
        doc_v = u64::from(proto.doc),
        crypto_v = u64::from(proto.crypto),
        "sunrise-tui starting"
    );
    // Non-interactive subcommands live in `sunrise-cli`; this binary is the
    // interactive client and nothing else.
    if let Some(arg) = std::env::args().nth(1) {
        return Err(format!(
            "sunrise-tui takes no arguments (got {arg:?}); the subcommands are `sunrise`"
        )
        .into());
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
    // `startup_log` is the demo banner, not a log: it names cert *file paths*,
    // which the redaction allowlist does not admit. `livesync::apply_plan`
    // has already emitted the structured events for the same steps. With the
    // alternate screen about to open there is nowhere safe to print it, so it
    // is dropped rather than written over the user's board.
    drop(startup_log);

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
    // Bracketed paste turns a pasted block into one `Event::Paste` instead of
    // a burst of synthetic keystrokes. Without it a pasted URL is dispatched
    // character by character through the *normal-mode* keymap, which is how a
    // paste into the wrong pane deletes tasks. Terminals that do not support
    // it ignore the sequence, so the failure mode is the old behaviour.
    stdout.execute(EnableBracketedPaste)?;
    // Mouse capture is **opt-in** (`SUNRISE_MOUSE=1`). Capturing steals the
    // terminal's own selection and copy, which is a bad trade to force on a
    // client whose users live in tmux and copy text out of panes all day —
    // and `docs/07-clients/tui.md` makes mouse support optional and insists
    // the TUI works without it.
    if mouse_enabled() {
        stdout.execute(EnableMouseCapture)?;
    }
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Whether to capture the mouse. Off unless `SUNRISE_MOUSE` is set to
/// something other than `0`.
fn mouse_enabled() -> bool {
    std::env::var("SUNRISE_MOUSE").is_ok_and(|v| v != "0" && !v.is_empty())
}

fn teardown(term: &mut Tty) -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    if mouse_enabled() {
        term.backend_mut().execute(DisableMouseCapture)?;
    }
    term.backend_mut().execute(DisableBracketedPaste)?;
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
    term.backend_mut().execute(EnableBracketedPaste)?;
    if mouse_enabled() {
        term.backend_mut().execute(EnableMouseCapture)?;
    }
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
    // A keymap warning names the offending action and key, both of which come
    // from the user's own config file rather than from vault content — but the
    // *count* is what a support session actually needs, and the file is right
    // there to read. So the log records how many, not which.
    if !key_warnings.is_empty() {
        tracing::warn!(
            ev = "ui.keymap.invalid",
            n_ops = key_warnings.len() as u64,
            result = "skipped",
            "keys.toml entries ignored; defaults used for them"
        );
    }
    state.keymap = map;
    // Saved views (`docs/07-clients/parity-matrix.md`: MUST). Loaded once;
    // rewritten whenever `:save` or `:unsave` changes the set. A malformed
    // line costs that view and nothing else, exactly as for `keys.toml`.
    let (saved, view_warnings) =
        sunrise_client_core::views::load(sunrise_client_core::views::config_path().as_deref());
    if !view_warnings.is_empty() {
        tracing::warn!(
            ev = "ui.keymap.invalid",
            n_ops = view_warnings.len() as u64,
            result = "skipped",
            "views.toml entries ignored"
        );
    }
    state.saved_views = saved;
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
        // The one place the running timer reads the clock. Nothing accumulates
        // it: `render` derives elapsed/remaining from this value against the
        // immutable session record, every frame, from scratch.
        state.now_ms = core.now_ms();
        // A "page" is a screenful of the list the user is looking at, so the
        // page keys need the real terminal size. Measured here, once per
        // frame, and read by the reducer — which stays pure.
        state.viewport_rows =
            render::viewport_rows(term.size()?.height, state.capture_preview.is_some());
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

        let action = match ev {
            // A bracketed paste is one action, whatever it contains — and it
            // is only ever text, so it is dropped outside a text prompt rather
            // than being replayed as commands.
            Event::Paste(text) => {
                if matches!(state.mode, keymap::Mode::Insert | keymap::Mode::Command) {
                    keymap::Action::InsertStr(text)
                } else {
                    state.status = "paste: open a prompt first (c to capture)".into();
                    continue;
                }
            }
            Event::Key(k) => {
                // Windows and kitty-protocol terminals also report key
                // *release*; acting on both would double every keystroke.
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                match state.keymap.dispatch(
                    k.code,
                    k.modifiers,
                    state.dispatch_mode(),
                    state.vim_mode,
                    state.view,
                ) {
                    Some(a) => a,
                    // An unbound key while a `g` chord is half-typed abandons
                    // it: leaving the latch armed would make the *next*
                    // keypress a jump the user did not ask for.
                    None => {
                        if state.cancel_chord() {
                            state.status.clear();
                        }
                        continue;
                    }
                }
            }
            Event::Mouse(m) => {
                let area = term.size()?;
                let area = ratatui::layout::Rect {
                    x: 0,
                    y: 0,
                    width: area.width,
                    height: area.height,
                };
                match m.kind {
                    // The wheel drives the same cursor keys, so scrolling a
                    // list and pressing `j` cannot diverge.
                    MouseEventKind::ScrollDown => keymap::Action::Next,
                    MouseEventKind::ScrollUp => keymap::Action::Prev,
                    MouseEventKind::Down(MouseButton::Left) => {
                        match sunrise_tui::hit(area, &state, m.column, m.row) {
                            Some(h) => keymap::Action::Click(h),
                            None => continue,
                        }
                    }
                    _ => continue,
                }
            }
            // Resize (and everything else) just falls through to the redraw
            // at the top of the loop; the layout is derived from `f.area()`.
            _ => continue,
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
            Outcome::ShowActivity { entity, title } => {
                // Enough history to answer "what happened?" without the
                // overlay becoming an archive browser.
                const ACTIVITY_LIMIT: u32 = 200;
                let q = Query::ActivityTimeline {
                    entity,
                    limit: ACTIVITY_LIMIT,
                };
                match core.query(q).await {
                    Ok(QueryResult::Activity(rows)) => state.show_activity(title, rows),
                    _ => state.status = "could not load the activity feed".into(),
                }
            }
            Outcome::ShowDevices => match core.query(Query::DeviceList).await {
                Ok(QueryResult::Devices(rows)) => state.show_devices(rows),
                _ => state.status = "could not load devices".into(),
            },
            Outcome::CaptureAside(text) => {
                // Committed through `Core::capture_aside`, which drops any
                // `#stream` the parser resolved so the aside lands in the
                // Inbox rather than in the focused task's stream.
                match core.capture_aside(&text, &state.tz).await {
                    Ok(c) => {
                        if let Err(e) = core.submit(Command::CreateTask(c.draft)).await {
                            state.status = format!("error: {e}");
                        }
                    }
                    Err(e) => state.status = format!("error: {e}"),
                }
                refresh(core, &mut state).await;
            }
            Outcome::SubmitThenCascade { cmds, task } => {
                for cmd in cmds {
                    if let Err(e) = core.submit(cmd).await {
                        state.status = format!("error: {e}");
                    }
                }
                state.focus.cascade = load_cascade(core, task).await;
                refresh(core, &mut state).await;
            }
            Outcome::Export {
                dataset,
                format,
                path,
            } => {
                state.status = export_stats(core, dataset, format, path).await;
            }
            Outcome::PersistViews => {
                if let Some(path) = sunrise_client_core::views::config_path() {
                    let body = sunrise_client_core::views::to_file(&state.saved_views);
                    // A failure to persist must not lose the *in-memory* set:
                    // the views still work for this session, and the status
                    // line says why they will not survive it.
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    if let Err(e) = std::fs::write(&path, body) {
                        state.status = format!("could not write {}: {e}", path.display());
                    }
                }
            }
            Outcome::ShowFocusStats => {
                let q = Query::FocusStats {
                    stream: None,
                    since_ms: None,
                    now_ms: core.now_ms(),
                };
                match core.query(q).await {
                    Ok(QueryResult::FocusStats(s)) => state.show_focus_stats(s),
                    _ => state.status = "could not load focus stats".into(),
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

/// Read the unblock cascade for a task that was just completed, resolving the
/// released tasks' titles so the report names work rather than ids.
///
/// Informational only: it says what moved and keeps no score, per
/// `docs/08-features/focus-mode.md` §What we don't do.
async fn load_cascade(core: &Core, task: sunrise_id::EntityRef) -> Option<CascadeReport> {
    /// Enough released tasks to make the payoff concrete without turning the
    /// pane into a list.
    const MAX_NAMED: usize = 4;
    let cascade = match core.query(Query::UnblockCascade(task)).await {
        Ok(QueryResult::UnblockCascade(c)) => *c,
        _ => return None,
    };
    let mut released = Vec::new();
    for id in cascade.released.iter().take(MAX_NAMED) {
        if let Ok(QueryResult::Task(t)) = core.query(Query::EntityById(*id)).await {
            released.push(t.title);
        }
    }
    Some(CascadeReport { cascade, released })
}

/// Read the focus-session side of the world: the running session (if any), the
/// ranked planner queue, and the focused task's session log.
///
/// `RunningFocusSessions` is read on **every** refresh, not just in the Focus
/// view: a `start` with no `end` is exactly what a crash — or a session begun
/// on another device — leaves behind, and `after_focus_loaded` turns that into
/// the session taking the keyboard back over.
async fn refresh_focus(core: &Core, state: &mut ViewState) {
    if let Ok(QueryResult::FocusSessions(rows)) = core.query(Query::RunningFocusSessions).await {
        state.focus.running = rows
            .into_iter()
            .max_by_key(|r| r.session.start.started_at_ms());
    }
    // Whatever the session is on wins over the list cursor: the pane must show
    // the task whose clock is running.
    if let Some(task) = state.focus.running_task() {
        if state.focused_task.as_ref().is_none_or(|t| t.id != task) {
            if let Ok(QueryResult::Task(t)) = core.query(Query::EntityById(task)).await {
                state.focused_task = Some(*t);
            }
        }
    }
    if state.view != View::Focus && !state.focus.is_running() {
        return;
    }
    // The planner is the *idle* Focus view; a running session replaces it.
    if state.focus.is_running() {
        state.focus.plan.clear();
    } else if let Ok(QueryResult::FocusPlan(rows)) = core
        .query(Query::FocusPlan {
            stream: None,
            energy: state.focus.energy,
            length: state.focus.length,
            limit: 12,
        })
        .await
    {
        state.focus.plan = rows;
    }
    // The session log behind `break_after`: work segments already finished.
    state.focus.sessions.clear();
    if let Some(task) = state
        .focus
        .running_task()
        .or_else(|| state.focused_task.as_ref().map(|t| t.id))
    {
        if let Ok(QueryResult::FocusSessions(rows)) = core
            .query(Query::TaskFocusSessions { task, limit: 64 })
            .await
        {
            state.focus.sessions = rows;
        }
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
        state.after_contexts_loaded();
    }
    refresh_deps(core, state).await;
    refresh_focus(core, state).await;
    state.after_focus_loaded();
    // A triage pass always reads the Inbox, whatever the nominal view is.
    if state.triage {
        load_tasks(core, state, Query::Inbox).await;
        return;
    }
    match state.view {
        View::Today => {
            let now_ms = core.now_ms();
            let q = Query::Today {
                now_ms,
                // Today is the one query that filters by context itself, so
                // the index does the work here; every other view narrows the
                // loaded list. See `ViewState::apply_context_filter`.
                contexts: state.context_filter.clone(),
            };
            load_tasks(core, state, q).await;
            // The view is grouped (overdue / due today / scheduled / anytime),
            // and the grouping only reads as one if the rows are in group
            // order. Sorted here rather than in the query because the grouping
            // is a *civil-day* question in the user's zone, which the core
            // deliberately does not know.
            sort_today(&mut state.tasks, now_ms, &state.tz);
            state.after_tasks_loaded();
        }
        View::Inbox => load_tasks(core, state, Query::Inbox).await,
        View::Stream => match state.browse_target() {
            // The sidebar's two lists answer the domain's two axes: a Stream
            // partitions work, a Context cuts across every Stream.
            Some(BrowseTarget::Stream(id)) => load_tasks(core, state, Query::StreamTasks(id)).await,
            Some(BrowseTarget::Context(id)) => {
                load_tasks(core, state, Query::ContextTasks(id)).await
            }
            None => {
                state.tasks.clear();
                state.after_tasks_loaded();
            }
        },
        View::Search => {
            let q = Query::Search {
                text: state.search_query.clone(),
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
        View::Review => refresh_review(core, state).await,
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

/// Render a stats dataset and write it to disk.
///
/// Writing a file rather than printing is not a convenience: this binary owns
/// the alternate screen, and a CSV dumped to stdout lands in the middle of the
/// user's board where the diffing renderer will never repaint over it. The
/// status line reports the path, which is the part a user needs next.
async fn export_stats(
    core: &Core,
    dataset: sunrise_domain::ExportDataset,
    format: sunrise_domain::ExportFormat,
    path: Option<std::path::PathBuf>,
) -> String {
    /// Weeks of history a trend export covers, matching the Review view.
    const TREND_WEEKS: u32 = 12;
    let now_ms = core.now_ms();
    let q = Query::ExportStats {
        dataset,
        format,
        weeks: TREND_WEEKS,
        now_ms,
    };
    let body = match core.query(q).await {
        Ok(QueryResult::Export(s)) => s,
        Ok(other) => return format!("export: unexpected result {other:?}"),
        Err(e) => return format!("export failed: {e}"),
    };
    let path = path.unwrap_or_else(|| default_export_path(dataset, format, now_ms));
    match std::fs::write(&path, body.as_bytes()) {
        Ok(()) => format!(
            "exported {} rows to {}",
            body.lines().count(),
            path.display()
        ),
        Err(e) => format!("could not write {}: {e}", path.display()),
    }
}

/// `./sunrise-trends-2026-03-02.csv` — dated, so a second export does not
/// silently overwrite the first.
fn default_export_path(
    dataset: sunrise_domain::ExportDataset,
    format: sunrise_domain::ExportFormat,
    now_ms: u64,
) -> std::path::PathBuf {
    let day = i64::try_from(now_ms)
        .ok()
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map_or_else(
            || "undated".to_string(),
            |ts| {
                let d = ts.to_zoned(jiff::tz::TimeZone::system()).date();
                format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())
            },
        );
    std::path::PathBuf::from(format!(
        "sunrise-{}-{day}.{}",
        dataset.as_str(),
        format.as_str()
    ))
}

/// Refresh the derived dependency state every list row reads.
///
/// Neither number is stored on a Task: `open_blockers` and `unblocks` are
/// recomputed by `Query::Actionable` against the blockers' *current* states,
/// which is what lets a blocker completing anywhere — here or merged in from
/// another device — flip its dependents with no repair pass. Reading
/// `blocked_by.len()` instead would mark a task blocked forever once anything
/// had ever blocked it.
///
/// One query per refresh, over open tasks only, so it costs the same
/// dependency walk the planner already does.
async fn refresh_deps(core: &Core, state: &mut ViewState) {
    /// Open tasks whose dependency state is projected. Past this the badges
    /// are simply absent rather than wrong — `Actionable` returns
    /// actionable-first, so the tail is the least interesting end of the list.
    const LIMIT: u32 = 2000;
    let q = Query::Actionable {
        stream: None,
        limit: LIMIT,
    };
    if let Ok(QueryResult::Actionable(rows)) = core.query(q).await {
        state.deps = rows
            .into_iter()
            .map(|r| {
                (
                    r.task.id,
                    sunrise_tui::Dep {
                        open_blockers: r.open_blockers,
                        unblocks: r.unblocks,
                    },
                )
            })
            .collect();
    }
}

/// Load whichever Review panel is showing.
///
/// One panel, one query: the weekly review is an expensive fold over the whole
/// op log, and running all four on every refresh would make the view cost four
/// folds to show one of them. The trends panel piggybacks on the weekly fold
/// when it is already loaded, since `WeeklyReview` carries the same `Trends`
/// the standalone query returns.
async fn refresh_review(core: &Core, state: &mut ViewState) {
    /// Weeks of history the trend panel shows (the spec's "last 12 weeks").
    const TREND_WEEKS: u32 = 12;
    /// How far back the daily glance looks: yesterday evening to now.
    const GLANCE_MS: u64 = 18 * 60 * 60 * 1000;
    /// Saved reviews listed in History.
    const HISTORY_LIMIT: u32 = 50;

    let now_ms = core.now_ms();
    match state.review.pane {
        sunrise_tui::ReviewPane::Weekly => {
            let q = Query::WeeklyReview {
                week_start_ms: state.review.week_start_ms,
                now_ms,
            };
            if let Ok(QueryResult::WeeklyReview(w)) = core.query(q).await {
                state.review.weekly = Some(w);
            }
        }
        sunrise_tui::ReviewPane::Daily => {
            let q = Query::DailyReview {
                since_ms: now_ms.saturating_sub(GLANCE_MS),
                now_ms,
            };
            if let Ok(QueryResult::DailyReview(d)) = core.query(q).await {
                state.review.daily = Some(d);
            }
        }
        sunrise_tui::ReviewPane::Trends => {
            let q = Query::StreamTrends {
                weeks: TREND_WEEKS,
                now_ms,
            };
            if let Ok(QueryResult::Trends(t)) = core.query(q).await {
                state.review.trends = Some(t);
            }
        }
        sunrise_tui::ReviewPane::History => {
            let q = Query::ReviewHistory {
                limit: HISTORY_LIMIT,
            };
            if let Ok(QueryResult::ReviewSnapshots(rows)) = core.query(q).await {
                state.review.history = rows;
            }
        }
    }
}

async fn load_tasks(core: &Core, state: &mut ViewState, q: Query) {
    if let Ok(QueryResult::Tasks(tasks) | QueryResult::StreamTasks(tasks)) = core.query(q).await {
        state.tasks = tasks;
        state.apply_context_filter();
        state.after_tasks_loaded();
    }
}
