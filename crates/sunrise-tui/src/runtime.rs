//! Action reducer: the single place a keypress becomes a core [`Command`].
//!
//! [`apply_action`] is pure over ([`ViewState`], `now_ms`) — it mutates view
//! state and *returns* the I/O the runtime must perform, rather than doing it.
//! That keeps every keybinding unit-testable without a terminal or a `Core`:
//! the tests press a key through [`crate::dispatch`], feed the resulting
//! [`Action`] here, and assert on the [`Outcome`].

use crate::command::parse_command;
use crate::keymap::{Action, Mode};
use crate::view::{Prompt, StreamPane, View, ViewState};
use crate::{apply_command, AppEffect};
use std::path::PathBuf;
use std::time::Duration;
use sunrise_core::{Command, DomainEvent};
use sunrise_domain::{StreamDraft, TaskDraft, TaskPatch};
use sunrise_id::EntityRef;

/// Work the runtime must do after [`apply_action`] has updated the view state.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing beyond the next repaint.
    None,
    /// Re-run the active view's query, then repaint.
    Refresh,
    /// Submit this command to the core, then refresh.
    Submit(Box<Command>),
    /// Run `Query::StreamList` and hand the rows to
    /// [`ViewState::open_stream_picker`].
    OpenStreamPicker {
        /// Task being moved.
        task: EntityRef,
        /// Its title, echoed in the picker header.
        title: String,
    },
    /// Load an image into the Focus preview pane (`:preview`).
    Preview(PathBuf),
    /// Exit the application.
    Quit,
}

/// Maximum characters accepted into the shared input line.
const MAX_INPUT: usize = 512;

