//! Saved views: the per-device file they used to live in, and what the
//! client layer adds to the synced entity they live in now (#341).
//!
//! A saved view is a `SavedView` entity of the vault
//! (`sunrise_domain::saved_view`): a view saved on one device exists on
//! every device of the account, and names its contexts by id, so renaming a
//! context keeps it working. The core evaluates it (`Query::SavedView`).
//!
//! Before that, a saved view was one line of a per-device file, written in a
//! flat TOML subset this module reads itself:
//!
//! ```toml
//! # ~/.config/sunrise/views.toml
//! errands = "view=search;query=passport;contexts=errands,home"
//! deep    = "view=today;contexts=deep-work"
//! ```
//!
//! The file is read for one purpose now: the one-time import of a device's
//! views into a vault ([`plan_import`]). The CLI runs it the first time
//! `sunrise views` meets a vault and records that it did in
//! [`IMPORT_MARKER`]; it no longer writes the file. The Apple client still
//! reads and writes it through [`SavedViewStore`] until its own import ships
//! (`TODO(#341)`: the Apple client's import and its switch to the entity).
//!
//! The file named contexts by name, because an id meant nothing to another
//! device's file. The import resolves each name to an id once, and a name
//! that resolves to nothing is reported and kept on the view, never dropped.

use std::path::PathBuf;

mod entity;

pub use entity::{
    describe_entity, parse_view_kind, plan_import, primary_view, view_kind, ImportPlan,
    IMPORT_MARKER,
};

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
    /// The calendar grid: day and week time-blocking.
    Calendar,
    /// Focus mode: one task, one session.
    Focus,
    /// Routines: recurring templates, with full CRUD.
    Routines,
    /// Review: the weekly review flow, the daily glance, the trends and the
    /// saved-snapshot history.
    Review,
    /// The morning summary: what today looks like before it starts.
    Morning,
    /// The end-of-day brief: what landed, what did not, what tomorrow holds.
    Evening,
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
/// else, because a typo in a preference file must never cost the user a
/// working app.
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
    let pairs = match parse_pairs(&src) {
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
        "# Sunrise saved views. Written by Sunrise; safe to edit.\n\
         # <name> = \"view=today;query=…;contexts=a,b\"\n",
    );
    for v in views {
        out.push_str(&v.to_line());
        out.push('\n');
    }
    out
}

/// `~/.config/sunrise/views.toml`, honouring `XDG_CONFIG_HOME`, read from
/// this process's environment. See [`config_path_in`].
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_path_in(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Where the views file lives, given the two variables that decide it:
/// `$XDG_CONFIG_HOME/sunrise/views.toml` when that is set and not empty, else
/// `$HOME/.config/sunrise/views.toml`, else nowhere.
///
/// Nowhere is an answer rather than a guess: a store with no path reads as
/// empty and refuses to save.
#[must_use]
pub fn config_path_in(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = xdg_config_home.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join("sunrise").join("views.toml"));
    }
    Some(
        PathBuf::from(home?)
            .join(".config")
            .join("sunrise")
            .join("views.toml"),
    )
}

/// Why the views file was not written.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// The store has no path on this device.
    #[error("no saved-views path on this device")]
    NoPath,
    /// The directory or the file could not be written.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A view holds text the file's subset cannot carry, so a later
    /// [`load`] would drop it or read it back as something else. Nothing was
    /// written.
    #[error("saved view {name:?} cannot be written to the views file: {reason}")]
    Unencodable {
        /// The view's name.
        name: String,
        /// What in it the file cannot carry.
        reason: &'static str,
    },
}

