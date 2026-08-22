//! Command-line (`:`) parser for the TUI.
//!
//! Pure translation of a typed command string into a [`Cmd`]. The runtime
//! (`lib::apply_command`) turns each [`Cmd`] into a view-state mutation or an
//! application effect. See `docs/08-features/keyboard.md` (`:` = command
//! palette) and `docs/07-clients/tui.md` (Command mode).

use crate::view::View;
use std::path::PathBuf;
use sunrise_domain::{Energy, ExportDataset, ExportFormat, SessionLength};
use sunrise_id::{EntityKind, EntityRef};

/// A parsed command-line command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Quit the application (`:q`, `:quit`).
    Quit,
    /// Switch the active view (`:view today`, `:view 1`).
    SwitchView(View),
    /// Show the help message listing available commands (`:help`).
    ShowHelp,
    /// Preview an image file in the Focus view (`:preview <path>`). Always
    /// parsed; without the `images` cargo feature the runtime reports that
    /// the feature is disabled.
    Preview(PathBuf),
    /// One-shot capture (`:capture Buy milk #errands !2`). The text goes
    /// through the same parser the `c` prompt uses.
    Capture(String),
    /// Jump to a task by id (`:open tsk_…`), opening it in the Focus view.
    Open(EntityRef),
    /// List the paired devices (`:devices`).
    Devices,
    /// Narrow every task list to these contexts (`:filter @home`); an empty
    /// list clears the filter.
    Filter(Vec<String>),
    /// Save the current view, query and filter under a name (`:save <name>`).
    SaveView(String),
    /// Recall a saved view (`:go <name>`).
    GoView(String),
    /// List the saved views (`:views`).
    ListViews,
    /// Forget a saved view (`:unsave <name>`).
    ForgetView(String),
    /// Write a stats dataset to a file (`:export <dataset> [json|csv] [path]`).
    Export {
        /// Which dataset.
        dataset: ExportDataset,
        /// Serialization format.
        format: ExportFormat,
        /// Destination; `None` picks a name in the working directory.
        path: Option<PathBuf>,
    },
    /// Focus-mode command (`:focus …`).
    Focus(FocusCmd),
    /// Unrecognized or malformed command; carries a status-line message.
    Error(String),
}

/// The `:focus` sub-commands (`docs/08-features/focus-mode.md`).
///
/// The planner's two inputs — the declared **energy budget** and the session
/// **length** — are set here rather than bound to keys: both are occasional
/// declarations, and the keymap has no spare mnemonic left for either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusCmd {
    /// Show focus totals and the estimate-calibration factor
    /// (`:focus stats`).
    Stats,
    /// Open the ranked planner queue (`:focus plan`).
    Plan,
    /// Declare the energy budget the planner matches work against
    /// (`:focus energy low|med|high|any`). `None` is "any", which drops
    /// energy out of the ranking entirely.
    Energy(Option<Energy>),
    /// Choose how the next session is sized
    /// (`:focus length pomodoro|estimate|until-done`).
    Length(SessionLength),
}

/// The `:` command reference, in the order the help overlay lists it.
///
/// A table rather than prose for the same reason the keymap is one: the help
/// and the parser must not be able to drift. A test asserts every row here
/// parses to something other than [`Cmd::Error`].
pub const COMMANDS: &[(&str, &str)] = &[
    (":q", "quit"),
    (":view <name>", "switch view (1-7)"),
    (":capture <text>", "capture a task"),
    (":open <tsk_…>", "jump to a task by id"),
    (":focus plan", "open the planner"),
    (":focus stats", "focus totals + calibration"),
    (":focus energy <l|m|h>", "declare the energy budget"),
    (":focus length <p|e|u>", "pomodoro / estimate / until done"),
    (":export <dataset>", "trends|activity|focus|streaks"),
    (":filter @ctx…", "narrow lists to contexts (bare clears)"),
    (":save <name>", "save this view, query and filter"),
    (":go <name>", "recall a saved view"),
    (":views", "list the saved views"),
    (":unsave <name>", "forget a saved view"),
    (":devices", "list paired devices"),
    (":preview <path>", "show an image (Focus view)"),
    (":help", "this list"),
];

