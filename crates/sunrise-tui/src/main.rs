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
    clippy::module_name_repetitions
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
use sunrise_tui::{dispatch, render, Action, Mode, View, ViewState};

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
    refresh(core, &mut state).await;

    loop {
        term.draw(|f| {
            let area = f.area();
            render(f, area, &state);
        })?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        let Some(action) = dispatch(k.code, state.mode, state.vim_mode) else {
            continue;
        };
        match action {
            Action::Quit => break,
            Action::SwitchView(v) => {
                state.view = v;
                refresh(core, &mut state).await;
            }
            Action::Next => state.select_next(),
            Action::Prev => state.select_prev(),
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
            Action::EnterInsert => {
                state.mode = Mode::Insert;
                state.input.clear();
            }
            Action::Escape => {
                state.mode = Mode::Normal;
                state.input.clear();
                state.status.clear();
            }
            Action::InsertChar(c) => {
                if state.input.len() < 512 {
                    state.input.push(c);
                }
            }
            Action::Backspace => {
                state.input.pop();
            }
            Action::Submit => {
                if state.view == View::Search {
                    // Search is client-side over current task list; in v1 just
                    // re-fetch the current view's data and filter by substring.
                    refresh(core, &mut state).await;
                    let needle = state.input.to_lowercase();
                    state.tasks = state
                        .tasks
                        .iter()
                        .filter(|t| t.title.to_lowercase().contains(&needle))
                        .cloned()
                        .collect();
                    state.after_tasks_loaded();
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
                }
                state.mode = Mode::Normal;
                state.input.clear();
                state.status.clear();
            }
        }
    }
    Ok(())
}

async fn refresh(core: &Core, state: &mut ViewState) {
    let q = match state.view {
        View::Today => Query::Today {
            now_ms: now_ms(),
            contexts: vec![],
        },
        View::Inbox => Query::Inbox,
        View::Stream => Query::Inbox, // v1: no stream picker yet — use Inbox.
        View::Search => Query::Inbox,
        View::Focus => Query::Inbox,
    };
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
