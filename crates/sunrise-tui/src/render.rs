//! Pure render functions for each view. Each takes a Ratatui `Frame` and
//! the data it needs, and writes widgets. No I/O.

use crate::keymap::{Keymap, Mode};
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
    // The capture preview claims one row directly above the status line, and
    // only while the capture prompt is open — every other frame keeps the
    // original three-row layout.
    let preview_rows = u16::from(state.capture_preview.is_some());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),            // tab bar
            Constraint::Min(1),               // body
            Constraint::Length(preview_rows), // capture preview
            Constraint::Length(1),            // status line
        ])
        .split(area);
    render_tab_bar(f, chunks[0], state);
    if state.triage {
        render_triage(f, chunks[1], state);
    } else {
        match state.view {
            View::Today => render_today(f, chunks[1], state),
            View::Inbox => render_inbox(f, chunks[1], state),
            View::Stream => render_stream(f, chunks[1], state),
            View::Search => render_search(f, chunks[1], state),
            View::Focus => {
                #[cfg(feature = "images")]
                render_focus(f, chunks[1], state, preview);
                #[cfg(not(feature = "images"))]
                render_focus(f, chunks[1], state);
            }
            View::Routines => {
                render_routines(f, chunks[1], &state.routines, state.selected_routine)
            }
        }
    }
    if let Some(text) = state.capture_preview.as_ref() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" ⟶ {text}"),
                Style::default().fg(Color::Cyan),
            ))),
            chunks[2],
        );
    }
    render_status(f, chunks[3], state);
    // Overlays paint last so they sit above the view. At most one is up: the
    // picker owns Mode::Picker, the help overlay is a Normal-mode toggle.
    if let Some(picker) = state.picker.as_ref() {
        render_stream_picker(f, area, picker);
    }
    if let Some(devices) = state.devices.as_ref() {
        render_devices(f, area, devices);
    }
    if state.show_help {
        render_help(f, area, &state.keymap, state.mode);
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
        Mode::Visual => Style::default().fg(Color::Magenta),
        Mode::Triage => Style::default().fg(Color::Yellow),
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
        // Visual mode says how much is selected, so a bulk operator is never a
        // surprise about *how many*.
        Mode::Visual => {
            if let Some((lo, hi)) = state.visual_range() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{} selected", hi - lo + 1),
                    Style::default().fg(Color::Magenta),
                ));
            }
        }
        Mode::Normal | Mode::Confirm | Mode::Picker | Mode::Triage => {}
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
pub fn render_today(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let tasks = &state.tasks;
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

    render_task_list_styled(
        f,
        chunks[1],
        tasks,
        state.selected,
        "Tasks",
        Style::default(),
        state.visual_range(),
    );
}

/// Render the Inbox view.
pub fn render_inbox(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    render_task_list_styled(
        f,
        area,
        &state.tasks,
        state.selected,
        "Inbox",
        Style::default(),
        state.visual_range(),
    )
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
        state.visual_range(),
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
    render_task_list_styled(
        f,
        chunks[1],
        &state.tasks,
        state.selected,
        "Results",
        Style::default(),
        state.visual_range(),
    );
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

/// The `:devices` overlay: the vault's paired devices (`Query::DeviceList`).
///
/// Read-only by design — the core has no pair/revoke command yet, so listing is
/// the whole of what can honestly be offered.
fn render_devices(f: &mut Frame<'_>, area: Rect, devices: &[sunrise_core::queries::DeviceRow]) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if devices.is_empty() {
        lines.push(Line::from(Span::styled(
            "no paired devices",
            Style::default().fg(Color::Gray),
        )));
    }
    for d in devices {
        let id = short_device_id(&d.device_id);
        let mark = if d.revoked { " [revoked]" } else { "" };
        lines.push(Line::from(format!(
            "{id}  {:<18} {}{mark}",
            truncate(&d.nickname, 18),
            d.platform
        )));
    }
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let rect = centered(area, 60, height.max(3));
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Devices — any key to close")
                .border_style(Style::default().fg(Color::Blue)),
        ),
        rect,
    );
}

