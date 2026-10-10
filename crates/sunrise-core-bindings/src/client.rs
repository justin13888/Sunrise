//! Client-side logic that is not any one client's, across the seam.
//!
//! Two things sit between `sunrise-core` and a user interface without
//! belonging to either: undo/redo, and saved views. `sunrise-client-core`
//! holds both — rescued from the deleted TUI so the next client would not
//! reimplement them — and until this module existed it held them for nobody:
//! the crate had **zero consumers**. A correct, well-tested library that no
//! shipping binary can reach is the failure this epic keeps finding, so the
//! point of this module is reachability, not novelty.
//!
//! # Undo is a new write
//!
//! [`sunrise_client_core::undo`] builds *inverse commands*: the command that
//! restores the fields another command is about to change. That is not a
//! rollback, and the difference is visible to the user:
//!
//! * Undoing a completion is a re-open op. It converges like any other write,
//!   and a device undoing what another has since changed loses the LWW tie
//!   exactly as a manual edit would.
//! * **Undoing a create deletes what it made**, and that step is recorded
//!   *after* the write rather than before it — the id to delete is the one the
//!   core just minted, and it does not exist a moment earlier. Redoing it
//!   creates a fresh entity with a fresh id, so the step is re-bound to that
//!   id as it is replayed; see [`sunrise_client_core::undo::rebind_creates`].
//! * **A delete cannot be undone.** The core writes a tombstone and has no
//!   restore op. The seam says so — [`UndoRefusal::Deleted`] — rather than
//!   accepting the step and offering an undo that would silently do nothing.
//! * **A defer's counter does not come back down.** `deferred_count` is a
//!   PN-counter; undo restores the date and the count keeps its history. A
//!   review that said "deferred three times" should not change its mind
//!   because one of them was undone.
//!
//! The rows the inverse is read from come from the **vault**, not from
//! whatever the client happens to be holding. `EntityLookup` was written as a
//! lookup precisely so each client could supply its own rows; here the vault
//! is both the nearest and the most honest source, and it means a row that has
//! scrolled off screen is still undoable.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use sunrise_client_core::undo::{
    invert, invert_create, EntityLookup, NotUndoable, UndoEntry, MAX_DEPTH,
};
use sunrise_client_core::views;
use sunrise_client_core::views::View;
use sunrise_core::queries::{ContextRow, StreamRow};
use sunrise_core::{Command, Core, Query, QueryResult};
use sunrise_domain::{RoutineRow, Task};
use sunrise_id::EntityRef;

use crate::dto::{EditGate, PreferenceItem};
use crate::{BindingError, SunriseCore};

// ---------------------------------------------------------------------------
// Undo / redo
// ---------------------------------------------------------------------------

/// Why a command left nothing to undo.
///
/// Exported rather than collapsed into a `bool` because the two cases mean
/// different things on screen: a delete is worth a confirmation *before* it
/// happens, and an unsupported command is simply not something the undo stack
/// tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum UndoRefusal {
    /// A tombstone was written. The core has no restore op, so there is
    /// nothing to invert into.
    Deleted,
    /// Nothing about the command is reversible.
    Unsupported,
}

impl From<NotUndoable> for UndoRefusal {
    fn from(n: NotUndoable) -> Self {
        match n {
            NotUndoable::Deleted => Self::Deleted,
            NotUndoable::Unsupported => Self::Unsupported,
        }
    }
}

/// What to tell the user when a step could not be recorded.
///
/// Exported rather than left to each client's `switch`. "Deleting cannot be
/// undone" is one sentence and therefore exactly the sort of thing two clients
/// word differently — and the two wordings would then disagree about *why*,
/// which is the only part that matters.
#[uniffi::export]
#[must_use]
pub fn undo_refusal_explanation(refusal: UndoRefusal) -> String {
    match refusal {
        UndoRefusal::Deleted => {
            "Deleting cannot be undone \u{2014} the vault keeps a tombstone.".into()
        }
        UndoRefusal::Unsupported => "This cannot be undone.".into(),
    }
}

/// What [`crate::SunriseCore::submit_undoable`] did.
#[derive(Debug, Clone, uniffi::Record)]
pub struct UndoableOutcome {
    /// What the command itself did.
    pub outcome: crate::dto::CommandOutcome,
    /// Absent when the step went on the undo stack; otherwise why it did not.
    pub not_undoable: Option<UndoRefusal>,
}

