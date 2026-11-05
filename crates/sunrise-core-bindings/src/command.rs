//! The write surface: every [`sunrise_core::Command`], in a shape UniFFI can
//! carry.
//!
//! One variant per command, no aggregation and no convenience wrappers. The
//! core's command list *is* the contract; a seam that offered a smaller one
//! would quietly decide what clients are allowed to do.

use sunrise_core::commands::FocusStartDraft;
use sunrise_core::Command;
use sunrise_domain::{Energy, InterruptionReason, SessionLength};
use sunrise_id::EntityRef;

use crate::dto::{
    AttachmentDraftIn, BlockDraftIn, BlockEdit, ContextDraftIn, ContextEdit, PreferenceTarget,
    PreferenceValue, RoutineDraftIn, RoutineEdit, SnapshotDraftIn, StreamDraftIn, StreamEdit,
    TaskDraftIn, TaskEdit, TimeValue,
};

/// A mutating command.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CoreCommand {
    /// Create a task (in the Inbox when `draft.stream_id` is absent).
    CreateTask {
        /// The draft.
        draft: TaskDraftIn,
    },
    /// Edit a task.
    UpdateTask {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: TaskEdit,
    },
    /// Set a task to `done`.
    CompleteTask {
        /// Target.
        id: EntityRef,
    },
    /// Push a task's scheduled time out and bump its deferral count.
    DeferTask {
        /// Target.
        id: EntityRef,
        /// New scheduled time (epoch ms).
        to_ms: u64,
    },
    /// Push a task's scheduled time out to a time of any kind, keeping that
    /// kind, and bump its deferral count. See
    /// [`sunrise_core::Command::DeferTaskTo`]: a floating "tomorrow 09:00"
    /// stays floating. `DeferTask` is this with an instant.
    DeferTaskTo {
        /// Target.
        id: EntityRef,
        /// New scheduled time.
        to: TimeValue,
    },
    /// Tombstone a task.
    DeleteTask {
        /// Target.
        id: EntityRef,
    },
    /// Move a task to another stream, or to no stream given the legacy Inbox
    /// id ([`crate::vocab::inbox_stream_id`]).
    ///
    /// Equivalent to `UpdateTask` with `TaskEdit::stream_id` set — the core
    /// routes `PromoteToStream` straight into `update_task` with a
    /// stream-only patch — and kept because the core has the command and this
    /// enum mirrors the core's list rather than editing it down.
    PromoteToStream {
        /// Target.
        id: EntityRef,
        /// Destination stream.
        stream: EntityRef,
    },
    /// Create a stream.
    CreateStream {
        /// The draft.
        draft: StreamDraftIn,
    },
    /// Edit a stream.
    UpdateStream {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: StreamEdit,
    },
    /// Tombstone a stream, and move everything it held to no stream: its
    /// tasks, blocks and routine templates, and its child streams to the top
    /// level, in one commit. `DeleteStreamInto` moves them to another stream
    /// instead. See [`sunrise_core::Command::DeleteStream`].
    DeleteStream {
        /// Target.
        id: EntityRef,
    },
    /// Create a context. Rejected if a live context already carries the name,
    /// compared case-insensitively.
    CreateContext {
        /// The draft.
        draft: ContextDraftIn,
    },
    /// Edit a context. Archiving is not deletion: an archived context stays on
    /// the tasks that carry it.
    UpdateContext {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: ContextEdit,
    },
    /// Tombstone a context, removing it from every task that carries it in the
    /// same transaction.
    DeleteContext {
        /// Target.
        id: EntityRef,
    },
    /// Create a routine and materialize its near-horizon occurrences.
    CreateRoutine {
        /// The draft.
        draft: RoutineDraftIn,
    },
    /// Edit a routine. A recurrence, timezone or anchor change regenerates
    /// future not-yet-started routine tasks.
    UpdateRoutine {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: RoutineEdit,
    },
    /// Tombstone a routine. Generation stops; existing tasks remain.
    DeleteRoutine {
        /// Target.
        id: EntityRef,
    },
    /// Skip one occurrence by its key (`YYYY-MM-DDTHH:MM` in the routine's
    /// zone).
    SkipRoutineOccurrence {
        /// Target.
        id: EntityRef,
        /// Occurrence key.
        occurrence_key: String,
    },
    /// Create a time block. Given no title and exactly one task, the core
    /// shadow-copies that task's title as it is now.
    CreateBlock {
        /// The draft.
        draft: BlockDraftIn,
    },
    /// Edit a block: move it, retitle it, or change title tracking.
    UpdateBlock {
        /// Target.
        id: EntityRef,
        /// The edit.
        edit: BlockEdit,
    },
    /// Tombstone a block. Bound tasks are untouched.
    DeleteBlock {
        /// Target.
        id: EntityRef,
    },
    /// Bind a task to a block. `TaskItem::blocks` is derived from the binding,
    /// so the symmetry needs no second command.
    BindTask {
        /// Target block.
        block: EntityRef,
        /// Task to bind.
        task: EntityRef,
    },
    /// Unbind a task from a block. The block survives with no tasks bound.
    UnbindTask {
        /// Target block.
        block: EntityRef,
        /// Task to unbind.
        task: EntityRef,
    },
    /// Record an attachment against a task. The bytes are uploaded first,
    /// sealed under a key the client generated; this writes the metadata that
    /// makes the blob findable.
    AttachFile {
        /// The draft.
        draft: AttachmentDraftIn,
    },
    /// Tombstone attachment metadata. The blob itself is reclaimed by the
    /// relay's GC, not here.
    DetachFile {
        /// Target.
        id: EntityRef,
    },
    /// Run materialization for every live routine.
    MaterializeRoutines {
        /// Wall clock (epoch ms) to materialize against.
        now_ms: u64,
    },
    /// Revoke a device and rotate every Stream key it could read.
    ///
    /// Replaces `TrustDevice`. Trust is no longer something a client hands the
    /// core: a device publishes an identity-signed certificate as an op when it
    /// first opens its vault, so the only device-list action left for a user is
    /// taking one away.
    /// The cut is the HLC of the op this emits rather than a value the caller
    /// nominates, so there is nothing here to pass and nothing to get wrong.
    RevokeDevice {
        /// The device to revoke.
        device_id: EntityRef,
        /// Why, for the device list to show later.
        reason: DeviceRevokeReason,
    },
    /// Mint a new epoch for one Stream and seal it to every current device.
    RotateStreamKey {
        /// The Stream to rotate.
        stream: EntityRef,
    },
    /// Open a focus session on a task.
    StartFocus {
        /// Task to focus on.
        task_id: EntityRef,
        /// Work or break.
        kind: sunrise_domain::FocusKind,
        /// How the session should be sized.
        length: SessionLength,
        /// Declared energy budget; the task's own facet when absent.
        energy: Option<Energy>,
    },
    /// Close a focus session. A separate record addressed to the same session
    /// id, never an edit of the start.
    EndFocus {
        /// Session to close.
        session: EntityRef,
        /// Focused time to freeze; the derived elapsed time when absent.
        actual_focused_ms: Option<u64>,
        /// Whether the task was completed in this session. `true` also
        /// auto-completes the task when it is still open: the focus screen's
        /// "complete" action is the user saying they finished it. Derived once
        /// on this device and emitted as an ordinary task update, so the
        /// session log is still not a second writer of task state.
        completed_task: bool,
    },
    /// Log one interruption against a running session.
    LogInterruption {
        /// The session.
        session: EntityRef,
        /// One-tap reason.
        reason: InterruptionReason,
    },
    /// Record that a weekly review was completed.
    SaveReviewSnapshot {
        /// The snapshot.
        draft: SnapshotDraftIn,
    },
    /// Set one preference in the store `target` names. See
    /// [`sunrise_core::Command::SetPreference`].
    SetPreference {
        /// The key.
        key: String,
        /// The value.
        value: PreferenceValue,
        /// The vault, or this device.
        target: PreferenceTarget,
    },
    /// Clear one preference from the store `target` names.
    ClearPreference {
        /// The key.
        key: String,
        /// The vault, or this device.
        target: PreferenceTarget,
    },
    /// Bring back a deleted task, stream, context, routine or block. See
    /// [`sunrise_core::Command::Restore`]; undo of a delete submits it.
    Restore {
        /// Target.
        id: EntityRef,
    },
    /// Settle one field's conflict on `value`. See
    /// [`sunrise_core::Command::ResolveConflict`].
    ResolveConflict {
        /// The entity.
        entity: EntityRef,
        /// The field's wire name, as `ConflictItem::field` names it.
        field: String,
        /// The value as canonical CBOR: a `ConflictWriteItem::value` handed
        /// back, or one the client encoded.
        value: Vec<u8>,
    },
    /// Merge two blocks into one, as one commit: the union is created and
    /// both are tombstoned together. See
    /// [`sunrise_core::Command::MergeBlocks`]; the result names the union.
    MergeBlocks {
        /// One block.
        a: EntityRef,
        /// The other.
        b: EntityRef,
    },
    /// Apply one action to each of several tasks, all or nothing, as one
    /// commit. See [`sunrise_core::Command::BulkTasks`].
    BulkTasks {
        /// The tasks and their actions; a task may appear once.
        items: Vec<BulkTaskItemIn>,
    },
    /// Place tasks, in the order given, between two neighbours in manual
    /// order; the core computes the keys. See
    /// [`sunrise_core::Command::MoveTasks`]. A drag's three fields come from
    /// [`crate::task_order::plan_task_move`].
    MoveTasks {
        /// The tasks to place, in the order they will be in.
        ids: Vec<EntityRef>,
        /// The task they land below, or none for the top.
        after: Option<EntityRef>,
        /// The task they land above, or none for the bottom.
        before: Option<EntityRef>,
    },
    /// Save a view, synced to every device. See
    /// [`sunrise_core::Command::CreateSavedView`].
    CreateSavedView {
        /// The view.
        draft: crate::saved_views::SavedViewDraftIn,
    },
    /// Edit a saved view: only the fields set are written. See
    /// [`sunrise_core::Command::UpdateSavedView`].
    UpdateSavedView {
        /// The view.
        id: EntityRef,
        /// The fields to write.
        edit: crate::saved_views::SavedViewEdit,
    },
    /// Delete a saved view; `Restore` brings it back.
    DeleteSavedView {
        /// The view.
        id: EntityRef,
    },
    /// Tombstone a stream, and move everything it held into the live stream
    /// `into`, in one commit (ADR-0046 §4): `DeleteStream` with a re-home
    /// target. See [`sunrise_core::Command::DeleteStream`].
    DeleteStreamInto {
        /// Target.
        id: EntityRef,
        /// The live stream its tasks, blocks and routine templates go to.
        into: EntityRef,
    },
    /// Triage tasks out of the Inbox without giving them a stream or a date
    /// (ADR-0046 §3). See [`sunrise_core::Command::MarkTriaged`].
    MarkTriaged {
        /// The tasks.
        ids: Vec<EntityRef>,
    },
    /// Undo `MarkTriaged`. See [`sunrise_core::Command::ReturnToInbox`].
    ReturnToInbox {
        /// The tasks.
        ids: Vec<EntityRef>,
    },
    /// One triage action over a selection, all or nothing, as one commit
    /// (ADR-0047 §4). See [`sunrise_core::Command::Triage`].
    Triage {
        /// The tasks.
        ids: Vec<EntityRef>,
        /// What to do to each.
        action: crate::lateness::TriageActionIn,
    },
}