/// Why `v` cannot be written as one line that [`load`] reads back as `v`, or
/// `None` when it can.
///
/// The subset splits a spec on `;` and `=`, a context list on `,`, and the
/// file on lines; it trims every part, drops a leading `@` from a context,
/// and reads an unquoted `#` as the start of a comment. A view is refused
/// when any of those would change it.
fn unencodable(v: &SavedView) -> Option<&'static str> {
    let breaks = |s: &str| s.contains(['\n', '\r']);
    let padded = |s: &str| s.trim() != s;
    if v.name.is_empty() {
        return Some("the name is empty");
    }
    if breaks(&v.name) || breaks(&v.query) || v.contexts.iter().any(|c| breaks(c)) {
        return Some("it holds a line break");
    }
    if padded(&v.name) {
        return Some("the name starts or ends with whitespace");
    }
    if v.name.contains(['=', '"']) {
        return Some("the name holds `=` or `\"`");
    }
    if v.name.starts_with('[') {
        return Some("the name starts with `[`, which reads as a table header");
    }
    if v.query.contains(';') {
        return Some("the query holds `;`");
    }
    if padded(&v.query) {
        return Some("the query starts or ends with whitespace");
    }
    for c in &v.contexts {
        if c.is_empty() || padded(c) {
            return Some("a context name is empty or starts or ends with whitespace");
        }
        if c.contains([',', ';']) {
            return Some("a context name holds `,` or `;`");
        }
        if c.starts_with('@') {
            return Some("a context name starts with `@`");
        }
    }
    let line = v.to_line();
    if strip_comment(&line).len() != line.len() {
        return Some("a `#` in it would be read as the start of a comment");
    }
    None
}

/// The saved-view file at one path, read and written whole.
///
/// The path is state: tests point at a scratch file, and every client points
/// at [`config_path`] — so a view saved in one is recalled in the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedViewStore {
    path: Option<PathBuf>,
}

impl SavedViewStore {
    /// A store at `path`; `None` reads as empty and refuses to save.
    #[must_use]
    pub fn at(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    /// Where this store reads and writes, if anywhere.
    #[must_use]
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }

    /// Read the file. An absent file is an empty set; see [`load`].
    #[must_use]
    pub fn load(&self) -> (Vec<SavedView>, Vec<String>) {
        load(self.path.as_deref())
    }

    /// Replace the file with `views`.
    ///
    /// Whole-file, because the file is the set: a partial write would leave
    /// the store disagreeing with what the picker just showed.
    ///
    /// # Errors
    /// [`SaveError::NoPath`] for a store with no path,
    /// [`SaveError::Unencodable`] for a view the file cannot carry (nothing
    /// is written), and [`SaveError::Io`] when the directory or the file
    /// cannot be written.
    pub fn save(&self, views: &[SavedView]) -> Result<(), SaveError> {
        let path = self.path.as_ref().ok_or(SaveError::NoPath)?;
        if let Some((v, reason)) = views.iter().find_map(|v| Some((v, unencodable(v)?))) {
            return Err(SaveError::Unencodable {
                name: v.name.clone(),
                reason,
            });
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, to_file(views))?;
        Ok(())
    }
}