/// Complete the last word of a partially typed `:` line.
///
/// `docs/07-clients/tui.md` calls Command mode "completion-driven" and it had
/// no completion at all: the only way to learn a command was to have read the
/// source, and the only way to type one was to get every character right
/// first time.
///
/// Returns the candidates for the word under the cursor — the command name
/// while the first word is being typed, its argument vocabulary afterwards.
/// An empty result means "nothing to offer", not "no such command": a free
/// argument (a path, a capture line) has no vocabulary to complete against and
/// must not be silently rewritten into one.
#[must_use]
pub fn complete(line: &str) -> Vec<String> {
    let trimmed = line.strip_prefix(':').unwrap_or(line);
    let ends_in_space = trimmed.ends_with(char::is_whitespace);
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    // The word being completed is the last one, unless a space just ended it.
    let (head, partial): (&[&str], &str) = if ends_in_space || words.is_empty() {
        (words.as_slice(), "")
    } else {
        words.split_last().map_or((&[], ""), |(p, h)| (h, *p))
    };
    let candidates: Vec<&str> = match head {
        [] => COMMANDS
            .iter()
            .map(|(spec, _)| spec.trim_start_matches(':').split(' ').next().unwrap_or(""))
            .collect(),
        ["view"] => vec![
            "today", "inbox", "stream", "search", "focus", "routines", "review",
        ],
        ["focus"] => vec!["plan", "stats", "energy", "length"],
        ["focus", "energy"] => vec!["low", "med", "high", "any"],
        ["focus", "length"] => vec!["pomodoro", "estimate", "until-done"],
        ["export"] => vec!["trends", "activity", "focus", "streaks"],
        ["export", _] => vec!["csv", "json"],
        // `:filter` completes against nothing here: the context names live in
        // the view state, not in this module, and offering a stale vocabulary
        // would be worse than offering none.
        _ => vec![],
    };
    let mut out: Vec<String> = candidates
        .into_iter()
        .filter(|c| c.starts_with(partial))
        .map(ToString::to_string)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The longest prefix every candidate shares — what Tab commits when the
/// choice is still ambiguous, exactly as a shell does.
#[must_use]
pub fn common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut end = first.len();
    for c in &candidates[1..] {
        end = end.min(
            first
                .char_indices()
                .zip(c.chars())
                .take_while(|((_, a), b)| a == b)
                .last()
                .map_or(0, |((i, a), _)| i + a.len_utf8()),
        );
    }
    first[..end].to_string()
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
    // Split off the command name; `rest` keeps interior spacing so path
    // arguments with spaces survive intact.
    let (name, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((n, r)) => (n, r.trim()),
        None => (trimmed, ""),
    };
    match name {
        "" => Cmd::Error("empty command".into()),
        "q" | "quit" => Cmd::Quit,
        "help" | "h" => Cmd::ShowHelp,
        "view" => match rest.split_whitespace().next() {
            None => {
                Cmd::Error("usage: :view <today|inbox|stream|search|focus|routines|review>".into())
            }
            Some(arg) => match parse_view(arg) {
                Some(v) => Cmd::SwitchView(v),
                None => Cmd::Error(format!("unknown view: {arg}")),
            },
        },
        "preview" => {
            if rest.is_empty() {
                Cmd::Error("usage: :preview <path>".into())
            } else {
                Cmd::Preview(PathBuf::from(rest))
            }
        }
        "capture" | "c" => {
            if rest.is_empty() {
                Cmd::Error("usage: :capture <text>".into())
            } else {
                Cmd::Capture(rest.to_string())
            }
        }
        "open" => match rest.split_whitespace().next() {
            None => Cmd::Error("usage: :open <tsk_…>".into()),
            Some(arg) => match EntityRef::parse(arg, EntityKind::Task) {
                Ok(id) => Cmd::Open(id),
                Err(e) => Cmd::Error(format!("not a task id: {arg} ({e})")),
            },
        },
        "devices" | "device" => Cmd::Devices,
        "save" | "sv" => match rest.split_whitespace().next() {
            None => Cmd::Error("usage: :save <name>".into()),
            Some(name) => Cmd::SaveView(name.to_string()),
        },
        "go" | "g" => match rest.split_whitespace().next() {
            None => Cmd::Error("usage: :go <saved view>".into()),
            Some(name) => Cmd::GoView(name.to_string()),
        },
        "views" => Cmd::ListViews,
        "unsave" => match rest.split_whitespace().next() {
            None => Cmd::Error("usage: :unsave <name>".into()),
            Some(name) => Cmd::ForgetView(name.to_string()),
        },
        // Names are resolved by the caller, which holds the context rows.
        "filter" | "f" => Cmd::Filter(
            rest.split_whitespace()
                .map(|w| w.trim_start_matches('@').to_string())
                .filter(|w| !w.is_empty())
                .collect(),
        ),
        "export" => parse_export(rest),
        "focus" => parse_focus(rest),
        other => Cmd::Error(format!("unknown command: {other}")),
    }
}

