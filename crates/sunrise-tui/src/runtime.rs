//! Action reducer: the single place a keypress becomes a core [`Command`].
//!
//! [`apply_action`] is pure over ([`ViewState`], `now_ms`) — it mutates view
//! state and *returns* the I/O the runtime must perform, rather than doing it.
//! That keeps every keybinding unit-testable without a terminal or a `Core`:
//! the tests press a key through the state's [`crate::Keymap`], feed the
//! resulting [`Action`] here, and assert on the [`Outcome`] — including bulk
//! operations, which return an explicit command list rather than looping in the
//! binary.

use crate::capture::{now_ts, parse_line, preview_line, unresolved_note};
use crate::command::parse_command;
use crate::keymap::{Action, Mode};
use crate::view::{Prompt, StreamPane, View, ViewState};
use crate::{apply_command, AppEffect};
use std::path::PathBuf;
use std::time::Duration;
use sunrise_core::commands::FocusStartDraft;
use sunrise_core::{Command, DomainEvent};
use sunrise_domain::capture::parse_when;
use sunrise_domain::{FocusKind, InterruptionReason, StreamDraft, TaskPatch};
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
    /// Submit every command in order, then refresh once.
    ///
    /// Bulk operations live here rather than as a loop in the binary so that a
    /// visual-mode operator is one testable value: the tests assert the exact
    /// command list, not that some loop ran the right number of times.
    SubmitMany(Vec<Command>),
    /// Run `Query::StreamList` and hand the rows to
    /// [`ViewState::open_stream_picker`].
    OpenStreamPicker {
        /// Tasks being moved (more than one under visual mode).
        tasks: Vec<EntityRef>,
        /// Their title (or `"N tasks"`), echoed in the picker header.
        title: String,
    },
    /// Open the task's note body in `$EDITOR`, then apply the edited bytes via
    /// `TaskPatch.body`. The runtime owns the terminal suspend/restore.
    EditBody {
        /// Task whose body is being edited.
        id: EntityRef,
        /// Current body bytes, used to seed the temp file.
        body: Vec<u8>,
    },
    /// Resolve a task by id (`Query::EntityById`) and open it in Focus.
    OpenTask(EntityRef),
    /// Run `Query::DeviceList` and show the result overlay (`:devices`).
    ShowDevices,
    /// Load an image into the Focus preview pane (`:preview`).
    Preview(PathBuf),
    /// Parse this line with `Core::capture_aside` and commit the draft.
    ///
    /// Routed through the core rather than through the local
    /// [`crate::parse_line`] on purpose: `capture_aside` drops any resolved
    /// `#stream`, so a mid-session thought lands in the **Inbox** whatever
    /// stream the focused task belongs to
    /// (`docs/08-features/focus-mode.md` §Capture-aside).
    CaptureAside(String),
    /// Submit every command in order, then run `Query::UnblockCascade(task)`
    /// and report what the completion released.
    ///
    /// The cascade is the reason this is not a plain [`Outcome::SubmitMany`]:
    /// it must be read *after* the completion lands, against the blockers'
    /// new states.
    SubmitThenCascade {
        /// Commands to submit, in order.
        cmds: Vec<Command>,
        /// Task whose cascade to read once they have landed.
        task: EntityRef,
    },
    /// Run `Query::FocusStats` and show the folded result (`:focus stats`).
    ShowFocusStats,
    /// Exit the application.
    Quit,
}

impl Outcome {
    /// Wrap a command list in the narrowest outcome that fits: `None` for an
    /// empty list, [`Outcome::Submit`] for exactly one, [`Outcome::SubmitMany`]
    /// beyond that.
    ///
    /// Keeps "one keypress, one command" the literal shape of a single-row
    /// operation — a bulk operator is the only thing that produces a batch.
    #[must_use]
    fn submit_all(mut cmds: Vec<Command>) -> Self {
        match cmds.len() {
            0 => Self::None,
            1 => Self::Submit(Box::new(cmds.remove(0))),
            _ => Self::SubmitMany(cmds),
        }
    }
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
    // The `:devices` overlay behaves the same way: informational, dismissed by
    // whatever the user presses next.
    if state.devices.is_some() {
        state.devices = None;
        state.status.clear();
        if matches!(action, Action::Escape | Action::Quit) {
            return Outcome::None;
        }
    }
    // Ditto `:focus stats`. `EndFocus` is in the dismiss-and-stop set because
    // Esc *is* `EndFocus` while a session runs: closing an overlay must never
    // be a way to accidentally end the session behind it.
    if state.focus.stats.is_some() {
        state.focus.stats = None;
        state.status.clear();
        if matches!(action, Action::Escape | Action::Quit | Action::EndFocus) {
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
        // Enter on a planner row starts the session it proposes, so accepting
        // the top pick is one keypress (`docs/08-features/focus-mode.md`
        // §Focus Planner).
        Action::Activate if state.view == View::Focus && !state.focus.is_running() => {
            start_focus(state)
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
        // Completing the task a session is on is two commands and a cascade
        // read, not the plain bulk-complete path.
        Action::Toggle if state.focus.is_running() => complete_focused(state),
        Action::Toggle => {
            let ids = state.operand_ids();
            if ids.is_empty() {
                return Outcome::None;
            }
            let label = state.operand_label();
            let cmds: Vec<Command> = ids.into_iter().map(Command::CompleteTask).collect();
            finish_operator(state, "completed", &label);
            Outcome::submit_all(cmds)
        }
        Action::Capture => {
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::Capture);
            state.input.clear();
            state.status = "capture: #stream ^when !1-5 ~30m — Enter to save, Esc to cancel".into();
            refresh_capture_preview(state, now_ms);
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
        Action::EditBody => match state.selected_task().map(|t| (t.id, t.body.clone())) {
            Some((id, body)) => Outcome::EditBody {
                id,
                body: body.map(|b| b.0).unwrap_or_default(),
            },
            None => no_selection(state),
        },
        Action::Defer => {
            let ids = state.operand_ids();
            if ids.is_empty() {
                return no_selection(state);
            }
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::Defer(ids));
            state.input.clear();
            state.status = "defer by: 30m / 2h / 3d / 1w (blank = 1d), Esc to cancel".into();
            Outcome::None
        }
        Action::Schedule => {
            let ids = state.operand_ids();
            if ids.is_empty() {
                return no_selection(state);
            }
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::Schedule(ids));
            state.input.clear();
            state.status =
                "schedule at: today / tomorrow 9am / next friday / 2026-03-01 / +3d".into();
            Outcome::None
        }
        Action::Delete => {
            let ids = state.operand_ids();
            if ids.is_empty() {
                return no_selection(state);
            }
            // Destructive: park in Confirm mode. Nothing is submitted
            // until the user answers `y`.
            let title = state.operand_label();
            state.mode = Mode::Confirm;
            state.input.clear();
            state.status = format!("delete \"{title}\"? [y/N]");
            state.prompt = Some(Prompt::ConfirmDelete { ids, title });
            Outcome::None
        }
        Action::MoveToStream => {
            let tasks = state.operand_ids();
            if tasks.is_empty() {
                return no_selection(state);
            }
            Outcome::OpenStreamPicker {
                tasks,
                title: state.operand_label(),
            }
        }
        Action::VisualMode => {
            if state.enter_visual() {
                Outcome::None
            } else {
                no_selection(state)
            }
        }
        Action::Triage => {
            // Always refresh: triage operates on the Inbox, which may not be
            // the current view. `after_tasks_loaded` ends the pass immediately
            // if the Inbox turns out to be empty.
            state.enter_triage();
            Outcome::Refresh
        }
        Action::TriageKeep => {
            state.triage_advance(false);
            Outcome::None
        }
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
            if state.mode == Mode::Visual {
                state.exit_visual();
                Outcome::None
            } else if state.mode == Mode::Triage {
                state.exit_triage();
                state.status = "triage cancelled".into();
                Outcome::None
            } else if state.mode == Mode::Normal && state.view == View::Focus {
                state.close_focus();
                Outcome::Refresh
            } else {
                // A prompt opened from a triage pass returns to the card;
                // `reset_to_normal` keeps triage alive for exactly that reason.
                state.reset_to_normal();
                Outcome::None
            }
        }
        Action::InsertChar(c) => {
            if state.input.len() < MAX_INPUT {
                state.input.push(c);
            }
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        Action::Backspace => {
            state.input.pop();
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        Action::Submit => submit(state, now_ms),
        Action::StartFocus => start_focus(state),
        Action::EndFocus => end_focus(state),
        Action::TakeBreak => take_break(state),
        Action::CaptureAside => {
            if !state.focus.is_running() {
                state.status = "capture aside needs a running session".into();
                return Outcome::None;
            }
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::CaptureAside);
            state.input.clear();
            state.status = "aside → Inbox: Enter to save, Esc to cancel".into();
            Outcome::None
        }
        Action::Interrupt => {
            if !state.focus.is_running() {
                return Outcome::None;
            }
            state.mode = Mode::Interrupt;
            state.status = "interrupted by: s self · m meeting · b blocked · o other".into();
            Outcome::None
        }
        Action::InterruptReason(reason) => log_interruption(state, reason),
    }
}

