//! Saved views: a named view, query and context filter, recalled by name.
//!
//! `docs/07-clients/parity-matrix.md` marks "Saved searches / views" **MUST**
//! on every client. The vault has no entity for one — a saved view is a
//! *pointer at* data, not data — so this is a per-device local preference,
//! written in the [`crate::config`] TOML subset.
//!
//! ```toml
//! # ~/.config/sunrise/views.toml
//! errands = "view=search;query=passport;contexts=errands,home"
//! deep    = "view=today;contexts=deep-work"
//! ```
//!
//! # Contexts are stored by name, not by id
//!
//! An `EntityRef` is a vault-local ULID. A saved view written on one machine
//! and read on another — the same account, a paired laptop — would point at
//! nothing, and the failure would be silent: the filter would resolve to an
//! empty set and the view would look like an empty vault. Names are what the
//! user typed and what the capture parser already resolves against, so they
//! survive the trip; a name that no longer exists is reported on recall
//! instead of quietly filtering everything away.

use std::path::PathBuf;

/// The primary views every client presents, per
/// `docs/07-clients/parity-matrix.md`.
///
/// Lives here rather than in a client because a saved view names one, and a
/// saved view is written on one device and read on another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Today: scheduled blocks + due-today tasks + manually-pulled tasks.
    Today,
    /// Inbox: unassigned tasks awaiting triage.
    Inbox,
    /// Browse one Stream or Context.
    Stream,
    /// Free-text search.
    Search,
    /// Focus mode: one task, one session.
    Focus,
    /// Routines: recurring templates, with full CRUD.
    Routines,
    /// Review: the weekly review flow, the daily glance, the trends and the
    /// saved-snapshot history.
    Review,
}

/// A named view: where to be, what to search for, what to narrow to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedView {
    /// The name it is recalled by.
    pub name: String,
    /// Which primary view.
    pub view: View,
    /// Search text, for the Search view. Empty means none.
    pub query: String,
    /// Context **names** to filter to. Empty means no filter.
    pub contexts: Vec<String>,
}

impl SavedView {
    /// The `view=…;query=…;contexts=…` value written to the file.
    ///
    /// Only the parts that carry information are emitted, so a saved Today
    /// view with no filter reads as `view=today` rather than as a row of empty
    /// fields.
    #[must_use]
    pub fn spec(&self) -> String {
        let mut parts = vec![format!("view={}", view_name(self.view))];
        if !self.query.is_empty() {
            parts.push(format!("query={}", self.query));
        }
        if !self.contexts.is_empty() {
            parts.push(format!("contexts={}", self.contexts.join(",")));
        }
        parts.join(";")
    }

    /// One line of the file.
    #[must_use]
    pub fn to_line(&self) -> String {
        format!("{} = \"{}\"", self.name, self.spec())
    }

    /// A one-line summary for the `:views` overlay.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = vec![view_name(self.view).to_string()];
        if !self.query.is_empty() {
            parts.push(format!("/{}", self.query));
        }
        if !self.contexts.is_empty() {
            parts.push(
                self.contexts
                    .iter()
                    .map(|c| format!("@{c}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        parts.join(" · ")
    }
}

/// Parse one `name = "spec"` pair into a [`SavedView`].
///
/// # Errors
///
/// Returns a message naming what could not be read. Unknown keys inside the
/// spec are an error rather than being ignored: a typo'd `context=` that
/// silently saved no filter would recall the wrong thing forever.
pub fn parse_view(name: &str, spec: &str) -> Result<SavedView, String> {
    let mut out = SavedView {
        name: name.to_string(),
        view: View::Today,
        query: String::new(),
        contexts: Vec::new(),
    };
    let mut saw_view = false;
    for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| format!("{name}: {part:?} is not key=value"))?;
        match key.trim() {
            "view" => {
                out.view = parse_view_name(value.trim())
                    .ok_or_else(|| format!("{name}: unknown view {value:?}"))?;
                saw_view = true;
            }
            "query" => out.query = value.trim().to_string(),
            "contexts" => {
                out.contexts = value
                    .split(',')
                    .map(|c| c.trim().trim_start_matches('@').to_string())
                    .filter(|c| !c.is_empty())
                    .collect();
            }
            other => return Err(format!("{name}: unknown field {other:?}")),
        }
    }
    if !saw_view {
        return Err(format!("{name}: no view= in the spec"));
    }
    Ok(out)
}

/// Read the saved views, or an empty list if the file is absent.
///
/// Returns `(views, warnings)`. A malformed line costs that view and nothing
/// else — the same rule `keys.toml` follows, because a typo in a config file
/// must never cost the user a working app.
#[must_use]
pub fn load(path: Option<&std::path::Path>) -> (Vec<SavedView>, Vec<String>) {
    let Some(path) = path else {
        return (Vec::new(), Vec::new());
    };
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), Vec::new()),
        Err(e) => return (Vec::new(), vec![format!("{}: {e}", path.display())]),
    };
    let pairs = match crate::config::parse_pairs(&src) {
        Ok(p) => p,
        Err(e) => return (Vec::new(), vec![format!("{}: {e}", path.display())]),
    };
    let mut views = Vec::new();
    let mut warnings = Vec::new();
    for (name, spec) in pairs {
        match parse_view(&name, &spec) {
            Ok(v) => views.push(v),
            Err(e) => warnings.push(e),
        }
    }
    (views, warnings)
}

