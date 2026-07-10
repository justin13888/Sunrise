//! Pure render functions for each view. Each takes a Ratatui `Frame` and
//! the data it needs, and writes widgets. No I/O.

use crate::keymap::Mode;
use crate::view::{StreamPane, View, ViewState};
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
        View::Stream => render_stream(f, chunks[1], state),
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
    match state.mode {
        // Command-line entry mirrors vim: the buffer shows after a `:`.
        Mode::Command => {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!(":{}", state.input),
                Style::default().fg(Color::White),
            ));
        }
        Mode::Insert => {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("> {}", state.input),
                Style::default().fg(Color::White),
            ));
        }
        Mode::Normal => {}
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

/// Render the Stream view: split pane — stream list (with open-task-count
/// badges) on the left, tasks of the selected stream on the right. The
/// focused pane gets a highlighted border.
pub fn render_stream(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(area);
    render_stream_list(f, chunks[0], state);
    let title = state
        .selected_stream_row()
        .map_or_else(|| "Tasks".to_string(), |r| r.name.clone());
    render_task_list_styled(
        f,
        chunks[1],
        &state.tasks,
        state.selected,
        &title,
        pane_border(state, StreamPane::Tasks),
    );
}

/// Left pane of the Stream view: one row per stream, `Name [open_count]`.
fn render_stream_list(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let items: Vec<ListItem<'_>> = state
        .streams
        .iter()
        .map(|s| ListItem::new(format!("{} [{}]", s.name, s.open_task_count)))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Streams")
                .border_style(pane_border(state, StreamPane::Streams)),
        )
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Black)
                .bg(Color::White),
        )
        .highlight_symbol("▶ ");
    let mut s = ListState::default();
    s.select(state.selected_stream);
    f.render_stateful_widget(list, area, &mut s);
}

