//! Pure render functions for each view. Each takes a Ratatui `Frame` and
//! the data it needs, and writes widgets. No I/O.

use crate::keymap::{help_sections, Mode};
use crate::view::{RoutineRow, StreamPane, StreamPicker, SyncIndicator, View, ViewState};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use sunrise_domain::Task;
use sunrise_sync::SyncState;

/// Minimum terminal width required to render the UI (`docs/07-clients/tui.md`).
pub const MIN_WIDTH: u16 = 80;
/// Minimum terminal height required to render the UI.
pub const MIN_HEIGHT: u16 = 24;

/// Whether `area` is large enough for the full UI.
#[must_use]
pub const fn fits(area: Rect) -> bool {
    area.width >= MIN_WIDTH && area.height >= MIN_HEIGHT
}

/// Top-level dispatch: pick the renderer that matches the view.
///
/// With the `images` feature, `preview` is the runtime-owned image state
/// for the Focus view's preview pane (`None` when nothing is loaded).
pub fn render(
    f: &mut Frame<'_>,
    area: Rect,
    state: &ViewState,
    #[cfg(feature = "images")] preview: Option<&mut crate::images::Preview>,
) {
    // Minimum-size guard. Below 80x24 the split panes degenerate into
    // unreadable slivers, so render one centred message instead. Purely a
    // function of `area`, so growing the terminal back restores the UI on the
    // next frame with no extra state.
    if !fits(area) {
        render_too_small(f, area);
        return;
    }
    #[cfg(feature = "images")]
    render_chrome(f, area, state, preview);
    #[cfg(not(feature = "images"))]
    render_chrome(f, area, state);
}

/// The full UI (tab bar + body + status line + overlays), assuming `area`
/// already passed the [`fits`] check.
fn render_chrome(
    f: &mut Frame<'_>,
    area: Rect,
    state: &ViewState,
    #[cfg(feature = "images")] preview: Option<&mut crate::images::Preview>,
) {
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
        View::Focus => {
            #[cfg(feature = "images")]
            render_focus(f, chunks[1], state, preview);
            #[cfg(not(feature = "images"))]
            render_focus(f, chunks[1], state);
        }
        View::Routines => render_routines(f, chunks[1], &state.routines, state.selected_routine),
    }
    render_status(f, chunks[2], state);
    // Overlays paint last so they sit above the view. At most one is up: the
    // picker owns Mode::Picker, the help overlay is a Normal-mode toggle.
    if let Some(picker) = state.picker.as_ref() {
        render_stream_picker(f, area, picker);
    }
    if state.show_help {
        render_help(f, area);
    }
}