/// What can be undone or redone right now.
///
/// Labels rather than booleans: a menu item reading "Undo complete “Renew
/// passport”" is the difference between an undo the user trusts and one they
/// try in order to find out what it does.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct UndoState {
    /// Label of the step `undo` would reverse, if there is one.
    pub undo_label: Option<String>,
    /// Label of the step `redo` would replay, if there is one.
    pub redo_label: Option<String>,
}

/// The two stacks, and the rule that binds them.
#[derive(Debug, Default)]
pub(crate) struct UndoStacks {
    done: Vec<UndoEntry>,
    undone: Vec<UndoEntry>,
}

impl UndoStacks {
    /// Record a step. Clears the redo stack: once a new write lands, the
    /// branch that was undone is no longer reachable, and offering to "redo"
    /// into it would replay a command built against a vault that has moved.
    pub(crate) fn push(&mut self, entry: UndoEntry) {
        self.undone.clear();
        if self.done.len() == MAX_DEPTH {
            self.done.remove(0);
        }
        self.done.push(entry);
    }

    pub(crate) fn take_undo(&mut self) -> Option<UndoEntry> {
        self.done.pop()
    }

    pub(crate) fn take_redo(&mut self) -> Option<UndoEntry> {
        self.undone.pop()
    }

    /// A step that was just undone, ready to be replayed. Stored flipped, so
    /// `redo` submits its `backward` exactly as `undo` does.
    pub(crate) fn push_undone(&mut self, entry: UndoEntry) {
        self.undone.push(entry.flipped());
    }

    /// A step that was just redone, back on the undo stack. Deliberately does
    /// **not** clear the redo stack — a redo is not a new branch.
    pub(crate) fn push_redone(&mut self, entry: UndoEntry) {
        if self.done.len() == MAX_DEPTH {
            self.done.remove(0);
        }
        self.done.push(entry.flipped());
    }

    pub(crate) fn state(&self) -> UndoState {
        UndoState {
            undo_label: self.done.last().map(|e| e.label.clone()),
            redo_label: self.undone.last().map(|e| e.label.clone()),
        }
    }
}

/// The rows an inverse command is read from, fetched from the vault.
///
/// Only what the batch actually needs: a task edit costs one `EntityById`, and
/// nothing else is read at all.
#[derive(Default)]
pub(crate) struct VaultRows {
    tasks: BTreeMap<EntityRef, Task>,
    streams: Vec<StreamRow>,
    contexts: Vec<ContextRow>,
    routines: Vec<RoutineRow>,
}

impl EntityLookup for VaultRows {
    fn task(&self, id: EntityRef) -> Option<&Task> {
        self.tasks.get(&id)
    }
    fn stream(&self, id: EntityRef) -> Option<&StreamRow> {
        self.streams.iter().find(|s| s.id == id)
    }
    fn context(&self, id: EntityRef) -> Option<&ContextRow> {
        self.contexts.iter().find(|c| c.id == id)
    }
    fn routine(&self, id: EntityRef) -> Option<&RoutineRow> {
        self.routines.iter().find(|r| r.id == id)
    }
}

/// Which reads `cmds` will need before it is applied.
#[derive(Default)]
struct Wanted {
    tasks: Vec<EntityRef>,
    streams: bool,
    contexts: bool,
    routines: bool,
}

