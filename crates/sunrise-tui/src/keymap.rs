//! Vim-style keymap for the TUI.
//!
//! Matches `docs/07-clients/tui.md`. v1 covers a small set of
//! navigation/mutation actions; further operators (text-object motions,
//! macros) are out of scope for v1.
//!
//! Every binding lives in exactly one place: the [`BINDINGS`] table.
//! [`dispatch`] resolves a keypress by scanning it in order (first match
//! wins), and the `?` help overlay is generated from the same rows via
//! [`help_sections`] — so the help can never drift from the keymap.
//!
//! User remapping (`~/.config/sunrise/keys.toml`, per `docs/07-clients/tui.md`)
//! is layered on top as a [`Keymap`]: a list of *(row index, replacement key)*
//! overrides against [`BINDINGS`]. Nothing is copied, the table stays the
//! single source of truth, and the help overlay renders the overridden key
//! because it reads through the same [`Keymap`].

use crate::view::View;
use crossterm::event::{KeyCode, KeyModifiers};
use std::path::PathBuf;
use sunrise_domain::InterruptionReason;

/// Mode for the modal editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Default mode: keystrokes map to navigation/commands.
    Normal,
    /// Insert mode: keystrokes feed the active text input (capture, search).
    Insert,
    /// Command-line mode: `:` started a command (`:q`, `:w`, `:focus`).
    Command,
    /// Confirmation prompt: only `y` commits, anything else cancels.
    /// Gates destructive commands (`D` = delete).
    Confirm,
    /// Modal picker: a list overlay (e.g. `m` = move-to-stream) has focus.
    Picker,
    /// Visual mode (`V`): a contiguous run of rows is selected and the next
    /// operator (`x` / `d` / `D` / `m`) applies to all of them.
    Visual,
    /// Triage mode (`t`): the Inbox is presented one task at a time and every
    /// key is a decision (`docs/08-features/inbox-and-capture.md`).
    Triage,
    /// A focus session is running (`docs/08-features/focus-mode.md`). The
    /// session owns the keyboard and offers the few actions the spec names —
    /// complete, defer, capture-aside — plus the break and the interruption
    /// log. Entered and left by the *session*, never by a bare keypress:
    /// [`crate::ViewState::after_focus_loaded`] takes the keyboard when a
    /// running session appears and gives it back when one ends.
    Focus,
    /// The one-tap interruption-reason chooser (`i` during a session).
    Interrupt,
}

impl Mode {
    /// Uppercase label shown in the status line and as the help-overlay
    /// section header.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Command => "CMD",
            Self::Confirm => "CONFIRM",
            Self::Picker => "PICK",
            Self::Visual => "VISUAL",
            Self::Triage => "TRIAGE",
            Self::Focus => "FOCUS",
            Self::Interrupt => "INTERRUPT",
        }
    }
}

/// Discrete actions fired by the keymap. The runtime maps each action
/// onto a [`sunrise_core::Command`] or a view-state mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Quit the app.
    Quit,
    /// Switch to a primary view.
    SwitchView(View),
    /// Cursor down one item.
    Next,
    /// Cursor up one item.
    Prev,
    /// Jump to the first item. Emitted by the second `g` of a `gg` chord.
    GotoTop,
    /// Jump to the last item (`G`).
    GotoBottom,
    /// Cursor down one screenful (`PgDn` / `^F`).
    PageDown,
    /// Cursor up one screenful (`PgUp` / `^B`).
    PageUp,
    /// Cursor down half a screenful (`^D`).
    HalfPageDown,
    /// Cursor up half a screenful (`^U`).
    HalfPageUp,
    /// Mark current task as done.
    Toggle,
    /// Open the capture prompt.
    Capture,
    /// Edit the selected task's title (`e`).
    EditTitle,
    /// Edit the selected task's note body in `$EDITOR` (`E`).
    EditBody,
    /// Annotate the selected task(s) — priority, energy, estimate, due date,
    /// contexts, stream — with the capture sigils (`A`).
    Annotate,
    /// Schedule the selected task (`s`), prompting for a when-expression.
    Schedule,
    /// Enter visual (multi-select) mode (`V`).
    VisualMode,
    /// Toggle the mark on the row under the cursor (`Space`), then step down.
    MarkToggle,
    /// Cycle the Review view's panel (Tab).
    NextReviewPane,
    /// Step the reviewed week back (`[`) or forward (`]`).
    ShiftWeek(i8),
    /// Save a review snapshot for the reviewed week (Enter in the Review view).
    SaveReview,
    /// Drop every mark (`Ctrl-Space` / `:unmark`).
    MarkClear,
    /// Enter Inbox triage mode (`t`).
    Triage,
    /// Triage decision "keep": leave the task alone and advance (`k`).
    TriageKeep,
    /// Defer the selected task (`d`), prompting for an offset.
    Defer,
    /// Delete the selected task (`D`), behind a confirmation prompt.
    Delete,
    /// Move the selected task to another stream (`m`).
    MoveToStream,
    /// Create a stream (`S`).
    CreateStream,
    /// Create a context (`C`).
    CreateContext,
    /// Create a routine (`R`).
    CreateRoutine,
    /// Skip the routine's next occurrence (`s` in the Routines view).
    SkipOccurrence,
    /// Archive or unarchive the selected Browse sidebar row (`a`).
    ToggleArchive,
    /// Pause or resume the selected Stream or Routine (`p`).
    TogglePause,
    /// Show the selected entity's activity feed (`L`).
    ShowActivity,
    /// Toggle the `?` help overlay.
    ToggleHelp,
    /// Begin a search (switches to Insert in the search bar).
    BeginSearch,
    /// Begin a command-line entry (`:` switches to Command mode).
    BeginCommand,
    /// Enter Insert mode.
    EnterInsert,
    /// Toggle which Stream-view pane has focus (Tab).
    TogglePane,
    /// Focus the left (Streams) pane of the Stream view.
    PaneLeft,
    /// Focus the right (Tasks) pane of the Stream view.
    PaneRight,
    /// Activate the highlighted item (Enter in Normal mode): select a
    /// stream in the Streams pane, open Focus on a task elsewhere.
    Activate,
    /// Leave Insert/Command mode back to Normal (or close Focus).
    Escape,
    /// Append a character to the active input (Insert/Command mode).
    InsertChar(char),
    /// Insert a whole string at the caret — a bracketed paste.
    InsertStr(String),
    /// Backspace in the active input.
    Backspace,
    /// Delete the character under the caret (`Delete`).
    DeleteForward,
    /// Move the caret one character left.
    CursorLeft,
    /// Move the caret one character right.
    CursorRight,
    /// Move the caret to the start of the line (`Ctrl-A` / `Home`).
    CursorHome,
    /// Move the caret to the end of the line (`Ctrl-E` / `End`).
    CursorEnd,
    /// Move the caret one word left (`Alt-b`).
    CursorWordLeft,
    /// Move the caret one word right (`Alt-f`).
    CursorWordRight,
    /// Delete the word before the caret (`Ctrl-W`).
    DeleteWordBack,
    /// Delete from the caret to the start of the line (`Ctrl-U`).
    KillToStart,
    /// Delete from the caret to the end of the line (`Ctrl-K`).
    KillToEnd,
    /// Submit the current input (Enter).
    Submit,
    /// First half of a `gg` chord: arm the pending-`g` latch. The runtime
    /// turns the second `g` into [`Action::GotoTop`].
    GotoPrefix,
    /// Start a focus session on the current pick (`F`, or Enter on a planner
    /// row).
    StartFocus,
    /// End the running focus session without completing the task
    /// (`Esc` / `q` while a session runs).
    EndFocus,
    /// Capture a mid-session thought into the **Inbox** without leaving the
    /// session (`a`).
    CaptureAside,
    /// Open the one-tap interruption-reason chooser (`i`).
    Interrupt,
    /// Log an interruption with this reason (one tap in the chooser).
    InterruptReason(InterruptionReason),
    /// End the running work segment and start the break the pomodoro cycle
    /// owes (`b`).
    TakeBreak,
}