/// The below-minimum-size screen. One centred line, no layout to break.
fn render_too_small(f: &mut Frame<'_>, area: Rect) {
    let msg = format!("Sunrise needs at least {MIN_WIDTH} × {MIN_HEIGHT}");
    let lines = vec![
        Line::from(Span::styled(
            msg,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("this terminal is {} × {}", area.width, area.height)),
    ];
    let y = area.y + area.height.saturating_sub(2) / 2;
    let target = Rect {
        x: area.x,
        y,
        width: area.width,
        height: area.height.min(2),
    };
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true }),
        target,
    );
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
        make("Routines", View::Routines, '6'),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_status(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let mode_label = state.mode.label();
    let style = match state.mode {
        Mode::Normal => Style::default().fg(Color::Cyan),
        Mode::Insert => Style::default().fg(Color::Green),
        Mode::Command => Style::default().fg(Color::Magenta),
        Mode::Confirm => Style::default().fg(Color::Red),
        Mode::Picker => Style::default().fg(Color::Blue),
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
        Mode::Normal | Mode::Confirm | Mode::Picker => {}
    }
    // With a sync indicator, split off a right-aligned column for it so the
    // left status text is never clobbered; otherwise render across the whole
    // line as before (keeps the sync-off snapshots byte-identical).
    if let Some(sync) = state.sync {
        let text = sync.text();
        let width = u16::try_from(text.chars().count())
            .unwrap_or(u16::MAX)
            .saturating_add(1);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(width)])
            .split(area);
        f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, sync_style(sync))))
                .alignment(Alignment::Right),
            cols[1],
        );
    } else {
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

/// Color for the sync indicator by driver state.
fn sync_style(sync: SyncIndicator) -> Style {
    let color = match sync.state {
        None => Color::DarkGray,
        Some(SyncState::Live) => Color::Green,
        Some(SyncState::CatchingUp) => Color::Yellow,
        Some(SyncState::Disconnected) => Color::Red,
    };
    Style::default().fg(color)
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

/// Render the Focus view: task detail on the left, attachment/image preview
/// pane on the right.
///
/// With the `images` feature, `preview` holds the image loaded via
/// `:preview <path>`; `None` (or a build without the feature) renders the
/// placeholder pane instead.
pub fn render_focus(
    f: &mut Frame<'_>,
    area: Rect,
    state: &ViewState,
    #[cfg(feature = "images")] preview: Option<&mut crate::images::Preview>,
) {
    let block = Block::default().borders(Borders::ALL).title("Focus");
    let Some(t) = state.focused_task.as_ref() else {
        f.render_widget(Paragraph::new("no task selected").block(block), area);
        return;
    };
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let (area, preview_area) = (chunks[0], chunks[1]);
    #[cfg(feature = "images")]
    render_preview_pane(f, preview_area, preview);
    #[cfg(not(feature = "images"))]
    render_preview_placeholder(f, preview_area);

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

/// Right pane of the Focus view: the loaded image, or the placeholder.
#[cfg(feature = "images")]
fn render_preview_pane(
    f: &mut Frame<'_>,
    area: Rect,
    preview: Option<&mut crate::images::Preview>,
) {
    let Some(p) = preview else {
        render_preview_placeholder(f, area);
        return;
    };
    let block = Block::default().borders(Borders::ALL).title("Preview");
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_stateful_widget(ratatui_image::StatefulImage::new(None), inner, p);
}

/// Placeholder for the Focus preview pane when no image is loaded (or the
/// `images` feature is compiled out).
// TODO(core): needs Command::AttachFile + Query::TaskAttachments before task
// attachments can be listed/previewed here; until then `:preview <path>`
// side-loads an arbitrary image file.
fn render_preview_placeholder(f: &mut Frame<'_>, area: Rect) {
    let msg = "attachment preview — no attachments API in core v1";
    f.render_widget(
        Paragraph::new(msg)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL).title("Preview")),
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

/// Render the Routines view: one row per live routine, showing its RRULE
/// summary and next occurrence.
pub fn render_routines(
    f: &mut Frame<'_>,
    area: Rect,
    routines: &[RoutineRow],
    selected: Option<usize>,
) {
    if routines.is_empty() {
        f.render_widget(
            Paragraph::new("no routines — create them from the desktop client (core v1 has no TUI routine CRUD)")
                .style(Style::default().fg(Color::Gray))
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title("Routines")),
            area,
        );
        return;
    }
    let items: Vec<ListItem<'_>> = routines
        .iter()
        .map(|r| ListItem::new(routine_line(r)))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Routines"))
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Black)
                .bg(Color::White),
        )
        .highlight_symbol("▶ ");
    let mut st = ListState::default();
    st.select(selected);
    f.render_stateful_widget(list, area, &mut st);
}

/// One Routines-view row: `title — <rrule summary> · next <ts>`.
fn routine_line(r: &RoutineRow) -> String {
    let next = r
        .next
        .map_or_else(|| "none in horizon".to_string(), |t| t.to_string());
    let paused = if r.paused { " [paused]" } else { "" };
    format!("{}{paused} — {} · next {next}", r.title, r.rrule)
}

/// Modal move-to-stream picker (`m`), drawn over the current view.
fn render_stream_picker(f: &mut Frame<'_>, area: Rect, picker: &StreamPicker) {
    let height = u16::try_from(picker.rows.len().min(12) + 2).unwrap_or(u16::MAX);
    let rect = centered(area, 52, height.max(3));
    let items: Vec<ListItem<'_>> = picker
        .rows
        .iter()
        .map(|r| ListItem::new(format!("{} [{}]", r.name, r.open_task_count)))
        .collect();
    let title = format!("move: {}", truncate(&picker.task_title, 40));
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Black)
                .bg(Color::White),
        )
        .highlight_symbol("▶ ");
    let mut st = ListState::default();
    st.select(Some(picker.selected));
    f.render_widget(Clear, rect);
    f.render_stateful_widget(list, rect, &mut st);
}

/// The `?` overlay. Content is generated from the keymap's binding table
/// (see [`crate::keymap::BINDINGS`]), so it cannot drift from the keymap.
///
/// Falls back to two columns when the single-column form would not fit — at
/// the 80x24 minimum the full binding list is taller than the screen, and
/// silently clipping half the keys is worse than a denser layout.
fn render_help(f: &mut Frame<'_>, area: Rect) {
    let blocks = help_blocks();
    let total: usize = blocks.iter().map(Vec::len).sum::<usize>() + blocks.len().saturating_sub(1);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Keys — ? or Esc to close")
        .border_style(Style::default().fg(Color::Yellow));

    if total + 2 <= area.height as usize {
        let rect = centered(area, 72, u16::try_from(total + 2).unwrap_or(u16::MAX));
        f.render_widget(Clear, rect);
        f.render_widget(Paragraph::new(join_blocks(&blocks)).block(block), rect);
        return;
    }

    let (left, right) = split_blocks(&blocks, total.div_ceil(2));
    let (left, right) = (join_blocks(&left), join_blocks(&right));
    let rows = left.len().max(right.len());
    let rect = centered(area, 78, u16::try_from(rows + 2).unwrap_or(u16::MAX));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    f.render_widget(Paragraph::new(left), cols[0]);
    f.render_widget(Paragraph::new(right), cols[1]);
}