/// First 8 hex digits of a device id — enough to tell two devices apart in a
/// list without eating the row.
fn short_device_id(id: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    id[..4].iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Triage mode: one Inbox task at a time, with the decision keys spelled out.
///
/// `docs/08-features/inbox-and-capture.md` asks for "one-task-at-a-time
/// presentation, one keypress per outcome"; showing the legend on the card is
/// what makes the second half discoverable.
fn render_triage(f: &mut Frame<'_>, area: Rect, state: &ViewState) {
    let done = state.selected.unwrap_or(0);
    let total = state.tasks.len();
    let title = format!("Triage — {} of {total}", (done + 1).min(total.max(1)));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow));
    let Some(t) = state.selected_task() else {
        f.render_widget(
            Paragraph::new("inbox is empty — nothing to triage")
                .style(Style::default().fg(Color::Gray))
                .block(block),
            area,
        );
        return;
    };
    let dash = || "—".to_string();
    let mut lines = vec![
        Line::from(Span::styled(
            t.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "priority:  {}",
            t.priority.map_or_else(dash, |p| p.to_string())
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
    if let Some(body) = t.body.as_ref().filter(|b| !b.is_empty()) {
        lines.push(Line::from(""));
        for row in String::from_utf8_lossy(&body.0).lines().take(4) {
            lines.push(Line::from(row.to_string()));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "k keep · p promote · s schedule · d defer · x done · D delete · Esc leave",
        Style::default().fg(Color::Cyan),
    )));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// The `?` overlay. Content is generated from the keymap's binding table
/// (see [`crate::keymap::BINDINGS`]), so it cannot drift from the keymap.
///
/// **Contextual**, per `docs/08-features/keyboard.md` ("`?` in any view opens a
/// contextual cheat sheet"): the always-available Normal-mode keys, plus the
/// section for the mode the user is actually in. Showing all seven sections at
/// once stopped fitting an 80x24 terminal when visual and triage modes landed,
/// and silently clipping half the keys is worse than showing the half that
/// applies right now.
///
/// Falls back to two columns when the single-column form would not fit; the
/// split is by line rather than by section, because the Normal-mode section
/// alone is taller than a minimum-size terminal.
fn render_help(f: &mut Frame<'_>, area: Rect, keymap: &Keymap, mode: Mode) {
    let lines = help_lines(keymap, mode);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Keys — ? or Esc to close")
        .border_style(Style::default().fg(Color::Yellow));

    if lines.len() + 2 <= area.height as usize {
        let rect = centered(area, 72, u16::try_from(lines.len() + 2).unwrap_or(u16::MAX));
        f.render_widget(Clear, rect);
        f.render_widget(Paragraph::new(lines).block(block), rect);
        return;
    }

    let cut = lines.len().div_ceil(2);
    let (left, right) = lines.split_at(cut);
    let rows = left.len().max(right.len());
    let rect = centered(area, 78, u16::try_from(rows + 2).unwrap_or(u16::MAX));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);
    f.render_widget(Paragraph::new(left.to_vec()), cols[0]);
    f.render_widget(Paragraph::new(right.to_vec()), cols[1]);
}

/// Which keymap sections the overlay shows in `mode`: Normal always (those keys
/// are where the user returns to), plus the current mode's own section.
#[must_use]
pub fn help_modes(mode: Mode) -> Vec<&'static str> {
    if mode == Mode::Normal {
        vec![Mode::Normal.label()]
    } else {
        vec![Mode::Normal.label(), mode.label()]
    }
}

