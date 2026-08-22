//! Undo and redo, built from **inverse commands**.
//!
//! `docs/08-features/keyboard.md` lists `u` and `Ctrl-r` on every platform,
//! the TUI included. The core has no undo: v1 is entity-level LWW over an
//! append-only op log ([ADR-0014]), and adding a real one means either an
//! inverse-op journal in the protocol or a snapshot per write. Neither is a
//! client's decision to make.
//!
//! What a client *can* do — correctly, and without touching the wire — is
//! remember what a value was immediately before it changed it and offer to set
//! it back. That is what this module builds: for every command the reducer
//! submits, the command that restores the fields it touched, read from the
//! state the view already holds.
//!
//! # What that buys, and what it does not
//!
//! * It is a **new write**, not a rollback. Undo of a completion is a re-open
//!   op, and it converges like any other. Two devices undoing the same thing
//!   is idempotent; a device undoing what another has since changed loses the
//!   LWW tie exactly as a manual edit would. That is the honest behaviour.
//! * A **delete cannot be undone.** `Command::DeleteTask` writes a tombstone
//!   and the core has no restore op, so there is nothing to invert into. `u`
//!   says so rather than silently skipping it — which is the whole reason `D`
//!   is behind a confirmation gate.
//! * A **defer's counter does not come back down.** `deferred_count` is a
//!   PN-counter the engine increments; undo restores the date, and the count
//!   keeps its history. A review that said "deferred three times" should not
//!   change its mind because one of them was undone.
//!
//! [ADR-0014]: ../../../docs/11-adr/0014-entity-level-lww-merge.md

use crate::view::ViewState;
use sunrise_core::Command;
use sunrise_domain::{ContextPatch, RoutinePatch, StreamPatch, TaskPatch, TaskState};
use sunrise_id::EntityRef;

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
    /// A tombstone: the core has no restore op.
    Deleted,
    /// Nothing about the command is reversible (or its target is not loaded).
    Unsupported,
}

/// Build the commands that undo `cmds`, or say why they cannot be built.
///
/// Reads the *current* view state, so it must be called before the core has
/// applied anything — which is where the reducer calls it, since a refresh is
/// the runtime's job and happens afterwards.
///
/// # Errors
///
/// Returns [`NotUndoable`] if any command in the batch has no inverse. All or
/// nothing on purpose: a half-undone bulk operation is worse than one that
/// refuses, because the user cannot tell which half moved.
pub fn invert(state: &ViewState, cmds: &[Command]) -> Result<Vec<Command>, NotUndoable> {
    let mut out = Vec::with_capacity(cmds.len());
    for cmd in cmds {
        out.push(invert_one(state, cmd)?);
    }
    // Applied in reverse, so a batch that touched one entity twice unwinds in
    // the order it was wound.
    out.reverse();
    Ok(out)
}

/// The inverse of one command.
fn invert_one(state: &ViewState, cmd: &Command) -> Result<Command, NotUndoable> {
    match cmd {
        Command::CompleteTask(id) => {
            let prev = task(state, *id)?.state;
            Ok(Command::UpdateTask {
                id: *id,
                patch: TaskPatch {
                    state: Some(prev),
                    ..Default::default()
                },
            })
        }
        Command::DeferTask { id, .. } => {
            // The date comes back; `deferred_count` is a PN-counter and keeps
            // its history on purpose.
            let prev = task(state, *id)?.scheduled_at;
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
            let prev = task(state, *id)?.stream_id;
            Ok(Command::PromoteToStream {
                id: *id,
                stream: prev,
            })
        }
        Command::UpdateStream { id, patch } => {
            let row = state
                .streams
                .iter()
                .find(|s| s.id == *id)
                .ok_or(NotUndoable::Unsupported)?;
            Ok(Command::UpdateStream {
                id: *id,
                patch: StreamPatch {
                    // Only the fields this client can actually read back are
                    // inverted; anything else stays untouched rather than
                    // being restored to a guess.
                    name: patch.name.as_ref().map(|_| row.name.clone()),
                    archived: patch.archived.map(|_| row.archived),
                    paused: patch.paused.map(|_| row.paused),
                    ..Default::default()
                },
            })
        }
        Command::UpdateContext { id, patch } => {
            let row = state
                .contexts
                .iter()
                .find(|c| c.id == *id)
                .ok_or(NotUndoable::Unsupported)?;
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
            let row = state
                .routines
                .iter()
                .find(|r| r.id == *id)
                .ok_or(NotUndoable::Unsupported)?;
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
        Command::DeleteTask(_)
        | Command::DeleteStream(_)
        | Command::DeleteContext(_)
        | Command::DeleteRoutine(_) => Err(NotUndoable::Deleted),
        _ => Err(NotUndoable::Unsupported),
    }
}

/// The patch that restores whatever `patch` is about to change.
///
/// One arm per field rather than a macro: `TaskPatch` uses `Option<Option<T>>`
/// for the clearable facets and plain `Option<T>` for the rest, and the two
/// invert differently.
fn inverse_task_patch(t: &sunrise_domain::Task, patch: &TaskPatch) -> TaskPatch {
    TaskPatch {
        title: patch.title.as_ref().map(|_| t.title.clone()),
        body: patch.body.as_ref().map(|_| t.body.clone()),
        stream_id: patch.stream_id.map(|_| t.stream_id),
        contexts: patch
            .contexts
            .as_ref()
            .map(|_| t.contexts.iter().copied().collect()),
        state: patch.state.map(|_| restore_state(t.state)),
        priority: patch.priority.map(|_| t.priority),
        energy: patch.energy.map(|_| t.energy),
        estimated_duration_s: patch.estimated_duration_s.map(|_| t.estimated_duration_s),
        scheduled_at: patch.scheduled_at.map(|_| t.scheduled_at),
        due_at: patch.due_at.map(|_| t.due_at),
        scheduling_constraints: patch
            .scheduling_constraints
            .as_ref()
            .map(|_| t.scheduling_constraints.clone()),
        blocked_by: patch
            .blocked_by
            .as_ref()
            .map(|_| t.blocked_by.iter().copied().collect()),
        assignee: patch.assignee.map(|_| t.assignee),
        archived: patch.archived.map(|_| t.archived),
    }
}

/// Undoing a completion returns the task to `Todo`, not to `InProgress`.
///
/// `Task.state` does not record how far along something was before it was
/// finished, so restoring `InProgress` would be inventing a claim. `Todo` is
/// the honest reading: it is back on the list.
const fn restore_state(current: TaskState) -> TaskState {
    match current {
        TaskState::Done | TaskState::Cancelled => TaskState::Todo,
        other => other,
    }
}

/// The task `id`, if the view is holding it.
fn task(state: &ViewState, id: EntityRef) -> Result<&sunrise_domain::Task, NotUndoable> {
    state
        .tasks
        .iter()
        .find(|t| t.id == id)
        .or_else(|| state.focused_task.as_ref().filter(|t| t.id == id))
        .ok_or(NotUndoable::Unsupported)
}