/// One renderable block of help lines per keymap section (header + rows).
fn help_blocks() -> Vec<Vec<Line<'static>>> {
    help_sections()
        .into_iter()
        .map(|(mode, rows)| {
            let mut lines = vec![Line::from(Span::styled(
                mode.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))];
            lines.extend(rows.into_iter().map(|(keys, desc)| {
                Line::from(vec![
                    Span::styled(format!("  {keys:<11}"), Style::default().fg(Color::Cyan)),
                    Span::raw(desc.to_string()),
                ])
            }));
            lines
        })
        .collect()
}

/// Flatten blocks into one line list, one blank line between sections.
fn join_blocks(blocks: &[Vec<Line<'static>>]) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for b in blocks {
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        out.extend(b.iter().cloned());
    }
    out
}

/// Split whole sections across two columns, aiming for `target` lines in the
/// first. Sections are never broken mid-way, and the first column always gets
/// at least one so the split terminates.
fn split_blocks(
    blocks: &[Vec<Line<'static>>],
    target: usize,
) -> (Vec<Vec<Line<'static>>>, Vec<Vec<Line<'static>>>) {
    let mut used = 0usize;
    let mut cut = 0usize;
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 && used + b.len() > target {
            break;
        }
        used += b.len() + usize::from(i > 0);
        cut = i + 1;
    }
    let (l, r) = blocks.split_at(cut.min(blocks.len()));
    (l.to_vec(), r.to_vec())
}

