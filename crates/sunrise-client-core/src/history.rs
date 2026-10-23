//! A session's undo history against a live vault.
//!
//! [`crate::undo`] builds an inverse from rows it is handed; this module is
//! the rest of what a client needs to offer undo: the two stacks and the rule
//! that binds them, the rows read **from the vault** before a write lands, and
//! the replay that re-binds a create to the entity it minted. It used to live
//! in the `UniFFI` seam, where only the Apple clients could reach it.
//!
//! # Undo is a new write
//!
//! * Undoing a completion is a re-open op. It converges like any other write,
//!   and a device undoing what another has since changed loses the LWW tie
//!   exactly as a manual edit would.
//! * **Undoing a create deletes what it made**, and that step is recorded
//!   *after* the write rather than before it — the id to delete is the one the
//!   core just minted, and it does not exist a moment earlier. Redoing it
//!   creates a fresh entity with a fresh id, so the step is re-bound to that
//!   id as it is replayed; see [`crate::undo::rebind_creates`].
//! * **A delete cannot be undone.** The core writes a tombstone and has no
//!   restore op, so [`UndoHistory::submit`] says so —
//!   [`NotUndoable::Deleted`] — rather than accepting the step and offering an
//!   undo that would silently do nothing.
//!
//! The rows the inverse is read from come from the **vault**, not from
//! whatever the client happens to be holding. [`EntityLookup`] was written as
//! a lookup precisely so each client could supply its own rows; the vault is
//! both the nearest and the most honest source, and it means a row that has
//! scrolled off screen is still undoable.
//!
//! The history is session-scoped and deliberately not persisted: an inverse
//! command is only meaningful against the vault state it was read from. It is
//! also **not** synced — undo is this device's record of what *it* did.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use sunrise_core::queries::{ContextRow, StreamRow};
use sunrise_core::{Command, CommandResult, Core, CoreError, Query, QueryResult};
use sunrise_domain::{RoutineRow, Task};
use sunrise_id::EntityRef;

pub mod persist;

use crate::undo::{
    invert, invert_create, is_create, rebind_creates, EntityLookup, NotUndoable, UndoEntry,
    MAX_DEPTH,
};

/// The two stacks, and the rule that binds them.
#[derive(Debug, Default, Clone)]
pub struct UndoStacks {
    done: Vec<UndoEntry>,
    undone: Vec<UndoEntry>,
}

/// What can be undone or redone right now, by label.
///
/// Labels rather than booleans: a menu item reading "Undo complete “Renew
/// passport”" is the difference between an undo the user trusts and one they
/// try in order to find out what it does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoLabels {
    /// Label of the step an undo would reverse, if there is one.
    pub undo: Option<String>,
    /// Label of the step a redo would replay, if there is one.
    pub redo: Option<String>,
}

impl UndoStacks {
    /// Record a step. Clears the redo stack: once a new write lands, the
    /// branch that was undone is no longer reachable, and offering to "redo"
    /// into it would replay a command built against a vault that has moved.
    pub fn push(&mut self, entry: UndoEntry) {
        self.undone.clear();
        self.push_done(entry);
    }

    /// The most recent step, off the undo stack.
    pub fn take_undo(&mut self) -> Option<UndoEntry> {
        self.done.pop()
    }

    /// The most recently undone step, off the redo stack.
    pub fn take_redo(&mut self) -> Option<UndoEntry> {
        self.undone.pop()
    }

    /// A step that was just undone, ready to be replayed. Stored flipped, so
    /// a redo submits its `backward` exactly as an undo does.
    pub fn push_undone(&mut self, entry: UndoEntry) {
        self.undone.push(entry.flipped());
    }

    /// A step that was just redone, back on the undo stack. Deliberately does
    /// **not** clear the redo stack — a redo is not a new branch.
    pub fn push_redone(&mut self, entry: UndoEntry) {
        self.push_done(entry.flipped());
    }

    /// The labels of the two tops.
    #[must_use]
    pub fn labels(&self) -> UndoLabels {
        UndoLabels {
            undo: self.done.last().map(|e| e.label.clone()),
            redo: self.undone.last().map(|e| e.label.clone()),
        }
    }

    fn push_done(&mut self, entry: UndoEntry) {
        if self.done.len() == MAX_DEPTH {
            self.done.remove(0);
        }
        self.done.push(entry);
    }
}

