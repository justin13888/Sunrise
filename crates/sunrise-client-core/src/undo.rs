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

use sunrise_core::queries::{ContextRow, StreamRow};
use sunrise_core::{BulkTaskItem, Command};
use sunrise_domain::{
    ContextPatch, RoutinePatch, RoutineRow, SavedView, SavedViewPatch, StreamPatch, Task,
    TaskPatch, TaskState,
};
use sunrise_id::{EntityKind, EntityRef};

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
        Command::CreateBlock(_) => Ok(Command::DeleteBlock(created)),
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

/// The inverse of one command.
fn invert_one<S: EntityLookup + ?Sized>(state: &S, cmd: &Command) -> Result<Command, NotUndoable> {
    match cmd {
        Command::CompleteTask(id) => {
            let prev = task(state, *id)?.state.clone();
            Ok(Command::UpdateTask {
                id: *id,
                patch: TaskPatch {
                    state: Some(prev),
                    ..Default::default()
                },
            })
        }
        Command::DeferTask { id, .. } | Command::DeferTaskTo { id, .. } => {
            // The date comes back; `deferred_count` is a PN-counter and keeps
            // its history on purpose.
            let prev = task(state, *id)?.scheduled_at.clone();
            Ok(Command::UpdateTask {
                id: *id,
                patch: TaskPatch {
                    scheduled_at: Some(prev),
                    ..Default::default()
                },
            })
        }
        Command::UpdateTask { id, patch } => Ok(Command::UpdateTask {
            id: *id,
            patch: inverse_task_patch(task(state, *id)?, patch),
        }),
        Command::PromoteToStream { id, .. } => {
            // No stream is spelled as the legacy Inbox reference.
            let prev = sunrise_domain::stream_key(task(state, *id)?.stream_id);
            Ok(Command::PromoteToStream {
                id: *id,
                stream: prev,
            })
        }
        Command::UpdateStream { id, patch } => {
            let row = state.stream(*id).ok_or(NotUndoable::Unsupported)?;
            Ok(Command::UpdateStream {
                id: *id,
                patch: StreamPatch {
                    // Only the fields this client can actually read back are
                    // inverted; anything else stays untouched rather than
                    // being restored to a guess.
                    name: patch.name.as_ref().map(|_| row.name.clone()),
                    archived: patch.archived.map(|_| row.archived),
                    paused: patch.paused.map(|_| row.paused),
                    // A reorder inverts by putting the row's old key back,
                    // which is possible precisely because the fractional
                    // index moved nothing else: the undo is one write, like
                    // the drag was. The `is_valid` guard skips a row still
                    // holding the "never ordered" sentinel — that is a value
                    // the core would refuse, and refusing the whole undo over
                    // it would be worse than not restoring a position the row
                    // never had.
                    sort_order: patch
                        .sort_order
                        .as_ref()
                        .filter(|_| sunrise_domain::sort_order::is_valid(&row.sort_order))
                        .map(|_| row.sort_order.clone()),
                    ..Default::default()
                },
            })
        }
        Command::UpdateContext { id, patch } => {
            let row = state.context(*id).ok_or(NotUndoable::Unsupported)?;
            Ok(Command::UpdateContext {
                id: *id,
                patch: ContextPatch {
                    name: patch.name.as_ref().map(|_| row.name.clone()),
                    archived: patch.archived.map(|_| row.archived),
                    ..Default::default()
                },
            })
        }
        Command::UpdateRoutine { id, patch } => {
            let row = state.routine(*id).ok_or(NotUndoable::Unsupported)?;
            Ok(Command::UpdateRoutine {
                id: *id,
                patch: RoutinePatch {
                    template: patch.template.as_ref().map(|_| row.template.clone()),
                    rrule: patch.rrule.as_ref().map(|_| row.rule.clone()),
                    paused: patch.paused.map(|_| row.paused),
                    ..Default::default()
                },
            })
        }
        // A delete is undone by a later write of `deleted = false`, which
        // brings the entity back as it was (ADR-0065 §3).
        // A stream's restore brings the stream back, and what its deletion
        // re-homed stays where it went: each was its own write (ADR-0046 §4).
        Command::DeleteTask(id)
        | Command::DeleteStream { id, .. }
        | Command::DeleteContext(id)
        | Command::DeleteRoutine(id)
        | Command::DeleteBlock(id)
        | Command::DeleteSavedView(id) => Ok(Command::Restore(*id)),
        Command::MarkTriaged { ids } => invert_triage(state, ids, true),
        Command::ReturnToInbox { ids } => invert_triage(state, ids, false),
        Command::UpdateSavedView { id, patch } => invert_view_edit(state, *id, patch),
        Command::Restore(id) => delete_of(*id).ok_or(NotUndoable::Unsupported),
        // A bulk command is undone by one bulk command, so the undo is as
        // atomic as the write: each item's inverse, in reverse.
        Command::BulkTasks(items) => {
            let mut back = Vec::with_capacity(items.len());
            for item in items.iter().rev() {
                let inverse = invert_one(state, &item.as_command())?;
                back.push(BulkTaskItem::of_command(&inverse).ok_or(NotUndoable::Unsupported)?);
            }
            Ok(Command::BulkTasks(back))
        }
        // A move is undone by putting back the keys it replaced, read off the
        // rows before it, as one bulk command when it moved more than one
        // task. An unplaced task goes back to unplaced.
        Command::MoveTasks { ids, .. } => invert_move(state, ids),
        // Creates land here too, and `Unsupported` is the right answer *from
        // here*: a create is reversible, but the entity to delete does not
        // exist until the core has minted it. See [`invert_create`], which the
        // caller reaches for once it holds the result of the write.
        _ => Err(NotUndoable::Unsupported),
    }
}