/// A centred sub-rect at most `w` x `h`, clamped to `area`.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let width = w.min(area.width);
    let height = h.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Truncate `s` to `max` characters, appending an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
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

    /// Feature-agnostic wrapper for the chrome renderer (no preview).
    ///
    /// Deliberately calls [`render_chrome`] rather than [`render`]: the view
    /// snapshots below use small backends to keep their diffs readable, and
    /// the 80x24 guard on `render` would replace all of them with the
    /// "too small" screen. The guard has its own contract tests.
    fn draw_frame(f: &mut Frame<'_>, state: &ViewState) {
        let area = f.area();
        #[cfg(feature = "images")]
        render_chrome(f, area, state, None);
        #[cfg(not(feature = "images"))]
        render_chrome(f, area, state);
    }

    #[test]
    fn render_dispatches_by_view() {
        let backend = TestBackend::new(60, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut state = ViewState::default();
        state.view = View::Inbox;
        term.draw(|f| draw_frame(f, &state)).unwrap();
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
        term.draw(|f| draw_frame(f, &state)).unwrap();
        let buf = term.backend().buffer();
        let s = buffer_text(buf);
        assert!(s.contains("Search"));
        assert!(s.contains("/ test"));
    }

    fn frame_to_string(width: u16, height: u16, state: &ViewState) -> String {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw_frame(f, state)).unwrap();
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
    fn snapshot_focus_preview_placeholder() {
        // No image loaded: the right pane shows the attachments placeholder
        // (identical with or without the `images` feature).
        let mut state = ViewState::default();
        state.view = View::Focus;
        state.focused_task = Some(fixtures::fake_task(3));
        insta::assert_snapshot!(frame_to_string(70, 10, &state));
    }

    #[cfg(feature = "images")]
    #[test]
    fn snapshot_focus_preview_halfblocks() {
        use ratatui_image::picker::Picker;

        // Generate a tiny 4x4 checkerboard PNG at test time — no binary
        // fixture is committed.
        let png_path = std::env::temp_dir().join(format!(
            "sunrise-tui-preview-fixture-{}.png",
            std::process::id()
        ));
        let img = image::RgbImage::from_fn(4, 4, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        });
        img.save(&png_path).unwrap();

        // Manual Picker (no terminal query): defaults to halfblocks, which
        // renders deterministic `▀` cells.
        let mut picker = Picker::new((8, 16));
        let mut preview = crate::images::load_preview(&mut picker, &png_path).unwrap();
        std::fs::remove_file(&png_path).ok();

        let mut state = ViewState::default();
        state.view = View::Focus;
        state.focused_task = Some(fixtures::fake_task(3));
        let backend = TestBackend::new(70, 10);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_chrome(f, area, &state, Some(&mut preview));
        })
        .unwrap();
        insta::assert_snapshot!(buffer_text(term.backend().buffer()));
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

    #[test]
    fn status_line_shows_live_sync_indicator() {
        let mut state = ViewState::default();
        state.sync = Some(SyncIndicator::live(SyncState::CatchingUp, 2));
        let s = frame_to_string(70, 6, &state);
        assert!(
            s.contains("sync: catching-up (2 pending)"),
            "expected sync indicator, got:\n{s}"
        );
    }

    #[test]
    fn status_line_shows_off_when_sync_disabled() {
        let mut state = ViewState::default();
        state.sync = Some(SyncIndicator::off(0));
        let s = frame_to_string(60, 6, &state);
        assert!(
            s.contains("sync: off (0 pending)"),
            "expected off indicator, got:\n{s}"
        );
    }

    /// Draw through the **public** [`render`] entry point (size guard
    /// included) and return the buffer text.
    fn guarded_frame(width: u16, height: u16, state: &ViewState) -> String {
        let backend = TestBackend::new(width, height);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            #[cfg(feature = "images")]
            render(f, area, state, None);
            #[cfg(not(feature = "images"))]
            render(f, area, state);
        })
        .unwrap();
        buffer_text(term.backend().buffer())
    }

    #[test]
    fn below_minimum_size_renders_only_the_guard_message() {
        let state = ViewState::default();
        for (w, h) in [(79, 24), (80, 23), (40, 10)] {
            let s = guarded_frame(w, h, &state);
            assert!(
                s.contains(&format!("at least {MIN_WIDTH} × {MIN_HEIGHT}")),
                "{w}x{h} should show the guard, got:\n{s}"
            );
            // The broken layout must not be drawn underneath it.
            assert!(!s.contains("1:Today"), "{w}x{h} still drew the tab bar");
        }
    }

    #[test]
    fn at_minimum_size_the_ui_comes_back() {
        // Recovery after a resize is stateless: the same state at >= 80x24
        // renders the real UI again.
        let state = ViewState::default();
        let s = guarded_frame(MIN_WIDTH, MIN_HEIGHT, &state);
        assert!(s.contains("1:Today"), "expected the tab bar, got:\n{s}");
        assert!(!s.contains("at least"), "guard still showing:\n{s}");
    }

    #[test]
    fn routines_view_lists_rrule_summary_and_next_occurrence() {
        let mut state = ViewState::default();
        state.view = View::Routines;
        state.routines = vec![RoutineRow {
            id: fixtures::fake_task(1).id,
            title: "Water plants".into(),
            rrule: "every 2 weeks on Mo".into(),
            next: Some("2026-03-02T09:00:00Z".parse().unwrap()),
            paused: false,
        }];
        state.after_routines_loaded();
        let s = guarded_frame(100, 24, &state);
        assert!(s.contains("Water plants"), "got:\n{s}");
        assert!(s.contains("every 2 weeks on Mo"), "got:\n{s}");
        assert!(s.contains("2026-03-02"), "got:\n{s}");
        // And the view is reachable from the tab bar.
        assert!(s.contains("6:Routines"), "got:\n{s}");
    }

    #[test]
    fn routines_view_without_routines_explains_itself() {
        let mut state = ViewState::default();
        state.view = View::Routines;
        let s = guarded_frame(100, 24, &state);
        assert!(s.contains("no routines"), "got:\n{s}");
    }

    #[test]
    fn help_overlay_is_rendered_from_the_keymap_table() {
        let mut state = ViewState::default();
        state.show_help = true;
        // Roomy (single column) and at the 80x24 minimum (two columns): every
        // documented binding must survive both layouts. This is the assertion
        // that keeps the overlay honest when a binding is renamed or added.
        for (w, h) in [(100, 40), (MIN_WIDTH, MIN_HEIGHT)] {
            let s = guarded_frame(w, h, &state);
            for (mode, rows) in help_sections() {
                assert!(s.contains(mode), "{w}x{h} missing section {mode}:\n{s}");
                for (keys, desc) in rows {
                    assert!(s.contains(keys), "{w}x{h} missing keys {keys:?}:\n{s}");
                    assert!(s.contains(desc), "{w}x{h} missing text {desc:?}:\n{s}");
                }
            }
        }
    }

    #[test]
    fn stream_picker_overlay_shows_the_candidates() {
        let mut state = ViewState::default();
        state.view = View::Inbox;
        state.tasks = vec![fixtures::fake_task(1)];
        state.after_tasks_loaded();
        let id = state.tasks[0].id;
        state.open_stream_picker(
            id,
            "Pay invoice".into(),
            vec![fixtures::inbox_row(1), fixtures::stream_row(7, "Work", 3)],
        );
        let s = guarded_frame(100, 24, &state);
        assert!(s.contains("move: Pay invoice"), "got:\n{s}");
        assert!(s.contains("Work [3]"), "got:\n{s}");
        assert!(s.contains("PICK"), "expected the PICK mode label:\n{s}");
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
