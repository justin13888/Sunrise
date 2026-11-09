//! Undo and redo, built from **inverse commands**.
//!
//! `docs/08-features/keyboard.md` lists `u` and `Ctrl-r` on every platform.
//! The core has no undo: it merges by entity-level LWW over an append-only op log
//! ([ADR-0014]), and adding a real one means either an inverse-op journal in
//! the protocol or a snapshot per write. Neither is a client's decision to
//! make.
//!
//! What a client *can* do — correctly, and without touching the wire — is
//! remember what a value was immediately before it changed it and offer to set
//! it back. That is what this module builds: for every command a client
//! submits, the command that restores the fields it touched, read from the
//! rows the client is already holding ([`EntityLookup`]).
//!
//! # What that buys, and what it does not
//!
//! * It is a **new write**, not a rollback. Undo of a completion is a re-open
//!   op, and it converges like any other. Two devices undoing the same thing
//!   is idempotent; a device undoing what another has since changed loses the
//!   LWW tie exactly as a manual edit would. That is the honest behaviour.
//! * A **create is undone by deleting what it minted** ([`invert_create`]).
//!   That inverse is the one that cannot be built by [`invert`]: the id to
//!   delete does not exist until the core has minted it, so it is read off
//!   `CommandResult::entity` *after* the write instead of off a row before it.
//! * A **delete is undone by a restore** (ADR-0065 §3): `Command::Restore`
//!   is a later write of `deleted = false`, so undo of a delete brings the
//!   entity back with the fields it had, and undo of a restore deletes it
//!   again. An attachment has no restore, so detaching one stays
//!   [`NotUndoable::Unsupported`].
//! * A **conflict resolution cannot be undone.** `Command::ResolveConflict`
//!   settles a field by writing a version that descends from every write the
//!   device had seen. Setting the old winner back is an ordinary edit, but
//!   the conflict itself, the concurrent writes, cannot be brought back by
//!   any write, and an undo that put the value back and called the conflict
//!   restored would be lying about it.
//! * A **defer's counter does not come back down.** `deferred_count` is a
//!   PN-counter the engine increments; undo restores the date, and the count
//!   keeps its history. A review that said "deferred three times" should not
//!   change its mind because one of them was undone.
//!
//! # Redoing a create mints a fresh entity
//!
//! Undo of a create deletes the entity; redo replays the create, and the core
//! mints a **new** id for it — there is no id-preserving create command, and
//! inventing one would mean either a caller-supplied id on every create draft
//! or a restore op the merge model does not have. So the step is re-bound
//! instead: [`rebind_creates`] points the replayed step's inverse at the id
//! the replay actually minted, so `create → undo → redo → undo` deletes the
//! *second* entity rather than re-deleting the first and leaking the second.
//!
//! Nothing can be holding a reference to the first id by then. Recording any
//! new write clears the redo stack, so the only path back to a redo is
//! create → undo → redo with no write in between, and a reference to the
//! created entity would itself have been a write.
//!
//! [ADR-0014]: ../../../docs/11-adr/0014-entity-level-lww-merge.md

mod inverse;
#[cfg(test)]
mod tests;

use inverse::invert_one;
use sunrise_core::queries::{ContextRow, StreamRow};
use sunrise_core::Command;
use sunrise_domain::{RoutineRow, SavedView, Task};
use sunrise_id::EntityRef;

/// What [`invert`] needs to read out of whatever the client is holding.
///
/// An inverse command is built from the *current* value of the fields a
/// command is about to change, and the client already has those on screen.
/// This is deliberately a lookup rather than a snapshot type: a terminal list,
/// a `SwiftUI` view model and a test fixture hold their rows differently, and
/// none of them should have to copy them into a shape this module invented.
///
/// A miss is not an error here — it becomes [`NotUndoable::Unsupported`],
/// because a row that has scrolled out of the client's hands cannot be
/// restored from a value nobody remembers.
pub trait EntityLookup {
    /// The task `id`, if this client is holding it.
    fn task(&self, id: EntityRef) -> Option<&Task>;
    /// The stream row `id`, if this client is holding it.
    fn stream(&self, id: EntityRef) -> Option<&StreamRow>;
    /// The context row `id`, if this client is holding it.
    fn context(&self, id: EntityRef) -> Option<&ContextRow>;
    /// The routine row `id`, if this client is holding it.
    fn routine(&self, id: EntityRef) -> Option<&RoutineRow>;
    /// The saved view `id`, if this client is holding it. A client that
    /// holds none can leave this out: an edit of a view is then not
    /// undoable, which is what a miss always means here.
    fn saved_view(&self, id: EntityRef) -> Option<&SavedView> {
        let _ = id;
        None
    }
}

/// How many steps back `u` can walk.
///
/// Bounded because every entry holds full patches: unbounded, a long session
/// of bulk edits would grow the stack without limit for a depth nobody uses.
pub const MAX_DEPTH: usize = 64;

/// One reversible step: what was done, and what undoes it.
#[derive(Debug, Clone)]
pub struct UndoEntry {
    /// Human label for the status line ("completed 3 tasks").
    pub label: String,
    /// The commands that were submitted.
    pub forward: Vec<Command>,
    /// The commands that put things back.
    pub backward: Vec<Command>,
}

impl UndoEntry {
    /// The same step, facing the other way.
    #[must_use]
    pub fn flipped(self) -> Self {
        Self {
            label: self.label,
            forward: self.backward,
            backward: self.forward,
        }
    }
}

