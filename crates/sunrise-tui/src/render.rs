//! Pure render functions for each view. Each takes a Ratatui `Frame` and
//! the data it needs, and writes widgets. No I/O.

use crate::keymap::Mode;
use crate::view::{View, ViewState};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sunrise_domain::Task;

/// Top-level dispatch: pick the renderer that matches the view.
pub fn render(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // tab bar
            Constraint::Min(1),    // body
            Constraint::Length(1), // status line
        ])
        .split(area);
    render_tab_bar(f, chunks[0], state);
    match state.view {
        View::Today => render_today(f, chunks[1], &state.tasks, state.selected),
        View::Inbox => render_inbox(f, chunks[1], &state.tasks, state.selected),
        View::Stream => render_stream(f, chunks[1], &state.tasks, state.selected),
        View::Search => render_search(f, chunks[1], state),
        View::Focus => render_focus(f, chunks[1], state),
    }
    render_status(f, chunks[2], state);
}

fn render_tab_bar(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let make = |label: &str, view: View, key: char| {
        let style = if view == state.view {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        Span::styled(format!(" {key}:{label} "), style)
    };
    let line = Line::from(vec![
        make("Today", View::Today, '1'),
        make("Inbox", View::Inbox, '2'),
        make("Stream", View::Stream, '3'),
        make("Search", View::Search, '4'),
        make("Focus", View::Focus, '5'),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_status(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let mode_label = match state.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Command => "CMD",
    };
    let style = match state.mode {
        Mode::Normal => Style::default().fg(Color::Cyan),
        Mode::Insert => Style::default().fg(Color::Green),
        Mode::Command => Style::default().fg(Color::Magenta),
    };
    let mut spans = vec![
        Span::styled(
            format!(" {mode_label} "),
            style.add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::raw(state.status.clone()),
    ];
    if state.mode != Mode::Normal {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("> {}", state.input),
            Style::default().fg(Color::White),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Render the Today view: header + task list.
pub fn render_today(f: &mut Frame<'_>, area: Rect, tasks: &[Task], selected: Option<usize>) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);

    let header_text = Line::from(vec![
        Span::styled(
            "Sunrise — Today",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("    ("),
        Span::raw(format!("{}", tasks.len())),
        Span::raw(" tasks)"),
    ]);
    let header = Paragraph::new(header_text).block(Block::default().borders(Borders::BOTTOM));
    f.render_widget(header, chunks[0]);

    render_task_list(f, chunks[1], tasks, selected, "Tasks");
}

/// Render the Inbox view.
pub fn render_inbox(f: &mut Frame<'_>, area: Rect, tasks: &[Task], selected: Option<usize>) {
    render_task_list(f, area, tasks, selected, "Inbox")
}

/// Render the Stream view.
pub fn render_stream(f: &mut Frame<'_>, area: Rect, tasks: &[Task], selected: Option<usize>) {
    render_task_list(f, area, tasks, selected, "Stream")
}

/// Render the Search view: query line + results.
pub fn render_search(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let query = Paragraph::new(format!("/ {}", state.input))
        .block(Block::default().borders(Borders::ALL).title("Search"))
        .wrap(Wrap { trim: false });
    f.render_widget(query, chunks[0]);
    render_task_list(f, chunks[1], &state.tasks, state.selected, "Results");
}

/// Render the Focus view (selected task fullscreen).
pub fn render_focus(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let block = Block::default().borders(Borders::ALL).title("Focus");
    if let Some(t) = state.selected_task() {
        let lines = vec![
            Line::from(vec![Span::styled(
                t.title.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::raw("state: "),
                Span::raw(format!("{:?}", t.state)),
            ]),
            Line::from(vec![
                Span::raw("priority: "),
                Span::raw(
                    t.priority
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "—".into()),
                ),
            ]),
            Line::from(vec![
                Span::raw("due: "),
                Span::raw(
                    t.due_at
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_else(|| "—".into()),
                ),
            ]),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    } else {
        f.render_widget(Paragraph::new("no task selected").block(block), area);
    }
}

fn render_task_list(
    f: &mut Frame<'_>,
    area: Rect,
    tasks: &[Task],
    selected: Option<usize>,
    title: &str,
) {
    let items: Vec<ListItem<'_>> = tasks
        .iter()
        .map(|t| {
            let label = format!("[{}] {}", task_state_short(t.state), t.title);
            ListItem::new(label)
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_string()),
        )
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Black)
                .bg(Color::White),
        )
        .highlight_symbol("▶ ");
    let mut s = ListState::default();
    s.select(selected);
    f.render_stateful_widget(list, area, &mut s);
}

const fn task_state_short(s: sunrise_domain::TaskState) -> &'static str {
    match s {
        sunrise_domain::TaskState::Todo => " ",
        sunrise_domain::TaskState::InProgress => "·",
        sunrise_domain::TaskState::Done => "x",
        sunrise_domain::TaskState::Cancelled => "/",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn render_today_with_empty_list() {
        let backend = TestBackend::new(40, 6);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_today(f, area, &[], None);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let s = buffer_text(buf);
        assert!(s.contains("Sunrise — Today"));
        assert!(s.contains("(0 tasks)"));
        assert!(s.contains("Tasks"));
    }

    #[test]
    fn render_dispatches_by_view() {
        let backend = TestBackend::new(60, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = ViewState::default();
        state.view = View::Inbox;
        term.draw(|f| {
            let area = f.area();
            render(f, area, &state);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let s = buffer_text(buf);
        assert!(s.contains("Inbox"));
        // Tab bar is rendered.
        assert!(s.contains("Today"));
    }

    #[test]
    fn search_view_renders_query_box() {
        let backend = TestBackend::new(60, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = ViewState::default();
        state.view = View::Search;
        state.input = "test".into();
        term.draw(|f| {
            let area = f.area();
            render(f, area, &state);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let s = buffer_text(buf);
        assert!(s.contains("Search"));
        assert!(s.contains("/ test"));
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let c = buf.cell((x, y)).map_or(" ", ratatui::buffer::Cell::symbol);
                out.push_str(c);
            }
            out.push('\n');
        }
        out
    }
}