/// Apply `action` to `state`, returning the [`Outcome`] the runtime owes.
///
/// `now_ms` is the wall clock supplied by the caller (the binary reads it
/// once per event); passing it in keeps deferral arithmetic deterministic in
/// tests and keeps this module free of clock access.
pub fn apply_action(action: Action, state: &mut ViewState, now_ms: u64) -> Outcome {
    // The `gg` chord latch survives exactly one keystroke.
    let g_armed = std::mem::take(&mut state.pending_g);
    // The help overlay is dismissed by the next key, whatever it is. Esc and
    // `q` dismiss it and stop there — closing an overlay must never be a way
    // to accidentally quit the app; every other key dismisses and then does
    // its normal job.
    if state.show_help && !matches!(action, Action::ToggleHelp) {
        state.show_help = false;
        if matches!(action, Action::Escape | Action::Quit) {
            return Outcome::None;
        }
    }

    match action {
        Action::Quit => Outcome::Quit,
        Action::SwitchView(View::Focus) => {
            state.open_focus();
            Outcome::Refresh
        }
        Action::SwitchView(v) => {
            state.view = v;
            Outcome::Refresh
        }
        Action::Next => {
            match state.picker.as_mut() {
                Some(p) => p.next(),
                None => state.nav_next(),
            }
            Outcome::None
        }
        Action::Prev => {
            match state.picker.as_mut() {
                Some(p) => p.prev(),
                None => state.nav_prev(),
            }
            Outcome::None
        }
        Action::GotoPrefix => {
            if g_armed {
                state.nav_first();
            } else {
                state.pending_g = true;
            }
            Outcome::None
        }
        Action::GotoTop => {
            state.nav_first();
            Outcome::None
        }
        Action::GotoBottom => {
            state.nav_last();
            Outcome::None
        }
        Action::TogglePane => {
            state.toggle_pane();
            Outcome::None
        }
        Action::PaneLeft => {
            state.focus_pane(StreamPane::Streams);
            Outcome::None
        }
        Action::PaneRight => {
            state.focus_pane(StreamPane::Tasks);
            Outcome::None
        }
        Action::Activate => {
            if state.view == View::Stream && state.pane == StreamPane::Streams {
                // Confirm the highlighted stream: load its tasks and move
                // focus to the task pane.
                state.pane = StreamPane::Tasks;
                Outcome::Refresh
            } else if state.selected_task().is_some() {
                state.open_focus();
                Outcome::Refresh
            } else {
                Outcome::None
            }
        }
        Action::Toggle => match state.selected_task().map(|t| t.id) {
            Some(id) => Outcome::Submit(Box::new(Command::CompleteTask(id))),
            None => Outcome::None,
        },
        Action::Capture => {
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::Capture);
            state.input.clear();
            state.status = "capture: type title, Enter to save, Esc to cancel".into();
            Outcome::None
        }
        Action::EditTitle => match state.selected_task().map(|t| (t.id, t.title.clone())) {
            Some((id, title)) => {
                state.mode = Mode::Insert;
                state.prompt = Some(Prompt::EditTitle(id));
                state.input = title;
                state.status = "edit title: Enter to save, Esc to cancel".into();
                Outcome::None
            }
            None => no_selection(state),
        },
        Action::Defer => match state.selected_task().map(|t| t.id) {
            Some(id) => {
                state.mode = Mode::Insert;
                state.prompt = Some(Prompt::Defer(id));
                state.input.clear();
                state.status = "defer by: 30m / 2h / 3d / 1w (blank = 1d), Esc to cancel".into();
                Outcome::None
            }
            None => no_selection(state),
        },
        Action::Delete => match state.selected_task().map(|t| (t.id, t.title.clone())) {
            Some((id, title)) => {
                // Destructive: park in Confirm mode. Nothing is submitted
                // until the user answers `y`.
                state.mode = Mode::Confirm;
                state.input.clear();
                state.status = format!("delete \"{title}\"? [y/N]");
                state.prompt = Some(Prompt::ConfirmDelete { id, title });
                Outcome::None
            }
            None => no_selection(state),
        },
        Action::MoveToStream => match state.selected_task().map(|t| (t.id, t.title.clone())) {
            Some((task, title)) => Outcome::OpenStreamPicker { task, title },
            None => no_selection(state),
        },
        Action::CreateStream => {
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::CreateStream);
            state.input.clear();
            state.status = "new stream: type name, Enter to create, Esc to cancel".into();
            Outcome::None
        }
        Action::ToggleHelp => {
            state.show_help = !state.show_help;
            Outcome::None
        }
        Action::BeginSearch => {
            state.view = View::Search;
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::Search);
            state.input.clear();
            state.status = "search: type query, Enter to commit, Esc to cancel".into();
            Outcome::None
        }
        Action::BeginCommand => {
            state.mode = Mode::Command;
            state.prompt = None;
            state.input.clear();
            state.status.clear();
            Outcome::None
        }
        Action::EnterInsert => {
            state.mode = Mode::Insert;
            // `i` in the Search view resumes the query; elsewhere it is capture.
            state.prompt = Some(if state.view == View::Search {
                Prompt::Search
            } else {
                Prompt::Capture
            });
            state.input.clear();
            Outcome::None
        }
        Action::Escape => {
            if state.mode == Mode::Normal && state.view == View::Focus {
                state.close_focus();
                Outcome::Refresh
            } else {
                state.reset_to_normal();
                Outcome::None
            }
        }
        Action::InsertChar(c) => {
            if state.input.len() < MAX_INPUT {
                state.input.push(c);
            }
            Outcome::None
        }
        Action::Backspace => {
            state.input.pop();
            Outcome::None
        }
        Action::Submit => submit(state, now_ms),
    }
}

/// Shared "nothing is selected" response for the task-scoped bindings.
fn no_selection(state: &mut ViewState) -> Outcome {
    state.status = "no task selected".into();
    Outcome::None
}

/// Handle Enter (or `y` in a confirmation) according to the active mode.
fn submit(state: &mut ViewState, now_ms: u64) -> Outcome {
    match state.mode {
        Mode::Command => submit_command_line(state),
        Mode::Confirm => submit_confirm(state),
        Mode::Picker => submit_picker(state),
        Mode::Insert => submit_prompt(state, now_ms),
        Mode::Normal => Outcome::None,
    }
}

/// `:`-line submit: parse and apply, mapping the pure [`AppEffect`] onto an
/// [`Outcome`].
fn submit_command_line(state: &mut ViewState) -> Outcome {
    let cmd = parse_command(&state.input);
    state.mode = Mode::Normal;
    state.input.clear();
    match apply_command(cmd, state) {
        Some(AppEffect::Quit) => Outcome::Quit,
        Some(AppEffect::Preview(path)) => Outcome::Preview(path),
        None => Outcome::Refresh,
    }
}

/// `y` on a confirmation prompt. The only confirmable action in v1 is delete.
fn submit_confirm(state: &mut ViewState) -> Outcome {
    let prompt = state.prompt.take();
    state.reset_to_normal();
    match prompt {
        Some(Prompt::ConfirmDelete { id, title }) => {
            state.status = format!("deleted \"{title}\"");
            Outcome::Submit(Box::new(Command::DeleteTask(id)))
        }
        _ => Outcome::None,
    }
}