/// One item of [`CoreCommand::BulkTasks`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct BulkTaskItemIn {
    /// The task.
    pub id: EntityRef,
    /// What to do to it.
    pub action: BulkTaskActionIn,
}

/// What [`CoreCommand::BulkTasks`] does to one task: each is the single
/// command of the same name.
// UniFFI lifts an enum by value; a box would not cross the seam.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, uniffi::Enum)]
pub enum BulkTaskActionIn {
    /// `CompleteTask`.
    Complete,
    /// `DeferTaskTo`.
    DeferTo {
        /// New scheduled time.
        to: TimeValue,
    },
    /// `UpdateTask`: a move, a drop to `cancelled`, or any other edit.
    Update {
        /// The edit.
        edit: TaskEdit,
    },
    /// `DeleteTask`.
    Delete,
    /// `Restore`.
    Restore,
}

impl From<BulkTaskItemIn> for sunrise_core::BulkTaskItem {
    fn from(item: BulkTaskItemIn) -> Self {
        use sunrise_core::BulkTaskAction;
        Self {
            id: item.id,
            action: match item.action {
                BulkTaskActionIn::Complete => BulkTaskAction::Complete,
                BulkTaskActionIn::DeferTo { to } => BulkTaskAction::DeferTo(to.into()),
                BulkTaskActionIn::Update { edit } => BulkTaskAction::Update(Box::new(edit.into())),
                BulkTaskActionIn::Delete => BulkTaskAction::Delete,
                BulkTaskActionIn::Restore => BulkTaskAction::Restore,
            },
        }
    }
}