/// Build the `StartFocus` command for whatever the user is pointing at.
///
/// The planner cursor and [`ViewState::focused_task`] are kept in step
/// (`sync_focus_pick`), so "the current pick" is one thing whether the user
/// arrived from a task list or from the ranked queue.
fn start_focus(state: &mut ViewState) -> Outcome {
    if state.focus.is_running() {
        state.status = "a focus session is already running".into();
        return Outcome::None;
    }
    let target = state
        .focused_task
        .as_ref()
        .filter(|_| state.view == View::Focus)
        .or_else(|| state.selected_task())
        .map(|t| (t.id, t.title.clone()));
    let Some((task_id, title)) = target else {
        return no_selection(state);
    };
    // A new session starts from a clean slate: the previous completion's
    // cascade is about work that is already done.
    state.focus.cascade = None;
    state.status = format!("focusing \"{title}\"");
    Outcome::Submit(Box::new(Command::StartFocus(FocusStartDraft {
        task_id,
        kind: FocusKind::Work,
        length: state.focus.length,
        energy: state.focus.energy,
    })))
}

/// Close the running session without completing the task.
///
/// `actual_focused_ms: None` is load-bearing — it tells the core to freeze the
/// elapsed span it derives from its own clock, which is why the TUI never has
/// to accumulate one.
fn end_focus(state: &mut ViewState) -> Outcome {
    let Some(session) = state.focus.running_session() else {
        return Outcome::None;
    };
    state.focus.running = None;
    state.mode = state.resting_mode();
    state.status = "session ended".into();
    Outcome::Submit(Box::new(Command::EndFocus {
        session,
        actual_focused_ms: None,
        completed_task: false,
    }))
}

/// Complete the focused task and close the session as a completion, then read
/// the unblock cascade.
fn complete_focused(state: &mut ViewState) -> Outcome {
    let (Some(task), Some(session)) = (state.focus.running_task(), state.focus.running_session())
    else {
        return Outcome::None;
    };
    let title = state.focused_title();
    state.focus.running = None;
    state.mode = state.resting_mode();
    state.status = format!("completed \"{title}\"");
    Outcome::SubmitThenCascade {
        // Order matters: the task must be `Done` before the cascade is read,
        // and the session records the completion rather than causing it — the
        // session log is never a second writer of task state.
        cmds: vec![
            Command::CompleteTask(task),
            Command::EndFocus {
                session,
                actual_focused_ms: None,
                completed_task: true,
            },
        ],
        task,
    }
}

/// End the running work segment and open the break the pomodoro cycle owes.
///
/// The 25/5-with-a-long-break-every-fourth arithmetic is not repeated here:
/// [`sunrise_domain::break_after`] (through [`crate::FocusState::next_segment`])
/// names the segment for the status line, and the core sizes the break from
/// the same rule when it sees `FocusKind::Break`.
fn take_break(state: &mut ViewState) -> Outcome {
    let (Some(task_id), Some(session)) =
        (state.focus.running_task(), state.focus.running_session())
    else {
        return Outcome::None;
    };
    if state.focus.on_break() {
        state.status = "already on a break".into();
        return Outcome::None;
    }
    let segment = state.focus.next_segment();
    state.status = format!(
        "{} — {}",
        crate::segment_label(segment),
        state.focused_title()
    );
    Outcome::SubmitMany(vec![
        Command::EndFocus {
            session,
            actual_focused_ms: None,
            completed_task: false,
        },
        Command::StartFocus(FocusStartDraft {
            task_id,
            kind: FocusKind::Break,
            length: state.focus.length,
            energy: state.focus.energy,
        }),
    ])
}

/// Log one interruption against the running session.
///
/// Deliberately leaves `focus.running` alone: an interruption is a note in the
/// distraction journal, not the end of the session
/// (`docs/08-features/focus-mode.md` §Interruption capture — "no shame UI").
fn log_interruption(state: &mut ViewState, reason: InterruptionReason) -> Outcome {
    state.mode = state.resting_mode();
    let Some(session) = state.focus.running_session() else {
        return Outcome::None;
    };
    state.status = format!("logged interruption: {}", reason.as_str());
    Outcome::Submit(Box::new(Command::LogInterruption { session, reason }))
}