/// Overlay lines: the selected sections, one blank line between them.
fn help_lines(keymap: &Keymap, mode: Mode) -> Vec<Line<'static>> {
    let wanted = help_modes(mode);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (label, rows) in keymap.help_sections() {
        if !wanted.contains(&label) {
            continue;
        }
        if !out.is_empty() {
            out.push(Line::from(""));
        }
        out.push(Line::from(Span::styled(
            label.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        out.extend(rows.into_iter().map(|(keys, desc)| {
            Line::from(vec![
                Span::styled(format!("  {keys:<11}"), Style::default().fg(Color::Cyan)),
                Span::raw(desc.to_string()),
            ])
        }));
    }
    out
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

/// Render a task list, with an explicit border style (the Stream view uses it
/// to highlight the focused pane) and an optional visual-mode selection range.
///
/// While a visual run is active every row grows a two-column gutter, marked on
/// the selected rows: the cursor highlight alone cannot show a *range*, and a
/// bulk operator must never be ambiguous about what it is about to hit. The
/// gutter only exists while visual mode is up, so ordinary frames are byte-for-
/// byte what they were.
fn render_task_list_styled(
    f: &mut Frame<'_>,
    area: Rect,
    tasks: &[Task],
    selected: Option<usize>,
    title: &str,
    border_style: Style,
    visual: Option<(usize, usize)>,
) {
    let items: Vec<ListItem<'_>> = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let label = format!("[{}] {}", task_state_short(t.state), t.title);
            match visual {
                Some((lo, hi)) if (lo..=hi).contains(&i) => ListItem::new(format!("● {label}"))
                    .style(
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ),
                Some(_) => ListItem::new(format!("  {label}")),
                None => ListItem::new(label),
            }
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
        let state = ViewState::default();
        term.draw(|f| {
            let area = f.area();
            render_today(f, area, &state);
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
        // The overlay is contextual: Normal-mode keys always, plus the section
        // for the mode the user is in. Every row of those sections must survive
        // both the roomy single-column layout and the two-column fallback at
        // the 80x24 minimum. This is the assertion that keeps the overlay
        // honest when a binding is renamed or added.
        for mode in [Mode::Normal, Mode::Visual, Mode::Triage] {
            let mut state = ViewState::default();
            state.show_help = true;
            state.mode = mode;
            let wanted = crate::render::help_modes(mode);
            for (w, h) in [(100, 40), (MIN_WIDTH, MIN_HEIGHT)] {
                let s = guarded_frame(w, h, &state);
                for (section, rows) in crate::help_sections() {
                    if !wanted.contains(&section) {
                        continue;
                    }
                    assert!(
                        s.contains(section),
                        "{w}x{h} missing section {section}:\n{s}"
                    );
                    for (keys, desc) in rows {
                        assert!(s.contains(&keys), "{w}x{h} missing keys {keys:?}:\n{s}");
                        assert!(s.contains(desc), "{w}x{h} missing text {desc:?}:\n{s}");
                    }
                }
            }
        }
    }

    #[test]
    fn every_documented_binding_is_reachable_from_some_help_context() {
        // Contextual filtering must not orphan a section: every mode that has
        // help rows has to be a mode the overlay can be opened in.
        for (section, _) in crate::help_sections() {
            let mode = [
                Mode::Normal,
                Mode::Insert,
                Mode::Command,
                Mode::Confirm,
                Mode::Picker,
                Mode::Visual,
                Mode::Triage,
            ]
            .into_iter()
            .find(|m| m.label() == section)
            .expect("a section label is a mode label");
            assert!(
                crate::render::help_modes(mode).contains(&section),
                "{section} keys are documented but never shown"
            );
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
            vec![id],
            "Pay invoice".into(),
            vec![fixtures::inbox_row(1), fixtures::stream_row(7, "Work", 3)],
        );
        let s = guarded_frame(100, 24, &state);
        assert!(s.contains("move: Pay invoice"), "got:\n{s}");
        assert!(s.contains("Work [3]"), "got:\n{s}");
        assert!(s.contains("PICK"), "expected the PICK mode label:\n{s}");
    }

    #[test]
    fn the_capture_preview_renders_under_the_input_line() {
        let mut state = ViewState::default();
        state.view = View::Inbox;
        state.mode = Mode::Insert;
        state.input = "Buy milk #travel !2".into();
        state.capture_preview = Some("title \"Buy milk\" · #Travel · !2 · ~1h".into());
        let s = guarded_frame(100, 24, &state);
        // The structured reading is on screen, on its own line, above the
        // status line that still shows the raw text being typed.
        assert!(s.contains("title \"Buy milk\""), "got:\n{s}");
        assert!(s.contains("#Travel"), "got:\n{s}");
        assert!(s.contains("~1h"), "got:\n{s}");
        assert!(s.contains("> Buy milk #travel !2"), "got:\n{s}");
    }

    #[test]
    fn no_preview_row_is_reserved_when_not_capturing() {
        // The preview must not cost a row (or shift the layout) in the frames
        // where it has nothing to say.
        let mut state = ViewState::default();
        state.view = View::Inbox;
        state.tasks = vec![fixtures::fake_task(1)];
        state.after_tasks_loaded();
        let quiet = guarded_frame(100, 24, &state);
        state.capture_preview = None;
        assert_eq!(quiet, guarded_frame(100, 24, &state));
        assert!(!quiet.contains("⟶"), "got:\n{quiet}");
    }

    #[test]
    fn visually_selected_rows_are_marked() {
        let mut state = ViewState::default();
        state.view = View::Inbox;
        state.tasks = (0u8..3).map(fixtures::fake_task).collect();
        state.after_tasks_loaded();
        state.selected = Some(0);
        assert!(state.enter_visual());
        state.selected = Some(1);

        let s = guarded_frame(100, 24, &state);
        // Rows 0 and 1 carry the selection marker; row 2 does not.
        assert!(s.contains("● [ ] task 0"), "got:\n{s}");
        assert!(s.contains("● [ ] task 1"), "got:\n{s}");
        assert!(s.contains("  [ ] task 2"), "got:\n{s}");
        assert!(!s.contains("● [ ] task 2"), "row 2 is not selected:\n{s}");
        // And the status line says how many, so the operator is unambiguous.
        assert!(s.contains("VISUAL"), "got:\n{s}");
        assert!(s.contains("2 selected"), "got:\n{s}");
    }

    #[test]
    fn the_triage_card_shows_one_task_and_its_decision_keys() {
        let mut state = ViewState::default();
        state.view = View::Inbox;
        let mut t = fixtures::fake_task(1);
        t.title = "Pay invoice".into();
        t.priority = Some(2);
        state.tasks = vec![t, fixtures::fake_task(2)];
        state.after_tasks_loaded();
        state.enter_triage();

        let s = guarded_frame(100, 24, &state);
        assert!(s.contains("Triage — 1 of 2"), "got:\n{s}");
        assert!(s.contains("Pay invoice"), "got:\n{s}");
        assert!(s.contains("priority:  2"), "got:\n{s}");
        // One keypress per outcome, spelled out on the card.
        for key in ["k keep", "p promote", "s schedule", "d defer", "D delete"] {
            assert!(s.contains(key), "missing {key:?}:\n{s}");
        }
        // The other Inbox rows are not shown: triage is one task at a time.
        assert!(!s.contains("task 2"), "got:\n{s}");
        assert!(s.contains("TRIAGE"), "got:\n{s}");
    }

    #[test]
    fn the_devices_overlay_lists_paired_devices() {
        use sunrise_core::queries::DeviceRow;
        let mut state = ViewState::default();
        state.show_devices(vec![
            DeviceRow {
                device_id: [0xab; 16],
                nickname: "laptop".into(),
                platform: "linux".into(),
                revoked: false,
            },
            DeviceRow {
                device_id: [0xcd; 16],
                nickname: "old phone".into(),
                platform: "android".into(),
                revoked: true,
            },
        ]);
        let s = guarded_frame(100, 24, &state);
        assert!(s.contains("Devices"), "got:\n{s}");
        assert!(s.contains("laptop"), "got:\n{s}");
        assert!(s.contains("linux"), "got:\n{s}");
        assert!(s.contains("abababab"), "got:\n{s}");
        assert!(s.contains("[revoked]"), "got:\n{s}");
    }

    #[test]
    fn an_empty_device_list_says_so() {
        let mut state = ViewState::default();
        state.show_devices(Vec::new());
        let s = guarded_frame(100, 24, &state);
        assert!(s.contains("no paired devices"), "got:\n{s}");
    }

    #[test]
    fn the_help_overlay_advertises_a_remapped_key() {
        let mut state = ViewState::default();
        state.show_help = true;
        let (map, _) = crate::keymap::Keymap::from_config(&[("capture".into(), "n".into())]);
        state.keymap = map;
        let s = guarded_frame(100, 40, &state);
        // The row reads under the key the user actually has to press.
        let row = s
            .lines()
            .find(|l| l.contains("capture a task"))
            .expect("the capture help row");
        assert!(row.contains('n'), "help row was {row:?}");
        assert!(!row.trim_start().starts_with('c'), "help row was {row:?}");
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