/// Read exactly what [`invert`] will ask for, **before** `cmds` is applied.
///
/// Once the write has landed the old values are gone, so the ordering here is
/// the whole correctness argument.
pub(crate) async fn snapshot_for(core: &Core, cmds: &[Command]) -> Result<VaultRows, BindingError> {
    let mut wanted = Wanted::default();
    for cmd in cmds {
        match cmd {
            Command::CompleteTask(id)
            | Command::DeferTask { id, .. }
            | Command::UpdateTask { id, .. }
            | Command::PromoteToStream { id, .. } => wanted.tasks.push(*id),
            Command::UpdateStream { .. } => wanted.streams = true,
            Command::UpdateContext { .. } => wanted.contexts = true,
            Command::UpdateRoutine { .. } => wanted.routines = true,
            // Everything else is either a tombstone or has no inverse; both
            // reach `invert` as a refusal and need no rows read for them.
            _ => {}
        }
    }

    let mut rows = VaultRows::default();
    for id in wanted.tasks {
        if let QueryResult::Task(t) = core.query(Query::EntityById(id)).await? {
            rows.tasks.insert(id, *t);
        }
    }
    if wanted.streams {
        if let QueryResult::Streams(s) = core.query(Query::StreamList).await? {
            rows.streams = s;
        }
    }
    if wanted.contexts {
        if let QueryResult::Contexts(c) = core.query(Query::Contexts).await? {
            rows.contexts = c;
        }
    }
    if wanted.routines {
        if let QueryResult::Routines(r) = core.query(Query::Routines).await? {
            // `RoutineRow` is what `EntityLookup` speaks, and the projection
            // that makes one is the domain's — not a shape invented here.
            let now =
                jiff::Timestamp::from_millisecond(i64::try_from(core.now_ms()).unwrap_or(i64::MAX))
                    .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
            rows.routines = sunrise_domain::routine_rows(&r, now);
        }
    }
    Ok(rows)
}

/// Build the step that undoes `cmds`, or say why it cannot be built.
pub(crate) async fn record(
    core: &Core,
    label: String,
    cmds: Vec<Command>,
) -> Result<UndoEntry, UndoRefusal> {
    let rows = snapshot_for(core, &cmds)
        .await
        // A read that failed is not evidence the command is irreversible, but
        // it is evidence this step cannot be trusted to reverse — which is the
        // same answer as far as the user is concerned, and the honest one.
        .map_err(|_| UndoRefusal::Unsupported)?;
    let backward = invert(&rows, &cmds).map_err(UndoRefusal::from)?;
    Ok(UndoEntry {
        label,
        forward: cmds,
        backward,
    })
}

/// Build the step that undoes a create, from the id the core just minted.
///
/// The mirror image of [`record`], and the ordering is inverted for the same
/// reason it is fixed there: an edit's inverse reads values the write is about
/// to overwrite, so it must be built *first*; a create's inverse names an
/// entity that does not exist until the write lands, so it can only be built
/// *after*. `created` is the `CommandResult::entity` of that write.
pub(crate) fn record_create(
    label: String,
    cmd: Command,
    created: EntityRef,
) -> Result<UndoEntry, UndoRefusal> {
    let backward = vec![invert_create(&cmd, created).map_err(UndoRefusal::from)?];
    Ok(UndoEntry {
        label,
        forward: vec![cmd],
        backward,
    })
}

// ---------------------------------------------------------------------------
// Saved views
// ---------------------------------------------------------------------------

/// See [`sunrise_client_core::views::View`] — which primary view a saved view
/// names.
///
/// **Named `PrimaryView`, not `View`, on purpose.** This is the same hazard as
/// [`crate::dto::TaskItem`]: a generated Swift `View` shadows `SwiftUI.View`,
/// and every `struct SomeScreen: View` in the app then fails to compile
/// against an enum. Kotlin has no such collision, but the name has to be
/// chosen once for all targets.
///
/// Mirrored rather than declared with `#[uniffi::remote]`, which is what the
/// rename costs — remote requires the upstream name. The exhaustive `match`
/// in both directions below buys the drift-proofing back: adding a view
/// upstream fails this crate's build, exactly as a remote declaration would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PrimaryView {
    /// Today: scheduled blocks, due-today tasks, manually-pulled tasks.
    Today,
    /// Inbox: unassigned tasks awaiting triage.
    Inbox,
    /// Browse one stream or context.
    Stream,
    /// Free-text search.
    Search,
    /// The calendar grid: day and week time-blocking.
    Calendar,
    /// Focus mode: one task, one session.
    Focus,
    /// Routines: recurring templates, with full CRUD.
    Routines,
    /// Review: weekly, daily, trends and snapshot history.
    Review,
    /// The morning summary: what today looks like before it starts.
    Morning,
    /// The end-of-day brief: what landed, what did not, what tomorrow holds.
    Evening,
}