impl Action {
    /// Stable snake_case name used by `~/.config/sunrise/keys.toml` to address
    /// this action. Names are part of the config contract: renaming one breaks
    /// every user's key file, so they are chosen once and left alone.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::SwitchView(View::Today) => "view_today",
            Self::SwitchView(View::Inbox) => "view_inbox",
            Self::SwitchView(View::Stream) => "view_stream",
            Self::SwitchView(View::Search) => "view_search",
            Self::SwitchView(View::Focus) => "view_focus",
            Self::SwitchView(View::Routines) => "view_routines",
            Self::SwitchView(View::Review) => "view_review",
            Self::Next => "next",
            Self::Prev => "prev",
            Self::GotoTop => "goto_top",
            Self::GotoBottom => "goto_bottom",
            Self::PageDown => "page_down",
            Self::PageUp => "page_up",
            Self::HalfPageDown => "half_page_down",
            Self::HalfPageUp => "half_page_up",
            Self::GotoPrefix => "goto_prefix",
            Self::Toggle => "toggle",
            Self::Capture => "capture",
            Self::EditTitle => "edit_title",
            Self::EditBody => "edit_body",
            Self::Annotate => "annotate",
            Self::Schedule => "schedule",
            Self::VisualMode => "visual",
            Self::MarkToggle => "mark_toggle",
            Self::NextReviewPane => "next_review_pane",
            Self::ShiftWeek(n) if *n < 0 => "prev_week",
            Self::ShiftWeek(_) => "next_week",
            Self::SaveReview => "save_review",
            Self::MarkClear => "mark_clear",
            Self::Triage => "triage",
            Self::TriageKeep => "triage_keep",
            Self::Defer => "defer",
            Self::Delete => "delete",
            Self::MoveToStream => "move_to_stream",
            Self::CreateStream => "create_stream",
            Self::CreateContext => "create_context",
            Self::CreateRoutine => "create_routine",
            Self::SkipOccurrence => "skip_occurrence",
            Self::ToggleArchive => "toggle_archive",
            Self::TogglePause => "toggle_pause",
            Self::ShowActivity => "activity",
            Self::ToggleHelp => "help",
            Self::BeginSearch => "search",
            Self::BeginCommand => "command",
            Self::EnterInsert => "insert",
            Self::TogglePane => "toggle_pane",
            Self::PaneLeft => "pane_left",
            Self::PaneRight => "pane_right",
            Self::Activate => "activate",
            Self::Escape => "escape",
            Self::InsertChar(_) => "insert_char",
            Self::InsertStr(_) => "insert_str",
            Self::Backspace => "backspace",
            Self::DeleteForward => "delete_forward",
            Self::CursorLeft => "cursor_left",
            Self::CursorRight => "cursor_right",
            Self::CursorHome => "cursor_home",
            Self::CursorEnd => "cursor_end",
            Self::CursorWordLeft => "cursor_word_left",
            Self::CursorWordRight => "cursor_word_right",
            Self::DeleteWordBack => "delete_word_back",
            Self::KillToStart => "kill_to_start",
            Self::KillToEnd => "kill_to_end",
            Self::Submit => "submit",
            Self::StartFocus => "start_focus",
            Self::EndFocus => "end_focus",
            Self::CaptureAside => "capture_aside",
            Self::Interrupt => "interrupt",
            Self::InterruptReason(InterruptionReason::SelfInterrupt) => "interrupt_self",
            Self::InterruptReason(InterruptionReason::Meeting) => "interrupt_meeting",
            Self::InterruptReason(InterruptionReason::Blocked) => "interrupt_blocked",
            Self::InterruptReason(InterruptionReason::Other) => "interrupt_other",
            Self::TakeBreak => "take_break",
        }
    }
}

/// When a [`Binding`] applies, beyond its mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Always.
    Any,
    /// Only when vim-mode is on.
    Vim,
    /// Only in this view.
    In(View),
    /// Only in this view, and only when vim-mode is on.
    InVim(View),
}

impl Scope {
    /// Whether this scope admits the current `vim_mode` / `view` context.
    #[must_use]
    const fn matches(self, vim_mode: bool, view: View) -> bool {
        match self {
            Self::Any => true,
            Self::Vim => vim_mode,
            Self::In(v) => matches_view(v, view),
            Self::InVim(v) => vim_mode && matches_view(v, view),
        }
    }
}

/// `View` is not `const`-comparable via `PartialEq`, so compare discriminants
/// by hand for use inside `const fn`.
const fn matches_view(a: View, b: View) -> bool {
    a as u8 == b as u8
}

/// One row of the keymap: which key, in which mode/scope, produces which
/// action — plus the help text the `?` overlay renders for it.
#[derive(Debug)]
pub struct Binding {
    /// Mode this binding is active in.
    pub mode: Mode,
    /// The key.
    pub key: KeyCode,
    /// Modifiers that must be held. [`KeyModifiers::NONE`] for the ordinary
    /// rows; a chord row (`Ctrl-W`) names its own. Shift is normalised away
    /// before comparison (see [`Keymap::dispatch`]) because an uppercase
    /// `KeyCode::Char('D')` already carries it.
    pub mods: KeyModifiers,
    /// Extra applicability conditions.
    pub scope: Scope,
    /// Action produced.
    pub action: Action,
    /// `Some((keys, description))` to list this row in the help overlay.
    /// `None` hides aliases and duplicates so the overlay stays readable.
    pub help: Option<(&'static str, &'static str)>,
}

/// Shorthand constructor keeping [`BINDINGS`] legible.
const fn b(
    mode: Mode,
    key: KeyCode,
    scope: Scope,
    action: Action,
    help: Option<(&'static str, &'static str)>,
) -> Binding {
    Binding {
        mode,
        key,
        mods: KeyModifiers::NONE,
        scope,
        action,
        help,
    }
}

/// [`b`] for a chord row: the same, with modifiers that must be held.
const fn bm(
    mode: Mode,
    key: KeyCode,
    mods: KeyModifiers,
    scope: Scope,
    action: Action,
    help: Option<(&'static str, &'static str)>,
) -> Binding {
    Binding {
        mode,
        key,
        mods,
        scope,
        action,
        help,
    }
}

/// `Ctrl`, spelled once.
const CTRL: KeyModifiers = KeyModifiers::CONTROL;
/// `Alt`, spelled once.
const ALT: KeyModifiers = KeyModifiers::ALT;

