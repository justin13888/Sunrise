//! `sunrise-tui` binary entrypoint.
//!
//! v1 scope: stand up the terminal, open a Core against a vault directory
//! (env: `SUNRISE_VAULT`, default `~/.sunrise/vault`), and render the
//! current view. Quits on `q` / Esc / `:q`.

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

use crossterm::event::{self, Event};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::sync::Arc;
use std::time::Duration;
use sunrise_core::{Command, Core, CoreConfig, Query, QueryResult, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::TaskDraft;
use sunrise_tui::{
    apply_command, dispatch, parse_command, render, Action, AppEffect, Mode, StreamPane, View,
    ViewState,
};

type Tty = Terminal<CrosstermBackend<Stdout>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vault_dir = vault_dir();
    std::fs::create_dir_all(&vault_dir).ok();
    let cfg = CoreConfig::production(vault_dir.clone(), env!("CARGO_PKG_VERSION"));
    let core = Core::open(cfg, default_unlock()).await?;

    let mut term = setup()?;
    let result = run(&mut term, &core).await;
    teardown(&mut term)?;
    core.close().await.ok();
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

/// v1 self-host single-user dev mode: derive the vault root from a fixed
/// constant. Production reads a passphrase or completes a pairing flow.
fn default_unlock() -> Unlock {
    Unlock::DevicePaired(VaultRootKey::from_bytes([7u8; 32]))
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

async fn run(term: &mut Tty, core: &Core) -> Result<(), Box<dyn std::error::Error>> {
    let _ = Arc::new(SystemRng); // make explicit that we have an RNG injected via core cfg
    let mut state = ViewState::default();
    // Image-preview state (`:preview <path>`). Owned here because Picker and
    // the protocol state are not Clone; render fns borrow them per frame.
    // init_picker queries the terminal, so this runs after entering the
    // alternate screen but before the event loop reads input.
    #[cfg(feature = "images")]
    let mut picker = sunrise_tui::images::init_picker();
    #[cfg(feature = "images")]
    let mut preview: Option<sunrise_tui::images::Preview> = None;
    refresh(core, &mut state).await;

    loop {
        term.draw(|f| {
            let area = f.area();
            #[cfg(feature = "images")]
            render(f, area, &state, preview.as_mut());
            #[cfg(not(feature = "images"))]
            render(f, area, &state);
        })?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        let Some(action) = dispatch(k.code, state.mode, state.vim_mode, state.view) else {
            continue;
        };
        match action {
            Action::Quit => break,
            Action::SwitchView(View::Focus) => {
                // Focus shows the task selected in the previous view.
                state.open_focus();
                refresh(core, &mut state).await;
            }
            Action::SwitchView(v) => {
                state.view = v;
                refresh(core, &mut state).await;
            }
            Action::Next => state.nav_next(),
            Action::Prev => state.nav_prev(),
            Action::TogglePane => state.toggle_pane(),
            Action::PaneLeft => state.focus_pane(StreamPane::Streams),
            Action::PaneRight => state.focus_pane(StreamPane::Tasks),
            Action::Activate => {
                if state.view == View::Stream && state.pane == StreamPane::Streams {
                    // Confirm the highlighted stream: load its tasks and move
                    // focus to the task pane.
                    state.pane = StreamPane::Tasks;
                    refresh(core, &mut state).await;
                } else if state.selected_task().is_some() {
                    state.open_focus();
                    refresh(core, &mut state).await;
                }
            }
            Action::Toggle => {
                if let Some(t) = state.selected_task() {
                    let id = t.id;
                    let _ = core.submit(Command::CompleteTask(id)).await;
                    refresh(core, &mut state).await;
                }
            }
            Action::Capture => {
                state.mode = Mode::Insert;
                state.input.clear();
                state.status = "capture: type title, Enter to save, Esc to cancel".into();
            }
            Action::BeginSearch => {
                state.view = View::Search;
                state.mode = Mode::Insert;
                state.input.clear();
                state.status = "search: type query, Enter to commit, Esc to cancel".into();
            }
            Action::BeginCommand => {
                state.mode = Mode::Command;
                state.input.clear();
                state.status.clear();
            }
            Action::EnterInsert => {
                state.mode = Mode::Insert;
                state.input.clear();
            }
            Action::Escape => {
                if state.mode == Mode::Normal && state.view == View::Focus {
                    // Close the Focus view back to where it was opened from.
                    state.close_focus();
                    #[cfg(feature = "images")]
                    {
                        preview = None;
                    }
                    refresh(core, &mut state).await;
                } else {
                    state.mode = Mode::Normal;
                    state.input.clear();
                    state.status.clear();
                }
            }
            Action::InsertChar(c) => {
                if state.input.len() < 512 {
                    state.input.push(c);
                }
            }
            Action::Backspace => {
                state.input.pop();
            }
            Action::Submit if state.mode == Mode::Command => {
                // Command-line submit: parse `:…` and apply. `apply_command`
                // writes any view switch / status message; we handle the effect.
                let cmd = parse_command(&state.input);
                state.mode = Mode::Normal;
                state.input.clear();
                match apply_command(cmd, &mut state) {
                    Some(AppEffect::Quit) => break,
                    Some(AppEffect::Preview(path)) => {
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
                    None => refresh(core, &mut state).await,
                }
            }
            Action::Submit => {
                if state.view == View::Search {
                    // Full-text search via the core's FTS index. The query
                    // text stays in `input` so it remains visible above the
                    // results; Esc clears it.
                    state.mode = Mode::Normal;
                    state.status.clear();
                    refresh(core, &mut state).await;
                } else {
                    // Capture: create a task with the input as its title.
                    let title = state.input.trim().to_string();
                    if !title.is_empty() {
                        let _ = core
                            .submit(Command::CreateTask(TaskDraft {
                                title,
                                ..Default::default()
                            }))
                            .await;
                        refresh(core, &mut state).await;
                    }
                    state.mode = Mode::Normal;
                    state.input.clear();
                    state.status.clear();
                }
            }
        }
    }
    Ok(())
}

async fn refresh(core: &Core, state: &mut ViewState) {
    match state.view {
        View::Today => {
            let q = Query::Today {
                now_ms: now_ms(),
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

fn now_ms() -> u64 {
    #[allow(clippy::disallowed_methods)]
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}
