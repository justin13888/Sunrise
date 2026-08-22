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
use crate::edit::parse_edit;
use crate::keymap::{Action, Mode};
use crate::view::{DeleteTarget, Prompt, SidebarRow, View, ViewState};
use crate::{apply_command, AppEffect};
use std::path::PathBuf;
use std::time::Duration;
use sunrise_core::commands::FocusStartDraft;
use sunrise_core::{Command, DomainEvent};
use sunrise_domain::capture::parse_when;
use sunrise_domain::{
    ContextDraft, ContextPatch, FocusKind, InterruptionReason, RoutineCatchupPolicy, RoutineDraft,
    RoutinePatch, StreamDraft, StreamPatch, TaskPatch, TaskTemplate,
};
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
    /// Run `Query::ActivityTimeline` for this entity and show the feed (`L`).
    ShowActivity {
        /// Task or Stream whose feed to read.
        entity: EntityRef,
        /// Display name for the overlay title.
        title: String,
    },
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
    /// Render a stats dataset and write it to `path` (`:export`).
    Export {
        /// Which dataset.
        dataset: sunrise_domain::ExportDataset,
        /// Serialization format.
        format: sunrise_domain::ExportFormat,
        /// Destination; `None` picks a name in the working directory.
        path: Option<PathBuf>,
    },
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
        // Movement scrolls the overlay instead of dismissing it: the key list
        // is taller than a minimum-size terminal, so an overlay that could not
        // be scrolled would document only its first screenful.
        //
        // `HELP_ROWS` is a generous upper bound rather than the real content
        // height, which only the renderer knows. Over-scrolling shows a short
        // last page; the renderer clamps what it actually draws.
        const HELP_ROWS: usize = 128;
        let half = isize::try_from(state.half_page_rows()).unwrap_or(1);
        let step = match action {
            Action::Next => Some(1),
            Action::Prev => Some(-1),
            Action::PageDown | Action::HalfPageDown => Some(half),
            Action::PageUp | Action::HalfPageUp => Some(-half),
            Action::GotoTop => Some(isize::MIN / 2),
            Action::GotoBottom => Some(isize::MAX / 2),
            _ => None,
        };
        if let Some(delta) = step {
            state.scroll_help(delta, HELP_ROWS);
            return Outcome::None;
        }
        state.show_help = false;
        state.help_scroll = 0;
        if matches!(action, Action::Escape | Action::Quit) {
            return Outcome::None;
        }
    }
    // The activity feed is informational like the others, but long enough to
    // need scrolling, so movement drives it instead of closing it.
    if let Some(feed) = state.activity.as_mut() {
        let page = state.viewport_rows.max(1);
        let step = match action {
            Action::Next => Some(1),
            Action::Prev => Some(-1),
            Action::PageDown | Action::HalfPageDown => Some(isize::try_from(page).unwrap_or(1)),
            Action::PageUp | Action::HalfPageUp => Some(-isize::try_from(page).unwrap_or(1)),
            Action::GotoTop => Some(isize::MIN / 2),
            Action::GotoBottom => Some(isize::MAX / 2),
            _ => None,
        };
        if let Some(delta) = step {
            feed.scroll_by(delta, page);
            return Outcome::None;
        }
        state.activity = None;
        state.status.clear();
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

    // The Review panel is a page, not a list, so its scroll bound is the
    // rendered row count less one screenful. Recomputed before every action
    // because a synced op can change the panel's height under the cursor.
    if state.view == View::Review {
        let rows = crate::render::review_rows(state);
        state.review.max_scroll = rows.saturating_sub(state.viewport_rows);
    }

    // Undo and redo replay recorded steps; recording them again would make
    // the stack chase its own tail.
    if matches!(action, Action::Undo) {
        return undo_step(state);
    }
    if matches!(action, Action::Redo) {
        return redo_step(state);
    }

    // The undo entry is built from the state as it stands *now*, before the
    // core has applied anything: the fields a command is about to overwrite
    // still hold the values that put them back. `apply_action` only mutates
    // view state (mode, cursor, status), never task data, so reading it after
    // the arm has run is still reading the pre-command values.
    let fallback_label = state.operand_label();
    let outcome = match action {
        Action::Quit => Outcome::Quit,
        Action::SwitchView(View::Focus) => {
            state.open_focus();
            Outcome::Refresh
        }
        Action::SwitchView(v) => {
            state.switch_view(v);
            Outcome::Refresh
        }
        Action::MarkToggle => {
            if !state.toggle_mark() {
                return no_selection(state);
            }
            Outcome::None
        }
        Action::MarkClear => {
            state.clear_marks();
            state.status = "marks cleared".into();
            Outcome::None
        }
        Action::Next => {
            match state.picker.as_mut() {
                Some(p) => p.next(),
                None => state.nav_next(),
            }
            after_nav(state)
        }
        Action::Prev => {
            match state.picker.as_mut() {
                Some(p) => p.prev(),
                None => state.nav_prev(),
            }
            after_nav(state)
        }
        Action::GotoPrefix => {
            if g_armed {
                state.nav_first();
                return after_nav(state);
            }
            state.pending_g = true;
            state.status =
                "g… g top · t today · i inbox · s browse · / search · f focus · r routines · v review"
                    .into();
            Outcome::None
        }
        Action::GotoTop => {
            state.nav_first();
            after_nav(state)
        }
        Action::GotoBottom => {
            state.nav_last();
            after_nav(state)
        }
        Action::PageDown | Action::PageUp | Action::HalfPageDown | Action::HalfPageUp => {
            let rows = match action {
                Action::PageDown | Action::PageUp => state.page_rows(),
                _ => state.half_page_rows(),
            };
            let delta = isize::try_from(rows).unwrap_or(1);
            let delta = match action {
                Action::PageUp | Action::HalfPageUp => -delta,
                _ => delta,
            };
            match state.picker.as_mut() {
                // The picker is a short overlay list of its own; paging it
                // means paging the picker, not the list behind it.
                Some(p) => p.nav_by(delta),
                None => state.nav_by(delta),
            }
            after_nav(state)
        }
        Action::TogglePane => {
            state.toggle_pane();
            Outcome::None
        }
        Action::PaneLeft => {
            state.focus_sidebar();
            Outcome::None
        }
        Action::PaneRight => {
            state.focus_tasks();
            Outcome::None
        }
        // Enter on a planner row starts the session it proposes, so accepting
        // the top pick is one keypress (`docs/08-features/focus-mode.md`
        // §Focus Planner).
        Action::Activate if state.view == View::Focus && !state.focus.is_running() => {
            start_focus(state)
        }
        Action::Activate => {
            if state.view == View::Stream && state.pane.is_sidebar() {
                // Confirm the highlighted sidebar row: point the task pane at
                // it and move focus there.
                state.open_sidebar_row();
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
            let (cmds, verb) = toggle_cmds(state, &ids);
            finish_operator(state, verb, &label);
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
        // In the Routines view the row under the cursor is a template, so `e`
        // edits the thing that makes it a routine — its recurrence — and `E`
        // renames it, rather than both meaning nothing.
        Action::EditTitle if state.view == View::Routines => match state.selected_routine_row() {
            Some(row) => {
                let (id, rrule) = (row.id, row.rrule.clone());
                state.mode = Mode::Insert;
                state.prompt = Some(Prompt::EditRecurrence(id));
                state.input.set(rrule);
                state.status = "recurrence: every day · weekdays · every 2 weeks on tue".into();
                Outcome::None
            }
            None => no_routine(state),
        },
        Action::EditBody if state.view == View::Routines => match state.selected_routine_row() {
            Some(row) => {
                let (id, title) = (row.id, row.title.clone());
                state.mode = Mode::Insert;
                state.prompt = Some(Prompt::RenameRoutine(id));
                state.input.set(title);
                state.status = "rename routine: Enter to save, Esc to cancel".into();
                Outcome::None
            }
            None => no_routine(state),
        },
        Action::EditTitle if state.sidebar_row().is_some() => {
            let (row, name) = state.sidebar_row().expect("just checked");
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::Rename(row));
            // `@` is display sugar on a context row, not part of its name.
            state.input.set(name.trim_start_matches('@').to_string());
            state.status = "rename: Enter to save, Esc to cancel".into();
            Outcome::None
        }
        Action::EditTitle => match state.selected_task().map(|t| (t.id, t.title.clone())) {
            Some((id, title)) => {
                state.mode = Mode::Insert;
                state.prompt = Some(Prompt::EditTitle(id));
                state.input.set(title);
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
        Action::Annotate => {
            let ids = state.operand_ids();
            if ids.is_empty() {
                return no_selection(state);
            }
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::Annotate(ids));
            state.input.clear();
            state.status =
                "annotate: !1-5 %low|med|high ~30m ^when due:when @ctx @-ctx #stream · - clears"
                    .into();
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
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
            // In the Browse sidebar `D` means the row under the cursor; in a
            // task list it means the operand set. Same key, same gate.
            if state.view == View::Routines {
                let Some(row) = state.selected_routine_row() else {
                    return no_routine(state);
                };
                let (id, title) = (row.id, row.title.clone());
                state.mode = Mode::Confirm;
                state.input.clear();
                state.status =
                    format!("delete routine \"{title}\" — tasks it already made stay? [y/N]");
                state.prompt = Some(Prompt::ConfirmDelete {
                    target: DeleteTarget::Routine(id),
                    title,
                });
                return Outcome::None;
            }
            let (target, title, warning) = match state.sidebar_row() {
                Some((SidebarRow::Stream(id), name)) => {
                    (DeleteTarget::Stream(id), name, " — its tasks go with it")
                }
                Some((SidebarRow::Context(id), name)) => (
                    DeleteTarget::Context(id),
                    name,
                    " — removed from every task carrying it",
                ),
                None => {
                    let ids = state.operand_ids();
                    if ids.is_empty() {
                        return no_selection(state);
                    }
                    (DeleteTarget::Tasks(ids), state.operand_label(), "")
                }
            };
            // Destructive: park in Confirm mode. Nothing is submitted
            // until the user answers `y`.
            state.mode = Mode::Confirm;
            state.input.clear();
            state.status = format!("delete \"{title}\"{warning}? [y/N]");
            state.prompt = Some(Prompt::ConfirmDelete { target, title });
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
        Action::CreateContext => {
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::CreateContext);
            state.input.clear();
            state.status = "new context: type a name (no @), Enter to create".into();
            Outcome::None
        }
        Action::CreateRoutine => {
            state.mode = Mode::Insert;
            state.prompt = Some(Prompt::CreateRoutine);
            state.input.clear();
            state.status = "new routine: <title #stream !p ~dur @ctx ^first> | <every day>".into();
            Outcome::None
        }
        Action::SkipOccurrence => skip_occurrence(state),
        Action::NextReviewPane => {
            state.review.pane = state.review.pane.next();
            state.review.scroll = 0;
            state.status.clear();
            // Each panel is a different query; the runtime reloads on refresh.
            Outcome::Refresh
        }
        Action::ShiftWeek(n) => {
            state.review.shift_week(i64::from(n));
            Outcome::Refresh
        }
        Action::SaveReview => save_review(state),
        Action::ToggleArchive => toggle_archive(state),
        Action::TogglePause => toggle_pause(state),
        Action::CompleteCommand => {
            complete_command_line(state);
            Outcome::None
        }
        Action::RecallHistory(d) => {
            state.recall_history(d);
            Outcome::None
        }
        Action::LinkBlockers => link_blockers(state),
        Action::ClearBlockers => clear_blockers(state),
        Action::ShowActivity => match activity_target(state) {
            Some((id, title)) => Outcome::ShowActivity { entity: id, title },
            None => {
                state.status = "activity: select a task, stream or routine first".into();
                Outcome::None
            }
        },
        Action::ToggleHelp => {
            state.show_help = !state.show_help;
            state.help_scroll = 0;
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
            state.history_pos = None;
            state.status = "Tab completes · ↑ recalls".into();
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
            if state.mode == Mode::Normal && !state.marked.is_empty() {
                state.clear_marks();
                state.status = "marks cleared".into();
                Outcome::None
            } else if state.mode == Mode::Normal && state.view != View::Focus {
                // Nothing to back out of. Say so rather than silently doing
                // nothing, so a user reaching for "get me out of here" is told
                // where the exit is.
                state.status = "nothing to cancel — q to quit, ? for keys".into();
                Outcome::None
            } else if state.mode == Mode::Visual {
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
            if state.input.len() + c.len_utf8() <= MAX_INPUT {
                state.input.insert_char(c);
            }
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        Action::InsertStr(text) => {
            // A paste is clamped as a whole rather than truncated mid-way: a
            // half-inserted paste is worse than a refused one, because the
            // user cannot tell which half made it.
            if state.input.len() + text.len() <= MAX_INPUT {
                state.input.insert_str(&text);
            } else {
                state.status = "paste too long for this prompt".into();
            }
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        Action::Backspace => {
            state.input.backspace();
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        Action::DeleteForward => {
            state.input.delete_forward();
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        // Caret motions never re-parse: the text is unchanged, so the capture
        // preview under it is still correct.
        Action::CursorLeft => {
            state.input.left();
            Outcome::None
        }
        Action::CursorRight => {
            state.input.right();
            Outcome::None
        }
        Action::CursorHome => {
            state.input.home();
            Outcome::None
        }
        Action::CursorEnd => {
            state.input.end();
            Outcome::None
        }
        Action::CursorWordLeft => {
            state.input.word_left();
            Outcome::None
        }
        Action::CursorWordRight => {
            state.input.word_right();
            Outcome::None
        }
        Action::DeleteWordBack => {
            state.input.delete_word_back();
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        Action::KillToStart => {
            state.input.kill_to_start();
            refresh_capture_preview(state, now_ms);
            Outcome::None
        }
        Action::KillToEnd => {
            state.input.kill_to_end();
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
        // Handled above, before anything could be recorded.
        Action::Undo | Action::Redo => Outcome::None,
    };
    // The arm's own status line is the best description of what just
    // happened ("archived Work", "annotated 3 tasks"); the operand label is
    // the fallback for the arms that set none.
    let label = if state.status.is_empty() {
        fallback_label
    } else {
        state.status.clone()
    };
    record_undo(state, &label, &outcome);
    outcome
}

/// Record `outcome`'s commands as one reversible step, if they all invert.
///
/// Silent when they do not: a step that cannot be undone should not push a
/// entry that fails when `u` reaches it. `u` says why at the point the user
/// asks, which is where the explanation is useful.
fn record_undo(state: &mut ViewState, label: &str, outcome: &Outcome) {
    let cmds: &[Command] = match outcome {
        Outcome::Submit(cmd) => std::slice::from_ref(&**cmd),
        Outcome::SubmitMany(cmds) | Outcome::SubmitThenCascade { cmds, .. } => cmds,
        _ => return,
    };
    // Focus-session bookkeeping is a log, not an edit: a `start`/`end` pair is
    // append-only by construction (ADR-0013), so there is nothing to put back.
    if cmds.iter().any(|c| {
        matches!(
            c,
            Command::StartFocus(_)
                | Command::EndFocus { .. }
                | Command::LogInterruption { .. }
                | Command::SaveReviewSnapshot(_)
        )
    }) {
        return;
    }
    match crate::undo::invert(state, cmds) {
        Ok(backward) => {
            state.last_irreversible = None;
            state.push_undo(crate::undo::UndoEntry {
                label: label.to_string(),
                forward: cmds.to_vec(),
                backward,
            });
        }
        // Remembered rather than reported now: the user has not asked yet, and
        // "that cannot be undone" is only useful at the moment they press `u`.
        Err(why) => state.last_irreversible = Some(why),
    }
}

/// Walk one step back.
fn undo_step(state: &mut ViewState) -> Outcome {
    let Some(entry) = state.undo.pop() else {
        state.status = match state.last_irreversible {
            Some(crate::NotUndoable::Deleted) => {
                "a delete cannot be undone — the core has no restore op".into()
            }
            Some(crate::NotUndoable::Unsupported) => "the last change cannot be undone".to_string(),
            None => "nothing to undo".to_string(),
        };
        return Outcome::None;
    };
    state.last_irreversible = None;
    state.status = format!("undid \"{}\"", entry.label);
    let cmds = entry.backward.clone();
    state.redo.push(entry.flipped());
    Outcome::submit_all(cmds)
}

/// Walk one step forward again.
fn redo_step(state: &mut ViewState) -> Outcome {
    let Some(entry) = state.redo.pop() else {
        state.status = "nothing to redo".into();
        return Outcome::None;
    };
    state.status = format!("redid \"{}\"", entry.label);
    let cmds = entry.backward.clone();
    // Pushed directly rather than through `push_undo`, which clears the redo
    // stack: walking forward must not destroy the rest of the forward history.
    state.undo.push(entry.flipped());
    Outcome::submit_all(cmds)
}

/// Make the marked tasks block the one under the cursor.
///
/// The dependency graph is the load-bearing input to the whole Focus feature:
/// the planner drops blocked work so the queue is never a dead end, it ranks
/// by how much finishing a task releases, and the unblock cascade reports what
/// a completion freed. `Query::Actionable`, `FocusPlanRow.unblocks` and
/// `Query::UnblockCascade` all read it — and no client could **write** it, so
/// in practice every vault's graph was empty and every leverage number was
/// zero.
///
/// The gesture reuses the mark set rather than adding a picker: "these, then
/// that" is how a dependency is stated out loud, and marks already survive the
/// cursor moving, which is exactly what selecting a blocker and then walking
/// to its dependent requires.
fn link_blockers(state: &mut ViewState) -> Outcome {
    let blockers = state.marked_ids();
    if blockers.is_empty() {
        state.status = "blockers: Space-mark what must finish first, then b on the task".into();
        return Outcome::None;
    }
    let Some(target) = state.selected_task().map(|t| (t.id, t.title.clone())) else {
        return no_selection(state);
    };
    let (id, title) = target;
    if blockers.contains(&id) {
        // A one-task cycle is the only one reachable from this gesture; the
        // core rejects longer ones at submit, where the whole graph is known.
        state.status = "a task cannot block itself".into();
        return Outcome::None;
    }
    // The set replaces rather than extends: `blocked_by` is one LWW register,
    // so "these block it" has to be the whole answer or a concurrent edit on
    // another device would drop half of it.
    let n = blockers.len();
    state.clear_marks();
    state.status = format!("{n} task(s) now block \"{title}\"");
    Outcome::Submit(Box::new(Command::UpdateTask {
        id,
        patch: TaskPatch {
            blocked_by: Some(blockers),
            ..Default::default()
        },
    }))
}

/// Clear the selection's blockers, so a dependency entered by mistake is not
/// permanent.
fn clear_blockers(state: &mut ViewState) -> Outcome {
    let ids = state.operand_ids();
    if ids.is_empty() {
        return no_selection(state);
    }
    let label = state.operand_label();
    let cmds: Vec<Command> = ids
        .iter()
        .map(|id| Command::UpdateTask {
            id: *id,
            patch: TaskPatch {
                blocked_by: Some(Vec::new()),
                ..Default::default()
            },
        })
        .collect();
    finish_operator(state, "unblocked", &label);
    Outcome::submit_all(cmds)
}

/// Tab on the `:` line: commit the longest unambiguous completion, and name
/// the alternatives when more than one remains.
///
/// Shell behaviour, deliberately: completing to the common prefix makes
/// repeated Tab presses converge, and listing the candidates is what turns the
/// command line from something you must already know into something you can
/// discover.
fn complete_command_line(state: &mut ViewState) {
    let line = state.input.text().to_string();
    let candidates = crate::command::complete(&line);
    if candidates.is_empty() {
        state.status = "no completion".into();
        return;
    }
    let prefix = crate::command::common_prefix(&candidates);
    // The word being completed is whatever follows the last space.
    let head_len = line.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let typed = &line[head_len..];
    if prefix.len() > typed.len() {
        let mut next = line[..head_len].to_string();
        next.push_str(&prefix);
        // A single candidate is settled: add the space that starts the next
        // word, so `:view` + Tab + Tab reaches the view list.
        if candidates.len() == 1 {
            next.push(' ');
        }
        state.input.set(next);
    }
    state.status = if candidates.len() == 1 {
        String::new()
    } else {
        candidates.join("  ")
    };
}

/// What `L` reports on: the sidebar row if the Browse sidebar has the
/// keyboard, otherwise the task under the cursor.
///
/// Contexts have no feed of their own — the op log records what happened to
/// Tasks and Streams — so a context row falls through to saying so rather than
/// opening an empty overlay.
fn activity_target(state: &ViewState) -> Option<(EntityRef, String)> {
    if let Some((row, name)) = state.sidebar_row() {
        return match row {
            SidebarRow::Stream(id) => Some((id, name)),
            SidebarRow::Context(_) => None,
        };
    }
    state
        .selected_task()
        .or(state.focused_task.as_ref())
        .map(|t| (t.id, t.title.clone()))
}

/// Save a snapshot of the review currently on screen.
///
/// The draft is built by `WeeklyReview::to_draft`, from the assembled review
/// itself, so the saved counts are the ones the user just read
/// (`docs/08-features/reviews-and-stats.md` step 5). Recomputing them here
/// would let a snapshot disagree with the screen it was saved from — which is
/// exactly the thing a review artefact must never do.
fn save_review(state: &mut ViewState) -> Outcome {
    if state.review.pane != crate::ReviewPane::Weekly {
        state.status = "Tab to the Weekly panel to save a review".into();
        return Outcome::None;
    }
    let Some(weekly) = state.review.weekly.as_ref() else {
        state.status = "the review has not loaded yet".into();
        return Outcome::None;
    };
    let draft = weekly.to_draft(None);
    state.status = format!(
        "review saved — {} completed, {} deferred, {} dropped",
        draft.totals.completed, draft.totals.deferred, draft.totals.dropped
    );
    Outcome::Submit(Box::new(Command::SaveReviewSnapshot(draft)))
}

/// Shared "no routine is selected" response.
fn no_routine(state: &mut ViewState) -> Outcome {
    state.status = "no routine selected".into();
    Outcome::None
}

/// Skip the selected routine's next occurrence.
///
/// The occurrence key is the local `YYYY-MM-DDTHH:MM` the engine mints
/// occurrences under, so it is derived from the projected `next` rather than
/// invented here — a key that disagrees with the generator would silently skip
/// nothing.
fn skip_occurrence(state: &mut ViewState) -> Outcome {
    let Some(row) = state.selected_routine_row() else {
        return no_routine(state);
    };
    let (id, title) = (row.id, row.title.clone());
    let Some(next) = row.next else {
        state.status = format!("{title} has no upcoming occurrence to skip");
        return Outcome::None;
    };
    let key = occurrence_key(next, &state.tz);
    state.status = format!("skipped {title} on {key}");
    Outcome::Submit(Box::new(Command::SkipRoutineOccurrence {
        id,
        occurrence_key: key,
    }))
}

/// `YYYY-MM-DDTHH:MM` in the routine's zone — the engine's occurrence key.
fn occurrence_key(at: jiff::Timestamp, tz: &jiff::tz::TimeZone) -> String {
    let dt = at.to_zoned(tz.clone()).datetime();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute()
    )
}

/// Turn a `<capture line> | <recurrence>` routine line into a `CreateRoutine`.
///
/// The two halves are split explicitly rather than sniffed apart: a title can
/// contain the word "every", and guessing where the recurrence starts would
/// mean a routine whose name is silently truncated. The left half is the
/// **same** capture parser every other prompt uses, so `#stream`, `@ctx`,
/// `!priority` and `~duration` mean what they always mean, and `^when` sets
/// the series anchor rather than one task's schedule.
fn create_routine_outcome(state: &mut ViewState, text: &str, now_ms: u64) -> Outcome {
    let Some((head, tail)) = text.split_once('|') else {
        state.prompt = Some(Prompt::CreateRoutine);
        state.status =
            "routine: put the recurrence after a | (e.g. water plants | every day)".into();
        return Outcome::None;
    };
    let rule = match crate::recur::parse_recurrence(tail) {
        Ok(r) => r,
        Err(e) => {
            state.prompt = Some(Prompt::CreateRoutine);
            state.status = format!("routine: {e}");
            return Outcome::None;
        }
    };
    let parsed = parse_line(head, &state.streams, &state.contexts, now_ms, &state.tz);
    if parsed.draft.title.trim().is_empty() {
        state.prompt = Some(Prompt::CreateRoutine);
        state.status = "routine: a title is required before the |".into();
        return Outcome::None;
    }
    let title = parsed.draft.title.clone();
    // `^when` anchors the series; with none given the series starts now, which
    // is what "every day, starting today" means.
    let starts_at = parsed.draft.scheduled_at.unwrap_or_else(|| now_ts(now_ms));
    let draft = RoutineDraft {
        template: TaskTemplate {
            title: parsed.draft.title,
            stream_id: parsed
                .draft
                .stream_id
                .unwrap_or_else(sunrise_domain::inbox_stream_ref),
            contexts: parsed.draft.contexts,
            energy: parsed.draft.energy,
            priority: parsed.draft.priority,
            estimated_duration_s: parsed.draft.estimated_duration_s,
            body: None,
        },
        rrule: rule,
        timezone: state.tz.iana_name().unwrap_or("UTC").to_string(),
        starts_at,
        ends_at: None,
        scheduling_constraints: Vec::new(),
        catchup_policy: RoutineCatchupPolicy::Skip,
    };
    state.reset_to_normal();
    state.status = match unresolved_note(&parsed.unresolved) {
        Some(note) => note,
        None => format!(
            "routine \"{title}\" — {}",
            crate::rrule_summary(&draft.rrule)
        ),
    };
    Outcome::Submit(Box::new(Command::CreateRoutine(draft)))
}

/// Archive or unarchive the Browse sidebar row under the cursor.
///
/// Archiving is the non-destructive counterpart of `D`, and the spec leans on
/// it: an archived Stream or Context stays on the tasks that carry it and only
/// drops out of pickers and `@name` resolution
/// (`docs/02-domain/contexts-and-tags.md`). Without a key for it the only way
/// to retire a finished project was to delete it.
fn toggle_archive(state: &mut ViewState) -> Outcome {
    let Some((row, name)) = state.sidebar_row() else {
        state.status = "archive applies to a stream or context (Browse sidebar)".into();
        return Outcome::None;
    };
    let archived = state.sidebar_row_archived();
    let verb = if archived { "unarchived" } else { "archived" };
    state.status = format!("{verb} {name}");
    Outcome::Submit(Box::new(match row {
        SidebarRow::Stream(id) => Command::UpdateStream {
            id,
            patch: StreamPatch {
                archived: Some(!archived),
                ..Default::default()
            },
        },
        SidebarRow::Context(id) => Command::UpdateContext {
            id,
            patch: ContextPatch {
                archived: Some(!archived),
                ..Default::default()
            },
        },
    }))
}

/// Pause or resume the selected Stream.
///
/// A paused Stream keeps its tasks and stops competing for attention
/// (`docs/02-domain/streams.md`); the field was already projected and had no
/// way to be set from any client.
fn toggle_pause(state: &mut ViewState) -> Outcome {
    match state.sidebar_row() {
        Some((SidebarRow::Stream(id), name)) => {
            let paused = state.paused_stream_selected();
            let verb = if paused { "resumed" } else { "paused" };
            state.status = format!("{verb} {name}");
            Outcome::Submit(Box::new(Command::UpdateStream {
                id,
                patch: StreamPatch {
                    paused: Some(!paused),
                    // Resuming clears any expiry; a stream cannot be both
                    // running and scheduled to stop being paused.
                    paused_until: Some(None),
                    ..Default::default()
                },
            }))
        }
        Some((SidebarRow::Context(_), _)) => {
            state.status = "contexts cannot be paused — a is archive".into();
            Outcome::None
        }
        None => pause_routine(state),
    }
}

/// Pause or resume the selected Routine (the Routines view's use of `p`).
fn pause_routine(state: &mut ViewState) -> Outcome {
    let Some(row) = state.selected_routine_row() else {
        state.status = "pause applies to a stream or a routine".into();
        return Outcome::None;
    };
    let (id, paused, title) = (row.id, row.paused, row.title.clone());
    let verb = if paused { "resumed" } else { "paused" };
    state.status = format!("{verb} {title}");
    Outcome::Submit(Box::new(Command::UpdateRoutine {
        id,
        patch: RoutinePatch {
            paused: Some(!paused),
            paused_until: Some(None),
            ..Default::default()
        },
    }))
}

/// What a cursor move owes the runtime.
///
/// Only one list re-queries when it moves: the Browse sidebar, whose task pane
/// follows the selection live. Everywhere else a cursor move is pure state and
/// the next repaint is enough.
fn after_nav(state: &mut ViewState) -> Outcome {
    if state.view == View::Review {
        return Outcome::None;
    }
    if state.view == View::Stream && state.pane.is_sidebar() && state.sync_browse_from_cursor() {
        return Outcome::Refresh;
    }
    Outcome::None
}

/// Turn a toggle into the commands it means for `ids`.
///
/// `x` is documented as **toggle done**, and until now it only ever completed:
/// pressing it on a finished task re-sent `CompleteTask`, which the core
/// accepts as a no-op, so the key silently did nothing and there was no way at
/// all to re-open a task from the TUI. A user who ticks the wrong row had to
/// leave the app.
///
/// Reopening goes through `TaskPatch.state` rather than a dedicated command
/// because the core has no `ReopenTask`, and `Todo` (not `InProgress`) is the
/// destination: the task is back on the list, and how far along it is is a
/// separate claim the user can make themselves.
///
/// A mixed selection resolves in the direction of the majority verb rather
/// than flipping each row independently: `x` over a run that is half done
/// should finish the run, not invert it into a half-open one.
fn toggle_cmds(state: &ViewState, ids: &[EntityRef]) -> (Vec<Command>, &'static str) {
    use sunrise_domain::TaskState;
    let is_done = |id: &EntityRef| {
        state
            .tasks
            .iter()
            .find(|t| t.id == *id)
            .or(state.focused_task.as_ref().filter(|t| t.id == *id))
            .is_some_and(|t| t.state == TaskState::Done)
    };
    let done = ids.iter().filter(|id| is_done(id)).count();
    if done * 2 > ids.len() {
        let cmds = ids
            .iter()
            .map(|id| Command::UpdateTask {
                id: *id,
                patch: TaskPatch {
                    state: Some(TaskState::Todo),
                    ..Default::default()
                },
            })
            .collect();
        (cmds, "reopened")
    } else {
        (
            ids.iter().copied().map(Command::CompleteTask).collect(),
            "completed",
        )
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
    if let Some(Prompt::Annotate(_)) = state.prompt.as_ref() {
        refresh_annotate_preview(state, now_ms);
        return;
    }
    if state.prompt != Some(Prompt::Capture) {
        state.capture_preview = None;
        return;
    }
    let text = state.input.trimmed();
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

/// Re-describe the annotate buffer under the input line.
///
/// An annotate line changes *existing* data, often several tasks at once, so
/// the stakes of a mistyped token are higher than in capture: the preview says
/// what will change before Enter commits it.
fn refresh_annotate_preview(state: &mut ViewState, now_ms: u64) {
    let text = state.input.trimmed();
    if text.is_empty() {
        state.capture_preview = None;
        return;
    }
    let edit = parse_edit(text, &state.streams, &state.contexts, now_ms, &state.tz);
    let mut line = edit.preview(&state.streams, &state.contexts, &state.tz);
    if let Some(note) = edit.error_note() {
        line.push_str("  ");
        line.push_str(&note);
    }
    state.capture_preview = Some(line);
}

/// Turn an annotate line into the commands it means for `ids`.
///
/// A stream move is `PromoteToStream` and everything else is one `UpdateTask`
/// per task; both can appear in the same line, and the move goes first so the
/// patch lands on the task where it ends up.
fn annotate_outcome(state: &mut ViewState, ids: &[EntityRef], text: &str, now_ms: u64) -> Outcome {
    let edit = parse_edit(text, &state.streams, &state.contexts, now_ms, &state.tz);
    if edit.is_empty() {
        // Nothing applied — keep the prompt open rather than reporting a
        // successful edit that changed nothing.
        state.prompt = Some(Prompt::Annotate(ids.to_vec()));
        state.status = edit
            .error_note()
            .unwrap_or_else(|| "annotate: nothing to change".into());
        return Outcome::None;
    }
    let label = state.operand_label();
    let mut cmds: Vec<Command> = Vec::new();
    for id in ids {
        // A task that has scrolled out of the list still has a patch to
        // receive — but the context arithmetic needs its current set, so a
        // task we cannot see is patched without touching contexts.
        let task = state
            .tasks
            .iter()
            .find(|t| t.id == *id)
            .or(state.focused_task.as_ref().filter(|t| t.id == *id));
        if let Some(stream) = edit.stream() {
            cmds.push(Command::PromoteToStream { id: *id, stream });
        }
        let current: Vec<EntityRef> = task
            .map(|t| t.contexts.iter().copied().collect())
            .unwrap_or_default();
        let patch = edit.patch_for(&current);
        if !patch_is_empty(&patch) {
            cmds.push(Command::UpdateTask { id: *id, patch });
        }
    }
    state.reset_to_normal();
    finish_operator(state, "annotated", &label);
    if let Some(note) = edit.error_note() {
        state.status = note;
    }
    Outcome::submit_all(cmds)
}

/// Whether a patch would change nothing (a move-only annotate line).
fn patch_is_empty(p: &TaskPatch) -> bool {
    p.title.is_none()
        && p.body.is_none()
        && p.stream_id.is_none()
        && p.contexts.is_none()
        && p.state.is_none()
        && p.priority.is_none()
        && p.energy.is_none()
        && p.estimated_duration_s.is_none()
        && p.scheduled_at.is_none()
        && p.due_at.is_none()
        && p.scheduling_constraints.is_none()
        && p.blocked_by.is_none()
        && p.assignee.is_none()
        && p.archived.is_none()
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
    // A mark set is spent by the operator that reads it. Leaving it standing
    // would make the *next* keypress a second bulk operation over rows the
    // user has stopped thinking about.
    state.clear_marks();
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
        Mode::Normal | Mode::Visual | Mode::Triage | Mode::Focus | Mode::Interrupt | Mode::Goto => {
            Outcome::None
        }
    }
}

/// `:`-line submit: parse and apply, mapping the pure [`AppEffect`] onto an
/// [`Outcome`].
fn submit_command_line(state: &mut ViewState, now_ms: u64) -> Outcome {
    let line = state.input.text().to_string();
    let cmd = parse_command(&line);
    state.push_history(&line);
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
        Some(AppEffect::Export {
            dataset,
            format,
            path,
        }) => Outcome::Export {
            dataset,
            format,
            path,
        },
        None => Outcome::Refresh,
    }
}

/// `y` on a confirmation prompt. The only confirmable action in v1 is delete.
fn submit_confirm(state: &mut ViewState) -> Outcome {
    let prompt = state.prompt.take();
    let triage = state.triage;
    state.reset_to_normal();
    match prompt {
        Some(Prompt::ConfirmDelete { target, title }) => {
            state.clear_marks();
            state.status = format!("deleted \"{title}\"");
            match target {
                DeleteTarget::Tasks(ids) => {
                    if triage {
                        // A deleted task leaves the Inbox: the list closes up
                        // under the cursor, so holding the index is "next".
                        state.triage_advance(true);
                    }
                    Outcome::submit_all(ids.into_iter().map(Command::DeleteTask).collect())
                }
                DeleteTarget::Stream(id) => {
                    // The task pane was showing this stream's tasks.
                    state.browse = None;
                    Outcome::Submit(Box::new(Command::DeleteStream(id)))
                }
                DeleteTarget::Context(id) => {
                    state.browse = None;
                    Outcome::Submit(Box::new(Command::DeleteContext(id)))
                }
                DeleteTarget::Routine(id) => Outcome::Submit(Box::new(Command::DeleteRoutine(id))),
            }
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
    state.clear_marks();
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
    let text = state.input.trimmed().to_string();
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
        Some(Prompt::Rename(row)) => {
            if text.is_empty() {
                state.prompt = Some(Prompt::Rename(row));
                state.status = "name cannot be empty".into();
                return Outcome::None;
            }
            state.reset_to_normal();
            state.status = format!("renamed to {text}");
            Outcome::Submit(Box::new(match row {
                SidebarRow::Stream(id) => Command::UpdateStream {
                    id,
                    patch: StreamPatch {
                        name: Some(text),
                        ..Default::default()
                    },
                },
                SidebarRow::Context(id) => Command::UpdateContext {
                    id,
                    patch: ContextPatch {
                        name: Some(text),
                        ..Default::default()
                    },
                },
            }))
        }
        Some(Prompt::CreateRoutine) => {
            if text.is_empty() {
                state.reset_to_normal();
                return Outcome::None;
            }
            create_routine_outcome(state, &text, now_ms)
        }
        Some(Prompt::EditRecurrence(id)) => match crate::recur::parse_recurrence(&text) {
            Ok(rule) => {
                state.reset_to_normal();
                state.status = format!("recurrence: {}", crate::rrule_summary(&rule));
                Outcome::Submit(Box::new(Command::UpdateRoutine {
                    id,
                    patch: RoutinePatch {
                        rrule: Some(rule),
                        ..Default::default()
                    },
                }))
            }
            Err(e) => {
                state.prompt = Some(Prompt::EditRecurrence(id));
                state.status = format!("recurrence: {e}");
                Outcome::None
            }
        },
        Some(Prompt::RenameRoutine(id)) => {
            if text.is_empty() {
                state.prompt = Some(Prompt::RenameRoutine(id));
                state.status = "a routine needs a title".into();
                return Outcome::None;
            }
            // `RoutinePatch.template` replaces the whole template, so the
            // untouched fields have to be carried over explicitly — patching
            // only the title would silently drop the stream and the contexts.
            let Some(mut template) = state
                .selected_routine_row()
                .filter(|r| r.id == id)
                .map(|r| r.template.clone())
            else {
                state.reset_to_normal();
                return no_routine(state);
            };
            template.title.clone_from(&text);
            state.reset_to_normal();
            state.status = format!("renamed routine to {text}");
            Outcome::Submit(Box::new(Command::UpdateRoutine {
                id,
                patch: RoutinePatch {
                    template: Some(template),
                    ..Default::default()
                },
            }))
        }
        Some(Prompt::CreateContext) => {
            if text.is_empty() {
                state.prompt = Some(Prompt::CreateContext);
                state.status = "context name cannot be empty".into();
                return Outcome::None;
            }
            state.reset_to_normal();
            state.status = format!("created @{text}");
            Outcome::Submit(Box::new(Command::CreateContext(ContextDraft {
                name: text,
                description: None,
            })))
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
        Some(Prompt::Annotate(ids)) => {
            if text.is_empty() {
                state.reset_to_normal();
                return Outcome::None;
            }
            annotate_outcome(state, &ids, &text, now_ms)
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
    use crate::view::fixtures::{context_row, fake_task, inbox_row, routine_row, stream_row};
    use crate::view::StreamPane;
    use crossterm::event::KeyCode;

    /// Fixed "now" for deferral arithmetic: 2026-01-01T00:00:00Z.
    const NOW_MS: u64 = 1_767_225_600_000;
    const MIN_MS: u64 = 60 * 1000;

    /// Press a key: run it through the real keymap, then the reducer. Panics
    /// if the key has no binding, so a test that presses an unbound key fails
    /// loudly instead of silently asserting nothing.
    fn press(state: &mut ViewState, key: KeyCode) -> Outcome {
        press_mod(state, key, crossterm::event::KeyModifiers::NONE)
    }

    /// Press a chord (a key with modifiers held), mirroring the binary's own
    /// dispatch — including that an unbound key abandons a half-typed `g`.
    fn press_mod(
        state: &mut ViewState,
        key: KeyCode,
        mods: crossterm::event::KeyModifiers,
    ) -> Outcome {
        let action =
            state
                .keymap
                .dispatch(key, mods, state.dispatch_mode(), state.vim_mode, state.view);
        match action {
            Some(a) => apply_action(a, state, NOW_MS),
            None if state.cancel_chord() => Outcome::None,
            None => panic!(
                "no binding for {mods:?}+{key:?} in {:?}",
                state.dispatch_mode()
            ),
        }
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
    fn the_caret_can_go_back_and_fix_a_typo_mid_line() {
        // The whole point of the editable line: a mistake three words back
        // used to cost the entire capture.
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "buy milkk today");
        for _ in 0..6 {
            let _ = press(&mut s, KeyCode::Left);
        }
        let _ = press(&mut s, KeyCode::Backspace);
        assert_eq!(s.input, "buy milk today");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateTask(d) => assert_eq!(d.title, "buy milk today"),
                other => panic!("expected CreateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn readline_chords_edit_the_prompt() {
        use crossterm::event::KeyModifiers as M;
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "buy milk today");
        let _ = press_mod(&mut s, KeyCode::Char('w'), M::CONTROL);
        assert_eq!(s.input, "buy milk ");
        let _ = press_mod(&mut s, KeyCode::Char('a'), M::CONTROL);
        assert_eq!(s.input.cursor(), 0);
        let _ = press_mod(&mut s, KeyCode::Char('k'), M::CONTROL);
        assert!(s.input.is_empty());
    }

    #[test]
    fn the_capture_preview_follows_an_edit_made_behind_the_caret() {
        // The preview is the user's only confirmation that `#stream` resolved;
        // it must re-parse on a mid-line edit, not only on an append.
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "milk !2 later");
        assert!(s.capture_preview.as_deref().unwrap().contains("!2"));
        // Walk back onto the priority digit and raise it.
        for _ in 0..6 {
            let _ = press(&mut s, KeyCode::Left);
        }
        let _ = press(&mut s, KeyCode::Backspace);
        let _ = press(&mut s, KeyCode::Char('1'));
        let preview = s.capture_preview.as_deref().unwrap();
        assert!(preview.contains("!1"), "{preview}");
        assert!(!preview.contains("!2"), "{preview}");
    }

    #[test]
    fn a_paste_lands_at_the_caret_as_one_edit() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('c'));
        type_text(&mut s, "read ");
        let _ = apply_action(
            Action::InsertStr(
                "the
manual"
                    .into(),
            ),
            &mut s,
            NOW_MS,
        );
        assert_eq!(s.input, "read themanual");
    }

    #[test]
    fn an_over_long_paste_is_refused_whole_rather_than_truncated() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('c'));
        let huge = "x".repeat(MAX_INPUT + 1);
        let _ = apply_action(Action::InsertStr(huge), &mut s, NOW_MS);
        assert!(s.input.is_empty());
        assert!(s.status.contains("too long"));
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
    fn space_marks_rows_and_operators_apply_to_the_marked_set() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let first = s.tasks[0].id;
        let third = s.tasks[2].id;
        // Space marks and advances, so marking row 0 then row 2 is
        // Space, j, Space.
        let _ = press(&mut s, KeyCode::Char(' '));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char(' '));
        assert_eq!(s.marked_ids(), vec![first, third]);
        assert!(s.status.contains("2 marked"));
        // The cursor has moved on; the operator still means the marks.
        match press(&mut s, KeyCode::Char('x')) {
            Outcome::SubmitMany(cmds) => {
                assert_eq!(cmds.len(), 2);
                assert!(matches!(cmds[0], Command::CompleteTask(id) if id == first));
                assert!(matches!(cmds[1], Command::CompleteTask(id) if id == third));
            }
            other => panic!("expected SubmitMany, got {other:?}"),
        }
        assert!(s.marked.is_empty(), "an operator spends the mark set");
    }

    #[test]
    fn space_on_a_marked_row_unmarks_it() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        assert!(s.marked_ids().is_empty());
    }

    #[test]
    fn page_keys_move_by_a_screenful_and_clamp_at_the_ends() {
        use crossterm::event::KeyModifiers as M;
        let mut s = ViewState::default();
        s.view = View::Inbox;
        s.tasks = (0u8..30).map(fake_task).collect();
        s.after_tasks_loaded();
        s.selected = Some(0);
        s.viewport_rows = 10;

        let _ = press(&mut s, KeyCode::PageDown);
        assert_eq!(s.selected, Some(10));
        let _ = press_mod(&mut s, KeyCode::Char('d'), M::CONTROL);
        assert_eq!(s.selected, Some(15));
        let _ = press_mod(&mut s, KeyCode::Char('u'), M::CONTROL);
        assert_eq!(s.selected, Some(10));
        let _ = press(&mut s, KeyCode::PageUp);
        assert_eq!(s.selected, Some(0));
        // Clamps rather than wrapping: a page jump that silently teleported to
        // the far end would lose the user's place with nothing to say so.
        let _ = press(&mut s, KeyCode::PageUp);
        assert_eq!(s.selected, Some(0));
        let _ = press(&mut s, KeyCode::End);
        assert_eq!(s.selected, Some(29));
        let _ = press(&mut s, KeyCode::PageDown);
        assert_eq!(s.selected, Some(29));
        let _ = press(&mut s, KeyCode::Home);
        assert_eq!(s.selected, Some(0));
    }

    #[test]
    fn a_page_is_half_a_row_at_worst_never_zero() {
        // A one-row viewport must still move the cursor: `^D` that does
        // nothing reads as a broken key.
        let mut s = inbox_state();
        s.selected = Some(0);
        s.viewport_rows = 1;
        assert_eq!(s.half_page_rows(), 1);
        let _ = apply_action(Action::HalfPageDown, &mut s, NOW_MS);
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn esc_in_normal_mode_does_not_quit_the_app() {
        // Reflex-pressing Esc must not tear the client down; `q` is the
        // deliberate gesture.
        let mut s = inbox_state();
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert!(s.status.contains("q to quit"));
        assert!(matches!(press(&mut s, KeyCode::Char('q')), Outcome::Quit));
    }

    #[test]
    fn esc_drops_the_marks_before_anything_else() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        assert!(matches!(press(&mut s, KeyCode::Esc), Outcome::None));
        assert!(s.marked.is_empty());
    }

    #[test]
    fn ctrl_space_drops_every_mark() {
        use crossterm::event::KeyModifiers as M;
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        let _ = press_mod(&mut s, KeyCode::Char(' '), M::CONTROL);
        assert!(s.marked.is_empty());
        assert!(s.status.contains("cleared"));
    }

    #[test]
    fn marks_do_not_follow_the_user_into_another_view() {
        // An operator in the Inbox must never silently mean rows ticked in
        // Today, with nothing on screen saying so.
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        assert_eq!(s.marked_ids().len(), 1);
        let _ = press(&mut s, KeyCode::Char('1'));
        assert!(s.marked.is_empty());
    }

    #[test]
    fn a_mark_on_a_row_that_left_the_view_is_not_an_operand() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        let gone = s.tasks.remove(0);
        s.after_tasks_loaded();
        assert!(s.marked_ids().is_empty());
        // …but it is not destroyed: the row coming back restores the mark.
        s.tasks.insert(0, gone);
        s.after_tasks_loaded();
        assert_eq!(s.marked_ids().len(), 1);
    }

    fn browse_state() -> ViewState {
        let mut s = ViewState::default();
        s.view = View::Stream;
        s.streams = vec![inbox_row(1), stream_row(2, "Work", 3)];
        s.contexts = vec![context_row(1, "home"), context_row(2, "errands")];
        s.after_streams_loaded();
        s.after_contexts_loaded();
        s
    }

    #[test]
    fn the_browse_sidebar_reaches_both_axes_of_the_domain() {
        // Streams partition the work and Contexts cut across it; a sidebar
        // with only Streams leaves half the model with no place to stand.
        let mut s = browse_state();
        assert_eq!(s.pane, StreamPane::Streams);
        let _ = press(&mut s, KeyCode::Tab);
        assert_eq!(s.pane, StreamPane::Contexts);
        // Moving in the context list retargets the task pane live, so the
        // sidebar can be browsed without pressing Enter on every row.
        assert!(matches!(
            press(&mut s, KeyCode::Char('j')),
            Outcome::Refresh
        ));
        assert_eq!(
            s.browse_target(),
            Some(crate::BrowseTarget::Context(s.contexts[1].id))
        );
        assert_eq!(s.browse_title(), "@errands");
        // Enter moves focus to the tasks it opened.
        let _ = press(&mut s, KeyCode::Enter);
        assert_eq!(s.pane, StreamPane::Tasks);
    }

    #[test]
    fn moving_in_the_stream_list_retargets_the_task_pane() {
        let mut s = browse_state();
        s.selected_stream = Some(0);
        let _ = press(&mut s, KeyCode::Char('j'));
        assert_eq!(
            s.browse_target(),
            Some(crate::BrowseTarget::Stream(s.streams[1].id))
        );
        assert_eq!(s.browse_title(), "Work");
    }

    #[test]
    fn the_task_cursor_is_dropped_when_the_sidebar_retargets() {
        // Holding row 3 across a switch would leave the highlight on an
        // unrelated task of the new list.
        let mut s = browse_state();
        s.tasks = (0u8..4).map(fake_task).collect();
        s.selected = Some(3);
        s.pane = StreamPane::Streams;
        s.selected_stream = Some(0);
        let _ = press(&mut s, KeyCode::Char('j'));
        assert_eq!(s.selected, None);
    }

    #[test]
    fn visual_mode_is_refused_in_the_sidebar() {
        let mut s = browse_state();
        s.pane = StreamPane::Contexts;
        let _ = press(&mut s, KeyCode::Char('V'));
        assert_ne!(s.mode, Mode::Visual);
    }

    #[test]
    fn the_sidebar_renames_the_row_under_the_cursor_not_a_task() {
        let mut s = browse_state();
        s.tasks = vec![fake_task(7)];
        s.selected = Some(0);
        s.pane = StreamPane::Streams;
        s.selected_stream = Some(1);
        let _ = press(&mut s, KeyCode::Char('e'));
        assert_eq!(s.input, "Work");
        type_text(&mut s, "!");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateStream { id, patch } => {
                    assert_eq!(id, s.streams[1].id);
                    assert_eq!(patch.name.as_deref(), Some("Work!"));
                }
                other => panic!("expected UpdateStream, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn renaming_a_context_drops_the_display_only_at_sign() {
        let mut s = browse_state();
        s.pane = StreamPane::Contexts;
        s.selected_context = Some(0);
        let _ = press(&mut s, KeyCode::Char('e'));
        assert_eq!(s.input, "home", "the @ is sugar, not part of the name");
    }

    #[test]
    fn deleting_a_context_says_what_it_will_do_to_the_tasks() {
        let mut s = browse_state();
        s.pane = StreamPane::Contexts;
        s.selected_context = Some(1);
        let _ = press(&mut s, KeyCode::Char('D'));
        assert_eq!(s.mode, Mode::Confirm);
        assert!(s.status.contains("every task carrying it"), "{}", s.status);
        match press(&mut s, KeyCode::Char('y')) {
            Outcome::Submit(cmd) => {
                assert!(matches!(*cmd, Command::DeleteContext(id) if id == s.contexts[1].id));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn deleting_a_stream_warns_and_needs_the_gate() {
        let mut s = browse_state();
        s.pane = StreamPane::Streams;
        s.selected_stream = Some(1);
        let _ = press(&mut s, KeyCode::Char('D'));
        assert!(s.status.contains("tasks go with it"), "{}", s.status);
        // Anything but `y` cancels, exactly as for a task.
        assert!(matches!(press(&mut s, KeyCode::Char('n')), Outcome::None));
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn archive_is_the_non_destructive_alternative_to_delete() {
        let mut s = browse_state();
        s.pane = StreamPane::Contexts;
        s.selected_context = Some(0);
        match press(&mut s, KeyCode::Char('a')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateContext { patch, .. } => assert_eq!(patch.archived, Some(true)),
                other => panic!("expected UpdateContext, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        // And it flips back.
        s.contexts[0].archived = true;
        match press(&mut s, KeyCode::Char('a')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateContext { patch, .. } => assert_eq!(patch.archived, Some(false)),
                other => panic!("expected UpdateContext, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn pause_applies_to_streams_and_says_so_for_contexts() {
        let mut s = browse_state();
        s.pane = StreamPane::Streams;
        s.selected_stream = Some(1);
        match press(&mut s, KeyCode::Char('p')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateStream { patch, .. } => {
                    assert_eq!(patch.paused, Some(true));
                    assert_eq!(patch.paused_until, Some(None));
                }
                other => panic!("expected UpdateStream, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        s.pane = StreamPane::Contexts;
        assert!(matches!(press(&mut s, KeyCode::Char('p')), Outcome::None));
        assert!(s.status.contains("cannot be paused"));
    }

    #[test]
    fn creating_a_context_is_one_key_and_a_name() {
        let mut s = browse_state();
        let _ = press(&mut s, KeyCode::Char('C'));
        assert_eq!(s.mode, Mode::Insert);
        type_text(&mut s, "errands");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateContext(d) => assert_eq!(d.name, "errands"),
                other => panic!("expected CreateContext, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn p_pauses_a_routine_when_the_routines_view_has_the_cursor() {
        let mut s = ViewState::default();
        s.view = View::Routines;
        s.routines = vec![routine_row(1, "water the plants", "every day", false)];
        s.after_routines_loaded();
        s.selected_routine = Some(0);
        match press(&mut s, KeyCode::Char('p')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateRoutine { patch, .. } => assert_eq!(patch.paused, Some(true)),
                other => panic!("expected UpdateRoutine, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    fn routines_state() -> ViewState {
        let mut s = ViewState::default();
        s.view = View::Routines;
        s.routines = vec![routine_row(1, "water the plants", "every day", false)];
        s.routines[0].next = Some("2026-03-02T09:00:00Z".parse().unwrap());
        s.after_routines_loaded();
        s.selected_routine = Some(0);
        s
    }

    #[test]
    fn a_routine_can_be_created_from_a_capture_line_and_a_cadence() {
        let mut s = ViewState::default();
        s.streams = vec![stream_row(9, "Home", 0)];
        let _ = press(&mut s, KeyCode::Char('R'));
        assert_eq!(s.mode, Mode::Insert);
        type_text(&mut s, "water the plants #home ~10m | every 2 days");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::CreateRoutine(d) => {
                    assert_eq!(d.template.title, "water the plants");
                    assert_eq!(d.template.stream_id, s.streams[0].id);
                    assert_eq!(d.template.estimated_duration_s, Some(600));
                    assert_eq!(d.rrule.interval, 2);
                }
                other => panic!("expected CreateRoutine, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn the_routine_line_needs_both_halves_and_says_which_is_missing() {
        let mut s = ViewState::default();
        let _ = press(&mut s, KeyCode::Char('R'));
        type_text(&mut s, "water the plants every day");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert!(s.status.contains('|'), "{}", s.status);
        assert_eq!(s.mode, Mode::Insert, "the prompt stays open");

        let mut s = ViewState::default();
        let _ = press(&mut s, KeyCode::Char('R'));
        type_text(&mut s, "water the plants | sometimes");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert!(s.status.contains("sometimes"), "{}", s.status);

        let mut s = ViewState::default();
        let _ = press(&mut s, KeyCode::Char('R'));
        type_text(&mut s, " | every day");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert!(s.status.contains("title is required"), "{}", s.status);
    }

    #[test]
    fn e_edits_the_recurrence_in_the_routines_view() {
        let mut s = routines_state();
        let _ = press(&mut s, KeyCode::Char('e'));
        assert_eq!(s.input, "every day");
        let _ = press_mod(
            &mut s,
            KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        type_text(&mut s, "weekdays");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateRoutine { patch, .. } => {
                    assert_eq!(patch.rrule.expect("an rrule").by_day.len(), 5);
                    assert!(patch.template.is_none(), "only the cadence changed");
                }
                other => panic!("expected UpdateRoutine, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn renaming_a_routine_carries_the_rest_of_its_template() {
        // `RoutinePatch.template` replaces the whole template, so a rename
        // that only set the title would silently drop the stream, priority
        // and contexts.
        let mut s = routines_state();
        s.routines[0].template.priority = Some(2);
        let _ = press(&mut s, KeyCode::Char('E'));
        assert_eq!(s.input, "water the plants");
        type_text(&mut s, "!");
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateRoutine { patch, .. } => {
                    let t = patch.template.expect("a template");
                    assert_eq!(t.title, "water the plants!");
                    assert_eq!(t.priority, Some(2));
                }
                other => panic!("expected UpdateRoutine, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn s_skips_the_next_occurrence_with_the_engines_own_key_format() {
        let mut s = routines_state();
        match press(&mut s, KeyCode::Char('s')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::SkipRoutineOccurrence { occurrence_key, .. } => {
                    assert_eq!(occurrence_key, "2026-03-02T09:00");
                }
                other => panic!("expected SkipRoutineOccurrence, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn skipping_a_routine_with_nothing_upcoming_says_so() {
        let mut s = routines_state();
        s.routines[0].next = None;
        assert!(matches!(press(&mut s, KeyCode::Char('s')), Outcome::None));
        assert!(s.status.contains("no upcoming occurrence"), "{}", s.status);
    }

    #[test]
    fn deleting_a_routine_says_the_tasks_it_made_survive() {
        let mut s = routines_state();
        let _ = press(&mut s, KeyCode::Char('D'));
        assert!(s.status.contains("already made stay"), "{}", s.status);
        match press(&mut s, KeyCode::Char('y')) {
            Outcome::Submit(cmd) => {
                assert!(matches!(*cmd, Command::DeleteRoutine(_)));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    fn review_state() -> ViewState {
        let mut s = ViewState::default();
        s.view = View::Review;
        s.viewport_rows = 10;
        s.review.weekly = Some(Box::new(sunrise_domain::WeeklyReview {
            window: sunrise_domain::ReviewWindow {
                start_ms: NOW_MS,
                end_ms: NOW_MS + 7 * 24 * 60 * 60 * 1000,
            },
            streams: Vec::new(),
            inbox: Vec::new(),
            drifting_routines: Vec::new(),
            streaks: Vec::new(),
            slipped: Vec::new(),
            totals: sunrise_domain::ReviewTotals {
                completed: 8,
                deferred: 3,
                dropped: 1,
                created: 5,
                reopened: 0,
            },
            focus: sunrise_domain::FocusStats {
                sessions: 0,
                work_sessions: 0,
                running: 0,
                total_focused_ms: 0,
                interruptions: 0,
                per_stream: Vec::new(),
                per_energy: Vec::new(),
                overall: None,
                top_interruptions: Vec::new(),
            },
            trends: sunrise_domain::Trends {
                week_starts: Vec::new(),
                overall: Vec::new(),
                per_stream: Vec::new(),
            },
        }));
        s
    }

    #[test]
    fn tab_completes_the_command_line_and_names_the_alternatives() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char(':'));
        type_text(&mut s, "ex");
        let _ = press(&mut s, KeyCode::Tab);
        assert_eq!(
            s.input, "export ",
            "a settled completion opens the next word"
        );
        let _ = press(&mut s, KeyCode::Tab);
        // Several candidates: commit the shared prefix (none here) and list.
        assert!(s.status.contains("trends"), "{}", s.status);
        assert!(s.status.contains("streaks"), "{}", s.status);
        type_text(&mut s, "tr");
        let _ = press(&mut s, KeyCode::Tab);
        assert_eq!(s.input, "export trends ");
    }

    #[test]
    fn tab_on_something_uncompletable_says_so_and_types_nothing() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char(':'));
        type_text(&mut s, "capture buy mi");
        let _ = press(&mut s, KeyCode::Tab);
        assert_eq!(s.input, "capture buy mi", "a capture line is free text");
        assert!(s.status.contains("no completion"));
    }

    #[test]
    fn the_command_history_walks_both_ways_and_back_to_a_blank_line() {
        let mut s = inbox_state();
        for line in ["view inbox", "view today"] {
            let _ = press(&mut s, KeyCode::Char(':'));
            type_text(&mut s, line);
            let _ = press(&mut s, KeyCode::Enter);
        }
        let _ = press(&mut s, KeyCode::Char(':'));
        let _ = press(&mut s, KeyCode::Up);
        assert_eq!(s.input, "view today", "newest first");
        let _ = press(&mut s, KeyCode::Up);
        assert_eq!(s.input, "view inbox");
        let _ = press(&mut s, KeyCode::Up);
        assert_eq!(s.input, "view inbox", "the oldest line holds");
        let _ = press(&mut s, KeyCode::Down);
        assert_eq!(s.input, "view today");
        let _ = press(&mut s, KeyCode::Down);
        assert!(
            s.input.is_empty(),
            "walking off the end returns a blank line"
        );
    }

    #[test]
    fn re_running_the_same_command_does_not_cost_two_presses_of_up() {
        let mut s = inbox_state();
        for _ in 0..3 {
            let _ = press(&mut s, KeyCode::Char(':'));
            type_text(&mut s, "view today");
            let _ = press(&mut s, KeyCode::Enter);
        }
        assert_eq!(s.cmd_history, vec!["view today".to_string()]);
    }

    #[test]
    fn marked_tasks_become_the_blockers_of_the_one_under_the_cursor() {
        // The dependency graph feeds the planner's whole ranking, the
        // actionable filter and the unblock cascade — and no client could
        // write it, so every vault's graph was empty and every leverage
        // number zero.
        let mut s = inbox_state();
        s.selected = Some(0);
        let blocker = s.tasks[0].id;
        let _ = press(&mut s, KeyCode::Char(' ')); // mark row 0, advance
        let target = s.tasks[1].id;
        match press(&mut s, KeyCode::Char('b')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { id, patch } => {
                    assert_eq!(id, target);
                    assert_eq!(patch.blocked_by, Some(vec![blocker]));
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(s.marked.is_empty(), "the gesture spends the marks");
    }

    #[test]
    fn b_with_nothing_marked_explains_the_gesture() {
        let mut s = inbox_state();
        assert!(matches!(press(&mut s, KeyCode::Char('b')), Outcome::None));
        assert!(s.status.contains("Space-mark"), "{}", s.status);
    }

    #[test]
    fn a_task_cannot_be_made_to_block_itself() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        s.selected = Some(0);
        assert!(matches!(press(&mut s, KeyCode::Char('b')), Outcome::None));
        assert!(s.status.contains("cannot block itself"), "{}", s.status);
    }

    #[test]
    fn capital_b_clears_the_blockers_again() {
        let mut s = inbox_state();
        match press(&mut s, KeyCode::Char('B')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { patch, .. } => {
                    assert_eq!(patch.blocked_by, Some(vec![]));
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn u_puts_a_completion_back() {
        let mut s = inbox_state();
        let id = selected_id(&s);
        let _ = press(&mut s, KeyCode::Char('x'));
        assert_eq!(s.undo.len(), 1);
        match press(&mut s, KeyCode::Char('u')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { id: got, patch } => {
                    assert_eq!(got, id);
                    assert_eq!(patch.state, Some(sunrise_domain::TaskState::Todo));
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(s.undo.is_empty());
        assert_eq!(s.redo.len(), 1, "and it can be walked forward again");
    }

    #[test]
    fn redo_replays_the_step_u_walked_back() {
        use crossterm::event::KeyModifiers as M;
        let mut s = inbox_state();
        let id = selected_id(&s);
        let _ = press(&mut s, KeyCode::Char('x'));
        let _ = press(&mut s, KeyCode::Char('u'));
        match press_mod(&mut s, KeyCode::Char('r'), M::CONTROL) {
            Outcome::Submit(cmd) => {
                assert!(matches!(*cmd, Command::CompleteTask(got) if got == id));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.undo.len(), 1, "and back onto the undo stack");
    }

    #[test]
    fn undo_restores_every_facet_an_annotate_touched() {
        let mut s = inbox_state();
        let i = s.selected.expect("a selection");
        s.tasks[i].priority = Some(4);
        s.tasks[i].energy = None;
        let _ = press(&mut s, KeyCode::Char('A'));
        type_text(&mut s, "!1 %high");
        let _ = press(&mut s, KeyCode::Enter);
        match press(&mut s, KeyCode::Char('u')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { patch, .. } => {
                    assert_eq!(patch.priority, Some(Some(4)), "the old value, not a clear");
                    assert_eq!(patch.energy, Some(None), "unset is a value too");
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn undo_of_a_bulk_step_is_one_step_not_three() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char('x'));
        assert_eq!(s.undo.len(), 1, "one operator, one step");
        match press(&mut s, KeyCode::Char('u')) {
            Outcome::SubmitMany(cmds) => assert_eq!(cmds.len(), 3),
            other => panic!("expected SubmitMany, got {other:?}"),
        }
    }

    #[test]
    fn a_delete_says_it_cannot_be_undone_rather_than_pretending() {
        // The core writes a tombstone and has no restore op. Saying "nothing
        // to undo" would read as "your delete did not happen".
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('D'));
        let _ = press(&mut s, KeyCode::Char('y'));
        assert!(s.undo.is_empty());
        assert!(matches!(press(&mut s, KeyCode::Char('u')), Outcome::None));
        assert!(s.status.contains("cannot be undone"), "{}", s.status);
    }

    #[test]
    fn a_focus_session_is_a_log_not_an_edit_so_it_records_no_step() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('F'));
        assert!(s.undo.is_empty(), "a session start has nothing to put back");
    }

    #[test]
    fn a_new_change_forgets_the_forward_history() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('x'));
        let _ = press(&mut s, KeyCode::Char('u'));
        assert_eq!(s.redo.len(), 1);
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char('x'));
        assert!(s.redo.is_empty(), "branching history helps nobody");
    }

    #[test]
    fn the_undo_stack_is_bounded() {
        let mut s = inbox_state();
        for _ in 0..(crate::undo::MAX_DEPTH + 10) {
            s.selected = Some(0);
            let _ = press(&mut s, KeyCode::Char('x'));
        }
        assert_eq!(s.undo.len(), crate::undo::MAX_DEPTH);
    }

    #[test]
    fn tab_cycles_the_review_panels_and_reloads_each() {
        let mut s = review_state();
        assert_eq!(s.review.pane, crate::ReviewPane::Weekly);
        // Each panel is a different query, so switching owes a refresh.
        assert!(matches!(press(&mut s, KeyCode::Tab), Outcome::Refresh));
        assert_eq!(s.review.pane, crate::ReviewPane::Daily);
        let _ = press(&mut s, KeyCode::Tab);
        let _ = press(&mut s, KeyCode::Tab);
        assert_eq!(s.review.pane, crate::ReviewPane::History);
        let _ = press(&mut s, KeyCode::Tab);
        assert_eq!(s.review.pane, crate::ReviewPane::Weekly);
    }

    #[test]
    fn brackets_walk_the_reviewed_week() {
        let mut s = review_state();
        let start = NOW_MS;
        assert!(matches!(
            press(&mut s, KeyCode::Char('[')),
            Outcome::Refresh
        ));
        let week = 7 * 24 * 60 * 60 * 1000;
        assert_eq!(s.review.week_start_ms, Some(start - week));
        let _ = press(&mut s, KeyCode::Char(']'));
        assert_eq!(s.review.week_start_ms, Some(start));
    }

    #[test]
    fn enter_saves_a_snapshot_of_the_review_on_screen() {
        // The counts must be the ones the user just read: a snapshot that
        // recomputed them could disagree with the screen it was saved from.
        let mut s = review_state();
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::SaveReviewSnapshot(d) => {
                    assert_eq!(d.totals.completed, 8);
                    assert_eq!(d.totals.deferred, 3);
                    assert_eq!(d.window_start_ms, NOW_MS);
                }
                other => panic!("expected SaveReviewSnapshot, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(s.status.contains("review saved"));
    }

    #[test]
    fn saving_is_refused_from_the_panels_that_are_not_a_review() {
        let mut s = review_state();
        s.review.pane = crate::ReviewPane::Trends;
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert!(s.status.contains("Weekly panel"), "{}", s.status);
    }

    #[test]
    fn the_review_panel_scrolls_and_clamps_to_its_own_content() {
        let mut s = review_state();
        let rows = crate::render::review_rows(&s);
        s.viewport_rows = 2;
        for _ in 0..200 {
            let _ = press(&mut s, KeyCode::Char('j'));
        }
        assert_eq!(s.review.scroll, rows.saturating_sub(2));
        for _ in 0..200 {
            let _ = press(&mut s, KeyCode::Char('k'));
        }
        assert_eq!(s.review.scroll, 0);
    }

    #[test]
    fn movement_scrolls_the_help_overlay_instead_of_dismissing_it() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('?'));
        assert!(s.show_help);
        let _ = press(&mut s, KeyCode::Char('j'));
        assert!(s.show_help, "j must scroll, not dismiss");
        assert_eq!(s.help_scroll, 1);
        let _ = press(&mut s, KeyCode::Char('k'));
        assert_eq!(s.help_scroll, 0);
        // Anything that is not movement still dismisses, as before.
        let _ = press(&mut s, KeyCode::Char('c'));
        assert!(!s.show_help);
        assert_eq!(s.help_scroll, 0);
    }

    #[test]
    fn annotate_reaches_the_facets_no_other_key_could_touch() {
        let mut s = inbox_state();
        s.contexts = vec![crate::view::fixtures::context_row(1, "deep-work")];
        let id = selected_id(&s);
        let _ = press(&mut s, KeyCode::Char('A'));
        assert_eq!(s.mode, Mode::Insert);
        type_text(&mut s, "!1 %high ~45m @deep-work");
        assert!(s.capture_preview.as_deref().unwrap().contains("!1"));
        match press(&mut s, KeyCode::Enter) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { id: got, patch } => {
                    assert_eq!(got, id);
                    assert_eq!(patch.priority, Some(Some(1)));
                    assert_eq!(patch.energy, Some(Some(sunrise_domain::Energy::High)));
                    assert_eq!(patch.estimated_duration_s, Some(Some(45 * 60)));
                    assert_eq!(patch.contexts.as_deref().map(<[_]>::len), Some(1));
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(s.mode, Mode::Normal);
    }

    #[test]
    fn annotate_applies_to_the_whole_marked_set_in_one_prompt() {
        let mut s = inbox_state();
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char(' '));
        let _ = press(&mut s, KeyCode::Char(' '));
        let _ = press(&mut s, KeyCode::Char('A'));
        type_text(&mut s, "!3");
        match press(&mut s, KeyCode::Enter) {
            Outcome::SubmitMany(cmds) => {
                assert_eq!(cmds.len(), 2);
                assert!(cmds.iter().all(|c| matches!(
                    c,
                    Command::UpdateTask { patch, .. } if patch.priority == Some(Some(3))
                )));
            }
            other => panic!("expected SubmitMany, got {other:?}"),
        }
    }

    #[test]
    fn annotate_can_both_move_and_patch_in_one_line() {
        let mut s = inbox_state();
        s.streams = vec![crate::view::fixtures::stream_row(9, "Travel", 0)];
        let _ = press(&mut s, KeyCode::Char('A'));
        type_text(&mut s, "#travel !2");
        match press(&mut s, KeyCode::Enter) {
            Outcome::SubmitMany(cmds) => {
                // The move goes first, so the patch lands where the task ends up.
                assert!(matches!(cmds[0], Command::PromoteToStream { .. }));
                assert!(matches!(cmds[1], Command::UpdateTask { .. }));
            }
            other => panic!("expected SubmitMany, got {other:?}"),
        }
    }

    #[test]
    fn an_annotate_line_that_changes_nothing_keeps_the_prompt_open() {
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('A'));
        type_text(&mut s, "tomorow");
        assert!(matches!(press(&mut s, KeyCode::Enter), Outcome::None));
        assert_eq!(s.mode, Mode::Insert);
        assert!(matches!(s.prompt, Some(Prompt::Annotate(_))));
        assert!(s.status.contains("not an edit token"));
    }

    #[test]
    fn toggle_reopens_a_completed_task() {
        use sunrise_domain::TaskState;
        let mut s = inbox_state();
        let id = selected_id(&s);
        let i = s.selected.expect("a selection");
        s.tasks[i].state = TaskState::Done;
        match press(&mut s, KeyCode::Char('x')) {
            Outcome::Submit(cmd) => match *cmd {
                Command::UpdateTask { id: got, patch } => {
                    assert_eq!(got, id);
                    assert_eq!(patch.state, Some(TaskState::Todo));
                }
                other => panic!("expected UpdateTask, got {other:?}"),
            },
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(s.status.contains("reopened"));
    }

    #[test]
    fn a_mixed_run_finishes_rather_than_inverting() {
        use sunrise_domain::TaskState;
        let mut s = inbox_state();
        s.tasks[0].state = TaskState::Done;
        s.selected = Some(0);
        let _ = press(&mut s, KeyCode::Char('V'));
        let _ = press(&mut s, KeyCode::Char('j'));
        let _ = press(&mut s, KeyCode::Char('j'));
        match press(&mut s, KeyCode::Char('x')) {
            Outcome::SubmitMany(cmds) => {
                assert_eq!(cmds.len(), 3);
                assert!(cmds.iter().all(|c| matches!(c, Command::CompleteTask(_))));
            }
            other => panic!("expected SubmitMany, got {other:?}"),
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
        // A half-typed chord followed by something that is not part of one
        // abandons the chord and does nothing else. Letting the stray key also
        // run its normal action would mean `g` then a typo both cancels and
        // moves, and the user cannot tell which happened.
        let mut s = inbox_state();
        s.selected = Some(2);
        let _ = press(&mut s, KeyCode::Char('g'));
        let _ = press(&mut s, KeyCode::Char('k'));
        assert_eq!(s.selected, Some(2), "the stray key did nothing");
        assert!(!s.pending_g, "and the chord is gone");
        let _ = press(&mut s, KeyCode::Char('g'));
        assert_eq!(s.selected, Some(2), "one g does not jump");
        let _ = press(&mut s, KeyCode::Char('g'));
        assert_eq!(s.selected, Some(0), "two does");
    }

    #[test]
    fn g_chords_jump_between_views() {
        // `docs/08-features/keyboard.md`: Today is `g t`, Inbox is `g i`.
        let mut s = inbox_state();
        for (key, view) in [
            ('t', View::Today),
            ('i', View::Inbox),
            ('s', View::Stream),
            ('/', View::Search),
            ('f', View::Focus),
            ('r', View::Routines),
            ('v', View::Review),
        ] {
            let _ = press(&mut s, KeyCode::Char('g'));
            assert!(matches!(
                press(&mut s, KeyCode::Char(key)),
                Outcome::Refresh
            ));
            assert_eq!(s.view, view, "g{key}");
        }
    }

    #[test]
    fn a_chord_key_keeps_its_own_meaning_outside_the_chord() {
        // `t` is triage and `s` schedules; the chord must not steal either.
        let mut s = inbox_state();
        let _ = press(&mut s, KeyCode::Char('t'));
        assert!(s.triage, "t alone still triages");
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
            routine_row(1, "a", "every day", false),
            routine_row(2, "b", "every day", false),
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
            .dispatch(
                key,
                crossterm::event::KeyModifiers::NONE,
                state.dispatch_mode(),
                state.vim_mode,
                state.view,
            )
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