/// Shared "nothing is selected" response for the task-scoped bindings.
fn no_selection(state: &mut ViewState) -> Outcome {
    state.status = "no task selected".into();
    Outcome::None
}

/// Re-parse the capture buffer for the inline preview.
///
/// `docs/08-features/inbox-and-capture.md`: "Parser runs *as the user types*;
/// an inline preview shows the structured interpretation." Cheap enough to do
/// per keystroke — the parser is a single pass over a short line and reads no
/// I/O; the stream list it resolves against is already in memory.
fn refresh_capture_preview(state: &mut ViewState, now_ms: u64) {
    if state.prompt != Some(Prompt::Capture) {
        state.capture_preview = None;
        return;
    }
    let text = state.input.trim();
    if text.is_empty() {
        state.capture_preview = None;
        return;
    }
    let parsed = parse_line(text, &state.streams, &state.contexts, now_ms, &state.tz);
    let mut line = preview_line(&parsed, &state.streams, &state.contexts, &state.tz);
    if let Some(note) = unresolved_note(&parsed.unresolved) {
        line.push_str("  ");
        line.push_str(&note);
    }
    state.capture_preview = Some(line);
}

/// Turn a capture line into a `CreateTask`, reporting anything the parser
/// could not apply.
///
/// The parser guarantees unresolved text stays in the title; this puts the
/// matching explanation on the status line, so "nothing typed is silently
/// discarded" holds all the way to the screen and not just in the data.
fn capture_outcome(state: &mut ViewState, text: &str, now_ms: u64) -> Outcome {
    let parsed = parse_line(text, &state.streams, &state.contexts, now_ms, &state.tz);
    state.status = match unresolved_note(&parsed.unresolved) {
        Some(note) => note,
        None => format!("captured \"{}\"", parsed.draft.title),
    };
    Outcome::Submit(Box::new(Command::CreateTask(parsed.draft)))
}

/// Close out a visual-mode operator or a triage decision.
///
/// `verb` is the past-tense word for the status line and `label` names what was
/// operated on (captured *before* the selection is torn down). Visual mode
/// always ends after one operator (as in vim); triage advances to the next
/// card.
fn finish_operator(state: &mut ViewState, verb: &str, label: &str) {
    let triage = state.triage;
    if state.visual_anchor.is_some() {
        state.exit_visual();
    }
    state.status = format!("{verb} \"{label}\"");
    if triage {
        // Completing/scheduling/deferring leaves the task in the Inbox, so the
        // cursor has to step forward. Runs last so an end-of-pass message wins
        // over the per-task one.
        state.triage_advance(false);
    }
}

/// Handle Enter (or `y` in a confirmation) according to the active mode.
fn submit(state: &mut ViewState, now_ms: u64) -> Outcome {
    match state.mode {
        Mode::Command => submit_command_line(state, now_ms),
        Mode::Confirm => submit_confirm(state),
        Mode::Picker => submit_picker(state),
        Mode::Insert => submit_prompt(state, now_ms),
        Mode::Normal | Mode::Visual | Mode::Triage | Mode::Focus | Mode::Interrupt => Outcome::None,
    }
}

/// `:`-line submit: parse and apply, mapping the pure [`AppEffect`] onto an
/// [`Outcome`].
fn submit_command_line(state: &mut ViewState, now_ms: u64) -> Outcome {
    let cmd = parse_command(&state.input);
    // A running session keeps the keyboard across a `:` detour, the same way a
    // triage pass does — otherwise `:focus stats` mid-session would silently
    // drop the user out of the session's key set.
    state.mode = state.resting_mode();
    state.input.clear();
    match apply_command(cmd, state) {
        Some(AppEffect::Quit) => Outcome::Quit,
        Some(AppEffect::Preview(path)) => Outcome::Preview(path),
        // `:capture` is the same parse as the `c` prompt, by construction.
        Some(AppEffect::Capture(text)) => capture_outcome(state, &text, now_ms),
        Some(AppEffect::Open(id)) => Outcome::OpenTask(id),
        Some(AppEffect::Devices) => Outcome::ShowDevices,
        Some(AppEffect::FocusStats) => Outcome::ShowFocusStats,
        None => Outcome::Refresh,
    }
}

/// `y` on a confirmation prompt. The only confirmable action in v1 is delete.
fn submit_confirm(state: &mut ViewState) -> Outcome {
    let prompt = state.prompt.take();
    let triage = state.triage;
    state.reset_to_normal();
    match prompt {
        Some(Prompt::ConfirmDelete { ids, title }) => {
            state.status = format!("deleted \"{title}\"");
            if triage {
                // A deleted task leaves the Inbox: the list closes up under the
                // cursor, so holding the index is already "next".
                state.triage_advance(true);
            }
            Outcome::submit_all(ids.into_iter().map(Command::DeleteTask).collect())
        }
        _ => Outcome::None,
    }
}