/// The inverse of a move: the keys it is about to replace, read off the rows
/// before it, as one bulk command when it moves more than one task. An
/// unplaced task goes back to unplaced.
fn invert_move<S: EntityLookup + ?Sized>(
    state: &S,
    ids: &[EntityRef],
) -> Result<Command, NotUndoable> {
    let mut back = Vec::with_capacity(ids.len());
    for id in ids {
        let t = task(state, *id)?;
        back.push(BulkTaskItem {
            id: *id,
            action: sunrise_core::BulkTaskAction::Update(Box::new(TaskPatch {
                sort_order: Some(t.sort_order.clone()),
                ..TaskPatch::default()
            })),
        });
    }
    Ok(match back.as_slice() {
        [one] => one.as_command(),
        _ => Command::BulkTasks(back),
    })
}

/// The inverse of a saved-view edit: the fields it writes, as the view held
/// them before it.
fn invert_view_edit<S: EntityLookup + ?Sized>(
    state: &S,
    id: EntityRef,
    patch: &SavedViewPatch,
) -> Result<Command, NotUndoable> {
    let v = state.saved_view(id).ok_or(NotUndoable::Unsupported)?;
    Ok(Command::UpdateSavedView {
        id,
        patch: SavedViewPatch {
            name: patch.name.as_ref().map(|_| v.name.clone()),
            view: patch.view.as_ref().map(|_| v.view.clone()),
            filter: patch.filter.as_ref().map(|_| v.filter.clone()),
            sort: patch.sort.as_ref().map(|_| v.sort.clone()),
            sort_order: patch.sort_order.as_ref().map(|_| v.sort_order.clone()),
        },
    })
}

/// The inverse of a triage (`triaged`) or of its undo: the opposite command,
/// over the tasks it changed, the ones that did not already hold the value it
/// writes.
fn invert_triage<S: EntityLookup + ?Sized>(
    state: &S,
    ids: &[EntityRef],
    triaged: bool,
) -> Result<Command, NotUndoable> {
    let mut back = Vec::new();
    for id in ids {
        let task = state.task(*id).ok_or(NotUndoable::Unsupported)?;
        if task.triaged_at.is_some() != triaged {
            back.push(*id);
        }
    }
    if back.is_empty() {
        return Err(NotUndoable::Unsupported);
    }
    Ok(if triaged {
        Command::ReturnToInbox { ids: back }
    } else {
        Command::MarkTriaged { ids: back }
    })
}

/// The delete of `id`, for the kinds [`Command::Restore`] brings back.
const fn delete_of(id: EntityRef) -> Option<Command> {
    Some(match id.kind() {
        EntityKind::Task => Command::DeleteTask(id),
        EntityKind::Stream => Command::DeleteStream {
            id,
            disposition: sunrise_domain::Disposition::Detach,
        },
        EntityKind::Context => Command::DeleteContext(id),
        EntityKind::Routine => Command::DeleteRoutine(id),
        EntityKind::Block => Command::DeleteBlock(id),
        EntityKind::SavedView => Command::DeleteSavedView(id),
        _ => return None,
    })
}