impl From<View> for PrimaryView {
    fn from(v: View) -> Self {
        match v {
            View::Today => Self::Today,
            View::Inbox => Self::Inbox,
            View::Stream => Self::Stream,
            View::Search => Self::Search,
            View::Calendar => Self::Calendar,
            View::Focus => Self::Focus,
            View::Routines => Self::Routines,
            View::Review => Self::Review,
            View::Morning => Self::Morning,
            View::Evening => Self::Evening,
        }
    }
}

impl From<PrimaryView> for View {
    fn from(v: PrimaryView) -> Self {
        match v {
            PrimaryView::Today => Self::Today,
            PrimaryView::Inbox => Self::Inbox,
            PrimaryView::Stream => Self::Stream,
            PrimaryView::Search => Self::Search,
            PrimaryView::Calendar => Self::Calendar,
            PrimaryView::Focus => Self::Focus,
            PrimaryView::Routines => Self::Routines,
            PrimaryView::Review => Self::Review,
            PrimaryView::Morning => Self::Morning,
            PrimaryView::Evening => Self::Evening,
        }
    }
}

/// A named view: where to be, what to search for, what to narrow to.
///
/// See [`sunrise_client_core::views::SavedView`]. Contexts are carried by
/// **name**, not by id, and that is not a shortcut: an `EntityRef` is a
/// vault-local ULID, so a view saved on one machine and read on a paired one
/// would resolve to an empty set and look like an empty vault.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SavedView {
    /// The name it is recalled by.
    pub name: String,
    /// Which primary view.
    pub view: PrimaryView,
    /// Search text, for the Search view. Empty means none.
    pub query: String,
    /// Context names to narrow to. Empty means no filter.
    pub contexts: Vec<String>,
    /// The one-line summary the domain words, so a picker does not word it
    /// twice.
    pub summary: String,
}

impl From<&views::SavedView> for SavedView {
    fn from(v: &views::SavedView) -> Self {
        Self {
            name: v.name.clone(),
            view: v.view.into(),
            query: v.query.clone(),
            contexts: v.contexts.clone(),
            summary: v.describe(),
        }
    }
}

impl From<SavedView> for views::SavedView {
    fn from(v: SavedView) -> Self {
        Self {
            name: v.name,
            view: v.view.into(),
            query: v.query,
            contexts: v.contexts,
        }
    }
}

/// What one read of the views file found.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SavedViewFile {
    /// The views that parsed.
    pub views: Vec<SavedView>,
    /// One message per line that did not.
    ///
    /// Warnings, not an error: a typo in a preference file costs that view and
    /// nothing else. Losing the whole set — or the app — to one bad line is
    /// the failure this shape exists to avoid.
    pub warnings: Vec<String>,
}

/// The saved-view file, read and written.
///
/// An object rather than free functions because the path is state: tests point
/// at a scratch file, and the app points at the same
/// `~/.config/sunrise/views.toml` `sunrise-cli` uses — so a view saved in one
/// is recalled in the other.
///
/// The methods are **sync**. The file is a few hundred bytes of TOML; a caller
/// that wants it off its main thread should hop threads itself rather than
/// have the seam pretend the work is asynchronous.
#[derive(Debug, uniffi::Object)]
pub struct SavedViews {
    path: Option<PathBuf>,
}

#[uniffi::export]
impl SavedViews {
    /// `$XDG_CONFIG_HOME/sunrise/views.toml`, else `~/.config/…`.
    ///
    /// Yields a store with no path when neither is resolvable, which reads as
    /// empty and refuses to save — rather than inventing a location.
    #[uniffi::constructor]
    #[must_use]
    pub fn at_default_path() -> Arc<Self> {
        Arc::new(Self {
            path: views::config_path(),
        })
    }

    /// A store at an explicit path.
    #[uniffi::constructor]
    #[must_use]
    pub fn at_path(path: String) -> Arc<Self> {
        Arc::new(Self {
            path: Some(PathBuf::from(path)),
        })
    }

    /// Where this store reads and writes, if anywhere.
    #[must_use]
    pub fn location(&self) -> Option<String> {
        self.path.as_ref().map(|p| p.to_string_lossy().into_owned())
    }

    /// Read the file. An absent file is an empty set, not an error.
    #[must_use]
    pub fn load(&self) -> SavedViewFile {
        let (views, warnings) = views::load(self.path.as_deref());
        SavedViewFile {
            views: views.iter().map(SavedView::from).collect(),
            warnings,
        }
    }