/// Enter on the move-to-stream picker.
fn submit_picker(state: &mut ViewState) -> Outcome {
    let picker = state.picker.take();
    let triage = state.triage;
    state.reset_to_normal();
    let Some(picker) = picker else {
        return Outcome::None;
    };
    let Some(row) = picker.selected_row() else {
        return Outcome::None;
    };
    let (stream, name) = (row.id, row.name.clone());
    state.status = format!("moved \"{}\" → {name}", picker.task_title);
    let cmds = picker
        .tasks
        .iter()
        .map(|id| Command::PromoteToStream { id: *id, stream })
        .collect();
    if triage {
        // Promoting *to* the Inbox leaves the task where it is, so the cursor
        // must step forward instead of holding.
        state.triage_advance(stream != sunrise_domain::inbox_stream_ref());
    }
    Outcome::submit_all(cmds)
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
            capture_outcome(state, &text, now_ms)
        }
        Some(Prompt::CaptureAside) => {
            // `reset_to_normal` returns to FOCUS mode while a session runs:
            // an aside must not end or pause the session it was typed during.
            state.reset_to_normal();
            if text.is_empty() {
                return Outcome::None;
            }
            state.status = format!("aside → Inbox: \"{text}\"");
            Outcome::CaptureAside(text)
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
        Some(Prompt::Defer(ids)) => match parse_defer_ms(&text) {
            Some(delta_ms) => {
                let label = state.operand_label();
                let to_ms = now_ms.saturating_add(delta_ms);
                let cmds = ids
                    .iter()
                    .map(|id| Command::DeferTask { id: *id, to_ms })
                    .collect();
                state.reset_to_normal();
                finish_operator(state, "deferred", &label);
                Outcome::submit_all(cmds)
            }
            None => {
                state.prompt = Some(Prompt::Defer(ids));
                state.status = format!("defer: cannot parse \"{text}\" — try 30m / 2h / 3d / 1w");
                Outcome::None
            }
        },
        Some(Prompt::Schedule(ids)) => {
            // One when-parser for the whole app: the same
            // `sunrise_domain::capture::parse_when` that backs `^when` in
            // capture. Anything it declines keeps the prompt open rather than
            // scheduling a guess.
            match parse_when(&text, now_ts(now_ms), &state.tz) {
                Some(at) => {
                    let label = state.operand_label();
                    let cmds = ids
                        .iter()
                        .map(|id| Command::UpdateTask {
                            id: *id,
                            patch: TaskPatch {
                                scheduled_at: Some(Some(at)),
                                ..Default::default()
                            },
                        })
                        .collect();
                    state.reset_to_normal();
                    finish_operator(state, "scheduled", &label);
                    Outcome::submit_all(cmds)
                }
                None => {
                    state.prompt = Some(Prompt::Schedule(ids));
                    state.status = format!(
                        "schedule: cannot parse \"{text}\" — try today / tomorrow 9am / +3d"
                    );
                    Outcome::None
                }
            }
        }
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
    use crate::view::fixtures::{context_row, fake_task, inbox_row, stream_row};
    use crossterm::event::KeyCode;

    /// Fixed "now" for deferral arithmetic: 2026-01-01T00:00:00Z.
    const NOW_MS: u64 = 1_767_225_600_000;
    const MIN_MS: u64 = 60 * 1000;

    /// Press a key: run it through the real keymap, then the reducer. Panics
    /// if the key has no binding, so a test that presses an unbound key fails
    /// loudly instead of silently asserting nothing.
    fn press(state: &mut ViewState, key: KeyCode) -> Outcome {
        let action = state
            .keymap
            .dispatch(key, state.mode, state.vim_mode, state.view)
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
            Outcome::OpenStreamPicker { tasks, title: t } => {
                assert_eq!(tasks, vec![id]);
                assert_eq!(t, title);
                vec![inbox_row(3), stream_row(7, "Work", 2)]
            }
            other => panic!("expected OpenStreamPicker, got {other:?}"),
        };
        s.open_stream_picker(vec![id], title, rows);
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
        s.open_stream_picker(
            vec![id],
            "t".into(),
            vec![inbox_row(0), stream_row(7, "Work", 0)],
        );
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

    /// Extract the command list from an outcome, whichever shape it took.
    fn commands(o: Outcome) -> Vec<Command> {
        match o {
            Outcome::Submit(c) => vec![*c],
            Outcome::SubmitMany(cs) => cs,
            other => panic!("expected commands, got {other:?}"),
        }
    }

    /// An Inbox state that also knows about a "Travel" stream, so `#travel`
    /// resolves the way it would against a real vault.
    fn inbox_with_streams() -> ViewState {
        let mut s = inbox_state();
        s.streams = vec![inbox_row(3), stream_row(7, "Travel", 0)];
        s.contexts = vec![context_row(11, "errands")];
        s.after_streams_loaded();
        s
    }

    // ---- capture through the parser (docs/08-features/inbox-and-capture.md) ----

    #[test]
    fn capture_runs_the_line_through_the_parser() {
        let mut s = inbox_with_streams();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "Buy milk #inbox !2");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateTask(draft) => {
                    // The annotations are *parsed*, not left in the title.
                    assert_eq!(draft.title, "Buy milk");
                    assert_eq!(draft.priority, Some(2));
                    assert_eq!(draft.stream_id, Some(inbox_row(3).id));
                }
                other => panic!("expected CreateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn capture_resolves_a_context_against_the_vault() {
        let mut s = inbox_with_streams();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "Buy milk @errands");
        // The live preview names the resolved context, and no warning is raised
        // for a context that exists.
        let p = s.capture_preview.clone().expect("a preview");
        assert!(p.contains("@errands"), "got {p}");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateTask(draft) => {
                    assert_eq!(draft.title, "Buy milk");
                    assert_eq!(draft.contexts, vec![context_row(11, "errands").id]);
                }
                other => panic!("expected CreateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(
            s.status.starts_with("captured"),
            "status was {:?}",
            s.status
        );
    }

    #[test]
    fn capture_parses_stream_schedule_and_duration() {
        let mut s = inbox_with_streams();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "Renew passport #travel ^tomorrow !1 ~1h");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateTask(draft) => {
                    assert_eq!(draft.title, "Renew passport");
                    assert_eq!(draft.stream_id, Some(stream_row(7, "Travel", 0).id));
                    assert_eq!(draft.priority, Some(1));
                    assert_eq!(draft.estimated_duration_s, Some(3600));
                    assert_eq!(
                        draft.scheduled_at.map(|t| t.to_string()),
                        Some("2026-01-02T00:00:00Z".to_string())
                    );
                }
                other => panic!("expected CreateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn an_unresolved_stream_is_reported_not_dropped() {
        let mut s = inbox_with_streams();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "Buy milk #travl");
        let cmd = press(&mut s, KeyCode::Enter);
        // The status explains what was not understood ...
        assert_eq!(s.status, "note: unknown stream \"travl\"");
        match commands(cmd).remove(0) {
            // ... and the text itself survives into the title.
            Command::CreateTask(draft) => assert_eq!(draft.title, "Buy milk #travl"),
            other => panic!("expected CreateTask, got {other:?}"),
        }
    }

    #[test]
    fn the_capture_preview_updates_on_every_keystroke() {
        let mut s = inbox_with_streams();
        let _ = press(&mut s, KeyCode::Char('c'));
        // Nothing typed yet: no preview to show.
        assert_eq!(s.capture_preview, None);

        type_text(&mut s, "Buy milk");
        assert_eq!(s.capture_preview.as_deref(), Some("title \"Buy milk\""));

        type_text(&mut s, " #travel !2");
        let p = s.capture_preview.clone().expect("a preview");
        assert!(p.contains("title \"Buy milk\""), "got {p}");
        assert!(p.contains("#Travel"), "got {p}");
        assert!(p.contains("!2"), "got {p}");

        // Backspacing an annotation retracts it from the preview.
        for _ in 0..3 {
            let _ = press(&mut s, KeyCode::Backspace);
        }
        let p = s.capture_preview.clone().expect("a preview");
        assert!(!p.contains("!2"), "priority should be gone: {p}");

        // Cancelling clears the preview along with the prompt.
        let _ = press(&mut s, KeyCode::Esc);
        assert_eq!(s.capture_preview, None);
    }

    #[test]
    fn the_capture_preview_flags_an_unknown_stream_while_typing() {
        let mut s = inbox_with_streams();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "Buy milk #travl");
        let p = s.capture_preview.clone().expect("a preview");
        assert!(p.contains("unknown stream \"travl\""), "got {p}");
    }

    // ---- `s` = schedule (docs/07-clients/tui.md keybinding table) ----

    #[test]
    fn schedule_sets_scheduled_at_from_a_when_expression() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        assert!(matches!(press(&mut s, KeyCode::Char('s')), Outcome::None));
        assert_eq!(s.mode, Mode::Insert);
        type_text(&mut s, "tomorrow 9am");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { id: target, patch } => {
                    assert_eq!(target, id);
                    assert_eq!(
                        patch.scheduled_at.flatten().map(|t| t.to_string()),
                        Some("2026-01-02T09:00:00Z".to_string())
                    );
                    // Nothing else moves.
                    assert!(patch.title.is_none() && patch.state.is_none());
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn schedule_with_unparseable_input_submits_nothing() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('s'));
        type_text(&mut s, "whenever");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        // The prompt stays open rather than scheduling a guess.
        assert!(matches!(s.prompt, Some(Prompt::Schedule(_))));
        assert!(
            s.status.contains("cannot parse"),
            "status was {:?}",
            s.status
        );
    }

    #[test]
    fn schedule_accepts_the_same_forms_the_capture_parser_does() {
        for expr in [
            "today",
            "tomorrow",
            "next friday",
            "2026-03-01",
            "+3d",
            "9am",
        ] {
            let mut s = inbox_state();
            let _ = press(&mut s, KeyCode::Char('s'));
            type_text(&mut s, expr);
            assert!(
                matches!(press(&mut s, KeyCode::Enter), Outcome::Submit(_)),
                "`{expr}` should schedule"
            );
        }
    }

    // ---- `V` = visual mode + bulk operators (docs/08-features/keyboard.md) ----

    #[test]
    fn visual_mode_extends_over_a_run_of_rows() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char('V'));
        assert_eq!(s.mode, Mode::Visual);
        assert_eq!(s.visual_range(), Some((0, 0)));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char('j'));
        assert_eq!(s.visual_range(), Some((0, 2)));
        // Extension clamps at the end instead of wrapping round to row 0.
        let _ = press(&mut s, KeyCode::Char('j'));
        assert_eq!(s.visual_range(), Some((0, 2)));
    }

    #[test]
    fn visual_complete_emits_one_command_per_selected_task() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let ids: Vec<EntityRef> = s.tasks.iter().map(|t| t.id).collect();
        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let cmds = commands(press(&mut s, KeyCode::Char('x')));
        assert_eq!(cmds.len(), 3);
        for (cmd, id) in cmds.iter().zip(&ids) {
            match cmd {
                Command::CompleteTask(target) => assert_eq!(target, id),
                other => panic!("expected CompleteTask, got {other:?}"),
            }
        }
        // One operator, then back to Normal — as in vim.
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.visual_anchor, None);
    }

    #[test]
    fn visual_defer_emits_one_command_per_selected_task() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        assert!(matches!(press(&mut s, KeyCode::Char('d')), Outcome::None));
        type_text(&mut s, "2h");
        let cmds = commands(press(&mut s, KeyCode::Enter));
        assert_eq!(cmds.len(), 2);
        for cmd in &cmds {
            match cmd {
                Command::DeferTask { to_ms, .. } => {
                    assert_eq!(*to_ms, NOW_MS + 120 * MIN_MS);
                }
                other => panic!("expected DeferTask, got {other:?}"),
            }
        }
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn visual_delete_is_still_gated_on_the_confirmation() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        assert!(matches!(press(&mut s, KeyCode::Char('D')), Outcome::None));
        assert_eq!(s.mode, Mode::Confirm);
        assert!(s.status.contains("2 tasks"), "status was {:?}", s.status);
        // Declining deletes nothing.
        assert!(matches!(press(&mut s, KeyCode::Char('n')), Outcome::None));

        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char('D'));
        let cmds = commands(press(&mut s, KeyCode::Char('y')));
        assert_eq!(cmds.len(), 2);
        assert!(cmds.iter().all(|c| matches!(c, Command::DeleteTask(_))));
    }

    #[test]
    fn visual_move_promotes_every_selected_task() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let ids: Vec<EntityRef> = s.tasks.iter().map(|t| t.id).collect();
        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let (tasks, title) = match press(&mut s, KeyCode::Char('m')) {
            Outcome::OpenStreamPicker { tasks, title } => (tasks, title),
            other => panic!("expected OpenStreamPicker, got {other:?}"),
        };
        assert_eq!(tasks, ids);
        assert_eq!(title, "3 tasks");
        s.open_stream_picker(tasks, title, vec![inbox_row(3), stream_row(7, "Work", 0)]);
        let _ = press(&mut s, KeyCode::Char('j'));
        let cmds = commands(press(&mut s, KeyCode::Enter));
        assert_eq!(cmds.len(), 3);
        for (cmd, id) in cmds.iter().zip(&ids) {
            match cmd {
                Command::PromoteToStream { id: target, stream } => {
                    assert_eq!(target, id);
                    assert_eq!(*stream, stream_row(7, "Work", 0).id);
                }
                other => panic!("expected PromoteToStream, got {other:?}"),
            }
        }
    }

    #[test]
    fn escape_leaves_visual_mode_without_touching_anything() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.visual_anchor, None);
        // The cursor stays where the extension left it; nothing was submitted.
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn visual_mode_needs_a_selection() {
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.after_tasks_loaded();
        assert!(matches!(press(&mut s, KeyCode::Char('V')), Outcome::None));
        assert_eq!(s.mode, Mode::Normal);
    }

    // ---- `E` = note body in $EDITOR (docs/07-clients/tui.md) ----

    #[test]
    fn edit_body_hands_the_current_body_to_the_runtime() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        let i = s.selected.expect("a selection");
        s.tasks[i].body = Some(sunrise_domain::NoteBody(b"first draft".to_vec()));
        match press(&mut s, KeyCode::Char('E')) {
            Outcome::EditBody { id: target, body } => {
                assert_eq!(target, id);
                // NoteBody is opaque bytes, so plain UTF-8 round-trips.
                assert_eq!(String::from_utf8(body).unwrap(), "first draft");
            }
            other => panic!("expected EditBody, got {other:?}"),
        }
        // `e` is still the inline title edit.
        assert!(matches!(press(&mut s, KeyCode::Char('e')), Outcome::None));
        assert_eq!(s.prompt, Some(Prompt::EditTitle(id)));
    }

    #[test]
    fn edit_body_on_a_task_without_one_starts_empty() {
        let mut s = inbox_state();
        match press(&mut s, KeyCode::Char('E')) {
            Outcome::EditBody { body, .. } => assert!(body.is_empty()),
            other => panic!("expected EditBody, got {other:?}"),
        }
    }

    // ---- command mode ----

    #[test]
    fn colon_capture_goes_through_the_same_parser() {
        let mut s = inbox_with_streams();
        let _ = press(&mut s, KeyCode::Char(':'));
        type_text(&mut s, "capture Renew passport #travel !1");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateTask(draft) => {
                    assert_eq!(draft.title, "Renew passport");
                    assert_eq!(draft.priority, Some(1));
                    assert_eq!(draft.stream_id, Some(stream_row(7, "Travel", 0).id));
                }
                other => panic!("expected CreateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn colon_open_asks_the_runtime_to_resolve_the_id() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        let _ = press(&mut s, KeyCode::Char(':'));
        type_text(&mut s, &format!("open {}", id.to_str()));
        match press(&mut s, KeyCode::Enter) {
            Outcome::OpenTask(target) => assert_eq!(target, id),
            other => panic!("expected OpenTask, got {other:?}"),
        }
    }

    #[test]
    fn colon_devices_asks_for_the_device_list() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char(':'));
        type_text(&mut s, "devices");
        assert!(matches!(
            press(&mut s, KeyCode::Enter),
            Outcome::ShowDevices
        ));
    }

    #[test]
    fn the_devices_overlay_is_dismissed_by_the_next_key() {
        let mut s = inbox_state();
        s.show_devices(Vec::new());
        assert!(s.devices.is_some());
        // Esc closes it without quitting the app.
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert!(s.devices.is_none());
    }

    #[test]
    fn an_unknown_command_still_errors() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char(':'));
        type_text(&mut s, "frobnicate");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::Refresh));
        assert!(s.status.contains("frobnicate"), "status was {:?}", s.status);
    }

    // ---- triage (docs/08-features/inbox-and-capture.md) ----

    /// Enter triage the way the runtime does: press `t`, then let the refresh
    /// the reducer asked for land.
    fn triage_state() -> ViewState {
        let mut s = inbox_with_streams();
        assert!(matches!(
            press(&mut s, KeyCode::Char('t')),
            Outcome::Refresh
        ));
        s.after_tasks_loaded();
        s
    }

    #[test]
    fn triage_starts_on_the_first_inbox_task() {
        let mut s = ViewState::default();
        s.view = View::Today;
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        s.select_next();
        assert!(matches!(
            press(&mut s, KeyCode::Char('t')),
            Outcome::Refresh
        ));
        assert!(s.triage);
        assert_eq!(s.view, View::Inbox);
        assert_eq!(s.mode, Mode::Triage);
        assert_eq!(s.selected, Some(0));
    }

    #[test]
    fn triage_keep_advances_without_submitting_anything() {
        let mut s = triage_state();
        assert!(matches!(press(&mut s, KeyCode::Char('k')), Outcome::None));
        assert_eq!(s.selected, Some(1));
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert_eq!(s.selected, Some(2));
        // Keeping the last task ends the pass.
        let _ = press(&mut s, KeyCode::Char('k'));
        assert!(!s.triage);
        assert_eq!(s.mode, Mode::Normal);
        assert_eq!(s.status, "triage complete");
    }

    #[test]
    fn triage_defers_and_moves_on() {
        let mut s = triage_state();
        let first = selected_id(&s);
        let _ = press(&mut s, KeyCode::Char('d'));
        type_text(&mut s, "1d");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::DeferTask { id, .. } => assert_eq!(id, first),
                other => panic!("expected DeferTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        // A deferred task stays in the Inbox, so the cursor steps forward and
        // the pass continues.
        assert!(s.triage);
        assert_eq!(s.mode, Mode::Triage);
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn triage_schedules_the_current_task() {
        let mut s = triage_state();
        let first = selected_id(&s);
        let _ = press(&mut s, KeyCode::Char('s'));
        type_text(&mut s, "tomorrow");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { id, patch } => {
                    assert_eq!(id, first);
                    assert!(patch.scheduled_at.flatten().is_some());
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn triage_promotes_out_of_the_inbox_and_holds_the_cursor() {
        let mut s = triage_state();
        let first = selected_id(&s);
        let (tasks, title) = match press(&mut s, KeyCode::Char('p')) {
            Outcome::OpenStreamPicker { tasks, title } => (tasks, title),
            other => panic!("expected OpenStreamPicker, got {other:?}"),
        };
        assert_eq!(tasks, vec![first]);
        s.open_stream_picker(tasks, title, vec![inbox_row(3), stream_row(7, "Work", 0)]);
        let _ = press(&mut s, KeyCode::Char('j'));
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::PromoteToStream { id, .. } => assert_eq!(id, first),
                other => panic!("expected PromoteToStream, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        // The promoted task leaves the Inbox, so the list closes up under a
        // stationary cursor rather than skipping the next task.
        assert!(s.triage);
        assert_eq!(s.selected, Some(0));
    }

    #[test]
    fn triage_delete_still_confirms() {
        let mut s = triage_state();
        let first = selected_id(&s);
        assert!(matches!(press(&mut s, KeyCode::Char('D')), Outcome::None));
        assert_eq!(s.mode, Mode::Confirm);
        match press(&mut s, KeyCode::Char('y')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::DeleteTask(id) => assert_eq!(id, first),
                other => panic!("expected DeleteTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(s.triage, "the pass continues after a delete");
        assert_eq!(s.selected, Some(0));
    }

    #[test]
    fn escape_leaves_triage() {
        let mut s = triage_state();
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert!(!s.triage);
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn triage_over_an_empty_inbox_ends_immediately() {
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.after_tasks_loaded();
        let _ = press(&mut s, KeyCode::Char('t'));
        // The refresh the reducer asked for finds nothing to triage.
        s.after_tasks_loaded();
        assert!(!s.triage);
        assert_eq!(s.status, "triage complete");
    }

    #[test]
    fn a_remapped_key_reaches_the_reducer() {
        // End-to-end for keys.toml: config → Keymap → dispatch → Command.
        let (map, warnings) = crate::keymap::Keymap::from_config(&[("capture".into(), "n".into())]);
        assert!(warnings.is_empty());
        let mut s = inbox_with_streams();
        s.keymap = map;
        let _ = press(&mut s, KeyCode::Char('n'));
        assert_eq!(s.prompt, Some(Prompt::Capture));
        type_text(&mut s, "Buy milk");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::Submit(_)));
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

#[cfg(test)]
mod focus_tests {
    use super::*;
    use crate::view::fixtures::{ended_work_session, fake_task, plan_row, running_session};
    use crate::view::CascadeReport;
    use crossterm::event::KeyCode;
    use sunrise_domain::{Energy, EnergyFit, InterruptionReason, SessionLength, POMODORO_MS};

    /// Fixed "now" shared with the rest of the reducer tests.
    const NOW_MS: u64 = 1_767_225_600_000;

    fn press(state: &mut ViewState, key: KeyCode) -> Outcome {
        let action = state
            .keymap
            .dispatch(key, state.mode, state.vim_mode, state.view)
            .unwrap_or_else(|| panic!("no binding for {key:?} in {:?}", state.mode));
        apply_action(action, state, NOW_MS)
    }

    fn inbox_state() -> ViewState {
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.tasks = (0u8..3).map(fake_task).collect();
        s.after_tasks_loaded();
        s.select_next();
        s
    }

    /// A state with a live session on task 1, exactly as a refresh would leave
    /// it: the session read back from the core, the keyboard handed over.
    fn session_state() -> ViewState {
        let task = fake_task(1);
        let mut s = ViewState::default();
        s.view = View::Focus;
        s.focused_task = Some(task.clone());
        s.focus.running = Some(running_session(task.id, NOW_MS, Some(POMODORO_MS)));
        s.after_focus_loaded();
        assert_eq!(s.mode, Mode::Focus);
        s
    }

    fn session_id(s: &ViewState) -> EntityRef {
        s.focus.running_session().expect("a running session")
    }

    #[test]
    fn f_starts_a_session_on_the_selected_task() {
        let mut s = inbox_state();
        let id = s.selected_task().expect("a selection").id;
        match press(&mut s, KeyCode::Char('F')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::StartFocus(d) => {
                    assert_eq!(d.task_id, id);
                    assert_eq!(d.kind, FocusKind::Work);
                    // The default sizing uses the estimate the user already
                    // recorded, chunking when it exceeds a sitting.
                    assert_eq!(d.length, SessionLength::SizedToEstimate);
                    assert_eq!(d.energy, None);
                }
                other => panic!("expected StartFocus, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn f_with_no_selection_says_so_instead_of_starting_something() {
        let mut s = ViewState::default();
        assert!(matches!(press(&mut s, KeyCode::Char('F')), Outcome::None));
        assert_eq!(s.status, "no task selected");
    }

    #[test]
    fn the_declared_energy_budget_and_length_reach_the_start_command() {
        let mut s = inbox_state();
        s.focus.energy = Some(Energy::Low);
        s.focus.length = SessionLength::OnePomodoro;
        match press(&mut s, KeyCode::Char('F')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::StartFocus(d) => {
                    assert_eq!(d.energy, Some(Energy::Low));
                    assert_eq!(d.length, SessionLength::OnePomodoro);
                }
                other => panic!("expected StartFocus, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_a_planner_row_starts_that_session() {
        let mut s = ViewState::default();
        s.view = View::Focus;
        s.focus.plan = vec![
            plan_row(1, 5, EnergyFit::Exact),
            plan_row(2, 0, EnergyFit::Over),
        ];
        s.after_focus_loaded();
        s.nav_next();
        let picked = s.focus.plan[1].task.id;
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::StartFocus(d) => assert_eq!(d.task_id, picked),
                other => panic!("expected StartFocus, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn a_second_start_is_refused_while_one_runs() {
        let mut s = session_state();
        // `F` is not a FOCUS-mode key, so reach the action directly.
        assert!(matches!(
            apply_action(Action::StartFocus, &mut s, NOW_MS),
            Outcome::None
        ));
        assert!(s.status.contains("already running"));
    }

    #[test]
    fn esc_ends_the_session_and_lets_the_core_freeze_the_time() {
        let mut s = session_state();
        let session = session_id(&s);
        match press(&mut s, KeyCode::Esc) {
            Outcome::Submit(cmd) => match *cmd {
                Command::EndFocus {
                    session: target,
                    actual_focused_ms,
                    completed_task,
                } => {
                    assert_eq!(target, session);
                    // `None` = "freeze the span you derive from your own
                    // clock". The TUI has no accumulated figure to send.
                    assert_eq!(actual_focused_ms, None);
                    assert!(!completed_task);
                }
                other => panic!("expected EndFocus, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(!s.focus.is_running());
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn q_ends_the_session_too_and_never_quits_the_app() {
        let mut s = session_state();
        assert!(matches!(
            press(&mut s, KeyCode::Char('q')),
            Outcome::Submit(_)
        ));
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn completing_mid_session_completes_closes_and_reads_the_cascade() {
        let mut s = session_state();
        let task = s.focus.running_task().expect("a task");
        let session = session_id(&s);
        match press(&mut s, KeyCode::Char('x')) {
            Outcome::SubmitThenCascade { cmds, task: target } => {
                assert_eq!(target, task);
                assert_eq!(cmds.len(), 2);
                match &cmds[0] {
                    Command::CompleteTask(t) => assert_eq!(*t, task),
                    other => panic!("expected CompleteTask first, got {other:?}"),
                }
                match &cmds[1] {
                    Command::EndFocus {
                        session: s2,
                        actual_focused_ms,
                        completed_task,
                    } => {
                        assert_eq!(*s2, session);
                        assert_eq!(*actual_focused_ms, None);
                        // The session *records* the completion; it never
                        // becomes a second writer of task state.
                        assert!(*completed_task);
                    }
                    other => panic!("expected EndFocus second, got {other:?}"),
                }
            }
            other => panic!("expected SubmitThenCascade, got {other:?}"),
        }
        assert!(!s.focus.is_running());
    }

    #[test]
    fn logging_an_interruption_does_not_end_the_session() {
        let mut s = session_state();
        let session = session_id(&s);
        assert!(matches!(press(&mut s, KeyCode::Char('i')), Outcome::None));
        assert_eq!(s.mode, Mode::Interrupt);
        match press(&mut s, KeyCode::Char('m')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::LogInterruption {
                    session: target,
                    reason,
                } => {
                    assert_eq!(target, session);
                    assert_eq!(reason, InterruptionReason::Meeting);
                }
                other => panic!("expected LogInterruption, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        // The whole point: the clock is still running afterwards.
        assert!(s.focus.is_running());
        assert_eq!(s.focus.running_session(), Some(session));
        assert_eq!(s.mode, Mode::Focus);
    }

    #[test]
    fn every_reason_in_the_domains_set_has_a_key_and_none_are_invented() {
        for (key, reason) in [
            ('s', InterruptionReason::SelfInterrupt),
            ('m', InterruptionReason::Meeting),
            ('b', InterruptionReason::Blocked),
            ('o', InterruptionReason::Other),
        ] {
            let mut s = session_state();
            let _ = press(&mut s, KeyCode::Char('i'));
            match press(&mut s, KeyCode::Char(key)) {
                Outcome::Submit(cmd) => match *cmd {
                    Command::LogInterruption { reason: got, .. } => assert_eq!(got, reason),
                    other => panic!("expected LogInterruption, got {other:?}"),
                },
                other => panic!("expected Submit, got {other:?}"),
            }
        }
    }

    #[test]
    fn cancelling_the_reason_chooser_leaves_the_session_running() {
        let mut s = session_state();
        let _ = press(&mut s, KeyCode::Char('i'));
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert_eq!(s.mode, Mode::Focus);
        assert!(s.focus.is_running());
    }

    #[test]
    fn capture_aside_routes_through_the_core_and_leaves_the_session_alone() {
        let mut s = session_state();
        // The focused task lives in a stream — the destination an ordinary
        // capture could inherit and an aside must not.
        s.focused_task.as_mut().expect("a task").stream_id =
            EntityRef::new(sunrise_id::EntityKind::Stream, [7u8; 16]);
        let session = session_id(&s);
        assert!(matches!(press(&mut s, KeyCode::Char('a')), Outcome::None));
        assert_eq!(s.mode, Mode::Insert);
        assert_eq!(s.prompt, Some(Prompt::CaptureAside));
        for c in "call the vet".chars() {
            let _ = press(&mut s, KeyCode::Char(c));
        }
        match press(&mut s, KeyCode::Enter) {
            // Deliberately *not* `Submit(CreateTask)`: building the draft
            // locally would let the focused task's stream leak into it. The
            // runtime hands the line to `Core::capture_aside`, which drops any
            // `#stream` so the destination is the Inbox.
            Outcome::CaptureAside(text) => assert_eq!(text, "call the vet"),
            other => panic!("expected CaptureAside, got {other:?}"),
        }
        assert!(s.focus.is_running(), "an aside never ends the session");
        assert_eq!(s.focus.running_session(), Some(session));
        assert_eq!(s.mode, Mode::Focus, "and it returns to the session");
    }

    #[test]
    fn an_empty_aside_commits_nothing() {
        let mut s = session_state();
        let _ = press(&mut s, KeyCode::Char('a'));
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert!(s.focus.is_running());
    }

    #[test]
    fn taking_a_break_closes_the_work_segment_and_opens_the_break() {
        let mut s = session_state();
        let task = s.focus.running_task().expect("a task");
        let session = session_id(&s);
        match press(&mut s, KeyCode::Char('b')) {
            Outcome::SubmitMany(cmds) => {
                assert_eq!(cmds.len(), 2);
                match &cmds[0] {
                    Command::EndFocus { session: s2, .. } => assert_eq!(*s2, session),
                    other => panic!("expected EndFocus first, got {other:?}"),
                }
                match &cmds[1] {
                    // The break's *length* is not passed: the core sizes it
                    // from the same `break_after` rule, so 5-vs-15 is decided
                    // in one place.
                    Command::StartFocus(d) => {
                        assert_eq!(d.task_id, task);
                        assert_eq!(d.kind, FocusKind::Break);
                    }
                    other => panic!("expected StartFocus second, got {other:?}"),
                }
            }
            other => panic!("expected SubmitMany, got {other:?}"),
        }
    }

    #[test]
    fn the_break_the_status_line_names_comes_from_break_after() {
        let mut s = session_state();
        let task = s.focus.running_task().expect("a task");
        // Three finished work segments plus the one running is the fourth
        // cycle, so the long break is what is owed.
        s.focus.sessions = (0u8..3).map(|i| ended_work_session(task, i + 20)).collect();
        let _ = press(&mut s, KeyCode::Char('b'));
        assert!(s.status.contains("long break"), "status was {:?}", s.status);
    }

    #[test]
    fn a_break_does_not_get_its_own_break() {
        let mut s = session_state();
        s.focus
            .running
            .as_mut()
            .expect("a session")
            .session
            .start
            .kind = FocusKind::Break;
        assert!(matches!(press(&mut s, KeyCode::Char('b')), Outcome::None));
        assert!(s.status.contains("already on a break"));
    }

    #[test]
    fn defer_mid_session_targets_the_focused_task_not_the_list_cursor() {
        let mut s = session_state();
        let task = s.focus.running_task().expect("a task");
        s.tasks = vec![fake_task(9)];
        s.selected = Some(0);
        assert!(matches!(press(&mut s, KeyCode::Char('d')), Outcome::None));
        assert_eq!(s.prompt, Some(Prompt::Defer(vec![task])));
    }

    #[test]
    fn focus_stats_is_reachable_from_the_command_line() {
        let mut s = ViewState::default();
        let _ = press(&mut s, KeyCode::Char(':'));
        for c in "focus stats".chars() {
            let _ = press(&mut s, KeyCode::Char(c));
        }
        assert!(matches!(
            press(&mut s, KeyCode::Enter),
            Outcome::ShowFocusStats
        ));
    }

    #[test]
    fn the_command_line_hands_the_keyboard_back_to_the_session() {
        let mut s = session_state();
        let _ = press(&mut s, KeyCode::Char(':'));
        assert_eq!(s.mode, Mode::Command);
        for c in "focus stats".chars() {
            let _ = press(&mut s, KeyCode::Char(c));
        }
        let _ = press(&mut s, KeyCode::Enter);
        assert_eq!(s.mode, Mode::Focus, "a `:` detour never orphans a session");
    }

    #[test]
    fn declaring_an_energy_budget_re_ranks_the_queue() {
        let mut s = ViewState::default();
        let _ = press(&mut s, KeyCode::Char(':'));
        for c in "focus energy high".chars() {
            let _ = press(&mut s, KeyCode::Char(c));
        }
        // Refresh: the new budget is an input to `Query::FocusPlan`.
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::Refresh));
        assert_eq!(s.focus.energy, Some(Energy::High));
    }

    #[test]
    fn dismissing_the_stats_overlay_with_esc_never_ends_the_session() {
        let mut s = session_state();
        s.focus.stats = Some(Box::new(sunrise_domain::fold_focus_stats(&[], NOW_MS)));
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert!(s.focus.stats.is_none());
        assert!(
            s.focus.is_running(),
            "Esc closed the overlay, not the session"
        );
    }

    #[test]
    fn starting_a_session_clears_the_previous_completions_cascade() {
        let mut s = inbox_state();
        s.focus.cascade = Some(CascadeReport {
            cascade: sunrise_domain::UnblockCascade {
                completed: s.tasks[0].id,
                released: vec![s.tasks[1].id],
                still_blocked: Vec::new(),
            },
            released: vec!["Deploy".into()],
        });
        let _ = press(&mut s, KeyCode::Char('F'));
        assert!(s.focus.cascade.is_none());
    }
}