/// Enter on the move-to-stream picker.
fn submit_picker(state: &mut ViewState) -> Outcome {
    let picker = state.picker.take();
    state.reset_to_normal();
    let Some(picker) = picker else {
        return Outcome::None;
    };
    let Some(row) = picker.selected_row() else {
        return Outcome::None;
    };
    let (stream, name) = (row.id, row.name.clone());
    state.status = format!("moved \"{}\" → {name}", picker.task_title);
    Outcome::Submit(Box::new(Command::PromoteToStream {
        id: picker.task,
        stream,
    }))
}

/// Enter on a text prompt (capture / edit / defer / new stream / search).
fn submit_prompt(state: &mut ViewState, now_ms: u64) -> Outcome {
    let text = state.input.trim().to_string();
    match state.prompt.take() {
        Some(Prompt::Capture) => {
            state.reset_to_normal();
            if text.is_empty() {
                return Outcome::None;
            }
            Outcome::Submit(Box::new(Command::CreateTask(TaskDraft {
                title: text,
                ..Default::default()
            })))
        }
        Some(Prompt::EditTitle(id)) => {
            if text.is_empty() {
                // An empty title would be rejected by domain validation;
                // keep the prompt open rather than round-tripping an error.
                state.prompt = Some(Prompt::EditTitle(id));
                state.status = "title cannot be empty".into();
                return Outcome::None;
            }
            state.reset_to_normal();
            Outcome::Submit(Box::new(Command::UpdateTask {
                id,
                patch: TaskPatch {
                    title: Some(text),
                    ..Default::default()
                },
            }))
        }
        Some(Prompt::Defer(id)) => match parse_defer_ms(&text) {
            Some(delta_ms) => {
                state.reset_to_normal();
                Outcome::Submit(Box::new(Command::DeferTask {
                    id,
                    to_ms: now_ms.saturating_add(delta_ms),
                }))
            }
            None => {
                state.prompt = Some(Prompt::Defer(id));
                state.status = format!("defer: cannot parse \"{text}\" — try 30m / 2h / 3d / 1w");
                Outcome::None
            }
        },
        Some(Prompt::CreateStream) => {
            if text.is_empty() {
                state.prompt = Some(Prompt::CreateStream);
                state.status = "stream name cannot be empty".into();
                return Outcome::None;
            }
            state.reset_to_normal();
            Outcome::Submit(Box::new(Command::CreateStream(StreamDraft {
                name: text,
                ..Default::default()
            })))
        }
        Some(Prompt::Search) => {
            // Keep `input` — it is both the live query and the text shown in
            // the search bar. Only the mode returns to Normal.
            state.mode = Mode::Normal;
            state.status.clear();
            Outcome::Refresh
        }
        // A confirmation prompt is never live in Insert mode (see `submit`),
        // and `None` means Enter with no prompt open.
        Some(Prompt::ConfirmDelete { .. }) | None => Outcome::None,
    }
}

/// Default coalescing window for inbound domain events. `docs/07-clients/tui.md`
/// specifies a 50 ms redraw debounce; the same figure reads well for data churn.
pub const CHANGE_DEBOUNCE: Duration = Duration::from_millis(50);

/// Coalesce a burst of domain events into a single repaint: wait one debounce
/// window, then drain whatever queued up during it. Returns the number of
/// additional events swallowed, so a caller can tell a lone op from a batch.
///
/// Called after `rx.recv()` has already yielded (or lagged), so a large
/// inbound sync batch costs one refresh instead of one per op.
pub async fn drain_changes(
    rx: &mut tokio::sync::broadcast::Receiver<DomainEvent>,
    debounce: Duration,
) -> usize {
    use tokio::sync::broadcast::error::TryRecvError;
    tokio::time::sleep(debounce).await;
    let mut drained = 0usize;
    // Keep draining through a lag: the queue is still non-empty. Stops on
    // Empty or Closed.
    while let Ok(_) | Err(TryRecvError::Lagged(_)) = rx.try_recv() {
        drained += 1;
    }
    drained
}