    /// Replace the file with `views`.
    ///
    /// Whole-file, because the file is the set: a partial write would leave
    /// the store disagreeing with what the picker just showed.
    pub fn save(&self, views_to_write: Vec<SavedView>) -> Result<(), BindingError> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| BindingError::Core("no saved-views path on this device".into()))?;
        let rows: Vec<views::SavedView> = views_to_write.into_iter().map(Into::into).collect();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| BindingError::Core(e.to_string()))?;
        }
        std::fs::write(path, views::to_file(&rows)).map_err(|e| BindingError::Core(e.to_string()))
    }
}

/// Read one `name = "view=…;query=…;contexts=…"` pair.
///
/// Exported so a client can validate a spec a user typed before writing it,
/// using the same reader that will parse it back.
///
/// # Errors
///
/// A message naming what could not be read. An unknown key inside the spec is
/// an error rather than being ignored: a typo'd `context=` that silently saved
/// no filter would recall the wrong thing forever.
#[uniffi::export]
pub fn parse_saved_view(name: String, spec: String) -> Result<SavedView, BindingError> {
    views::parse_view(&name, &spec)
        .map(|v| SavedView::from(&v))
        .map_err(BindingError::Core)
}

// ---------------------------------------------------------------------------
// Bootstrap preferences
// ---------------------------------------------------------------------------

/// The bootstrap preferences (`sync.relay_url`, `auth.oidc_issuer`,
/// `auth.oidc_client_id`) held in `dir`, resolved. Read without a vault: these
/// are what a device needs to find and unlock one.
///
/// `dir` is the directory the client keeps its vaults in; the file is
/// `preferences.bootstrap.json` there.
///
/// # Errors
/// The file exists and cannot be read.
#[uniffi::export]
pub fn bootstrap_preferences(dir: String) -> Result<Vec<PreferenceItem>, BindingError> {
    let file = read_bootstrap_file(std::path::Path::new(&dir))?;
    Ok(sunrise_core::BootstrapPreferences::resolve(file.as_deref())
        .into_iter()
        .map(PreferenceItem::from)
        .collect())
}

/// Set one bootstrap preference in `dir`, or remove it with `None`. The file
/// is written to a sibling, flushed to disk, and renamed over the old one, so
/// a crash leaves
/// one whole file or the other.
///
/// # Errors
/// A key that is not a bootstrap key, a value that does not fit it, or a
/// file that cannot be written.
#[uniffi::export]
pub fn set_bootstrap_preference(
    dir: String,
    key: String,
    value: Option<String>,
) -> Result<(), BindingError> {
    let dir = std::path::Path::new(&dir);
    let file = read_bootstrap_file(dir)?;
    let body = sunrise_core::BootstrapPreferences::set(file.as_deref(), &key, value.as_deref())
        .map_err(|e| BindingError::Core(e.to_string()))?;
    let io = |e: std::io::Error| BindingError::Core(format!("bootstrap preferences: {e}"));
    std::fs::create_dir_all(dir).map_err(io)?;
    let path = dir.join(sunrise_core::BOOTSTRAP_FILE);
    let tmp = path.with_extension("json.tmp");
    // Flushed before the rename: without it a power loss can make the rename
    // durable ahead of the bytes, leaving a truncated file that reads as empty
    // and loses the relay and sign-in settings.
    let mut f = std::fs::File::create(&tmp).map_err(io)?;
    std::io::Write::write_all(&mut f, &body).map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(io)
}

/// The bootstrap file's bytes in `dir`, `None` when there is none.
fn read_bootstrap_file(dir: &std::path::Path) -> Result<Option<Vec<u8>>, BindingError> {
    match std::fs::read(dir.join(sunrise_core::BOOTSTRAP_FILE)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(BindingError::Core(format!("bootstrap preferences: {e}"))),
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;
    use crate::dto::PreferenceValue;

    #[test]
    fn the_bootstrap_file_is_written_whole_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vaults");
        let at = path.to_string_lossy().into_owned();
        assert!(bootstrap_preferences(at.clone())
            .unwrap()
            .iter()
            .all(|p| p.value.is_none()));
        set_bootstrap_preference(
            at.clone(),
            "sync.relay_url".into(),
            Some("wss://relay.example/sync".into()),
        )
        .unwrap();
        let relay = bootstrap_preferences(at.clone())
            .unwrap()
            .into_iter()
            .find(|p| p.key == "sync.relay_url")
            .unwrap();
        assert_eq!(
            relay.value,
            Some(PreferenceValue::Text {
                value: "wss://relay.example/sync".into()
            })
        );
        assert!(path.join(sunrise_core::BOOTSTRAP_FILE).exists());
        assert!(!path.join("preferences.bootstrap.json.tmp").exists());
        assert!(set_bootstrap_preference(at, "week_start".into(), Some("MO".into())).is_err());
    }
}