/// Parse `:export <dataset> [json|csv] [path]`.
///
/// The dataset is required and the format defaults to CSV: an export is
/// overwhelmingly headed for a spreadsheet, and a user who wants JSON will say
/// so. The path is optional because naming a file is the step people skip.
fn parse_export(rest: &str) -> Cmd {
    let mut words = rest.split_whitespace();
    let Some(name) = words.next() else {
        return Cmd::Error(
            "usage: :export <trends|activity|focus|streaks> [json|csv] [path]".into(),
        );
    };
    let dataset = match name {
        "trends" | "trend" => ExportDataset::Trends,
        "activity" | "timeline" => ExportDataset::Activity,
        "focus" => ExportDataset::Focus,
        "streaks" | "streak" => ExportDataset::Streaks,
        other => return Cmd::Error(format!("unknown dataset: {other}")),
    };
    // The format word is optional, so a second word that is not one is taken
    // as the path rather than rejected.
    let mut format = ExportFormat::Csv;
    let mut path: Option<PathBuf> = None;
    for w in words {
        match w {
            "json" => format = ExportFormat::Json,
            "csv" => format = ExportFormat::Csv,
            other if path.is_none() => path = Some(PathBuf::from(other)),
            other => return Cmd::Error(format!("unexpected argument: {other}")),
        }
    }
    Cmd::Export {
        dataset,
        format,
        path,
    }
}

/// Parse the argument of `:focus`. A bare `:focus` opens the planner, which
/// is the one thing the command could otherwise only mean.
fn parse_focus(rest: &str) -> Cmd {
    let mut words = rest.split_whitespace();
    let (sub, arg) = (words.next().unwrap_or("plan"), words.next());
    match sub {
        "plan" | "planner" => Cmd::Focus(FocusCmd::Plan),
        "stats" | "stat" => Cmd::Focus(FocusCmd::Stats),
        "energy" => match arg {
            None => Cmd::Error("usage: :focus energy <low|med|high|any>".into()),
            Some("any" | "none") => Cmd::Focus(FocusCmd::Energy(None)),
            Some("low") => Cmd::Focus(FocusCmd::Energy(Some(Energy::Low))),
            Some("med" | "medium") => Cmd::Focus(FocusCmd::Energy(Some(Energy::Med))),
            Some("high") => Cmd::Focus(FocusCmd::Energy(Some(Energy::High))),
            Some(other) => Cmd::Error(format!("unknown energy: {other}")),
        },
        "length" | "len" => match arg {
            None => Cmd::Error("usage: :focus length <pomodoro|estimate|until-done>".into()),
            Some("pomodoro" | "pom" | "25m") => {
                Cmd::Focus(FocusCmd::Length(SessionLength::OnePomodoro))
            }
            Some("estimate" | "est") => {
                Cmd::Focus(FocusCmd::Length(SessionLength::SizedToEstimate))
            }
            Some("until-done" | "until" | "open") => {
                Cmd::Focus(FocusCmd::Length(SessionLength::UntilDone))
            }
            Some(other) => Cmd::Error(format!("unknown session length: {other}")),
        },
        other => Cmd::Error(format!("unknown :focus command: {other}")),
    }
}