/// Border style for a Stream-view pane: highlighted when it has keyboard
/// focus, matching the tab bar's active-item color.
fn pane_border(state: &ViewState, pane: StreamPane) -> Style {
    if state.pane == pane {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
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

/// Render the Focus view: the focused task fullscreen, with full detail.
pub fn render_focus(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let block = Block::default().borders(Borders::ALL).title("Focus");
    let Some(t) = state.focused_task.as_ref() else {
        f.render_widget(Paragraph::new("no task selected").block(block), area);
        return;
    };

    let dash = || "—".to_string();
    let mut lines = vec![
        Line::from(vec![Span::styled(
            format!("[{}] {}", task_state_short(t.state), t.title),
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from(format!("state:     {:?}", t.state)),
        Line::from(format!(
            "priority:  {}",
            t.priority.map_or_else(dash, |p| p.to_string())
        )),
        Line::from(format!(
            "energy:    {}",
            t.energy.map_or_else(dash, energy_label)
        )),
        Line::from(format!(
            "scheduled: {}",
            t.scheduled_at.map_or_else(dash, |ts| ts.to_string())
        )),
        Line::from(format!(
            "due:       {}",
            t.due_at.map_or_else(dash, |ts| ts.to_string())
        )),
    ];
    if let Some(name) = state
        .streams
        .iter()
        .find(|s| s.id == t.stream_id)
        .map(|s| s.name.clone())
    {
        lines.push(Line::from(format!("stream:    {name}")));
    }
    lines.push(Line::from(format!("deferred:  {}", t.deferred_count)));
    if !t.scheduling_constraints.is_empty() {
        lines.push(Line::from(constraint_summary(&t.scheduling_constraints)));
    }
    if let Some(body) = t.body.as_ref().filter(|b| !b.is_empty()) {
        lines.push(Line::from(""));
        for row in String::from_utf8_lossy(&body.0).lines() {
            lines.push(Line::from(row.to_string()));
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Human label for an energy level.
fn energy_label(e: sunrise_domain::Energy) -> String {
    match e {
        sunrise_domain::Energy::Low => "low".into(),
        sunrise_domain::Energy::Med => "med".into(),
        sunrise_domain::Energy::High => "high".into(),
    }
}

/// One-line scheduling-constraints summary, e.g. `2 constraints (1 hard)`.
fn constraint_summary(list: &[sunrise_domain::ScheduleConstraint]) -> String {
    let hard = list
        .iter()
        .filter(|c| c.severity == sunrise_domain::ConstraintSeverity::Hard)
        .count();
    let noun = if list.len() == 1 {
        "constraint"
    } else {
        "constraints"
    };
    format!("{} {noun} ({hard} hard)", list.len())
}

fn render_task_list(
    f: &mut Frame<'_>,
    area: Rect,
    tasks: &[Task],
    selected: Option<usize>,
    title: &str,
) {
    render_task_list_styled(f, area, tasks, selected, title, Style::default());
}

/// [`render_task_list`] with an explicit border style (used by the Stream
/// view to highlight the focused pane).
fn render_task_list_styled(
    f: &mut Frame<'_>,
    area: Rect,
    tasks: &[Task],
    selected: Option<usize>,
    title: &str,
    border_style: Style,
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
                .title(title.to_string())
                .border_style(border_style),
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
    use crate::view::fixtures;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use sunrise_domain::{
        ConstraintSeverity, DateRange, Energy, NoteBody, ScheduleConstraint, WeekdaySet,
    };

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

    fn frame_to_string(width: u16, height: u16, state: &ViewState) -> String {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render(f, area, state);
        })
        .unwrap();
        buffer_text(term.backend().buffer())
    }

    #[test]
    fn snapshot_today_empty() {
        let state = ViewState::default();
        insta::assert_snapshot!(frame_to_string(50, 10, &state));
    }

    #[test]
    fn snapshot_command_line_active() {
        let mut state = ViewState::default();
        state.mode = Mode::Command;
        state.input = "vi".into();
        insta::assert_snapshot!(frame_to_string(50, 10, &state));
    }

    #[test]
    fn snapshot_help_message() {
        let mut state = ViewState::default();
        let _ = crate::apply_command(crate::parse_command(":help"), &mut state);
        insta::assert_snapshot!(frame_to_string(80, 10, &state));
    }

    #[test]
    fn snapshot_stream_view_empty() {
        let mut state = ViewState::default();
        state.view = View::Stream;
        insta::assert_snapshot!(frame_to_string(60, 12, &state));
    }

    #[test]
    fn snapshot_stream_view_populated_second_stream_selected() {
        let mut state = ViewState::default();
        state.view = View::Stream;
        state.streams = vec![fixtures::inbox_row(1), fixtures::stream_row(7, "Work", 3)];
        state.selected_stream = Some(1);
        state.pane = StreamPane::Tasks;
        state.tasks = vec![fixtures::fake_task(1), fixtures::fake_task(2)];
        state.after_tasks_loaded();
        insta::assert_snapshot!(frame_to_string(60, 12, &state));
    }

    #[test]
    fn snapshot_focus_with_body_and_constraints() {
        let mut state = ViewState::default();
        state.view = View::Focus;
        state.streams = vec![fixtures::inbox_row(1), fixtures::stream_row(7, "Work", 3)];
        let mut t = fixtures::fake_task(9);
        t.title = "Write quarterly report".into();
        t.stream_id = state.streams[1].id;
        t.priority = Some(2);
        t.energy = Some(Energy::High);
        t.due_at = Some(jiff::Timestamp::UNIX_EPOCH);
        t.deferred_count = 1;
        t.scheduling_constraints = vec![
            ScheduleConstraint {
                time_of_day: None,
                days_of_week: WeekdaySet::default(),
                date_range: Some(DateRange {
                    start: jiff::civil::date(2026, 1, 1),
                    end: None,
                }),
                severity: ConstraintSeverity::Hard,
            },
            ScheduleConstraint {
                time_of_day: None,
                days_of_week: WeekdaySet::default(),
                date_range: Some(DateRange {
                    start: jiff::civil::date(2026, 2, 1),
                    end: None,
                }),
                severity: ConstraintSeverity::Soft,
            },
        ];
        t.body = Some(NoteBody(
            b"Draft the numbers section first.\nThen review with the team.".to_vec(),
        ));
        state.focused_task = Some(t);
        insta::assert_snapshot!(frame_to_string(60, 18, &state));
    }

    #[test]
    fn snapshot_focus_empty() {
        let mut state = ViewState::default();
        state.view = View::Focus;
        insta::assert_snapshot!(frame_to_string(60, 10, &state));
    }

    #[test]
    fn snapshot_search_results() {
        let mut state = ViewState::default();
        state.view = View::Search;
        state.input = "task".into();
        state.tasks = vec![fixtures::fake_task(1), fixtures::fake_task(2)];
        state.after_tasks_loaded();
        insta::assert_snapshot!(frame_to_string(60, 14, &state));
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
