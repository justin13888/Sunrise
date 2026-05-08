//! `sunrise-tui` binary entrypoint.
//!
//! v1 scope: stand up the terminal, render the empty Today view, exit on
//! `q`. Real Core integration (open vault, query Today, draw real tasks)
//! lands once the engine pipeline in `sunrise-core::submit/query` returns
//! Tasks (Phase 17).

#![allow(clippy::print_stderr)]

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::time::Duration;
use sunrise_tui::render_today;

type Tty = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut term = setup()?;
    let result = run(&mut term);
    teardown(&mut term)?;
    result
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

fn run(term: &mut Tty) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        term.draw(|f| {
            let area = f.area();
            render_today(f, area, &[]);
        })?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(k) = event::read()? {
                if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
                    break;
                }
            }
        }
    }
    Ok(())
}
