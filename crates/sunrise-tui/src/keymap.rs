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

use crate::view::View;
use crossterm::event::KeyCode;

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
    /// Mark current task as done.
    Toggle,
    /// Open the capture prompt.
    Capture,
    /// Edit the selected task's title (`e`).
    EditTitle,
    /// Defer the selected task (`d`), prompting for an offset.
    Defer,
    /// Delete the selected task (`D`), behind a confirmation prompt.
    Delete,
    /// Move the selected task to another stream (`m`).
    MoveToStream,
    /// Create a stream (`S`).
    CreateStream,
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
    /// Backspace in the active input.
    Backspace,
    /// Submit the current input (Enter).
    Submit,
    /// First half of a `gg` chord: arm the pending-`g` latch. The runtime
    /// turns the second `g` into [`Action::GotoTop`].
    GotoPrefix,
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
        scope,
        action,
        help,
    }
}

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
    b(Mode::Normal, KeyCode::Esc, Scope::Any, Action::Quit, None),
    b(
        Mode::Normal,
        KeyCode::Char('q'),
        Scope::Any,
        Action::Quit,
        Some(("q / Esc", "quit")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('1'),
        Scope::Any,
        Action::SwitchView(View::Today),
        Some(("1 - 6", "switch view")),
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
        KeyCode::Char('f'),
        Scope::Any,
        Action::SwitchView(View::Focus),
        Some(("f", "focus selected task")),
    ),
    b(
        Mode::Normal,
        KeyCode::Tab,
        Scope::In(View::Stream),
        Action::TogglePane,
        Some(("Tab", "switch pane (Stream)")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char('h'),
        Scope::InVim(View::Stream),
        Action::PaneLeft,
        Some(("h / l", "streams / tasks pane")),
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
        KeyCode::Char('x'),
        Scope::Any,
        Action::Toggle,
        Some(("x / Space", "toggle done")),
    ),
    b(
        Mode::Normal,
        KeyCode::Char(' '),
        Scope::Any,
        Action::Toggle,
        None,
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
        KeyCode::Char('d'),
        Scope::Any,
        Action::Defer,
        Some(("d", "defer (prompts)")),
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
];

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
    for binding in BINDINGS {
        if binding.mode == mode && binding.key == key && binding.scope.matches(vim_mode, view) {
            return Some(binding.action.clone());
        }
    }
    // Fallback: any other printable character feeds the active text input.
    // This is the one rule that cannot live in the table (it matches every
    // `Char`), so it runs last, after the table's Esc/Enter/Backspace rows.
    match (mode, key) {
        (Mode::Insert | Mode::Command, KeyCode::Char(c)) => Some(Action::InsertChar(c)),
        _ => None,
    }
}

/// Help-overlay content, derived from [`BINDINGS`]: one section per mode (in
/// table order), each holding the `(keys, description)` pairs of the rows that
/// opted into help.
#[must_use]
pub fn help_sections() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let mut out: Vec<(&'static str, Vec<(&'static str, &'static str)>)> = Vec::new();
    for binding in BINDINGS {
        let Some(entry) = binding.help else { continue };
        let label = binding.mode.label();
        match out.last_mut() {
            Some((existing, rows)) if *existing == label => rows.push(entry),
            _ => out.push((label, vec![entry])),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

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
        assert_eq!(d(KeyCode::Esc, Mode::Normal, true), Some(Action::Quit));
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
    fn esc_in_focus_view_escapes_instead_of_quitting() {
        assert_eq!(
            dispatch(KeyCode::Esc, Mode::Normal, true, View::Focus),
            Some(Action::Escape)
        );
        assert_eq!(
            dispatch(KeyCode::Esc, Mode::Normal, true, View::Today),
            Some(Action::Quit)
        );
    }

    #[test]
    fn help_sections_are_derived_from_the_binding_table() {
        let sections = help_sections();
        // Sections appear in table order, Normal first.
        assert_eq!(sections.first().map(|(m, _)| *m), Some("NORMAL"));
        let labels: Vec<&str> = sections.iter().map(|(m, _)| *m).collect();
        assert!(labels.contains(&"CONFIRM"), "got sections {labels:?}");
        assert!(labels.contains(&"PICK"), "got sections {labels:?}");

        // Every documented row's description is non-empty, and every row in
        // the table that opts into help is reachable from `dispatch`.
        for (_, rows) in &sections {
            for (keys, desc) in rows {
                assert!(!keys.is_empty() && !desc.is_empty());
            }
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
                dispatch(binding.key, binding.mode, true, view).as_ref(),
                Some(&binding.action),
                "binding {:?} in {:?} does not dispatch",
                binding.key,
                binding.mode
            );
        }
    }
}