/// Render the whole set back to a file body.
#[must_use]
pub fn to_file(views: &[SavedView]) -> String {
    let mut out = String::from(
        "# Sunrise saved views. Written by `:save <name>`; safe to edit.\n\
         # <name> = \"view=today;query=…;contexts=a,b\"\n",
    );
    for v in views {
        out.push_str(&v.to_line());
        out.push('\n');
    }
    out
}

/// `~/.config/sunrise/views.toml`, honouring `XDG_CONFIG_HOME`.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("sunrise").join("views.toml"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("sunrise")
            .join("views.toml"),
    )
}

/// Wire name for a view.
const fn view_name(v: View) -> &'static str {
    match v {
        View::Today => "today",
        View::Inbox => "inbox",
        View::Stream => "stream",
        View::Search => "search",
        View::Focus => "focus",
        View::Routines => "routines",
        View::Review => "review",
    }
}

/// The inverse of [`view_name`], accepting the `:view` spellings too.
fn parse_view_name(s: &str) -> Option<View> {
    Some(match s {
        "today" => View::Today,
        "inbox" => View::Inbox,
        "stream" | "browse" => View::Stream,
        "search" => View::Search,
        "focus" => View::Focus,
        "routines" | "routine" => View::Routines,
        "review" | "stats" => View::Review,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(name: &str, view: View, query: &str, contexts: &[&str]) -> SavedView {
        SavedView {
            name: name.into(),
            view,
            query: query.into(),
            contexts: contexts.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn a_saved_view_round_trips_through_the_file_format() {
        let v = saved("errands", View::Search, "passport", &["errands", "home"]);
        let parsed = parse_view("errands", &v.spec()).expect("parses");
        assert_eq!(parsed, v);
    }

    #[test]
    fn only_the_parts_that_carry_information_are_written() {
        assert_eq!(saved("t", View::Today, "", &[]).spec(), "view=today");
    }

    #[test]
    fn an_at_sign_on_a_context_is_accepted_and_dropped() {
        // Users type `@home`; the stored name has no sigil.
        let v = parse_view("x", "view=today;contexts=@home, @deep-work").expect("parses");
        assert_eq!(v.contexts, vec!["home", "deep-work"]);
    }

    #[test]
    fn an_unknown_field_is_an_error_not_a_shrug() {
        // A typo'd `context=` that silently saved no filter would recall the
        // wrong thing forever.
        let e = parse_view("x", "view=today;context=home").unwrap_err();
        assert!(e.contains("unknown field"), "{e}");
        assert!(parse_view("x", "query=hi").is_err(), "a view= is required");
        assert!(parse_view("x", "view=nope").is_err());
    }

    #[test]
    fn a_malformed_line_costs_that_view_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("sunrise-views-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let path = dir.join("views.toml");
        std::fs::write(
            &path,
            "good = \"view=inbox\"\nbad = \"view=nonsense\"\nalso_good = \"view=today\"\n",
        )
        .expect("write");
        let (views, warnings) = load(Some(&path));
        assert_eq!(views.len(), 2, "the two readable lines survive");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("bad"), "{warnings:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absent_file_is_silent() {
        let (views, warnings) = load(Some(std::path::Path::new("/nonexistent/views.toml")));
        assert!(views.is_empty());
        assert!(warnings.is_empty(), "absent is the normal case");
    }

    #[test]
    fn the_file_body_reparses_to_what_it_was_written_from() {
        let views = vec![
            saved("a", View::Today, "", &[]),
            saved("b", View::Search, "milk", &["home"]),
        ];
        let dir = std::env::temp_dir().join(format!("sunrise-views-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tempdir");
        let path = dir.join("views.toml");
        std::fs::write(&path, to_file(&views)).expect("write");
        let (back, warnings) = load(Some(&path));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(back, views);
        std::fs::remove_dir_all(&dir).ok();
    }
}