/// The patch that restores whatever `patch` is about to change.
///
/// One arm per field rather than a macro: `TaskPatch` uses `Option<Option<T>>`
/// for the clearable facets and plain `Option<T>` for the rest, and the two
/// invert differently.
fn inverse_task_patch(t: &Task, patch: &TaskPatch) -> TaskPatch {
    TaskPatch {
        title: patch.title.as_ref().map(|_| t.title.clone()),
        body: patch.body.as_ref().map(|_| t.body.clone()),
        stream_id: patch.stream_id.map(|_| t.stream_id),
        contexts: patch
            .contexts
            .as_ref()
            .map(|_| t.contexts.iter().copied().collect()),
        state: patch.state.as_ref().map(|_| restore_state(&t.state)),
        priority: patch.priority.map(|_| t.priority),
        energy: patch.energy.as_ref().map(|_| t.energy.clone()),
        estimated_duration_s: patch.estimated_duration_s.map(|_| t.estimated_duration_s),
        scheduled_at: patch.scheduled_at.as_ref().map(|_| t.scheduled_at.clone()),
        due_at: patch.due_at.as_ref().map(|_| t.due_at.clone()),
        scheduling_constraints: patch
            .scheduling_constraints
            .as_ref()
            .map(|_| t.scheduling_constraints.clone()),
        blocked_by: patch
            .blocked_by
            .as_ref()
            .map(|_| t.blocked_by.iter().copied().collect()),
        assignee: patch.assignee.map(|_| t.assignee),
        reminder_lead_s: patch.reminder_lead_s.map(|_| t.reminder_lead_s),
        archived: patch.archived.map(|_| t.archived),
        sort_order: patch.sort_order.as_ref().map(|_| t.sort_order.clone()),
    }
}

/// Undoing a completion returns the task to `Todo`, not to `InProgress`.
///
/// `Task.state` does not record how far along something was before it was
/// finished, so restoring `InProgress` would be inventing a claim. `Todo` is
/// the honest reading: it is back on the list.
///
/// Any other state, including one this build does not know, is restored
/// verbatim.
fn restore_state(current: &TaskState) -> TaskState {
    match current {
        TaskState::Done | TaskState::Cancelled => TaskState::Todo,
        other => other.clone(),
    }
}

