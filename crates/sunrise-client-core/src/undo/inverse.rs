//! The inverse table: for each command [`invert`] accepts, the command that
//! puts back what it is about to change, read off the rows before it.

#[cfg(doc)]
use super::{invert, invert_create};
use super::{EntityLookup, NotUndoable};
use sunrise_core::{BulkTaskItem, Command};
use sunrise_domain::{
    ContextPatch, RoutinePatch, SavedViewPatch, StreamPatch, Task, TaskPatch, TaskState,
};
use sunrise_id::{EntityKind, EntityRef};

/// The inverse of one command.
pub(super) fn invert_one<S: EntityLookup + ?Sized>(
    state: &S,
    cmd: &Command,
) -> Result<Command, NotUndoable> {
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
            let prev = task(state, *id)?.planned_at.clone();
            Ok(Command::UpdateTask {
                id: *id,
                patch: TaskPatch {
                    planned_at: Some(prev),
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
        // Only an edit of the whole series: an edit of one occurrence is a
        // task edit the core makes, and a split creates a routine and
        // rewrites occurrences in one group, and the routine row holds
        // neither's prior state. Both fall to the refusal below.
        Command::UpdateRoutine {
            id,
            scope: sunrise_domain::EditScope::All,
            patch,
        } => {
            let row = state.routine(*id).ok_or(NotUndoable::Unsupported)?;
            Ok(Command::UpdateRoutine {
                scope: sunrise_domain::EditScope::All,
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
        | Command::DeleteSavedView(id) => Ok(Command::Restore(*id)),
        // Only the whole block's: a delete of some occurrences edits the
        // series, whose prior exceptions and rule no row here holds.
        Command::DeleteBlock { id, scope } if scope.key().is_none() => Ok(Command::Restore(*id)),
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
        // A triage action is undone by putting back, per task, what the
        // action is about to change, as one bulk command (ADR-0047 §4).
        Command::Triage { ids, action } => invert_triage_action(state, ids, action),
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

/// The inverse of a triage action: per task, the field the action writes,
/// as the task held it before. A Defer puts the plan back (the deferral
/// count stays: it is a counter, and a counter only grows); an Already done
/// or a Drop puts the state back, which clears the completion or the reason;
/// a Keep puts the acknowledgement back.
fn invert_triage_action<S: EntityLookup + ?Sized>(
    state: &S,
    ids: &[EntityRef],
    action: &sunrise_domain::TriageAction,
) -> Result<Command, NotUndoable> {
    use sunrise_domain::TriageAction;
    let mut back = Vec::with_capacity(ids.len());
    for id in ids.iter().rev() {
        let t = task(state, *id)?;
        let patch = match action {
            TriageAction::Defer { .. } => TaskPatch {
                planned_at: Some(t.planned_at.clone()),
                ..TaskPatch::default()
            },
            TriageAction::AlreadyDone { .. } | TriageAction::Drop { .. } => TaskPatch {
                state: Some(restore_state(&t.state)),
                ..TaskPatch::default()
            },
            TriageAction::Keep => TaskPatch {
                late_acknowledged_at: Some(t.late_acknowledged_at),
                ..TaskPatch::default()
            },
        };
        back.push(BulkTaskItem {
            id: *id,
            action: sunrise_core::BulkTaskAction::Update(Box::new(patch)),
        });
    }
    Ok(Command::BulkTasks(back))
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
pub(super) const fn delete_of(id: EntityRef) -> Option<Command> {
    Some(match id.kind() {
        EntityKind::Task => Command::DeleteTask(id),
        EntityKind::Stream => Command::DeleteStream {
            id,
            disposition: sunrise_domain::Disposition::Detach,
        },
        EntityKind::Context => Command::DeleteContext(id),
        EntityKind::Routine => Command::DeleteRoutine(id),
        EntityKind::Block => Command::DeleteBlock {
            id,
            scope: sunrise_domain::EditScope::All,
        },
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
        planned_at: patch.planned_at.as_ref().map(|_| t.planned_at.clone()),
        late_acknowledged_at: patch.late_acknowledged_at.map(|_| t.late_acknowledged_at),
        target_at: patch.target_at.as_ref().map(|_| t.target_at.clone()),
        hard_due_at: patch.hard_due_at.as_ref().map(|_| t.hard_due_at.clone()),
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