/// Parse the file's TOML subset: comments, optional `[table]` headers, and
/// `name = "value"` pairs.
///
/// Hand-rolled on purpose: the file is a flat list of string pairs, and a TOML
/// dependency would be the largest in the tree for the smallest grammar in it.
/// Anything outside the subset is an [`Err`] naming the line.
///
/// # Errors
///
/// Returns a message naming the offending line number.
fn parse_pairs(src: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (n, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line.ends_with(']') {
                // A table header is accepted and ignored, so a hand-edited
                // file that grew a `[views]` line still reads as the flat
                // list it is.
                continue;
            }
            return Err(format!("line {}: unterminated table header", n + 1));
        }
        let Some((name, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected `name = \"value\"`", n + 1));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("line {}: missing name", n + 1));
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
/// `tagged = "view=search;query=#urgent"` survives).
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

/// Wire name for a view.
const fn view_name(v: View) -> &'static str {
    match v {
        View::Today => "today",
        View::Inbox => "inbox",
        View::Stream => "stream",
        View::Search => "search",
        View::Calendar => "calendar",
        View::Focus => "focus",
        View::Routines => "routines",
        View::Review => "review",
        View::Morning => "morning",
        View::Evening => "evening",
    }
}

/// The inverse of [`view_name`], also accepting the older spellings a views
/// file written before the names settled may still hold.
fn parse_view_name(s: &str) -> Option<View> {
    Some(match s {
        "today" => View::Today,
        "inbox" => View::Inbox,
        "stream" | "browse" => View::Stream,
        "search" => View::Search,
        "calendar" | "blocks" => View::Calendar,
        "focus" => View::Focus,
        "routines" | "routine" => View::Routines,
        "review" | "stats" => View::Review,
        "morning" | "brief" => View::Morning,
        "evening" | "eod" => View::Evening,
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

    /// Every view the app can be on has a wire name that reads back as
    /// itself. The two daily briefs are in here because they were the ones
    /// missing: without them `Destination::primary` had no word for the
    /// morning-summary and end-of-day screens, and the Save-view menu item
    /// greyed out on both.
    #[test]
    fn every_view_round_trips_through_its_wire_name() {
        for v in [
            View::Today,
            View::Inbox,
            View::Stream,
            View::Search,
            View::Calendar,
            View::Focus,
            View::Routines,
            View::Review,
            View::Morning,
            View::Evening,
        ] {
            // Exhaustive and wildcard-free on purpose: a variant added
            // upstream fails this test's build rather than going untested.
            match v {
                View::Today
                | View::Inbox
                | View::Stream
                | View::Search
                | View::Calendar
                | View::Focus
                | View::Routines
                | View::Review
                | View::Morning
                | View::Evening => {}
            }
            let name = view_name(v);
            assert_eq!(parse_view_name(name), Some(v), "{name}");
            assert_eq!(
                parse_view("x", &format!("view={name}")).map(|s| s.view),
                Ok(v)
            );
        }
    }

    #[test]
    fn the_two_briefs_accept_the_words_a_user_would_type() {
        assert_eq!(parse_view_name("brief"), Some(View::Morning));
        assert_eq!(parse_view_name("eod"), Some(View::Evening));
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

    #[test]
    fn the_store_saves_the_whole_set_and_makes_its_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("views.toml");
        let store = SavedViewStore::at(Some(path.clone()));
        assert_eq!(store.path(), Some(path.as_path()));
        assert_eq!(store.load(), (Vec::new(), Vec::new()));

        let views = vec![saved("deep", View::Today, "", &["deep-work"])];
        store.save(&views).unwrap();
        assert_eq!(store.load(), (views, Vec::new()));

        store.save(&[]).unwrap();
        assert!(store.load().0.is_empty(), "a save replaces, never appends");
    }

    #[test]
    fn a_store_with_no_path_reads_empty_and_refuses_to_save() {
        let store = SavedViewStore::at(None);
        assert_eq!(store.load(), (Vec::new(), Vec::new()));
        let err = store.save(&[]).unwrap_err();
        assert!(matches!(err, SaveError::NoPath));
        assert_eq!(err.to_string(), "no saved-views path on this device");
    }

    #[test]
    fn the_path_follows_xdg_then_home_then_nowhere() {
        assert_eq!(
            config_path_in(Some("/x"), Some("/h")),
            Some(PathBuf::from("/x/sunrise/views.toml"))
        );
        assert_eq!(
            config_path_in(Some(""), Some("/h")),
            Some(PathBuf::from("/h/.config/sunrise/views.toml")),
            "an empty XDG_CONFIG_HOME is unset"
        );
        assert_eq!(
            config_path_in(None, Some("/h")),
            Some(PathBuf::from("/h/.config/sunrise/views.toml"))
        );
        assert_eq!(config_path_in(None, None), None);
    }

    #[test]
    fn a_flat_list_of_pairs_parses() {
        let pairs = parse_pairs("a = \"view=inbox\"\nb = 'view=today'\n").expect("parses");
        assert_eq!(
            pairs,
            vec![
                ("a".to_string(), "view=inbox".to_string()),
                ("b".to_string(), "view=today".to_string()),
            ]
        );
    }

    #[test]
    fn comments_and_table_headers_are_ignored() {
        let src = "# a comment\n[views]\na = \"view=inbox\"  # trailing\n";
        assert_eq!(
            parse_pairs(src).expect("parses"),
            vec![("a".to_string(), "view=inbox".to_string())]
        );
    }

    #[test]
    fn a_hash_inside_a_quoted_value_survives() {
        assert_eq!(
            parse_pairs("t = \"view=search;query=#urgent\"").expect("parses"),
            vec![("t".to_string(), "view=search;query=#urgent".to_string())]
        );
    }

    #[test]
    fn anything_outside_the_subset_names_its_line() {
        for (src, needle) in [
            ("a view=inbox\n", "line 1"),
            ("a = view=inbox\n", "quoted string"),
            ("[views\n", "unterminated"),
            (" = \"view=inbox\"\n", "missing name"),
        ] {
            let e = parse_pairs(src).unwrap_err();
            assert!(e.contains(needle), "{src:?} -> {e}");
        }
    }
}