/// The task `id`, if the client is holding it.
fn task<S: EntityLookup + ?Sized>(state: &S, id: EntityRef) -> Result<&Task, NotUndoable> {
    state.task(id).ok_or(NotUndoable::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use sunrise_core::commands::FocusStartDraft;
    use sunrise_domain::{
        AttachmentDraft, BlockDraft, ContextDraft, Energy, FocusKind, RRule, RoutineCatchupPolicy,
        RoutineDraft, SessionLength, StreamColor, StreamDraft, SunriseTime, TaskDraft,
        TaskTemplate, Unknowns,
    };
    use sunrise_id::EntityKind;

    /// The simplest possible holder of rows — what every client is, minus the
    /// pixels.
    #[derive(Default)]
    struct Rows {
        tasks: Vec<Task>,
        streams: Vec<StreamRow>,
        contexts: Vec<ContextRow>,
        routines: Vec<RoutineRow>,
    }

    impl EntityLookup for Rows {
        fn task(&self, id: EntityRef) -> Option<&Task> {
            self.tasks.iter().find(|t| t.id == id)
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

    fn tid(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [n; 16])
    }
    fn sid(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [n; 16])
    }
    fn cid(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Context, [n; 16])
    }
    fn rid(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Routine, [n; 16])
    }

    fn task(n: u8) -> Task {
        Task {
            reminder_lead_s: None,
            id: tid(n),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            title: format!("task {n}"),
            body: None,
            stream_id: Some(sid(1)),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            sort_order: None,
            triaged_at: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn rows_with(t: Task) -> Rows {
        Rows {
            tasks: vec![t],
            ..Rows::default()
        }
    }

    /// A saved view's create is undone by deleting it, a delete by a restore,
    /// and an edit by writing back the fields it touched, read off the view.
    #[test]
    fn saved_view_commands_invert() {
        use sunrise_domain::{SavedViewDraft, ViewFilter, ViewKind, ViewSort};
        struct Views(SavedView);
        impl EntityLookup for Views {
            fn task(&self, _: EntityRef) -> Option<&Task> {
                None
            }
            fn stream(&self, _: EntityRef) -> Option<&StreamRow> {
                None
            }
            fn context(&self, _: EntityRef) -> Option<&ContextRow> {
                None
            }
            fn routine(&self, _: EntityRef) -> Option<&RoutineRow> {
                None
            }
            fn saved_view(&self, id: EntityRef) -> Option<&SavedView> {
                (id == self.0.id).then_some(&self.0)
            }
        }
        let id = EntityRef::new(EntityKind::SavedView, [7; 16]);
        let held = Views(SavedView {
            id,
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            name: "Old".into(),
            view: ViewKind::Today,
            filter: ViewFilter::default(),
            sort: ViewSort::Title,
            sort_order: Some("N".into()),
            deleted: false,
            unknown: Unknowns::new(),
        });
        let create = Command::CreateSavedView(SavedViewDraft {
            name: "x".into(),
            view: ViewKind::Today,
            filter: ViewFilter::default(),
            sort: ViewSort::Default,
        });
        assert!(is_create(&create));
        assert!(matches!(
            invert_create(&create, id),
            Ok(Command::DeleteSavedView(d)) if d == id
        ));
        assert!(matches!(
            invert(&held, &[Command::DeleteSavedView(id)]).unwrap()[..],
            [Command::Restore(r)] if r == id
        ));
        assert!(matches!(
            invert(&held, &[Command::Restore(id)]).unwrap()[..],
            [Command::DeleteSavedView(r)] if r == id
        ));
        let edit = Command::UpdateSavedView {
            id,
            patch: SavedViewPatch {
                name: Some("New".into()),
                ..SavedViewPatch::default()
            },
        };
        match &invert(&held, &[edit]).unwrap()[..] {
            [Command::UpdateSavedView { patch, .. }] => {
                assert_eq!(patch.name.as_deref(), Some("Old"));
                assert!(patch.sort.is_none(), "only what the edit touched");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A move is undone by putting back the keys it replaced: one task's as
    /// one update, several as one bulk command, and an unplaced task goes
    /// back to unplaced.
    #[test]
    fn a_move_inverts_to_the_keys_it_replaced() {
        let mut placed = task(1);
        placed.sort_order = Some("G".into());
        let rows = Rows {
            tasks: vec![placed, task(2)],
            ..Rows::default()
        };
        let one = Command::MoveTasks {
            ids: vec![tid(1)],
            after: None,
            before: None,
        };
        match &invert(&rows, &[one]).expect("invertible")[..] {
            [Command::UpdateTask { id, patch }] => {
                assert_eq!(*id, tid(1));
                assert_eq!(patch.sort_order, Some(Some("G".into())));
                assert!(patch.title.is_none(), "nothing else is touched");
            }
            other => panic!("{other:?}"),
        }
        let both = Command::MoveTasks {
            ids: vec![tid(2), tid(1)],
            after: None,
            before: None,
        };
        match &invert(&rows, &[both]).expect("invertible")[..] {
            [Command::BulkTasks(items)] => {
                let keys: Vec<_> = items
                    .iter()
                    .map(|i| match &i.action {
                        sunrise_core::BulkTaskAction::Update(p) => (i.id, p.sort_order.clone()),
                        other => panic!("{other:?}"),
                    })
                    .collect();
                assert_eq!(
                    keys,
                    vec![(tid(2), Some(None)), (tid(1), Some(Some("G".into())))]
                );
            }
            other => panic!("{other:?}"),
        }
        let missing = Command::MoveTasks {
            ids: vec![tid(9)],
            after: None,
            before: None,
        };
        assert_eq!(
            invert(&rows, &[missing]).unwrap_err(),
            NotUndoable::Unsupported
        );
    }

    #[test]
    fn completing_inverts_to_the_state_the_task_was_in() {
        let mut t = task(1);
        t.state = TaskState::InProgress;
        let rows = rows_with(t);
        let back = invert(&rows, &[Command::CompleteTask(tid(1))]).expect("invertible");
        match &back[..] {
            [Command::UpdateTask { id, patch }] => {
                assert_eq!(*id, tid(1));
                assert_eq!(patch.state, Some(TaskState::InProgress));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn restoring_a_finished_task_lands_on_todo_not_in_progress() {
        // `Task.state` does not record how far along something was before it
        // finished, so restoring `InProgress` would be inventing a claim.
        // `Todo` is the honest reading: it is back on the list.
        let mut t = task(1);
        t.state = TaskState::Done;
        let rows = rows_with(t);
        let forward = Command::UpdateTask {
            id: tid(1),
            patch: TaskPatch {
                state: Some(TaskState::InProgress),
                ..TaskPatch::default()
            },
        };
        let back = invert(&rows, &[forward]).expect("invertible");
        match &back[..] {
            [Command::UpdateTask { patch, .. }] => {
                assert_eq!(patch.state, Some(TaskState::Todo));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_patch_inverts_only_the_fields_it_touched() {
        let mut t = task(1);
        t.priority = Some(2);
        t.energy = Some(Energy::High);
        let rows = rows_with(t);
        let forward = Command::UpdateTask {
            id: tid(1),
            patch: TaskPatch {
                priority: Some(Some(5)),
                ..TaskPatch::default()
            },
        };
        let back = invert(&rows, &[forward]).expect("invertible");
        match &back[..] {
            [Command::UpdateTask { patch, .. }] => {
                assert_eq!(patch.priority, Some(Some(2)), "restored");
                assert_eq!(patch.energy, None, "untouched fields stay untouched");
                assert_eq!(patch.title, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_defer_restores_the_date_and_leaves_the_counter_alone() {
        // `deferred_count` is a PN-counter with a history a review reads; undo
        // is a new write, not a rollback.
        let mut t = task(1);
        t.deferred_count = 3;
        let rows = rows_with(t);
        let back = invert(
            &rows,
            &[Command::DeferTask {
                id: tid(1),
                to_ms: 1,
            }],
        )
        .expect("invertible");
        match &back[..] {
            [Command::UpdateTask { patch, .. }] => {
                assert_eq!(patch.scheduled_at, Some(None), "the date comes back");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A defer to a time of any kind undoes the same way, restoring the
    /// previous value with its own kind.
    #[test]
    fn a_defer_to_a_floating_time_restores_the_previous_one() {
        let was = sunrise_domain::SunriseTime::all_day(jiff::civil::date(2026, 6, 1));
        let mut t = task(1);
        t.scheduled_at = Some(was.clone());
        let rows = rows_with(t);
        let back = invert(
            &rows,
            &[Command::DeferTaskTo {
                id: tid(1),
                to: sunrise_domain::SunriseTime::floating(
                    jiff::civil::date(2026, 6, 2).at(9, 0, 0, 0),
                ),
            }],
        )
        .expect("invertible");
        match &back[..] {
            [Command::UpdateTask { patch, .. }] => {
                assert_eq!(patch.scheduled_at, Some(Some(was)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_move_between_streams_inverts_to_where_it_came_from() {
        let rows = rows_with(task(1));
        let back = invert(
            &rows,
            &[Command::PromoteToStream {
                id: tid(1),
                stream: sid(9),
            }],
        )
        .expect("invertible");
        assert!(matches!(
            back[..],
            [Command::PromoteToStream { stream, .. }] if stream == sid(1)
        ));
    }

    #[test]
    fn undo_of_a_delete_is_a_restore() {
        // No row is read: the restore brings back whatever the entity was.
        let rows = Rows::default();
        for id in [
            tid(1),
            sid(1),
            EntityRef::new(EntityKind::Context, [3; 16]),
            EntityRef::new(EntityKind::Routine, [4; 16]),
            EntityRef::new(EntityKind::Block, [5; 16]),
        ] {
            let delete = delete_of(id).expect("a kind with a delete");
            let back = invert(&rows, std::slice::from_ref(&delete)).expect("invertible");
            assert!(
                matches!(back[..], [Command::Restore(r)] if r == id),
                "{back:?}"
            );
            // And the restore undoes back into the same delete.
            let again = invert(&rows, &back).expect("invertible");
            assert!(
                matches!(&again[..], [d] if deletes(d) == Some(id)),
                "{again:?}"
            );
        }
    }

    #[test]
    fn a_batch_with_an_uninvertible_command_is_refused_whole() {
        // All or nothing: one un-invertible command sinks the whole batch,
        // because a half-undone bulk operation is worse than one that refuses.
        let rows = rows_with(task(1));
        let resolve = Command::ResolveConflict {
            entity: tid(1),
            field: "title".into(),
            value: serde_json::from_value(serde_json::json!("x")).expect("a CBOR text"),
        };
        assert_eq!(
            invert(&rows, &[Command::CompleteTask(tid(1)), resolve]).unwrap_err(),
            NotUndoable::Unsupported
        );
        assert_eq!(
            invert(
                &rows,
                &[Command::DetachFile(EntityRef::new(
                    EntityKind::Attachment,
                    [7; 16]
                ))]
            )
            .unwrap_err(),
            NotUndoable::Unsupported,
            "an attachment has no restore"
        );
    }

    #[test]
    fn a_row_the_client_is_not_holding_cannot_be_restored() {
        let rows = Rows::default();
        assert_eq!(
            invert(&rows, &[Command::CompleteTask(tid(1))]).unwrap_err(),
            NotUndoable::Unsupported
        );
    }

    #[test]
    fn a_batch_unwinds_in_the_order_it_was_wound() {
        let rows = Rows {
            tasks: vec![task(1), task(2)],
            ..Rows::default()
        };
        let back = invert(
            &rows,
            &[Command::CompleteTask(tid(1)), Command::CompleteTask(tid(2))],
        )
        .expect("invertible");
        assert!(matches!(&back[0], Command::UpdateTask { id, .. } if *id == tid(2)));
        assert!(matches!(&back[1], Command::UpdateTask { id, .. } if *id == tid(1)));
    }

    #[test]
    fn stream_context_and_routine_edits_invert_from_their_rows() {
        let rows = Rows {
            streams: vec![StreamRow {
                id: sid(1),
                name: "Travel".into(),
                color: StreamColor::Slate,
                open_task_count: 0,
                archived: false,
                paused: false,
                sort_order: "N".into(),
            }],
            contexts: vec![ContextRow {
                id: cid(1),
                name: "home".into(),
                description: None,
                archived: false,
                task_count: 0,
            }],
            routines: vec![RoutineRow {
                id: rid(1),
                title: "Water plants".into(),
                rrule: "every day".into(),
                next: None,
                paused: false,
                template: TaskTemplate {
                    title: "Water plants".into(),
                    stream_id: Some(sid(1)),
                    contexts: Vec::new(),
                    energy: None,
                    priority: None,
                    estimated_duration_s: None,
                    body: None,
                    unknown: sunrise_domain::Unknowns::new(),
                },
                rule: RRule::parse("FREQ=DAILY").expect("valid rrule"),
                streak: 0,
            }],
            ..Rows::default()
        };

        let back = invert(
            &rows,
            &[Command::UpdateStream {
                id: sid(1),
                patch: StreamPatch {
                    name: Some("Renamed".into()),
                    ..StreamPatch::default()
                },
            }],
        )
        .expect("invertible");
        assert!(matches!(
            &back[..],
            [Command::UpdateStream { patch, .. }] if patch.name.as_deref() == Some("Travel")
        ));

        // A reorder is undoable for the same reason it is cheap: the old key
        // is on the row the client is already holding, and putting it back
        // touches no sibling.
        let back = invert(
            &rows,
            &[Command::UpdateStream {
                id: sid(1),
                patch: StreamPatch {
                    sort_order: Some("ZN".into()),
                    ..StreamPatch::default()
                },
            }],
        )
        .expect("invertible");
        assert!(matches!(
            &back[..],
            [Command::UpdateStream { patch, .. }] if patch.sort_order.as_deref() == Some("N")
        ));

        let back = invert(
            &rows,
            &[Command::UpdateContext {
                id: cid(1),
                patch: ContextPatch {
                    archived: Some(true),
                    ..ContextPatch::default()
                },
            }],
        )
        .expect("invertible");
        assert!(matches!(
            &back[..],
            [Command::UpdateContext { patch, .. }] if patch.archived == Some(false)
        ));

        let back = invert(
            &rows,
            &[Command::UpdateRoutine {
                id: rid(1),
                patch: RoutinePatch {
                    paused: Some(true),
                    ..RoutinePatch::default()
                },
            }],
        )
        .expect("invertible");
        assert!(matches!(
            &back[..],
            [Command::UpdateRoutine { patch, .. }] if patch.paused == Some(false)
        ));
    }

    // ---- creates ----

    /// Every create the core has that owns an entity, paired with the id the
    /// core would mint for it. A table so the `match` in [`invert_create`]
    /// cannot grow a seventh create without this covering it.
    fn every_create() -> Vec<(Command, EntityRef)> {
        vec![
            (
                Command::CreateTask(TaskDraft {
                    title: "Renew passport".into(),
                    body: None,
                    stream_id: None,
                    contexts: Vec::new(),
                    priority: None,
                    energy: None,
                    estimated_duration_s: None,
                    scheduled_at: None,
                    due_at: None,
                    scheduling_constraints: Vec::new(),
                    assignee: None,
                    reminder_lead_s: None,
                }),
                tid(7),
            ),
            (
                Command::CreateStream(StreamDraft {
                    name: "Travel".into(),
                    ..StreamDraft::default()
                }),
                sid(7),
            ),
            (
                Command::CreateContext(ContextDraft {
                    name: "home".into(),
                    description: None,
                }),
                cid(7),
            ),
            (
                Command::CreateRoutine(RoutineDraft {
                    template: TaskTemplate {
                        title: "Water plants".into(),
                        stream_id: Some(sid(1)),
                        contexts: Vec::new(),
                        energy: None,
                        priority: None,
                        estimated_duration_s: None,
                        body: None,
                        unknown: sunrise_domain::Unknowns::new(),
                    },
                    rrule: RRule::parse("FREQ=DAILY").expect("valid rrule"),
                    timezone: "UTC".into(),
                    starts_at: jiff::Timestamp::UNIX_EPOCH.into(),
                    ends_at: None,
                    scheduling_constraints: Vec::new(),
                    catchup_policy: RoutineCatchupPolicy::Skip,
                }),
                rid(7),
            ),
            (
                Command::CreateBlock(BlockDraft {
                    stream_id: Some(sid(1)),
                    starts_at: SunriseTime::Instant {
                        at: jiff::Timestamp::UNIX_EPOCH,
                    },
                    ends_at: SunriseTime::Instant {
                        at: jiff::Timestamp::UNIX_EPOCH,
                    },
                    title: Some("Deep work".into()),
                    title_track_task: false,
                    tasks: Vec::new(),
                }),
                EntityRef::new(EntityKind::Block, [7; 16]),
            ),
            (
                Command::AttachFile(AttachmentDraft {
                    parent: tid(1),
                    filename: "scan.pdf".into(),
                    mime_type: "application/pdf".into(),
                    size_bytes: 1,
                    blob_key: [0; 32],
                    blob_id: [0; 16],
                    chunk_count: 1,
                    content_hash: [0; 32],
                    ciphertext_hash: [0; 32],
                    width: None,
                    height: None,
                    thumbnail: None,
                }),
                EntityRef::new(EntityKind::Attachment, [7; 16]),
            ),
        ]
    }

    /// The id a delete names, whichever kind of delete it is.
    fn deletes(cmd: &Command) -> Option<EntityRef> {
        match cmd {
            Command::DeleteTask(id)
            | Command::DeleteStream { id, .. }
            | Command::DeleteContext(id)
            | Command::DeleteRoutine(id)
            | Command::DeleteBlock(id)
            | Command::DetachFile(id) => Some(*id),
            _ => None,
        }
    }

    #[test]
    fn undoing_a_create_deletes_the_entity_it_minted() {
        for (cmd, minted) in every_create() {
            assert!(is_create(&cmd), "{cmd:?} is a create");
            let back = invert_create(&cmd, minted).expect("a create is invertible once applied");
            assert_eq!(
                deletes(&back),
                Some(minted),
                "{cmd:?} must invert to the delete of the id the core minted, not another"
            );
        }
    }

    #[test]
    fn a_create_cannot_be_inverted_before_it_is_applied() {
        // Not "unsupported for ever" — unsupported *from here*. `invert` reads
        // rows that already exist, and the entity a create makes does not.
        let rows = Rows::default();
        for (cmd, _) in every_create() {
            assert_eq!(
                invert(&rows, &[cmd]).unwrap_err(),
                NotUndoable::Unsupported,
                "the id to delete does not exist yet"
            );
        }
    }

    #[test]
    fn a_create_with_no_delete_to_invert_into_is_refused() {
        // `StartFocus` mints an append-only record and the core has no command
        // that removes one. Refused rather than mapped onto some other delete.
        let start = Command::StartFocus(FocusStartDraft {
            task_id: tid(1),
            kind: FocusKind::Work,
            length: SessionLength::UntilDone,
            energy: None,
        });
        assert!(!is_create(&start));
        assert_eq!(
            invert_create(&start, EntityRef::new(EntityKind::FocusSession, [1; 16])).unwrap_err(),
            NotUndoable::Unsupported
        );
    }

    #[test]
    fn redoing_a_create_rebinds_the_undo_to_the_entity_the_replay_minted() {
        // The whole point: a replayed create is a *different* entity. Without
        // rebinding, the next undo would delete the already-tombstoned first
        // id and leave the second live for ever.
        let (create, first) = every_create().remove(0);
        let entry = UndoEntry {
            label: "new task".into(),
            forward: vec![create.clone()],
            backward: vec![invert_create(&create, first).expect("invertible")],
        };
        assert_eq!(deletes(&entry.backward[0]), Some(first));

        // Undo stores the step flipped, and redo applies that step's
        // `backward` — the create — then binds its `forward` to what came
        // back.
        let mut flipped = entry.clone().flipped();
        let second = tid(9);
        rebind_creates(&flipped.backward, &mut flipped.forward, &[second]);
        let after_redo = flipped.flipped();
        assert_eq!(
            deletes(&after_redo.backward[0]),
            Some(second),
            "the next undo deletes what the redo made, not what the first create made"
        );
    }

    #[test]
    fn a_rebind_leaves_the_inverses_of_everything_that_is_not_a_create_alone() {
        let rows = rows_with(task(1));
        let forward = vec![
            Command::CompleteTask(tid(1)),
            Command::CreateContext(ContextDraft {
                name: "home".into(),
                description: None,
            }),
        ];
        // `invert` refuses a batch containing a create, so the inverse is
        // assembled the way a caller assembles it: the pre-read half, and the
        // minted half on top. Reversed order, like `invert`'s own output.
        let mut backward = vec![
            invert_create(&forward[1], cid(3)).expect("invertible"),
            invert(&rows, &forward[..1]).expect("invertible").remove(0),
        ];
        rebind_creates(&forward, &mut backward, &[tid(1), cid(4)]);
        assert!(
            matches!(&backward[1], Command::UpdateTask { id, .. } if *id == tid(1)),
            "the completion's inverse is untouched"
        );
        assert_eq!(
            deletes(&backward[0]),
            Some(cid(4)),
            "only the create is re-bound, and to the id its own replay returned"
        );
    }

    #[test]
    fn a_create_round_trip_converges() {
        // Two flips is the identity: create → undo → redo → undo settles on
        // "the entity is gone" rather than oscillating into a shape the two
        // sides disagree about.
        let (create, minted) = every_create().remove(0);
        let entry = UndoEntry {
            label: "new task".into(),
            forward: vec![create],
            backward: vec![Command::DeleteTask(minted)],
        };
        let round_trip = entry.clone().flipped().flipped();
        assert!(matches!(round_trip.forward[..], [Command::CreateTask(_)]));
        assert_eq!(deletes(&round_trip.backward[0]), Some(minted));
        assert_eq!(round_trip.label, entry.label);
    }

    #[test]
    fn a_step_flipped_is_the_same_step_facing_the_other_way() {
        let entry = UndoEntry {
            label: "completed 3 tasks".into(),
            forward: vec![Command::CompleteTask(tid(1))],
            backward: vec![Command::DeleteTask(tid(1))],
        };
        let flipped = entry.clone().flipped();
        assert_eq!(flipped.label, entry.label);
        assert!(matches!(flipped.forward[..], [Command::DeleteTask(_)]));
        assert!(matches!(flipped.backward[..], [Command::CompleteTask(_)]));
    }
}