/// Why a step could not be recorded, for the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotUndoable {
    /// A tombstone the core could not restore. Kept for clients that match
    /// on it; since `Command::Restore` exists, [`invert`] never returns it.
    Deleted,
    /// Nothing about the command is reversible (or its target is not loaded).
    Unsupported,
}

/// Build the commands that undo `cmds`, or say why they cannot be built.
///
/// Reads the *current* rows, so it must be called **before** the core has
/// applied `cmds`: once the write has landed the old values are gone.
///
/// # Errors
///
/// Returns [`NotUndoable`] if any command in the batch has no inverse. All or
/// nothing on purpose: a half-undone bulk operation is worse than one that
/// refuses, because the user cannot tell which half moved.
pub fn invert<S: EntityLookup + ?Sized>(
    state: &S,
    cmds: &[Command],
) -> Result<Vec<Command>, NotUndoable> {
    let mut out = Vec::with_capacity(cmds.len());
    for cmd in cmds {
        out.push(invert_one(state, cmd)?);
    }
    // Applied in reverse, so a batch that touched one entity twice unwinds in
    // the order it was wound.
    out.reverse();
    Ok(out)
}

/// Whether `cmd` mints a new entity.
///
/// The one shape [`invert`] cannot handle, and the reason is timing rather
/// than semantics: a create *is* reversible — by deleting what it made — but
/// the thing to delete does not exist until the core has made it. A caller
/// routes these to [`invert_create`] after the write instead of reading rows
/// before it.
#[must_use]
pub const fn is_create(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::CreateTask(_)
            | Command::CreateStream(_)
            | Command::CreateContext(_)
            | Command::CreateRoutine(_)
            | Command::CreateBlock(_)
            | Command::AttachFile(_)
            | Command::CreateSavedView(_)
    )
}

/// The inverse of a create: the delete of the entity it minted.
///
/// `created` is `CommandResult::entity` — the id the core assigned. Passing
/// anything else deletes the wrong entity, which is why this takes the result
/// of the write rather than trying to guess from the draft.
///
/// The delete names a **bare `EntityRef`**, and that is *not* the id-only
/// delete [ADR-0014] fixed. The ADR is about ops: `InnerOp::TaskDelete` carries
/// the whole `Task` with `deleted` set, because a tombstone that replaces only
/// an id leaves the rest of the row at whatever each replica happened to hold.
/// `Command::DeleteTask` is a *command*, and the engine builds the op from the
/// row it is about to tombstone: a `Patch` of `deleted` in a vault that
/// requires `core.field_ops`, and that full-state op in one that does not.
/// Commands name entities; ops carry state, and this returns a command.
///
/// # Errors
///
/// [`NotUndoable::Unsupported`] for a command that either creates nothing or
/// creates something with no delete to invert into: `StartFocus` and
/// `SaveReviewSnapshot` mint append-only records, and the core has no command
/// that removes one.
///
/// [ADR-0014]: ../../../docs/11-adr/0014-entity-level-lww-merge.md
pub fn invert_create(cmd: &Command, created: EntityRef) -> Result<Command, NotUndoable> {
    match cmd {
        Command::CreateTask(_) => Ok(Command::DeleteTask(created)),
        Command::CreateStream(_) => Ok(Command::DeleteStream {
            id: created,
            disposition: sunrise_domain::Disposition::Detach,
        }),
        Command::CreateContext(_) => Ok(Command::DeleteContext(created)),
        // Undoing a routine removes the routine, not the occurrences it has
        // already materialized — `DeleteRoutine` stops generation and leaves
        // existing Tasks standing, and undo is a new write, not a rollback.
        Command::CreateRoutine(_) => Ok(Command::DeleteRoutine(created)),
        Command::CreateBlock(_) => Ok(Command::DeleteBlock {
            id: created,
            scope: sunrise_domain::EditScope::All,
        }),
        Command::AttachFile(_) => Ok(Command::DetachFile(created)),
        Command::CreateSavedView(_) => Ok(Command::DeleteSavedView(created)),
        _ => Err(NotUndoable::Unsupported),
    }
}

/// Point the inverses of the creates in `applied` at the entities the core
/// just minted for them.
///
/// `entities[i]` is what `applied[i]` returned; `inverse` is what [`invert`]
/// produces, which is the inverses in **reverse** order, so `applied[i]`'s
/// inverse sits at `inverse[len - 1 - i]`.
///
/// Needed because a replayed create is not the same entity as the original:
/// redoing one mints a fresh id, and the delete that undoes it has to name the
/// new one. Without this, `create → undo → redo → undo` would re-delete the
/// already-tombstoned first entity and leave the second live for ever.
///
/// A no-op for a step with no creates in it, and for mismatched lengths —
/// which cannot happen from [`invert`]'s output, and would be a worse failure
/// to turn into an error after the write has already landed.
pub fn rebind_creates(applied: &[Command], inverse: &mut [Command], entities: &[EntityRef]) {
    if applied.is_empty() || applied.len() != inverse.len() || applied.len() != entities.len() {
        return;
    }
    let last = applied.len() - 1;
    for (i, (cmd, id)) in applied.iter().zip(entities).enumerate() {
        // `invert_create` refuses everything that is not a create, so this
        // doubles as the test for one.
        if let Ok(inv) = invert_create(cmd, *id) {
            inverse[last - i] = inv;
        }
    }
}