/// Parse a defer offset (`30m`, `2h`, `3d`, `1w`; a bare number means days;
/// blank means one day) into milliseconds.
///
/// Returns `None` for anything unparseable so the caller can keep the prompt
/// open instead of deferring by a surprise amount.
#[must_use]
pub fn parse_defer_ms(input: &str) -> Option<u64> {
    const MINUTE_MS: u64 = 60 * 1000;
    let text = input.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Some(24 * 60 * MINUTE_MS);
    }
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None => (text.as_str(), "d"),
    };
    let n: u64 = digits.parse().ok()?;
    let per_unit = match unit {
        "m" | "min" | "mins" => MINUTE_MS,
        "h" | "hr" | "hrs" => 60 * MINUTE_MS,
        "d" | "day" | "days" => 24 * 60 * MINUTE_MS,
        "w" | "wk" | "week" | "weeks" => 7 * 24 * 60 * MINUTE_MS,
        _ => return None,
    };
    n.checked_mul(per_unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::dispatch;
    use crate::view::fixtures::{fake_task, inbox_row, stream_row};
    use crossterm::event::KeyCode;

    /// Fixed "now" for deferral arithmetic: 2026-01-01T00:00:00Z.
    const NOW_MS: u64 = 1_767_225_600_000;
    const MIN_MS: u64 = 60 * 1000;

    /// Press a key: run it through the real keymap, then the reducer. Panics
    /// if the key has no binding, so a test that presses an unbound key fails
    /// loudly instead of silently asserting nothing.
    fn press(state: &mut ViewState, key: KeyCode) -> Outcome {
        let action = dispatch(key, state.mode, state.vim_mode, state.view)
            .unwrap_or_else(|| panic!("no binding for {key:?} in {:?}", state.mode));
        apply_action(action, state, NOW_MS)
    }

    /// Type a string into the active prompt.
    fn type_text(state: &mut ViewState, text: &str) {
        for c in text.chars() {
            let _ = press(state, KeyCode::Char(c));
        }
    }

    /// A state sitting in the Inbox with three tasks, the second selected.
    fn inbox_state() -> ViewState {
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        s.select_next();
        s
    }

    fn selected_id(s: &ViewState) -> EntityRef {
        s.selected_task().expect("a selection").id
    }

    #[test]
    fn capture_builds_create_task() {
        let mut s = inbox_state();
        assert!(matches!(press(&mut s, KeyCode::Char('c')), Outcome::None));
        assert_eq!(s.mode, Mode::Insert);
        type_text(&mut s, "buy milk");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateTask(draft) => assert_eq!(draft.title, "buy milk"),
                other => panic!("expected CreateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.input.is_empty());
    }

    #[test]
    fn toggle_builds_complete_task() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        match press(&mut s, KeyCode::Char('x')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CompleteTask(target) => assert_eq!(target, id),
                other => panic!("expected CompleteTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn edit_prefills_title_and_builds_update_task() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        let original = s.selected_task().expect("selection").title.clone();

        assert!(matches!(press(&mut s, KeyCode::Char('e')), Outcome::None));
        // The prompt starts from the existing title so `e` is an edit, not a
        // retype.
        assert_eq!(s.input, original);
        assert_eq!(s.prompt, Some(Prompt::EditTitle(id)));

        type_text(&mut s, " v2");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { id: target, patch } => {
                    assert_eq!(target, id);
                    assert_eq!(
                        patch.title.as_deref(),
                        Some(format!("{original} v2").as_str())
                    );
                    // Only the title moves.
                    assert!(patch.state.is_none() && patch.stream_id.is_none());
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn edit_rejects_an_empty_title_without_submitting() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('e'));
        for _ in 0..s.input.len() {
            let _ = press(&mut s, KeyCode::Backspace);
        }
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        // Prompt stays open so the user can fix it.
        assert!(matches!(s.prompt, Some(Prompt::EditTitle(_))));
        assert!(s.status.contains("empty"));
    }

    #[test]
    fn defer_builds_defer_task_at_the_parsed_offset() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        let _ = press(&mut s, KeyCode::Char('d'));
        type_text(&mut s, "2h");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::DeferTask { id: target, to_ms } => {
                    assert_eq!(target, id);
                    assert_eq!(to_ms, NOW_MS + 120 * MIN_MS);
                }
                other => panic!("expected DeferTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn defer_with_a_blank_prompt_means_one_day() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('d'));
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::DeferTask { to_ms, .. } => {
                    assert_eq!(to_ms, NOW_MS + 24 * 60 * MIN_MS);
                }
                other => panic!("expected DeferTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn defer_with_unparseable_input_submits_nothing() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('d'));
        type_text(&mut s, "soonish");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert!(matches!(s.prompt, Some(Prompt::Defer(_))));
        assert!(s.status.contains("defer"), "status was {:?}", s.status);
    }

    #[test]
    fn delete_is_gated_on_the_confirmation_prompt() {
        let mut s = inbox_state();
        let id = selected_id(&s);

        // `D` alone must not delete anything.
        assert!(matches!(press(&mut s, KeyCode::Char('D')), Outcome::None));
        assert_eq!(s.mode, Mode::Confirm);
        assert!(s.status.contains("[y/N]"), "status was {:?}", s.status);

        // Declining submits nothing and leaves the prompt behind.
        assert!(matches!(press(&mut s, KeyCode::Char('n')), Outcome::None));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.prompt, None);

        // Re-arm and confirm: only now is DeleteTask built.
        let _ = press(&mut s, KeyCode::Char('D'));
        match press(&mut s, KeyCode::Char('y')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::DeleteTask(target) => assert_eq!(target, id),
                other => panic!("expected DeleteTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn escape_cancels_a_pending_delete() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('D'));
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.prompt, None);
    }

    #[test]
    fn move_opens_a_picker_and_builds_promote_to_stream() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        let title = s.selected_task().expect("selection").title.clone();

        let rows = match press(&mut s, KeyCode::Char('m')) {
            Outcome::OpenStreamPicker { task, title: t } => {
                assert_eq!(task, id);
                assert_eq!(t, title);
                vec![inbox_row(3), stream_row(7, "Work", 2)]
            }
            other => panic!("expected OpenStreamPicker, got {other:?}"),
        };
        s.open_stream_picker(id, title, rows);
        assert_eq!(s.mode, Mode::Picker);
        // The cursor starts on the task's current stream (Inbox).
        assert_eq!(
            s.picker
                .as_ref()
                .and_then(|p| p.selected_row())
                .map(|r| r.name.clone()),
            Some("Inbox".to_string())
        );

        let _ = press(&mut s, KeyCode::Char('j'));
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::PromoteToStream { id: target, stream } => {
                    assert_eq!(target, id);
                    assert_eq!(stream, stream_row(7, "Work", 2).id);
                }
                other => panic!("expected PromoteToStream, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.mode, Mode::Normal);
        assert!(s.picker.is_none());
    }

    #[test]
    fn escaping_the_picker_moves_nothing() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        s.open_stream_picker(id, "t".into(), vec![inbox_row(0), stream_row(7, "Work", 0)]);
        let _ = press(&mut s, KeyCode::Char('j'));
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert!(s.picker.is_none());
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn create_stream_builds_create_stream() {
        let mut s = inbox_state();
        assert!(matches!(press(&mut s, KeyCode::Char('S')), Outcome::None));
        type_text(&mut s, "Work");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateStream(draft) => assert_eq!(draft.name, "Work"),
                other => panic!("expected CreateStream, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn create_stream_rejects_an_empty_name() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('S'));
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert_eq!(s.prompt, Some(Prompt::CreateStream));
    }

    #[test]
    fn task_bindings_are_inert_with_no_selection() {
        for key in ['e', 'd', 'D', 'm', 'x'] {
            let mut s = ViewState::default();
            s.view = View::Inbox;
            s.after_tasks_loaded();
            assert!(
                matches!(press(&mut s, KeyCode::Char(key)), Outcome::None),
                "`{key}` submitted a command with nothing selected"
            );
            assert_eq!(s.mode, Mode::Normal, "`{key}` opened a prompt regardless");
        }
    }

    #[test]
    fn gg_chord_needs_both_keystrokes() {
        let mut s = inbox_state();
        s.selected = Some(2);
        // A lone `g` only arms the latch.
        let _ = press(&mut s, KeyCode::Char('g'));
        assert_eq!(s.selected, Some(2));
        assert!(s.pending_g);
        let _ = press(&mut s, KeyCode::Char('g'));
        assert_eq!(s.selected, Some(0));
        assert!(!s.pending_g);
    }

    #[test]
    fn an_intervening_key_disarms_the_gg_latch() {
        let mut s = inbox_state();
        s.selected = Some(2);
        let _ = press(&mut s, KeyCode::Char('g'));
        let _ = press(&mut s, KeyCode::Char('k')); // moves to 1, clears latch
        let _ = press(&mut s, KeyCode::Char('g')); // arms again, does not jump
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn capital_g_jumps_to_the_last_row() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('G'));
        assert_eq!(s.selected, Some(2));
    }

    #[test]
    fn goto_is_list_aware_in_the_routines_and_stream_views() {
        let mut s = ViewState::default();
        s.view = View::Stream;
        s.pane = StreamPane::Streams;
        s.streams = vec![inbox_row(0), stream_row(1, "a", 0), stream_row(2, "b", 0)];
        s.after_streams_loaded();
        let _ = press(&mut s, KeyCode::Char('G'));
        assert_eq!(s.selected_stream, Some(2));

        let mut s = ViewState::default();
        s.view = View::Routines;
        s.routines = vec![
            crate::view::RoutineRow {
                id: fake_task(1).id,
                title: "a".into(),
                rrule: "every day".into(),
                next: None,
                paused: false,
            },
            crate::view::RoutineRow {
                id: fake_task(2).id,
                title: "b".into(),
                rrule: "every day".into(),
                next: None,
                paused: false,
            },
        ];
        s.after_routines_loaded();
        let _ = press(&mut s, KeyCode::Char('G'));
        assert_eq!(s.selected_routine, Some(1));
    }

    #[test]
    fn question_mark_toggles_the_help_overlay() {
        let mut s = inbox_state();
        assert!(!s.show_help);
        let _ = press(&mut s, KeyCode::Char('?'));
        assert!(s.show_help);
        let _ = press(&mut s, KeyCode::Char('?'));
        assert!(!s.show_help);
        // Esc also closes it, without quitting the app.
        let _ = press(&mut s, KeyCode::Char('?'));
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert!(!s.show_help);
    }

    #[test]
    fn switching_to_routines_refreshes() {
        let mut s = inbox_state();
        assert!(matches!(
            press(&mut s, KeyCode::Char('6')),
            Outcome::Refresh
        ));
        assert_eq!(s.view, View::Routines);
    }

    #[test]
    fn quit_still_quits() {
        let mut s = inbox_state();
        assert!(matches!(press(&mut s, KeyCode::Char('q')), Outcome::Quit));
    }

    #[test]
    fn command_line_submit_still_routes_through_apply_command() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char(':'));
        assert_eq!(s.mode, Mode::Command);
        type_text(&mut s, "q");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::Quit));
    }

    #[test]
    fn search_submit_keeps_the_query_and_refreshes() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('/'));
        type_text(&mut s, "milk");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::Refresh));
        assert_eq!(s.view, View::Search);
        // The query text stays visible above the results.
        assert_eq!(s.input, "milk");
        assert_eq!(s.mode, Mode::Normal);
    }

    #[tokio::test]
    async fn drain_changes_coalesces_a_burst_into_one_repaint() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<DomainEvent>(64);
        let id = fake_task(1).id;
        // A 40-op inbound batch: one recv() has already been consumed by the
        // event loop, the rest must be swallowed by the debounce window.
        for _ in 0..40 {
            tx.send(DomainEvent::Created(id)).expect("subscriber alive");
        }
        assert!(rx.recv().await.is_ok());
        assert_eq!(drain_changes(&mut rx, CHANGE_DEBOUNCE).await, 39);
        // Nothing left: the next event is genuinely new.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn drain_changes_survives_a_lagged_receiver() {
        // Overflow the buffer so the receiver lags; "something changed" still
        // holds, and draining must terminate rather than spin.
        let (tx, mut rx) = tokio::sync::broadcast::channel::<DomainEvent>(2);
        let id = fake_task(1).id;
        for _ in 0..10 {
            tx.send(DomainEvent::Created(id)).expect("subscriber alive");
        }
        let _ = drain_changes(&mut rx, CHANGE_DEBOUNCE).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn defer_parser_units() {
        assert_eq!(parse_defer_ms(""), Some(24 * 60 * MIN_MS));
        assert_eq!(parse_defer_ms("30m"), Some(30 * MIN_MS));
        assert_eq!(parse_defer_ms("2h"), Some(120 * MIN_MS));
        assert_eq!(parse_defer_ms("3d"), Some(3 * 24 * 60 * MIN_MS));
        assert_eq!(parse_defer_ms("1w"), Some(7 * 24 * 60 * MIN_MS));
        // A bare number is days, and case is irrelevant.
        assert_eq!(parse_defer_ms("2"), Some(2 * 24 * 60 * MIN_MS));
        assert_eq!(parse_defer_ms("  2D "), Some(2 * 24 * 60 * MIN_MS));
        // Garbage is rejected rather than guessed at.
        assert_eq!(parse_defer_ms("soon"), None);
        assert_eq!(parse_defer_ms("2y"), None);
        assert_eq!(parse_defer_ms("-1d"), None);
    }
}