impl CoreCommand {
    /// Lower into the core's own command type.
    ///
    /// Fallible in two places: `AttachFile` carries fixed-width byte arrays
    /// that UniFFI can only express as a `Vec<u8>` and a hex string, so their
    /// length is checked here, and `ResolveConflict`'s value must be canonical
    /// CBOR. Every id arrived as an already-lifted
    /// `EntityRef`, and that parsing happened at the boundary in
    /// [`crate::types`]'s `custom_type!`.
    pub(crate) fn into_core(self) -> Result<Command, crate::BindingError> {
        Ok(match self {
            Self::CreateTask { draft } => Command::CreateTask(draft.into()),
            Self::UpdateTask { id, edit } => Command::UpdateTask {
                id,
                patch: edit.into(),
            },
            Self::CompleteTask { id } => Command::CompleteTask(id),
            Self::DeferTask { id, to_ms } => Command::DeferTask { id, to_ms },
            Self::DeferTaskTo { id, to } => Command::DeferTaskTo { id, to: to.into() },
            Self::DeleteTask { id } => Command::DeleteTask(id),
            Self::PromoteToStream { id, stream } => Command::PromoteToStream { id, stream },
            Self::CreateStream { draft } => Command::CreateStream(draft.into()),
            Self::UpdateStream { id, edit } => Command::UpdateStream {
                id,
                patch: edit.into(),
            },
            Self::DeleteStream { id } => Command::DeleteStream {
                id,
                disposition: sunrise_domain::Disposition::Detach,
            },
            Self::CreateContext { draft } => Command::CreateContext(draft.into()),
            Self::UpdateContext { id, edit } => Command::UpdateContext {
                id,
                patch: edit.into(),
            },
            Self::DeleteContext { id } => Command::DeleteContext(id),
            Self::CreateRoutine { draft } => Command::CreateRoutine(draft.into()),
            Self::UpdateRoutine { id, edit } => Command::UpdateRoutine {
                id,
                scope: edit.scope.clone().map(Into::into).unwrap_or_default(),
                patch: edit.into(),
            },
            Self::DeleteRoutine { id } => Command::DeleteRoutine(id),
            Self::SkipRoutineOccurrence { id, occurrence_key } => {
                Command::SkipRoutineOccurrence { id, occurrence_key }
            }
            Self::CreateBlock { draft } => Command::CreateBlock(draft.into()),
            Self::UpdateBlock { id, edit } => Command::UpdateBlock {
                id,
                patch: edit.into(),
            },
            Self::DeleteBlock { id } => Command::DeleteBlock(id),
            Self::BindTask { block, task } => Command::BindTask { block, task },
            Self::UnbindTask { block, task } => Command::UnbindTask { block, task },
            Self::MaterializeRoutines { now_ms } => Command::MaterializeRoutines { now_ms },
            Self::RevokeDevice { device_id, reason } => Command::RevokeDevice {
                device_id,
                reason: reason.into(),
            },
            Self::RotateStreamKey { stream } => Command::RotateStreamKey { stream },
            Self::StartFocus {
                task_id,
                kind,
                length,
                energy,
            } => Command::StartFocus(FocusStartDraft {
                task_id,
                kind,
                length,
                energy,
            }),
            Self::EndFocus {
                session,
                actual_focused_ms,
                completed_task,
            } => Command::EndFocus {
                session,
                actual_focused_ms,
                completed_task,
            },
            Self::LogInterruption { session, reason } => {
                Command::LogInterruption { session, reason }
            }
            Self::AttachFile { draft } => Command::AttachFile(draft.try_into()?),
            Self::DetachFile { id } => Command::DetachFile(id),
            Self::SaveReviewSnapshot { draft } => Command::SaveReviewSnapshot(draft.into()),
            Self::SetPreference { key, value, target } => Command::SetPreference {
                key,
                value: value.into(),
                target: target.into(),
            },
            Self::ClearPreference { key, target } => Command::ClearPreference {
                key,
                target: target.into(),
            },
            Self::Restore { id } => Command::Restore(id),
            Self::ResolveConflict {
                entity,
                field,
                value,
            } => Command::ResolveConflict {
                entity,
                field,
                value: crate::conflicts::value_in(&value)?,
            },
            Self::MergeBlocks { a, b } => Command::MergeBlocks { a, b },
            Self::BulkTasks { items } => {
                Command::BulkTasks(items.into_iter().map(Into::into).collect())
            }
            Self::MoveTasks { ids, after, before } => Command::MoveTasks { ids, after, before },
            Self::CreateSavedView { draft } => Command::CreateSavedView(draft.into()),
            Self::UpdateSavedView { id, edit } => Command::UpdateSavedView {
                id,
                patch: edit.into(),
            },
            Self::DeleteSavedView { id } => Command::DeleteSavedView(id),
            Self::DeleteStreamInto { id, into } => Command::DeleteStream {
                id,
                disposition: sunrise_domain::Disposition::MoveTo(into),
            },
            Self::MarkTriaged { ids } => Command::MarkTriaged { ids },
            Self::ReturnToInbox { ids } => Command::ReturnToInbox { ids },
            Self::Triage { ids, action } => Command::Triage {
                ids,
                action: action.into(),
            },
        })
    }
}

/// Why a device was revoked, across the seam.
///
/// Mirrored rather than re-exported so the Swift cases read as they do on
/// screen; the exhaustive `From` below keeps it honest, because a variant added
/// upstream fails this crate's build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DeviceRevokeReason {
    /// Misplaced; may still turn up.
    Lost,
    /// Known to be in someone else's hands.
    Stolen,
    /// Deliberately retired by its owner.
    Retired,
    /// Believed to have had its keys extracted.
    Compromised,
}

impl From<DeviceRevokeReason> for sunrise_core::RevokeReason {
    fn from(r: DeviceRevokeReason) -> Self {
        match r {
            DeviceRevokeReason::Lost => Self::Lost,
            DeviceRevokeReason::Stolen => Self::Stolen,
            DeviceRevokeReason::Retired => Self::Retired,
            DeviceRevokeReason::Compromised => Self::Compromised,
        }
    }
}