/// The rows an inverse command is read from, fetched from the vault.
///
/// Only what the batch actually needs: a task edit costs one `EntityById`, and
/// nothing else is read at all.
#[derive(Debug, Default)]
pub struct VaultRows {
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
#[derive(Debug, Default, PartialEq, Eq)]
struct Wanted {
    tasks: Vec<EntityRef>,
    streams: bool,
    contexts: bool,
    routines: bool,
}

/// What [`invert`] will look up for `cmds`, decided without reading anything.
fn wanted(cmds: &[Command]) -> Wanted {
    let mut wanted = Wanted::default();
    for cmd in cmds {
        match cmd {
            Command::CompleteTask(id)
            | Command::DeferTask { id, .. }
            | Command::DeferTaskTo { id, .. }
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
    wanted
}

/// Read exactly what [`invert`] will ask for, **before** `cmds` is applied.
///
/// Once the write has landed the old values are gone, so the ordering here is
/// the whole correctness argument.
///
/// # Errors
/// Whatever the vault's reads raise.
pub async fn snapshot_for(core: &Core, cmds: &[Command]) -> Result<VaultRows, CoreError> {
    let wanted = wanted(cmds);
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
            // that makes one is the domain's — not a shape invented here. The
            // instant is the core's injected clock, never an ambient one.
            let now =
                jiff::Timestamp::from_millisecond(i64::try_from(core.now_ms()).unwrap_or(i64::MAX))
                    .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
            rows.routines = sunrise_domain::routine_rows(&r, now);
        }
    }
    Ok(rows)
}

/// Build the step that undoes `cmds`, or say why it cannot be built.
///
/// # Errors
/// [`NotUndoable`] when a command has no inverse, and
/// [`NotUndoable::Unsupported`] when the rows could not be read: a read that
/// failed is not evidence the command is irreversible, but it is evidence this
/// step cannot be trusted to reverse — the same answer as far as the user is
/// concerned, and the honest one.
pub async fn record(
    core: &Core,
    label: String,
    cmds: Vec<Command>,
) -> Result<UndoEntry, NotUndoable> {
    let rows = snapshot_for(core, &cmds)
        .await
        .map_err(|_| NotUndoable::Unsupported)?;
    let backward = invert(&rows, &cmds)?;
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
///
/// # Errors
/// [`NotUndoable`] when `cmd` is not a create this crate can invert.
pub fn record_create(
    label: String,
    cmd: Command,
    created: EntityRef,
) -> Result<UndoEntry, NotUndoable> {
    let backward = vec![invert_create(&cmd, created)?];
    Ok(UndoEntry {
        label,
        forward: vec![cmd],
        backward,
    })
}

/// One session's undo history: the stacks, behind a lock so a client can
/// share it across the tasks that submit.
#[derive(Debug, Default)]
pub struct UndoHistory {
    stacks: Mutex<UndoStacks>,
}

impl UndoHistory {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit one command **and record how to reverse it**.
    ///
    /// A command with no inverse is still **submitted** — refusing the write
    /// because it cannot be undone would be a worse client — and the second
    /// half of the answer says why nothing went on the stack.
    ///
    /// # Errors
    /// Whatever the submit raises; nothing is recorded then.
    pub async fn submit(
        &self,
        core: &Core,
        cmd: Command,
        label: String,
    ) -> Result<(CommandResult, Option<NotUndoable>), CoreError> {
        // A create is recorded *after* the write and everything else *before*
        // it, and neither order is a preference: an edit's inverse reads the
        // values the write is about to overwrite, and a create's inverse names
        // the entity the write is about to mint.
        let (res, recorded) = if is_create(&cmd) {
            let res = core.submit(cmd.clone()).await?;
            let recorded = record_create(label, cmd, res.entity);
            (res, recorded)
        } else {
            // Read before write. The order is the whole correctness argument.
            let recorded = record(core, label, vec![cmd.clone()]).await;
            (core.submit(cmd).await?, recorded)
        };
        let not_undoable = match recorded {
            Ok(entry) => {
                self.lock().push(entry);
                None
            }
            Err(refusal) => Some(refusal),
        };
        Ok((res, not_undoable))
    }

    /// Reverse the most recent recorded step. Returns its label, or `None`
    /// when there is nothing to undo.
    ///
    /// # Errors
    /// Whatever a replayed submit raises. The step is not put back then: it
    /// may have half applied, and replaying it again is not known to be safe.
    pub async fn undo(&self, core: &Core) -> Result<Option<String>, CoreError> {
        let Some(mut entry) = self.lock().take_undo() else {
            return Ok(None);
        };
        let minted = apply(core, &entry.backward).await?;
        // If the step being applied contains a create — which is what the redo
        // of an undone create is — the way back out has to name the entity
        // this replay minted, not the one the original create did.
        rebind_creates(&entry.backward, &mut entry.forward, &minted);
        let label = entry.label.clone();
        self.lock().push_undone(entry);
        Ok(Some(label))
    }

    /// Replay the most recently undone step. Returns its label, or `None`.
    ///
    /// # Errors
    /// As [`Self::undo`].
    pub async fn redo(&self, core: &Core) -> Result<Option<String>, CoreError> {
        let Some(mut entry) = self.lock().take_redo() else {
            return Ok(None);
        };
        let minted = apply(core, &entry.backward).await?;
        // Replaying a create makes a *different* entity. Bind the undo that
        // goes back on the stack to that one, or the next undo would delete an
        // already-tombstoned id and leave this entity live for ever.
        rebind_creates(&entry.backward, &mut entry.forward, &minted);
        let label = entry.label.clone();
        self.lock().push_redone(entry);
        Ok(Some(label))
    }

    /// What can be undone or redone right now, for the menu.
    #[must_use]
    pub fn labels(&self) -> UndoLabels {
        self.lock().labels()
    }

    /// A history that carries on from `stacks`: what a client whose session
    /// spans processes read back with [`persist::from_json`].
    #[must_use]
    pub fn from_stacks(stacks: UndoStacks) -> Self {
        Self {
            stacks: Mutex::new(stacks),
        }
    }

    /// The stacks as they stand, for [`persist::to_json`].
    #[must_use]
    pub fn snapshot(&self) -> UndoStacks {
        self.lock().clone()
    }

    /// The stacks. A panic while they were held cannot leave them torn — every
    /// mutation is one `Vec` operation — so a poisoned lock is still read.
    fn lock(&self) -> std::sync::MutexGuard<'_, UndoStacks> {
        self.stacks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Submit a recorded step, stopping at the first failure, and return the
/// entity each command touched.
///
/// The ids matter for exactly one command shape: a create, whose entity is
/// minted here and nowhere else. Everything else returns the id it was already
/// given, which the caller ignores.
async fn apply(core: &Core, cmds: &[Command]) -> Result<Vec<EntityRef>, CoreError> {
    let mut touched = Vec::with_capacity(cmds.len());
    for cmd in cmds {
        touched.push(core.submit(cmd.clone()).await?.entity);
    }
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn entry(label: &str) -> UndoEntry {
        UndoEntry {
            label: label.to_owned(),
            forward: Vec::new(),
            backward: Vec::new(),
        }
    }

    fn id(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [n; 16])
    }

    #[test]
    fn a_new_step_clears_the_redo_stack_but_a_redo_does_not() {
        let mut s = UndoStacks::default();
        s.push(entry("a"));
        let a = s.take_undo().unwrap();
        s.push_undone(a);
        assert_eq!(s.labels().redo.as_deref(), Some("a"));

        let a = s.take_redo().unwrap();
        s.push_redone(a);
        assert_eq!(s.labels().undo.as_deref(), Some("a"));

        let a = s.take_undo().unwrap();
        s.push_undone(a);
        s.push(entry("b"));
        assert_eq!(
            s.labels(),
            UndoLabels {
                undo: Some("b".into()),
                redo: None
            }
        );
    }

    #[test]
    fn the_undo_stack_is_bounded_and_drops_the_oldest() {
        let mut s = UndoStacks::default();
        for n in 0..=MAX_DEPTH {
            s.push(entry(&n.to_string()));
        }
        let mut seen = 0;
        let mut last = None;
        while let Some(e) = s.take_undo() {
            seen += 1;
            last = Some(e.label);
        }
        assert_eq!(seen, MAX_DEPTH);
        assert_eq!(last.as_deref(), Some("1"), "step 0 fell off the bottom");
    }

    /// An undone step is stored flipped, so a redo submits its `backward`
    /// exactly as an undo does.
    #[test]
    fn an_undone_step_is_stored_facing_the_other_way() {
        let mut s = UndoStacks::default();
        s.push(UndoEntry {
            label: "x".into(),
            forward: vec![Command::CompleteTask(id(1))],
            backward: Vec::new(),
        });
        let e = s.take_undo().unwrap();
        s.push_undone(e);
        let r = s.take_redo().unwrap();
        assert!(r.forward.is_empty());
        assert!(matches!(r.backward.as_slice(), [Command::CompleteTask(_)]));
    }

    #[test]
    fn only_what_the_inverse_reads_is_wanted() {
        assert_eq!(
            wanted(&[Command::CompleteTask(id(1)), Command::DeleteTask(id(2))]),
            Wanted {
                tasks: vec![id(1)],
                ..Wanted::default()
            }
        );
        assert_eq!(wanted(&[Command::DeleteTask(id(2))]), Wanted::default());
        // A defer to a time of any kind reads the task it moves, as a defer
        // by span does: the inverse restores its previous `scheduled_at`.
        assert_eq!(
            wanted(&[Command::DeferTaskTo {
                id: id(3),
                to: sunrise_domain::SunriseTime::floating(
                    jiff::civil::date(2026, 6, 2).at(9, 0, 0, 0)
                ),
            }]),
            Wanted {
                tasks: vec![id(3)],
                ..Wanted::default()
            }
        );
    }

    #[test]
    fn a_delete_records_nothing_and_says_why() {
        assert_eq!(
            record_create("x".into(), Command::DeleteTask(id(1)), id(1)).unwrap_err(),
            NotUndoable::Unsupported
        );
    }

    #[test]
    fn an_empty_history_offers_nothing() {
        assert_eq!(UndoHistory::new().labels(), UndoLabels::default());
    }
}