// ---------------------------------------------------------------------------
// Feature gate
// ---------------------------------------------------------------------------

#[uniffi::export]
impl SunriseCore {
    /// What this build may edit in this vault (ADR-0045 §8).
    ///
    /// Re-read it on every change batch: the op that locks a scope arrives by
    /// sync and is reported as an update to the vault-meta stream. Sync and
    /// reads carry on whatever it says.
    pub fn edit_gate(&self) -> Result<EditGate, BindingError> {
        Ok(EditGate::from_missing(&self.inner.missing_features()?))
    }
}

/// Whether `gate` leaves edits to the entity tagged `tag` (`"task"`,
/// `"stream"`, …) open.
///
/// Exported so a client asks one question per action rather than re-deriving
/// the rule from the record's fields.
#[uniffi::export]
#[must_use]
pub fn edit_gate_allows(gate: EditGate, tag: String) -> bool {
    !(gate.locks_all || gate.locked_tags.contains(&tag))
}

#[cfg(test)]
mod feature_gate_tests {
    use super::*;
    use crate::dto::{FeatureLock, MissingFeatureItem};
    use sunrise_core::{FeatureScope, MissingFeature};
    use sunrise_id::EntityKind;

    fn missing(id: &str, scope: FeatureScope) -> MissingFeature {
        MissingFeature {
            id: id.into(),
            scope,
        }
    }

    /// A refused write crosses the seam as its own variant, not as a `Core`
    /// message a client would have to parse.
    #[test]
    fn a_feature_refusal_crosses_as_feature_missing() {
        let e = BindingError::from(sunrise_core::CoreError::Engine(
            sunrise_core::EngineError::FeatureMissing {
                feature: "task.x".into(),
            },
        ));
        assert!(matches!(e, BindingError::FeatureMissing { feature } if feature == "task.x"));
    }

    #[test]
    fn nothing_missing_is_an_open_vault() {
        let gate = EditGate::from_missing(&[]);
        assert_eq!(gate, EditGate::default());
        assert!(edit_gate_allows(gate, "task".into()));
    }

    #[test]
    fn an_entity_feature_locks_that_entity_only() {
        let gate =
            EditGate::from_missing(&[missing("task.x", FeatureScope::Entity(EntityKind::Task))]);
        assert!(gate.read_only);
        assert!(!gate.locks_all);
        assert_eq!(gate.locked_tags, vec!["task".to_owned()]);
        assert_eq!(
            gate.missing,
            vec![MissingFeatureItem {
                feature: "task.x".into(),
                lock: FeatureLock::Entity { tag: "task".into() },
            }]
        );
        assert!(!edit_gate_allows(gate.clone(), "task".into()));
        assert!(edit_gate_allows(gate, "stream".into()));
    }

    #[test]
    fn a_structural_feature_locks_everything() {
        let gate = EditGate::from_missing(&[
            missing("core.x", FeatureScope::Structural),
            missing("task.x", FeatureScope::Entity(EntityKind::Task)),
        ]);
        assert!(gate.locks_all);
        assert!(gate.locked_tags.is_empty());
        assert!(!edit_gate_allows(gate, "stream".into()));
    }

    /// A feature of an entity this build lacks locks nothing, and the banner
    /// still shows: the user is missing data this build cannot display.
    #[test]
    fn an_unknown_entitys_feature_shows_the_banner_and_locks_nothing() {
        let gate = EditGate::from_missing(&[missing(
            "place.entity",
            FeatureScope::UnknownEntity("place".into()),
        )]);
        assert!(gate.read_only);
        assert!(!gate.locks_all);
        assert!(gate.locked_tags.is_empty());
        assert!(edit_gate_allows(gate, "task".into()));
    }
}