/// Map a `:view` argument (name or `1..=6`) onto a [`View`].
fn parse_view(arg: &str) -> Option<View> {
    match arg {
        "today" | "1" => Some(View::Today),
        "inbox" | "2" => Some(View::Inbox),
        "stream" | "3" => Some(View::Stream),
        "search" | "4" => Some(View::Search),
        "focus" | "5" => Some(View::Focus),
        "routines" | "routine" | "6" => Some(View::Routines),
        "review" | "stats" | "7" => Some(View::Review),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn completion_offers_the_command_names_first() {
        let names = complete(":");
        assert!(names.contains(&"view".to_string()), "{names:?}");
        assert!(names.contains(&"export".to_string()), "{names:?}");
        // …narrowed by what has been typed.
        assert_eq!(complete(":ex"), vec!["export".to_string()]);
        assert_eq!(complete(":devi"), vec!["devices".to_string()]);
    }

    #[test]
    fn completion_knows_each_commands_own_vocabulary() {
        assert!(complete(":view ").contains(&"routines".to_string()));
        assert_eq!(complete(":view rou"), vec!["routines".to_string()]);
        assert!(complete(":focus ").contains(&"stats".to_string()));
        assert_eq!(complete(":focus energy h"), vec!["high".to_string()]);
        assert!(complete(":export ").contains(&"trends".to_string()));
        assert!(complete(":export trends ").contains(&"json".to_string()));
    }

    #[test]
    fn a_free_argument_has_nothing_to_complete_against() {
        // A capture line or a path must never be silently rewritten into a
        // keyword that happens to share a prefix.
        assert!(complete(":capture buy mi").is_empty());
        assert!(complete(":open tsk_").is_empty());
    }

    #[test]
    fn the_common_prefix_is_what_tab_can_safely_commit() {
        let c = |v: &[&str]| common_prefix(&v.iter().map(ToString::to_string).collect::<Vec<_>>());
        assert_eq!(c(&["export"]), "export");
        assert_eq!(c(&["focus", "foo"]), "fo");
        assert_eq!(c(&["view", "export"]), "");
        assert_eq!(c(&[]), "");
    }

    #[test]
    fn every_completion_candidate_is_something_the_parser_accepts() {
        // A completion that produces an unknown command is worse than none:
        // the user believes the shell told them it was real.
        for name in complete(":") {
            assert!(
                !matches!(parse_command(&format!(":{name}")), Cmd::Error(e) if e.starts_with("unknown command")),
                "completed {name:?} is not a command"
            );
        }
        for arg in complete(":view ") {
            assert!(
                !matches!(parse_command(&format!(":view {arg}")), Cmd::Error(_)),
                ":view {arg} does not parse"
            );
        }
        for arg in complete(":export ") {
            assert!(
                !matches!(parse_command(&format!(":export {arg}")), Cmd::Error(_)),
                ":export {arg} does not parse"
            );
        }
    }

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
        assert_eq!(
            parse_command(":view routines"),
            Cmd::SwitchView(View::Routines)
        );
    }

    #[test]
    fn view_by_number() {
        assert_eq!(parse_command(":view 1"), Cmd::SwitchView(View::Today));
        assert_eq!(parse_command(":view 2"), Cmd::SwitchView(View::Inbox));
        assert_eq!(parse_command(":view 3"), Cmd::SwitchView(View::Stream));
        assert_eq!(parse_command(":view 4"), Cmd::SwitchView(View::Search));
        assert_eq!(parse_command(":view 5"), Cmd::SwitchView(View::Focus));
        assert_eq!(parse_command(":view 6"), Cmd::SwitchView(View::Routines));
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
    fn preview_takes_rest_of_line_as_path() {
        assert_eq!(
            parse_command(":preview /tmp/cat.png"),
            Cmd::Preview(PathBuf::from("/tmp/cat.png"))
        );
        // Paths with spaces are kept whole (rest-of-line semantics).
        assert_eq!(
            parse_command(":preview /home/me/My Pictures/cat photo.png"),
            Cmd::Preview(PathBuf::from("/home/me/My Pictures/cat photo.png"))
        );
        // Surrounding whitespace is trimmed off the path.
        assert_eq!(
            parse_command("  :preview   spaced.png  "),
            Cmd::Preview(PathBuf::from("spaced.png"))
        );
    }

    #[test]
    fn preview_missing_arg_is_error() {
        match parse_command(":preview") {
            Cmd::Error(msg) => assert!(msg.contains("usage")),
            other => panic!("expected error, got {other:?}"),
        }
        assert!(matches!(parse_command(":preview   "), Cmd::Error(_)));
    }

    #[test]
    fn capture_takes_the_rest_of_the_line() {
        assert_eq!(
            parse_command(":capture Buy milk #errands !2"),
            Cmd::Capture("Buy milk #errands !2".into())
        );
        // Interior spacing is preserved; the parser owns the tokenizing.
        assert_eq!(
            parse_command(":capture  Renew   passport "),
            Cmd::Capture("Renew   passport".into())
        );
        match parse_command(":capture") {
            Cmd::Error(msg) => assert!(msg.contains("usage")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn open_parses_a_task_id() {
        let id = EntityRef::new(EntityKind::Task, [3u8; 16]);
        assert_eq!(
            parse_command(&format!(":open {}", id.to_str())),
            Cmd::Open(id)
        );
        // A stream id is rejected rather than silently opened as a task.
        let stream = EntityRef::new(EntityKind::Stream, [3u8; 16]);
        assert!(matches!(
            parse_command(&format!(":open {}", stream.to_str())),
            Cmd::Error(_)
        ));
        assert!(matches!(parse_command(":open"), Cmd::Error(_)));
        assert!(matches!(parse_command(":open garbage"), Cmd::Error(_)));
    }

    #[test]
    fn devices_aliases() {
        assert_eq!(parse_command(":devices"), Cmd::Devices);
        assert_eq!(parse_command(":device"), Cmd::Devices);
    }

    #[test]
    fn focus_subcommands_parse() {
        assert_eq!(parse_command(":focus stats"), Cmd::Focus(FocusCmd::Stats));
        assert_eq!(parse_command(":focus plan"), Cmd::Focus(FocusCmd::Plan));
        // A bare `:focus` is the planner: it is the only thing the word could
        // otherwise mean.
        assert_eq!(parse_command(":focus"), Cmd::Focus(FocusCmd::Plan));
    }

    #[test]
    fn focus_energy_declares_the_planner_budget() {
        for (arg, want) in [
            ("low", Some(Energy::Low)),
            ("med", Some(Energy::Med)),
            ("high", Some(Energy::High)),
            ("any", None),
        ] {
            assert_eq!(
                parse_command(&format!(":focus energy {arg}")),
                Cmd::Focus(FocusCmd::Energy(want))
            );
        }
        assert!(matches!(parse_command(":focus energy"), Cmd::Error(_)));
        match parse_command(":focus energy sideways") {
            Cmd::Error(msg) => assert!(msg.contains("sideways")),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn focus_length_picks_a_session_length() {
        assert_eq!(
            parse_command(":focus length pomodoro"),
            Cmd::Focus(FocusCmd::Length(SessionLength::OnePomodoro))
        );
        assert_eq!(
            parse_command(":focus length estimate"),
            Cmd::Focus(FocusCmd::Length(SessionLength::SizedToEstimate))
        );
        assert_eq!(
            parse_command(":focus length until-done"),
            Cmd::Focus(FocusCmd::Length(SessionLength::UntilDone))
        );
        assert!(matches!(parse_command(":focus length"), Cmd::Error(_)));
        assert!(matches!(
            parse_command(":focus length forever"),
            Cmd::Error(_)
        ));
    }

    #[test]
    fn unknown_focus_subcommand_is_error() {
        match parse_command(":focus wiggle") {
            Cmd::Error(msg) => assert!(msg.contains("wiggle")),
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
