//! Command-line (`:`) parser for the TUI.
//!
//! Pure translation of a typed command string into a [`Cmd`]. The runtime
//! (`lib::apply_command`) turns each [`Cmd`] into a view-state mutation or an
//! application effect. See `docs/08-features/keyboard.md` (`:` = command
//! palette) and `docs/07-clients/tui.md` (Command mode).

use crate::view::View;

/// A parsed command-line command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Quit the application (`:q`, `:quit`).
    Quit,
    /// Switch the active view (`:view today`, `:view 1`).
    SwitchView(View),
    /// Show the help message listing available commands (`:help`).
    ShowHelp,
    /// Unrecognized or malformed command; carries a status-line message.
    Error(String),
}

/// Parse a command-line string into a [`Cmd`].
///
/// A single leading `:` is optional, so both `":q"` and `"q"` parse to
/// [`Cmd::Quit`]. Surrounding whitespace is ignored. Anything unrecognized
/// yields [`Cmd::Error`] with a human-readable message for the status line.
#[must_use]
pub fn parse_command(input: &str) -> Cmd {
    let trimmed = input.trim();
    let trimmed = trimmed.strip_prefix(':').unwrap_or(trimmed).trim();
    let mut parts = trimmed.split_whitespace();
    let Some(name) = parts.next() else {
        return Cmd::Error("empty command".into());
    };
    match name {
        "q" | "quit" => Cmd::Quit,
        "help" | "h" => Cmd::ShowHelp,
        "view" => match parts.next() {
            None => Cmd::Error("usage: :view <today|inbox|stream|search|focus>".into()),
            Some(arg) => match parse_view(arg) {
                Some(v) => Cmd::SwitchView(v),
                None => Cmd::Error(format!("unknown view: {arg}")),
            },
        },
        other => Cmd::Error(format!("unknown command: {other}")),
    }
}

/// Map a `:view` argument (name or `1..=5`) onto a [`View`].
fn parse_view(arg: &str) -> Option<View> {
    match arg {
        "today" | "1" => Some(View::Today),
        "inbox" | "2" => Some(View::Inbox),
        "stream" | "3" => Some(View::Stream),
        "search" | "4" => Some(View::Search),
        "focus" | "5" => Some(View::Focus),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_aliases() {
        assert_eq!(parse_command(":q"), Cmd::Quit);
        assert_eq!(parse_command(":quit"), Cmd::Quit);
        assert_eq!(parse_command("q"), Cmd::Quit);
        assert_eq!(parse_command("  :quit  "), Cmd::Quit);
    }

    #[test]
    fn help_aliases() {
        assert_eq!(parse_command(":help"), Cmd::ShowHelp);
        assert_eq!(parse_command(":h"), Cmd::ShowHelp);
    }

    #[test]
    fn view_by_name() {
        assert_eq!(parse_command(":view today"), Cmd::SwitchView(View::Today));
        assert_eq!(parse_command(":view inbox"), Cmd::SwitchView(View::Inbox));
        assert_eq!(parse_command(":view stream"), Cmd::SwitchView(View::Stream));
        assert_eq!(parse_command(":view search"), Cmd::SwitchView(View::Search));
        assert_eq!(parse_command(":view focus"), Cmd::SwitchView(View::Focus));
    }

    #[test]
    fn view_by_number() {
        assert_eq!(parse_command(":view 1"), Cmd::SwitchView(View::Today));
        assert_eq!(parse_command(":view 2"), Cmd::SwitchView(View::Inbox));
        assert_eq!(parse_command(":view 3"), Cmd::SwitchView(View::Stream));
        assert_eq!(parse_command(":view 4"), Cmd::SwitchView(View::Search));
        assert_eq!(parse_command(":view 5"), Cmd::SwitchView(View::Focus));
    }

    #[test]
    fn view_missing_arg_is_error() {
        assert!(matches!(parse_command(":view"), Cmd::Error(_)));
    }

    #[test]
    fn view_unknown_arg_is_error() {
        match parse_command(":view nope") {
            Cmd::Error(msg) => assert!(msg.contains("nope")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_command_is_error() {
        match parse_command(":frobnicate") {
            Cmd::Error(msg) => assert!(msg.contains("frobnicate")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn empty_is_error() {
        assert!(matches!(parse_command(""), Cmd::Error(_)));
        assert!(matches!(parse_command(":"), Cmd::Error(_)));
        assert!(matches!(parse_command("   "), Cmd::Error(_)));
    }
}