/// The single source of truth for the keymap. Scanned in order by
/// [`dispatch`]; the first row whose mode, key and scope all match wins, so
/// narrower rows (e.g. Esc inside the Focus view) must precede broader ones.
///
/// Help strings are kept short on purpose: the `?` overlay must stay legible
/// inside a two-column layout at the 80x24 minimum terminal size.
pub static BINDINGS: &[Binding] = &[
    // ---- Normal mode ----
    // Esc closes Focus rather than quitting while the Focus view is up.
    b(
        Mode::Normal,
        KeyCode::Esc,
        Scope::In(View::Focus),
        Action::Escape,
        None,
    ),
    // Esc is *not* quit. It reflexively means "back out of whatever this is"
    // to every terminal user alive, and wiring it to teardown means one stray
    // keypress closes the app — a hazard `q` already covers deliberately.
    b(Mode::Normal, KeyCode::Esc, Scope::Any, Action::Escape, None),
    b(
        Mode::Normal,
        KeyCode::Char('q'),
        Scope::Any,
        Action::Quit,
        Some(("q", "quit")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('1'),
        Scope::Any,
        Action::SwitchView(View::Today),
        Some(("1 - 7", "switch view")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('2'),
        Scope::Any,
        Action::SwitchView(View::Inbox),
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Char('3'),
        Scope::Any,
        Action::SwitchView(View::Stream),
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Char('4'),
        Scope::Any,
        Action::SwitchView(View::Search),
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Char('5'),
        Scope::Any,
        Action::SwitchView(View::Focus),
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Char('6'),
        Scope::Any,
        Action::SwitchView(View::Routines),
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Char('7'),
        Scope::Any,
        Action::SwitchView(View::Review),
        None,
    ),
    // ---- Review view ----
    // Scoped rows must precede the general ones they shadow: `dispatch` takes
    // the first match, so a `Scope::Any` Tab or Enter earlier in the table
    // would make these unreachable.
    b(
        Mode::Normal,
        KeyCode::Tab,
        Scope::In(View::Review),
        Action::NextReviewPane,
        Some(("Tab", "weekly/daily/trends")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('['),
        Scope::In(View::Review),
        Action::ShiftWeek(-1),
        Some(("[ / ]", "previous / next week")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char(']'),
        Scope::In(View::Review),
        Action::ShiftWeek(1),
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Enter,
        Scope::In(View::Review),
        Action::SaveReview,
        Some(("Enter", "save this review")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('f'),
        Scope::Any,
        Action::SwitchView(View::Focus),
        Some(("f", "focus selected task")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('F'),
        Scope::Any,
        Action::StartFocus,
        Some(("F", "start a focus session")),
    ),
    b(
        Mode::Normal,
        KeyCode::Tab,
        Scope::In(View::Stream),
        Action::TogglePane,
        Some(("Tab", "cycle browse panes")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('h'),
        Scope::InVim(View::Stream),
        Action::PaneLeft,
        Some(("h / l", "sidebar / tasks")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('l'),
        Scope::InVim(View::Stream),
        Action::PaneRight,
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Left,
        Scope::In(View::Stream),
        Action::PaneLeft,
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Right,
        Scope::In(View::Stream),
        Action::PaneRight,
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Enter,
        Scope::Any,
        Action::Activate,
        Some(("Enter", "open / confirm")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('j'),
        Scope::Vim,
        Action::Next,
        Some(("j / k", "down / up")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('k'),
        Scope::Vim,
        Action::Prev,
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Down,
        Scope::Any,
        Action::Next,
        Some(("↓ / ↑", "down / up")),
    ),
    b(Mode::Normal, KeyCode::Up, Scope::Any, Action::Prev, None),
    b(
        Mode::Normal,
        KeyCode::Char('g'),
        Scope::Any,
        Action::GotoPrefix,
        Some(("gg / G", "top / bottom")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('G'),
        Scope::Any,
        Action::GotoBottom,
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::PageDown,
        Scope::Any,
        Action::PageDown,
        Some(("PgDn / PgUp", "page down / up")),
    ),
    b(
        Mode::Normal,
        KeyCode::PageUp,
        Scope::Any,
        Action::PageUp,
        None,
    ),
    bm(
        Mode::Normal,
        KeyCode::Char('d'),
        CTRL,
        Scope::Any,
        Action::HalfPageDown,
        Some(("^D / ^U", "half page down / up")),
    ),
    bm(
        Mode::Normal,
        KeyCode::Char('u'),
        CTRL,
        Scope::Any,
        Action::HalfPageUp,
        None,
    ),
    bm(
        Mode::Normal,
        KeyCode::Char('f'),
        CTRL,
        Scope::Any,
        Action::PageDown,
        None,
    ),
    bm(
        Mode::Normal,
        KeyCode::Char('b'),
        CTRL,
        Scope::Any,
        Action::PageUp,
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Home,
        Scope::Any,
        Action::GotoTop,
        Some(("Home / End", "first / last row")),
    ),
    b(
        Mode::Normal,
        KeyCode::End,
        Scope::Any,
        Action::GotoBottom,
        None,
    ),
    b(
        Mode::Normal,
        KeyCode::Char('x'),
        Scope::Any,
        Action::Toggle,
        Some(("x", "toggle done / reopen")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char(' '),
        Scope::Any,
        Action::MarkToggle,
        Some(("Space", "mark row (multi-select)")),
    ),
    bm(
        Mode::Normal,
        KeyCode::Char(' '),
        CTRL,
        Scope::Any,
        Action::MarkClear,
        Some(("^Space", "clear marks")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('c'),
        Scope::Any,
        Action::Capture,
        Some(("c", "capture a task")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('e'),
        Scope::Any,
        Action::EditTitle,
        Some(("e", "edit title")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('E'),
        Scope::Any,
        Action::EditBody,
        Some(("E", "edit body in $EDITOR")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('A'),
        Scope::Any,
        Action::Annotate,
        Some(("A", "annotate facets")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('d'),
        Scope::Any,
        Action::Defer,
        Some(("d", "defer (prompts)")),
    ),
    // `s` schedules a task everywhere else; in the Routines view the row under
    // the cursor is a template, and skipping its next occurrence is the
    // scheduling decision that view can actually make.
    b(
        Mode::Normal,
        KeyCode::Char('s'),
        Scope::In(View::Routines),
        Action::SkipOccurrence,
        Some(("s", "skip next occurrence")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('s'),
        Scope::Any,
        Action::Schedule,
        Some(("s", "schedule (prompts)")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('D'),
        Scope::Any,
        Action::Delete,
        Some(("D", "delete (confirms)")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('m'),
        Scope::Any,
        Action::MoveToStream,
        Some(("m", "move to a stream")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('S'),
        Scope::Any,
        Action::CreateStream,
        Some(("S", "create a stream")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('C'),
        Scope::Any,
        Action::CreateContext,
        Some(("C", "create a context")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('R'),
        Scope::Any,
        Action::CreateRoutine,
        Some(("R", "create a routine")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('a'),
        Scope::Any,
        Action::ToggleArchive,
        Some(("a", "archive (sidebar row)")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('p'),
        Scope::Any,
        Action::TogglePause,
        Some(("p", "pause (stream/routine)")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('L'),
        Scope::Any,
        Action::ShowActivity,
        Some(("L", "activity: what happened")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('V'),
        Scope::Any,
        Action::VisualMode,
        Some(("V", "visual select")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('t'),
        Scope::Any,
        Action::Triage,
        Some(("t", "triage the inbox")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('/'),
        Scope::Any,
        Action::BeginSearch,
        Some(("/", "search")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char(':'),
        Scope::Any,
        Action::BeginCommand,
        Some((":", "command line")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('?'),
        Scope::Any,
        Action::ToggleHelp,
        Some(("?", "toggle this help")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('i'),
        Scope::Vim,
        Action::EnterInsert,
        Some(("i", "insert mode")),
    ),
    // ---- Insert mode (capture / edit / defer / stream name) ----
    b(
        Mode::Insert,
        KeyCode::Esc,
        Scope::Any,
        Action::Escape,
        Some(("Esc", "cancel the prompt")),
    ),
    b(
        Mode::Insert,
        KeyCode::Enter,
        Scope::Any,
        Action::Submit,
        Some(("Enter", "commit the prompt")),
    ),
    b(
        Mode::Insert,
        KeyCode::Backspace,
        Scope::Any,
        Action::Backspace,
        Some(("Backspace", "erase a char")),
    ),
    b(
        Mode::Insert,
        KeyCode::Left,
        Scope::Any,
        Action::CursorLeft,
        Some(("← / →", "move the caret")),
    ),
    b(
        Mode::Insert,
        KeyCode::Right,
        Scope::Any,
        Action::CursorRight,
        None,
    ),
    b(
        Mode::Insert,
        KeyCode::Home,
        Scope::Any,
        Action::CursorHome,
        Some(("Home / End", "line start / end")),
    ),
    b(
        Mode::Insert,
        KeyCode::End,
        Scope::Any,
        Action::CursorEnd,
        None,
    ),
    bm(
        Mode::Insert,
        KeyCode::Char('a'),
        CTRL,
        Scope::Any,
        Action::CursorHome,
        Some(("^A / ^E", "line start / end")),
    ),
    bm(
        Mode::Insert,
        KeyCode::Char('e'),
        CTRL,
        Scope::Any,
        Action::CursorEnd,
        None,
    ),
    bm(
        Mode::Insert,
        KeyCode::Char('b'),
        ALT,
        Scope::Any,
        Action::CursorWordLeft,
        Some(("M-b / M-f", "word left / right")),
    ),
    bm(
        Mode::Insert,
        KeyCode::Char('f'),
        ALT,
        Scope::Any,
        Action::CursorWordRight,
        None,
    ),
    b(
        Mode::Insert,
        KeyCode::Delete,
        Scope::Any,
        Action::DeleteForward,
        Some(("Del", "delete under caret")),
    ),
    bm(
        Mode::Insert,
        KeyCode::Char('w'),
        CTRL,
        Scope::Any,
        Action::DeleteWordBack,
        Some(("^W", "delete word back")),
    ),
    bm(
        Mode::Insert,
        KeyCode::Char('u'),
        CTRL,
        Scope::Any,
        Action::KillToStart,
        Some(("^U / ^K", "kill to start / end")),
    ),
    bm(
        Mode::Insert,
        KeyCode::Char('k'),
        CTRL,
        Scope::Any,
        Action::KillToEnd,
        None,
    ),
    // ---- Command mode (`:`) ----
    b(
        Mode::Command,
        KeyCode::Esc,
        Scope::Any,
        Action::Escape,
        Some(("Esc", "abandon the line")),
    ),
    b(
        Mode::Command,
        KeyCode::Enter,
        Scope::Any,
        Action::Submit,
        Some(("Enter", "run the command")),
    ),
    b(
        Mode::Command,
        KeyCode::Backspace,
        Scope::Any,
        Action::Backspace,
        None,
    ),
    b(
        Mode::Command,
        KeyCode::Left,
        Scope::Any,
        Action::CursorLeft,
        Some(("← / →", "move the caret")),
    ),
    b(
        Mode::Command,
        KeyCode::Right,
        Scope::Any,
        Action::CursorRight,
        None,
    ),
    b(
        Mode::Command,
        KeyCode::Home,
        Scope::Any,
        Action::CursorHome,
        Some(("Home / End", "line start / end")),
    ),
    b(
        Mode::Command,
        KeyCode::End,
        Scope::Any,
        Action::CursorEnd,
        None,
    ),
    bm(
        Mode::Command,
        KeyCode::Char('a'),
        CTRL,
        Scope::Any,
        Action::CursorHome,
        Some(("^A / ^E", "line start / end")),
    ),
    bm(
        Mode::Command,
        KeyCode::Char('e'),
        CTRL,
        Scope::Any,
        Action::CursorEnd,
        None,
    ),
    bm(
        Mode::Command,
        KeyCode::Char('b'),
        ALT,
        Scope::Any,
        Action::CursorWordLeft,
        Some(("M-b / M-f", "word left / right")),
    ),
    bm(
        Mode::Command,
        KeyCode::Char('f'),
        ALT,
        Scope::Any,
        Action::CursorWordRight,
        None,
    ),
    b(
        Mode::Command,
        KeyCode::Delete,
        Scope::Any,
        Action::DeleteForward,
        Some(("Del", "delete under caret")),
    ),
    bm(
        Mode::Command,
        KeyCode::Char('w'),
        CTRL,
        Scope::Any,
        Action::DeleteWordBack,
        Some(("^W", "delete word back")),
    ),
    bm(
        Mode::Command,
        KeyCode::Char('u'),
        CTRL,
        Scope::Any,
        Action::KillToStart,
        Some(("^U / ^K", "kill to start / end")),
    ),
    bm(
        Mode::Command,
        KeyCode::Char('k'),
        CTRL,
        Scope::Any,
        Action::KillToEnd,
        None,
    ),
    // ---- Confirmation prompt ----
    b(
        Mode::Confirm,
        KeyCode::Char('y'),
        Scope::Any,
        Action::Submit,
        Some(("y", "confirm")),
    ),
    b(
        Mode::Confirm,
        KeyCode::Char('Y'),
        Scope::Any,
        Action::Submit,
        None,
    ),
    b(
        Mode::Confirm,
        KeyCode::Char('n'),
        Scope::Any,
        Action::Escape,
        Some(("n / Esc", "cancel")),
    ),
    b(
        Mode::Confirm,
        KeyCode::Char('N'),
        Scope::Any,
        Action::Escape,
        None,
    ),
    b(
        Mode::Confirm,
        KeyCode::Esc,
        Scope::Any,
        Action::Escape,
        None,
    ),
    // ---- Modal picker ----
    b(
        Mode::Picker,
        KeyCode::Char('j'),
        Scope::Any,
        Action::Next,
        Some(("j/k/↓/↑", "move the cursor")),
    ),
    b(
        Mode::Picker,
        KeyCode::Char('k'),
        Scope::Any,
        Action::Prev,
        None,
    ),
    b(Mode::Picker, KeyCode::Down, Scope::Any, Action::Next, None),
    b(Mode::Picker, KeyCode::Up, Scope::Any, Action::Prev, None),
    b(
        Mode::Picker,
        KeyCode::PageDown,
        Scope::Any,
        Action::PageDown,
        None,
    ),
    b(
        Mode::Picker,
        KeyCode::PageUp,
        Scope::Any,
        Action::PageUp,
        None,
    ),
    bm(
        Mode::Picker,
        KeyCode::Char('d'),
        CTRL,
        Scope::Any,
        Action::HalfPageDown,
        None,
    ),
    bm(
        Mode::Picker,
        KeyCode::Char('u'),
        CTRL,
        Scope::Any,
        Action::HalfPageUp,
        None,
    ),
    bm(
        Mode::Picker,
        KeyCode::Char('f'),
        CTRL,
        Scope::Any,
        Action::PageDown,
        None,
    ),
    bm(
        Mode::Picker,
        KeyCode::Char('b'),
        CTRL,
        Scope::Any,
        Action::PageUp,
        None,
    ),
    b(
        Mode::Picker,
        KeyCode::Home,
        Scope::Any,
        Action::GotoTop,
        None,
    ),
    b(
        Mode::Picker,
        KeyCode::End,
        Scope::Any,
        Action::GotoBottom,
        None,
    ),
    b(
        Mode::Picker,
        KeyCode::Enter,
        Scope::Any,
        Action::Submit,
        Some(("Enter", "choose")),
    ),
    b(
        Mode::Picker,
        KeyCode::Esc,
        Scope::Any,
        Action::Escape,
        Some(("Esc", "cancel")),
    ),
    // ---- Visual (multi-select) mode ----
    // Movement extends the selection: the anchor stays put while the cursor
    // moves, so the run between them is what the next operator applies to.
    b(
        Mode::Visual,
        KeyCode::Char('j'),
        Scope::Any,
        Action::Next,
        Some(("j/k/↓/↑", "extend the selection")),
    ),
    b(
        Mode::Visual,
        KeyCode::Char('k'),
        Scope::Any,
        Action::Prev,
        None,
    ),
    b(Mode::Visual, KeyCode::Down, Scope::Any, Action::Next, None),
    b(Mode::Visual, KeyCode::Up, Scope::Any, Action::Prev, None),
    b(
        Mode::Visual,
        KeyCode::PageDown,
        Scope::Any,
        Action::PageDown,
        Some(("PgDn / PgUp", "page down / up")),
    ),
    b(
        Mode::Visual,
        KeyCode::PageUp,
        Scope::Any,
        Action::PageUp,
        None,
    ),
    bm(
        Mode::Visual,
        KeyCode::Char('d'),
        CTRL,
        Scope::Any,
        Action::HalfPageDown,
        Some(("^D / ^U", "half page down / up")),
    ),
    bm(
        Mode::Visual,
        KeyCode::Char('u'),
        CTRL,
        Scope::Any,
        Action::HalfPageUp,
        None,
    ),
    bm(
        Mode::Visual,
        KeyCode::Char('f'),
        CTRL,
        Scope::Any,
        Action::PageDown,
        None,
    ),
    bm(
        Mode::Visual,
        KeyCode::Char('b'),
        CTRL,
        Scope::Any,
        Action::PageUp,
        None,
    ),
    b(
        Mode::Visual,
        KeyCode::Home,
        Scope::Any,
        Action::GotoTop,
        Some(("Home / End", "first / last row")),
    ),
    b(
        Mode::Visual,
        KeyCode::End,
        Scope::Any,
        Action::GotoBottom,
        None,
    ),
    b(
        Mode::Visual,
        KeyCode::Char(' '),
        Scope::Any,
        Action::MarkToggle,
        Some(("Space", "mark row (multi-select)")),
    ),
    b(
        Mode::Visual,
        KeyCode::Char('x'),
        Scope::Any,
        Action::Toggle,
        Some(("x", "complete the selection")),
    ),
    b(
        Mode::Visual,
        KeyCode::Char('A'),
        Scope::Any,
        Action::Annotate,
        Some(("A", "annotate the selection")),
    ),
    b(
        Mode::Visual,
        KeyCode::Char('d'),
        Scope::Any,
        Action::Defer,
        Some(("d", "defer the selection")),
    ),
    b(
        Mode::Visual,
        KeyCode::Char('D'),
        Scope::Any,
        Action::Delete,
        Some(("D", "delete (confirms)")),
    ),
    b(
        Mode::Visual,
        KeyCode::Char('m'),
        Scope::Any,
        Action::MoveToStream,
        Some(("m", "move to a stream")),
    ),
    b(
        Mode::Visual,
        KeyCode::Esc,
        Scope::Any,
        Action::Escape,
        Some(("V / Esc", "leave visual mode")),
    ),
    b(
        Mode::Visual,
        KeyCode::Char('V'),
        Scope::Any,
        Action::Escape,
        None,
    ),
    b(
        Mode::Visual,
        KeyCode::Char('?'),
        Scope::Any,
        Action::ToggleHelp,
        Some(("?", "toggle this help")),
    ),
    // ---- Triage mode (one Inbox task at a time) ----
    b(
        Mode::Triage,
        KeyCode::Char('k'),
        Scope::Any,
        Action::TriageKeep,
        Some(("k / Enter", "keep, next task")),
    ),
    b(
        Mode::Triage,
        KeyCode::Enter,
        Scope::Any,
        Action::TriageKeep,
        None,
    ),
    b(
        Mode::Triage,
        KeyCode::Char('p'),
        Scope::Any,
        Action::MoveToStream,
        Some(("p", "promote to a stream")),
    ),
    b(
        Mode::Triage,
        KeyCode::Char('s'),
        Scope::Any,
        Action::Schedule,
        Some(("s", "schedule (prompts)")),
    ),
    b(
        Mode::Triage,
        KeyCode::Char('A'),
        Scope::Any,
        Action::Annotate,
        Some(("A", "annotate this task")),
    ),
    b(
        Mode::Triage,
        KeyCode::Char('d'),
        Scope::Any,
        Action::Defer,
        Some(("d", "defer (prompts)")),
    ),
    b(
        Mode::Triage,
        KeyCode::Char('D'),
        Scope::Any,
        Action::Delete,
        Some(("D", "delete (confirms)")),
    ),
    b(
        Mode::Triage,
        KeyCode::Char('x'),
        Scope::Any,
        Action::Toggle,
        Some(("x", "complete, next task")),
    ),
    b(
        Mode::Triage,
        KeyCode::Esc,
        Scope::Any,
        Action::Escape,
        Some(("Esc / q", "leave triage")),
    ),
    b(
        Mode::Triage,
        KeyCode::Char('q'),
        Scope::Any,
        Action::Escape,
        None,
    ),
    b(
        Mode::Triage,
        KeyCode::Char('?'),
        Scope::Any,
        Action::ToggleHelp,
        Some(("?", "toggle this help")),
    ),
    // ---- Focus mode (a session is running) ----
    // `docs/08-features/focus-mode.md` §Composition: "three actions only —
    // complete, defer, capture-aside", plus the break and the interruption
    // log the same document defines.
    b(
        Mode::Focus,
        KeyCode::Char('x'),
        Scope::Any,
        Action::Toggle,
        Some(("x", "complete, end session")),
    ),
    b(
        Mode::Focus,
        KeyCode::Char('d'),
        Scope::Any,
        Action::Defer,
        Some(("d", "defer (prompts)")),
    ),
    b(
        Mode::Focus,
        KeyCode::Char('a'),
        Scope::Any,
        Action::CaptureAside,
        Some(("a", "capture aside → Inbox")),
    ),
    b(
        Mode::Focus,
        KeyCode::Char('i'),
        Scope::Any,
        Action::Interrupt,
        Some(("i", "log an interruption")),
    ),
    b(
        Mode::Focus,
        KeyCode::Char('b'),
        Scope::Any,
        Action::TakeBreak,
        Some(("b", "end, take the break")),
    ),
    b(
        Mode::Focus,
        KeyCode::Esc,
        Scope::Any,
        Action::EndFocus,
        Some(("Esc / q", "end the session")),
    ),
    b(
        Mode::Focus,
        KeyCode::Char('q'),
        Scope::Any,
        Action::EndFocus,
        None,
    ),
    b(
        Mode::Focus,
        KeyCode::Char(':'),
        Scope::Any,
        Action::BeginCommand,
        Some((":", "command line")),
    ),
    b(
        Mode::Focus,
        KeyCode::Char('?'),
        Scope::Any,
        Action::ToggleHelp,
        Some(("?", "toggle this help")),
    ),
    // ---- Interruption reason chooser (one tap each) ----
    b(
        Mode::Interrupt,
        KeyCode::Char('s'),
        Scope::Any,
        Action::InterruptReason(InterruptionReason::SelfInterrupt),
        Some(("s", "self-interrupt")),
    ),
    b(
        Mode::Interrupt,
        KeyCode::Char('m'),
        Scope::Any,
        Action::InterruptReason(InterruptionReason::Meeting),
        Some(("m", "meeting")),
    ),
    b(
        Mode::Interrupt,
        KeyCode::Char('b'),
        Scope::Any,
        Action::InterruptReason(InterruptionReason::Blocked),
        Some(("b", "blocked")),
    ),
    b(
        Mode::Interrupt,
        KeyCode::Char('o'),
        Scope::Any,
        Action::InterruptReason(InterruptionReason::Other),
        Some(("o", "other")),
    ),
    b(
        Mode::Interrupt,
        KeyCode::Esc,
        Scope::Any,
        Action::Escape,
        Some(("Esc", "cancel")),
    ),
];

/// A resolved keymap: [`BINDINGS`] plus any user overrides loaded from
/// `~/.config/sunrise/keys.toml`.
///
/// An override is stored as *(row index into [`BINDINGS`], replacement key)*
/// rather than as a rewritten table, so the static table stays the one place a
/// binding is declared and the help overlay can tell an overridden row from a
/// default one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    /// Replacement keys, by index into [`BINDINGS`].
    overrides: Vec<(usize, KeyCode)>,
}

impl Keymap {
    /// The key row `i` answers to, after overrides.
    #[must_use]
    fn key_at(&self, i: usize) -> KeyCode {
        self.overrides
            .iter()
            .find(|(j, _)| *j == i)
            .map_or(BINDINGS[i].key, |(_, k)| *k)
    }

    /// Whether row `i` was remapped by the user.
    #[must_use]
    fn is_overridden(&self, i: usize) -> bool {
        self.overrides.iter().any(|(j, _)| *j == i)
    }

    /// [`dispatch`], honouring this keymap's overrides.
    ///
    /// `mods` carries the held modifiers. Shift is masked off before the
    /// comparison: a shifted letter already arrives as an uppercase
    /// `KeyCode::Char`, so requiring `SHIFT` as well would make `D` unreachable
    /// on every terminal that reports it — and matching it loosely would let
    /// `Ctrl-D` fall through to the plain `d` row.
    #[must_use]
    pub fn dispatch(
        &self,
        key: KeyCode,
        mods: KeyModifiers,
        mode: Mode,
        vim_mode: bool,
        view: View,
    ) -> Option<Action> {
        let mods = mods.difference(KeyModifiers::SHIFT);
        for (i, binding) in BINDINGS.iter().enumerate() {
            if binding.mode == mode
                && binding.mods == mods
                && self.key_at(i) == key
                && binding.scope.matches(vim_mode, view)
            {
                return Some(binding.action.clone());
            }
        }
        // Fallback: any other printable character feeds the active text input.
        // This is the one rule that cannot live in the table (it matches every
        // `Char`), so it runs last, after the table's Esc/Enter/Backspace rows.
        // Chords are excluded — an unbound `Ctrl-<x>` must not type an `x`.
        match (mode, key) {
            (Mode::Insert | Mode::Command, KeyCode::Char(c)) if mods.is_empty() => {
                Some(Action::InsertChar(c))
            }
            _ => None,
        }
    }

    /// [`help_sections`], honouring this keymap's overrides: a remapped row is
    /// listed under the key the user actually has to press.
    ///
    /// Filtered to `view` as well as to the mode. `docs/08-features/keyboard.md`
    /// asks for "a contextual cheat sheet", and a view-scoped row shown in the
    /// wrong view is worse than a missing one: the Review view's `[` and `]`
    /// are listed under NORMAL, and a user reading that in the Inbox would
    /// press them and get nothing.
    #[must_use]
    pub fn help_sections(&self, view: View) -> Vec<(&'static str, Vec<(String, &'static str)>)> {
        let mut out: Vec<(&'static str, Vec<(String, &'static str)>)> = Vec::new();
        for (i, binding) in BINDINGS.iter().enumerate() {
            let Some((keys, desc)) = binding.help else {
                continue;
            };
            if !binding.scope.matches(true, view) {
                continue;
            }
            // A remapped row's canned key string ("q / Esc") would be a lie, so
            // it is replaced by the new key's label.
            let keys = if self.is_overridden(i) {
                key_label(self.key_at(i))
            } else {
                keys.to_string()
            };
            let label = binding.mode.label();
            match out.last_mut() {
                Some((existing, rows)) if *existing == label => rows.push((keys, desc)),
                _ => out.push((label, vec![(keys, desc)])),
            }
        }
        out
    }

    /// Build a keymap from `action = "key"` config pairs.
    ///
    /// Each pair rebinds the action's **canonical** Normal-mode row — the one
    /// that carries the help entry, which is the key the overlay advertises and
    /// therefore the one the user means. Alias rows (`Esc` for quit, the arrow
    /// keys for movement) are left alone, so a remap never strips a user of
    /// their arrows or their Esc. Unknown action names and unparseable keys are
    /// reported as warnings and skipped: a bad line costs that one binding, not
    /// the app.
    #[must_use]
    pub fn from_config(pairs: &[(String, String)]) -> (Self, Vec<String>) {
        let mut overrides: Vec<(usize, KeyCode)> = Vec::new();
        let mut warnings = Vec::new();
        for (name, key) in pairs {
            let Some(code) = parse_key(key) else {
                warnings.push(format!("unknown key {key:?} for action {name:?}"));
                continue;
            };
            let canonical = |want_help: bool| {
                BINDINGS.iter().position(|b| {
                    b.mode == Mode::Normal
                        && b.action.name() == name
                        && (!want_help || b.help.is_some())
                })
            };
            match canonical(true).or_else(|| canonical(false)) {
                Some(i) => {
                    overrides.retain(|(j, _)| *j != i);
                    overrides.push((i, code));
                }
                None => warnings.push(format!("unknown action {name:?}")),
            }
        }
        (Self { overrides }, warnings)
    }
}

/// Translate a key event in a given mode (and active view) into an
/// [`Action`], by scanning [`BINDINGS`] in order.
///
/// `vim_mode = false` collapses normal-mode bindings down to a friendlier
/// "always insert" feel: `j/k` produce InsertChar instead of Next/Prev.
/// `view` disambiguates the few view-local bindings: Tab / `h` / `l`
/// switch panes only in the Stream view, and Esc closes the Focus view
/// instead of quitting.
#[must_use]
pub fn dispatch(key: KeyCode, mode: Mode, vim_mode: bool, view: View) -> Option<Action> {
    Keymap::default().dispatch(key, KeyModifiers::NONE, mode, vim_mode, view)
}

/// Help-overlay content, derived from [`BINDINGS`]: one section per mode (in
/// table order), each holding the `(keys, description)` pairs of the rows that
/// opted into help.
#[must_use]
pub fn help_sections(view: View) -> Vec<(&'static str, Vec<(String, &'static str)>)> {
    Keymap::default().help_sections(view)
}

/// Human label for a key, used by the help overlay for remapped rows.
#[must_use]
pub fn key_label(key: KeyCode) -> String {
    match key {
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::Delete => "Del".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    }
}

/// Parse a key name from `keys.toml`: any single character, or one of the
/// named keys below. Deliberately small — modifier chords are not part of the
/// TUI keymap, so accepting `ctrl+x` here would promise something the
/// dispatcher cannot deliver.
#[must_use]
pub fn parse_key(s: &str) -> Option<KeyCode> {
    let t = s.trim();
    let mut chars = t.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyCode::Char(c));
    }
    if let Some(n) = t
        .strip_prefix(['f', 'F'])
        .and_then(|d| d.parse::<u8>().ok())
        .filter(|n| (1..=12).contains(n))
    {
        return Some(KeyCode::F(n));
    }
    match t.to_ascii_lowercase().as_str() {
        "space" => Some(KeyCode::Char(' ')),
        "enter" | "return" => Some(KeyCode::Enter),
        "esc" | "escape" => Some(KeyCode::Esc),
        "tab" => Some(KeyCode::Tab),
        "backspace" => Some(KeyCode::Backspace),
        "up" => Some(KeyCode::Up),
        "down" => Some(KeyCode::Down),
        "left" => Some(KeyCode::Left),
        "right" => Some(KeyCode::Right),
        "home" => Some(KeyCode::Home),
        "end" => Some(KeyCode::End),
        "delete" | "del" => Some(KeyCode::Delete),
        "pageup" | "pgup" => Some(KeyCode::PageUp),
        "pagedown" | "pgdn" => Some(KeyCode::PageDown),
        _ => None,
    }
}

/// Parse the subset of TOML `keys.toml` needs: comments, optional `[table]`
/// headers, and `action = "key"` pairs.
///
/// Hand-rolled on purpose: the workspace has no TOML dependency, and pulling
/// one in to read a flat list of string pairs would be the largest dependency
/// in this crate for the smallest grammar in it. Anything outside the subset
/// is an [`Err`] naming the line, which the caller turns into a startup
/// warning — a malformed file must never be silently ignored.
pub fn parse_keys_toml(src: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (n, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line.ends_with(']') {
                // Table headers are accepted and ignored: `[keys]` and a flat
                // file mean the same thing here.
                continue;
            }
            return Err(format!("line {}: unterminated table header", n + 1));
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected `action = \"key\"`", n + 1));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("line {}: missing action name", n + 1));
        }
        let value = value.trim();
        let quoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')));
        let Some(key) = quoted else {
            return Err(format!("line {}: value must be a quoted string", n + 1));
        };
        out.push((name.to_string(), key.to_string()));
    }
    Ok(out)
}

/// Drop a trailing `#` comment, ignoring `#` inside a quoted value (so
/// `capture = "#"` survives).
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, '#') => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Where the user's key overrides live (`docs/07-clients/tui.md`), honouring
/// `XDG_CONFIG_HOME`.
#[must_use]
pub fn keys_config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("sunrise").join("keys.toml"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("sunrise")
            .join("keys.toml"),
    )
}

/// Load the user keymap, falling back to the defaults.
///
/// Returns `(keymap, warnings)`. An absent file is the normal case and yields
/// no warnings; a malformed one yields warnings *and* the default keymap, so
/// the app always starts with a usable keyboard.
#[must_use]
pub fn load_keymap(path: Option<&std::path::Path>) -> (Keymap, Vec<String>) {
    let Some(path) = path else {
        return (Keymap::default(), Vec::new());
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        // Absent (the common case) is silent; anything else — a directory, bad
        // permissions — is worth saying out loud.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (Keymap::default(), Vec::new())
        }
        Err(e) => {
            return (
                Keymap::default(),
                vec![format!("{}: {e}; using default keys", path.display())],
            )
        }
    };
    match parse_keys_toml(&src) {
        Ok(pairs) => {
            let (map, warnings) = Keymap::from_config(&pairs);
            (
                map,
                warnings
                    .into_iter()
                    .map(|w| format!("{}: {w}", path.display()))
                    .collect(),
            )
        }
        Err(e) => (
            Keymap::default(),
            vec![format!("{}: {e}; using default keys", path.display())],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// Shorthand: dispatch in the Today view (the default context).
    fn d(key: KeyCode, mode: Mode, vim: bool) -> Option<Action> {
        dispatch(key, mode, vim, View::Today)
    }

    #[test]
    fn quit_in_normal_mode() {
        assert_eq!(
            d(KeyCode::Char('q'), Mode::Normal, true),
            Some(Action::Quit)
        );
        assert_eq!(d(KeyCode::Esc, Mode::Normal, true), Some(Action::Escape));
    }

    #[test]
    fn vim_movement() {
        assert_eq!(
            d(KeyCode::Char('j'), Mode::Normal, true),
            Some(Action::Next)
        );
        assert_eq!(
            d(KeyCode::Char('k'), Mode::Normal, true),
            Some(Action::Prev)
        );
    }

    #[test]
    fn arrow_movement_works_in_both_modes() {
        assert_eq!(d(KeyCode::Down, Mode::Normal, false), Some(Action::Next));
        assert_eq!(d(KeyCode::Up, Mode::Normal, false), Some(Action::Prev));
    }

    #[test]
    fn switch_view_bindings() {
        assert_eq!(
            d(KeyCode::Char('2'), Mode::Normal, true),
            Some(Action::SwitchView(View::Inbox))
        );
        assert_eq!(
            d(KeyCode::Char('1'), Mode::Normal, true),
            Some(Action::SwitchView(View::Today))
        );
        assert_eq!(
            d(KeyCode::Char('6'), Mode::Normal, true),
            Some(Action::SwitchView(View::Routines))
        );
    }

    #[test]
    fn insert_mode_buffers_chars() {
        assert_eq!(
            d(KeyCode::Char('x'), Mode::Insert, true),
            Some(Action::InsertChar('x'))
        );
        assert_eq!(
            d(KeyCode::Backspace, Mode::Insert, true),
            Some(Action::Backspace)
        );
        assert_eq!(d(KeyCode::Esc, Mode::Insert, true), Some(Action::Escape));
    }

    #[test]
    fn colon_enters_command_mode() {
        assert_eq!(
            d(KeyCode::Char(':'), Mode::Normal, true),
            Some(Action::BeginCommand)
        );
    }

    #[test]
    fn command_mode_buffers_and_submits() {
        assert_eq!(
            d(KeyCode::Char('v'), Mode::Command, true),
            Some(Action::InsertChar('v'))
        );
        assert_eq!(
            d(KeyCode::Backspace, Mode::Command, true),
            Some(Action::Backspace)
        );
        assert_eq!(d(KeyCode::Enter, Mode::Command, true), Some(Action::Submit));
        assert_eq!(d(KeyCode::Esc, Mode::Command, true), Some(Action::Escape));
    }

    #[test]
    fn capture_and_search_bindings() {
        assert_eq!(
            d(KeyCode::Char('c'), Mode::Normal, true),
            Some(Action::Capture)
        );
        assert_eq!(
            d(KeyCode::Char('/'), Mode::Normal, true),
            Some(Action::BeginSearch)
        );
    }

    #[test]
    fn schedule_visual_triage_and_body_bindings() {
        assert_eq!(
            d(KeyCode::Char('s'), Mode::Normal, true),
            Some(Action::Schedule)
        );
        assert_eq!(
            d(KeyCode::Char('E'), Mode::Normal, true),
            Some(Action::EditBody)
        );
        assert_eq!(
            d(KeyCode::Char('V'), Mode::Normal, true),
            Some(Action::VisualMode)
        );
        assert_eq!(
            d(KeyCode::Char('t'), Mode::Normal, true),
            Some(Action::Triage)
        );
        // `e` still edits the title — the body binding is additive.
        assert_eq!(
            d(KeyCode::Char('e'), Mode::Normal, true),
            Some(Action::EditTitle)
        );
    }

    #[test]
    fn visual_mode_extends_and_operates() {
        for (key, action) in [
            (KeyCode::Char('j'), Action::Next),
            (KeyCode::Char('k'), Action::Prev),
            (KeyCode::Down, Action::Next),
            (KeyCode::Char('x'), Action::Toggle),
            (KeyCode::Char('d'), Action::Defer),
            (KeyCode::Char('D'), Action::Delete),
            (KeyCode::Char('m'), Action::MoveToStream),
            (KeyCode::Esc, Action::Escape),
            (KeyCode::Char('V'), Action::Escape),
        ] {
            assert_eq!(d(key, Mode::Visual, true), Some(action), "for {key:?}");
        }
        // Visual navigation does not depend on vim-mode, and stray keys are
        // inert rather than being swallowed as text.
        assert_eq!(
            d(KeyCode::Char('j'), Mode::Visual, false),
            Some(Action::Next)
        );
        assert_eq!(d(KeyCode::Char('z'), Mode::Visual, true), None);
    }

    #[test]
    fn triage_mode_is_one_keypress_per_outcome() {
        for (key, action) in [
            (KeyCode::Char('k'), Action::TriageKeep),
            (KeyCode::Enter, Action::TriageKeep),
            (KeyCode::Char('p'), Action::MoveToStream),
            (KeyCode::Char('s'), Action::Schedule),
            (KeyCode::Char('d'), Action::Defer),
            (KeyCode::Char('D'), Action::Delete),
            (KeyCode::Char('x'), Action::Toggle),
            (KeyCode::Esc, Action::Escape),
            (KeyCode::Char('q'), Action::Escape),
        ] {
            assert_eq!(d(key, Mode::Triage, true), Some(action), "for {key:?}");
        }
        assert_eq!(d(KeyCode::Char('z'), Mode::Triage, true), None);
    }

    #[test]
    fn every_action_has_a_unique_config_name() {
        // The config contract: two actions sharing a name would make
        // `keys.toml` ambiguous.
        let mut distinct_actions: Vec<(&str, String)> = BINDINGS
            .iter()
            .map(|b| (b.action.name(), format!("{:?}", b.action)))
            .collect();
        distinct_actions.sort_unstable();
        distinct_actions.dedup();
        for w in distinct_actions.windows(2) {
            assert_ne!(
                w[0].0, w[1].0,
                "{:?} and {:?} share a config name",
                w[0].1, w[1].1
            );
        }
    }

    #[test]
    fn keys_toml_subset_parses_pairs_headers_and_comments() {
        let src = "\
# my keys
[keys]
capture = \"n\"    # rebind capture
quit    = 'Q'
help    = \"#\"
";
        let pairs = parse_keys_toml(src).expect("valid subset");
        assert_eq!(
            pairs,
            vec![
                ("capture".to_string(), "n".to_string()),
                ("quit".to_string(), "Q".to_string()),
                // A `#` inside quotes is a key, not the start of a comment.
                ("help".to_string(), "#".to_string()),
            ]
        );
    }

    #[test]
    fn keys_toml_rejects_malformed_lines_by_line_number() {
        assert!(parse_keys_toml("capture n\n")
            .expect_err("no `=`")
            .contains("line 1"));
        assert!(parse_keys_toml("\n\ncapture = n\n")
            .expect_err("unquoted value")
            .contains("line 3"));
        assert!(parse_keys_toml("[keys\n").is_err());
        // An empty file is valid and simply changes nothing.
        assert_eq!(parse_keys_toml("\n# nothing\n"), Ok(Vec::new()));
    }

    #[test]
    fn config_overrides_the_key_for_an_action() {
        let (map, warnings) =
            Keymap::from_config(&[("capture".into(), "n".into()), ("quit".into(), "Q".into())]);
        assert!(warnings.is_empty(), "got {warnings:?}");
        assert_eq!(
            map.dispatch(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
                Mode::Normal,
                true,
                View::Today
            ),
            Some(Action::Capture)
        );
        // The old key no longer captures.
        assert_ne!(
            map.dispatch(
                KeyCode::Char('c'),
                KeyModifiers::NONE,
                Mode::Normal,
                true,
                View::Today
            ),
            Some(Action::Capture)
        );
        // The uppercase alias still quits: only the canonical row is rebound.
        assert_eq!(
            map.dispatch(
                KeyCode::Char('Q'),
                KeyModifiers::NONE,
                Mode::Normal,
                true,
                View::Today
            ),
            Some(Action::Quit)
        );
        // The help overlay advertises the key the user must actually press.
        let rows: Vec<(String, &str)> = map
            .help_sections(View::Today)
            .into_iter()
            .flat_map(|(_, r)| r)
            .collect();
        assert!(
            rows.iter().any(|(k, d)| k == "n" && *d == "capture a task"),
            "help still lists the default key: {rows:?}"
        );
    }

    #[test]
    fn config_warns_rather_than_failing_on_bad_entries() {
        let (map, warnings) = Keymap::from_config(&[
            ("frobnicate".into(), "z".into()),
            ("capture".into(), "ctrl+x".into()),
        ]);
        assert_eq!(warnings.len(), 2, "got {warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("frobnicate")));
        assert!(warnings.iter().any(|w| w.contains("ctrl+x")));
        // Nothing was applied, so the defaults still work.
        assert_eq!(map, Keymap::default());
    }

    #[test]
    fn an_absent_key_file_is_silent() {
        let (map, warnings) =
            load_keymap(Some(std::path::Path::new("/nonexistent/sunrise/keys.toml")));
        assert_eq!(map, Keymap::default());
        assert!(warnings.is_empty(), "got {warnings:?}");
        // A `None` path (no HOME) is equally silent.
        assert!(load_keymap(None).1.is_empty());
    }

    #[test]
    fn key_names_round_trip() {
        for (name, code) in [
            ("space", KeyCode::Char(' ')),
            ("enter", KeyCode::Enter),
            ("Esc", KeyCode::Esc),
            ("tab", KeyCode::Tab),
            ("down", KeyCode::Down),
            ("F5", KeyCode::F(5)),
            ("x", KeyCode::Char('x')),
        ] {
            assert_eq!(parse_key(name), Some(code), "for {name}");
        }
        assert_eq!(parse_key("ctrl+a"), None);
        assert_eq!(parse_key(""), None);
        assert_eq!(key_label(KeyCode::Char(' ')), "Space");
        assert_eq!(key_label(KeyCode::Enter), "Enter");
    }

    #[test]
    fn mutation_bindings() {
        assert_eq!(
            d(KeyCode::Char('e'), Mode::Normal, true),
            Some(Action::EditTitle)
        );
        assert_eq!(
            d(KeyCode::Char('d'), Mode::Normal, true),
            Some(Action::Defer)
        );
        assert_eq!(
            d(KeyCode::Char('D'), Mode::Normal, true),
            Some(Action::Delete)
        );
        assert_eq!(
            d(KeyCode::Char('m'), Mode::Normal, true),
            Some(Action::MoveToStream)
        );
        assert_eq!(
            d(KeyCode::Char('S'), Mode::Normal, true),
            Some(Action::CreateStream)
        );
        assert_eq!(
            d(KeyCode::Char('?'), Mode::Normal, true),
            Some(Action::ToggleHelp)
        );
    }

    #[test]
    fn goto_chord_and_bottom() {
        assert_eq!(
            d(KeyCode::Char('g'), Mode::Normal, true),
            Some(Action::GotoPrefix)
        );
        assert_eq!(
            d(KeyCode::Char('G'), Mode::Normal, true),
            Some(Action::GotoBottom)
        );
    }

    #[test]
    fn confirm_mode_only_commits_on_y() {
        assert_eq!(
            d(KeyCode::Char('y'), Mode::Confirm, true),
            Some(Action::Submit)
        );
        assert_eq!(
            d(KeyCode::Char('Y'), Mode::Confirm, true),
            Some(Action::Submit)
        );
        assert_eq!(
            d(KeyCode::Char('n'), Mode::Confirm, true),
            Some(Action::Escape)
        );
        assert_eq!(d(KeyCode::Esc, Mode::Confirm, true), Some(Action::Escape));
        // Stray keys are inert — they neither commit nor cancel.
        assert_eq!(d(KeyCode::Char('z'), Mode::Confirm, true), None);
        assert_eq!(d(KeyCode::Enter, Mode::Confirm, true), None);
    }

    #[test]
    fn picker_mode_navigates_and_chooses() {
        assert_eq!(
            d(KeyCode::Char('j'), Mode::Picker, true),
            Some(Action::Next)
        );
        // Picker navigation does not depend on vim-mode.
        assert_eq!(
            d(KeyCode::Char('k'), Mode::Picker, false),
            Some(Action::Prev)
        );
        assert_eq!(d(KeyCode::Down, Mode::Picker, true), Some(Action::Next));
        assert_eq!(d(KeyCode::Enter, Mode::Picker, true), Some(Action::Submit));
        assert_eq!(d(KeyCode::Esc, Mode::Picker, true), Some(Action::Escape));
        // Picker mode never swallows text into the input buffer.
        assert_eq!(d(KeyCode::Char('z'), Mode::Picker, true), None);
    }

    #[test]
    fn pane_switching_only_in_stream_view() {
        assert_eq!(
            dispatch(KeyCode::Tab, Mode::Normal, true, View::Stream),
            Some(Action::TogglePane)
        );
        assert_eq!(
            dispatch(KeyCode::Char('h'), Mode::Normal, true, View::Stream),
            Some(Action::PaneLeft)
        );
        assert_eq!(
            dispatch(KeyCode::Char('l'), Mode::Normal, true, View::Stream),
            Some(Action::PaneRight)
        );
        assert_eq!(
            dispatch(KeyCode::Left, Mode::Normal, false, View::Stream),
            Some(Action::PaneLeft)
        );
        // Outside the Stream view none of these bind.
        assert_eq!(
            dispatch(KeyCode::Tab, Mode::Normal, true, View::Today),
            None
        );
        assert_eq!(
            dispatch(KeyCode::Char('h'), Mode::Normal, true, View::Inbox),
            None
        );
    }

    #[test]
    fn enter_activates_in_normal_mode() {
        assert_eq!(
            d(KeyCode::Enter, Mode::Normal, true),
            Some(Action::Activate)
        );
        assert_eq!(
            dispatch(KeyCode::Enter, Mode::Normal, true, View::Stream),
            Some(Action::Activate)
        );
    }

    #[test]
    fn f_opens_focus_view() {
        assert_eq!(
            d(KeyCode::Char('f'), Mode::Normal, true),
            Some(Action::SwitchView(View::Focus))
        );
    }

    #[test]
    fn esc_never_reaches_quit_from_any_view() {
        // Esc backs out; `q` quits. Wiring Esc to teardown makes one stray
        // keypress close the app.
        for view in [View::Focus, View::Today, View::Inbox, View::Routines] {
            assert_eq!(
                dispatch(KeyCode::Esc, Mode::Normal, true, view),
                Some(Action::Escape),
                "Esc in {view:?}"
            );
        }
    }

    #[test]
    fn focus_keys_only_bind_while_a_session_owns_the_keyboard() {
        // `F` starts one from anywhere.
        assert_eq!(
            d(KeyCode::Char('F'), Mode::Normal, true),
            Some(Action::StartFocus)
        );
        // The session's own keys are its mode's, so a key that also exists in
        // Normal mode resolves to the *session's* meaning while one runs and
        // never leaks the other way round.
        assert_eq!(
            d(KeyCode::Char('a'), Mode::Focus, true),
            Some(Action::CaptureAside)
        );
        assert_eq!(
            d(KeyCode::Char('a'), Mode::Normal, true),
            Some(Action::ToggleArchive)
        );
        assert_eq!(
            d(KeyCode::Char('b'), Mode::Focus, true),
            Some(Action::TakeBreak)
        );
        assert_eq!(d(KeyCode::Char('b'), Mode::Normal, true), None);
        // Esc and `q` end the session rather than quitting the app.
        assert_eq!(d(KeyCode::Esc, Mode::Focus, true), Some(Action::EndFocus));
        assert_eq!(
            d(KeyCode::Char('q'), Mode::Focus, true),
            Some(Action::EndFocus)
        );
    }

    #[test]
    fn the_reason_chooser_covers_the_domains_whole_set() {
        for (key, reason) in [
            ('s', sunrise_domain::InterruptionReason::SelfInterrupt),
            ('m', sunrise_domain::InterruptionReason::Meeting),
            ('b', sunrise_domain::InterruptionReason::Blocked),
            ('o', sunrise_domain::InterruptionReason::Other),
        ] {
            assert_eq!(
                d(KeyCode::Char(key), Mode::Interrupt, true),
                Some(Action::InterruptReason(reason))
            );
        }
        assert_eq!(d(KeyCode::Esc, Mode::Interrupt, true), Some(Action::Escape));
    }

    #[test]
    fn focus_actions_have_stable_config_names() {
        // Renaming one of these breaks every user's keys.toml.
        assert_eq!(Action::StartFocus.name(), "start_focus");
        assert_eq!(Action::EndFocus.name(), "end_focus");
        assert_eq!(Action::CaptureAside.name(), "capture_aside");
        assert_eq!(Action::TakeBreak.name(), "take_break");
        assert_eq!(
            Action::InterruptReason(sunrise_domain::InterruptionReason::Blocked).name(),
            "interrupt_blocked"
        );
    }

    #[test]
    fn start_focus_can_be_remapped_like_any_other_normal_key() {
        let (map, warnings) = Keymap::from_config(&[("start_focus".to_string(), "z".to_string())]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            map.dispatch(
                KeyCode::Char('z'),
                KeyModifiers::NONE,
                Mode::Normal,
                true,
                View::Today
            ),
            Some(Action::StartFocus)
        );
    }

    #[test]
    fn help_sections_are_derived_from_the_binding_table() {
        let sections = help_sections(View::Today);
        // Sections appear in table order, Normal first.
        assert_eq!(sections.first().map(|(m, _)| *m), Some("NORMAL"));
        let labels: Vec<&str> = sections.iter().map(|(m, _)| *m).collect();
        assert!(labels.contains(&"CONFIRM"), "got sections {labels:?}");
        assert!(labels.contains(&"PICK"), "got sections {labels:?}");
        assert!(labels.contains(&"FOCUS"), "got sections {labels:?}");
        assert!(labels.contains(&"INTERRUPT"), "got sections {labels:?}");

        // Every documented row's description is non-empty, and every row in
        // the table that opts into help is reachable from `dispatch`.
        for (_, rows) in &sections {
            for (keys, desc) in rows {
                assert!(!keys.is_empty() && !desc.is_empty());
            }
        }
    }

    #[test]
    fn a_chord_never_falls_through_to_typing_its_letter() {
        // `Ctrl-W` must delete a word, and an *unbound* chord must do nothing
        // — typing a bare `w` because Ctrl was held is the failure mode this
        // guards.
        let map = Keymap::default();
        assert_eq!(
            map.dispatch(
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
                Mode::Insert,
                true,
                View::Today
            ),
            Some(Action::DeleteWordBack)
        );
        assert_eq!(
            map.dispatch(
                KeyCode::Char('z'),
                KeyModifiers::CONTROL,
                Mode::Insert,
                true,
                View::Today
            ),
            None
        );
    }

    #[test]
    fn shift_is_normalised_away_so_capitals_still_reach_their_rows() {
        // Terminals differ on whether they report SHIFT alongside an already
        // uppercase char; both readings must find the `D` row.
        let map = Keymap::default();
        for mods in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert_eq!(
                map.dispatch(KeyCode::Char('D'), mods, Mode::Normal, true, View::Today),
                Some(Action::Delete)
            );
        }
    }

    #[test]
    fn help_text_fits_the_two_column_overlay() {
        // The overlay is 74 columns wide with a 2-space indent and an
        // 11-column key gutter, leaving 59 for a description. Anything longer
        // is silently clipped, so the row documents nothing — exactly the
        // failure this table exists to prevent.
        const KEYS_WIDTH: usize = 11;
        const DESC_WIDTH: usize = 59;
        for binding in BINDINGS {
            let Some((keys, desc)) = binding.help else {
                continue;
            };
            assert!(
                keys.chars().count() <= KEYS_WIDTH,
                "key label {keys:?} is wider than the {KEYS_WIDTH}-column gutter"
            );
            assert!(
                desc.chars().count() <= DESC_WIDTH,
                "help text {desc:?} is wider than the {DESC_WIDTH}-column description"
            );
        }
    }

    #[test]
    fn every_help_row_is_a_live_binding() {
        // Guards against the overlay documenting a key that no longer
        // dispatches: each help-carrying row must resolve through `dispatch`
        // under some vim/view context.
        for binding in BINDINGS {
            if binding.help.is_none() {
                continue;
            }
            let view = match binding.scope {
                Scope::In(v) | Scope::InVim(v) => v,
                _ => View::Today,
            };
            assert_eq!(
                Keymap::default()
                    .dispatch(binding.key, binding.mods, binding.mode, true, view)
                    .as_ref(),
                Some(&binding.action),
                "binding {:?}+{:?} in {:?} does not dispatch",
                binding.mods,
                binding.key,
                binding.mode
            );
        }
    }
}
