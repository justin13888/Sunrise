//! An undo history written down, for a client whose session outlives its
//! process.
//!
//! The Apple apps keep [`UndoHistory`](super::UndoHistory) in memory for as
//! long as the window is open. The CLI has no window: every command is its own
//! process, so `sunrise undo` runs in a process that did not make the step it
//! reverses. The stacks therefore have to cross a process boundary, and this
//! is the one encoding of them.
//!
//! # Why not `serde_json` over [`Command`]
//!
//! The domain patches say "clear this field" as `Some(None)` and "leave it" as
//! `None`, and plain serde writes both as `null`. A step that undoes setting a
//! priority on a task that had none is exactly `priority: Some(None)`, so a
//! round trip through the derived impls would turn the undo into a no-op and
//! report success. Each patch here has a mirror whose double options are
//! written as *absent* versus `null`, and a mirror is a `serde(remote)` of the
//! domain type, so a field added to a patch fails this crate's build until it
//! is given an encoding.
//!
//! # What is kept
//!
//! Only the commands an [`UndoEntry`] can hold: the ones
//! [`crate::undo::invert`] inverts, the creates it inverts, and the inverses
//! it builds. A step holding anything else is dropped from the written
//! history rather than written lossily ([`to_json`] reports how many).
//! `AttachFile` is among the dropped: its draft carries the blob key, and a
//! history file is not where a key belongs.
//!
//! The encoding is plain JSON and carries task content (labels, titles,
//! notes). Where it is stored is the caller's decision, and a caller that
//! keeps it beside a vault seals it first (the CLI does; see
//! `sunrise_cli::undo_file`).

use serde::{Deserialize, Serialize};
use sunrise_core::{BulkTaskAction, BulkTaskItem, Command};
use sunrise_domain::{
    BlockDraft, ContextDraft, ContextPatch, Disposition, Energy, NoteBody, RRule,
    RoutineCatchupPolicy, RoutineDraft, RoutinePatch, SavedViewDraft, SavedViewPatch,
    ScheduleConstraint, StreamColor, StreamDraft, StreamPatch, StreamReviewCadence, SunriseTime,
    TaskDraft, TaskPatch, TaskState, TaskTemplate,
};
use sunrise_id::EntityRef;

use super::UndoStacks;
use crate::undo::UndoEntry;

/// The version of the written form. A file of another version is not read.
pub const PERSIST_VERSION: u32 = 1;

/// Why a written history could not be read back.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// Not JSON, or not this shape.
    #[error("undo history: {0}")]
    Malformed(String),
    /// Written by a build with another [`PERSIST_VERSION`].
    #[error("undo history version {0} is not one this build reads")]
    Version(u32),
}

/// The stacks as JSON, and how many steps were left out because they hold a
/// command this encoding does not carry.
///
/// # Errors
/// Only if `serde_json` cannot write a value, which none of these produce.
pub fn to_json(stacks: &UndoStacks) -> Result<(String, usize), PersistError> {
    let mut dropped = 0;
    let mut keep = |entries: &[UndoEntry]| -> Vec<Step> {
        entries
            .iter()
            .filter_map(|e| {
                let step = Step::of(e);
                dropped += usize::from(step.is_none());
                step
            })
            .collect()
    };
    let file = File {
        version: PERSIST_VERSION,
        done: keep(&stacks.done),
        undone: keep(&stacks.undone),
    };
    let json = serde_json::to_string(&file).map_err(|e| PersistError::Malformed(e.to_string()))?;
    Ok((json, dropped))
}

/// The stacks [`to_json`] wrote.
///
/// # Errors
/// [`PersistError::Malformed`] for text that is not this shape, and
/// [`PersistError::Version`] for one written by another version.
pub fn from_json(text: &str) -> Result<UndoStacks, PersistError> {
    #[derive(Deserialize)]
    struct Header {
        version: u32,
    }
    let header: Header =
        serde_json::from_str(text).map_err(|e| PersistError::Malformed(e.to_string()))?;
    if header.version != PERSIST_VERSION {
        return Err(PersistError::Version(header.version));
    }
    let file: File =
        serde_json::from_str(text).map_err(|e| PersistError::Malformed(e.to_string()))?;
    Ok(UndoStacks {
        done: file.done.into_iter().map(Step::into_entry).collect(),
        undone: file.undone.into_iter().map(Step::into_entry).collect(),
    })
}

#[derive(Serialize, Deserialize)]
struct File {
    version: u32,
    done: Vec<Step>,
    undone: Vec<Step>,
}

#[derive(Serialize, Deserialize)]
struct Step {
    label: String,
    forward: Vec<Cmd>,
    backward: Vec<Cmd>,
}

impl Step {
    fn of(e: &UndoEntry) -> Option<Self> {
        Some(Self {
            label: e.label.clone(),
            forward: e.forward.iter().map(Cmd::of).collect::<Option<_>>()?,
            backward: e.backward.iter().map(Cmd::of).collect::<Option<_>>()?,
        })
    }

    fn into_entry(self) -> UndoEntry {
        UndoEntry {
            label: self.label,
            forward: self.forward.into_iter().map(Cmd::into_command).collect(),
            backward: self.backward.into_iter().map(Cmd::into_command).collect(),
        }
    }
}

/// The commands an undo step can hold, each with a lossless encoding.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Cmd {
    CreateTask(TaskDraft),
    UpdateTask {
        id: EntityRef,
        #[serde(with = "TaskPatchDef")]
        patch: TaskPatch,
    },
    CompleteTask(EntityRef),
    DeferTask {
        id: EntityRef,
        to_ms: u64,
    },
    DeferTaskTo {
        id: EntityRef,
        to: SunriseTime,
    },
    DeleteTask(EntityRef),
    BulkTasks(Vec<BulkItem>),
    Restore(EntityRef),
    PromoteToStream {
        id: EntityRef,
        stream: EntityRef,
    },
    CreateStream(StreamDraft),
    UpdateStream {
        id: EntityRef,
        #[serde(with = "StreamPatchDef")]
        patch: StreamPatch,
    },
    /// A delete that detaches what the stream held: the only form a file
    /// written before ADR-0046 holds.
    DeleteStream(EntityRef),
    /// A delete that moves what the stream held `into` another stream.
    DeleteStreamInto {
        id: EntityRef,
        into: EntityRef,
    },
    MarkTriaged(Vec<EntityRef>),
    ReturnToInbox(Vec<EntityRef>),
    CreateContext(ContextDraft),
    UpdateContext {
        id: EntityRef,
        #[serde(with = "ContextPatchDef")]
        patch: ContextPatch,
    },
    DeleteContext(EntityRef),
    CreateRoutine(RoutineDraft),
    UpdateRoutine {
        id: EntityRef,
        #[serde(with = "RoutinePatchDef")]
        patch: RoutinePatch,
    },
    DeleteRoutine(EntityRef),
    CreateBlock(BlockDraft),
    DeleteBlock(EntityRef),
    DetachFile(EntityRef),
    MoveTasks {
        ids: Vec<EntityRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<EntityRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<EntityRef>,
    },
    CreateSavedView(Cbor<SavedViewDraft>),
    UpdateSavedView {
        id: EntityRef,
        patch: Cbor<SavedViewPatch>,
    },
    DeleteSavedView(EntityRef),
}

/// A value written as the hex of its canonical CBOR: a saved view's filter
/// carries fields a newer build added as CBOR values, which JSON cannot hold
/// losslessly.
struct Cbor<T>(T);

impl<T: Serialize> Serialize for Cbor<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let bytes = sunrise_cbor::encode_canonical(&self.0).map_err(serde::ser::Error::custom)?;
        let hex = bytes.iter().fold(String::new(), |mut out, x| {
            use std::fmt::Write;
            let _ = write!(out, "{x:02x}");
            out
        });
        s.serialize_str(&hex)
    }
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for Cbor<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let hex = String::deserialize(d)?;
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|i| {
                hex.get(i..i + 2)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or_else(|| serde::de::Error::custom("not hex"))
            })
            .collect::<Result<Vec<u8>, D::Error>>()?;
        sunrise_cbor::decode_lenient(&bytes)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl Cmd {
    fn of(cmd: &Command) -> Option<Self> {
        Some(match cmd.clone() {
            Command::CreateTask(d) => Self::CreateTask(d),
            Command::UpdateTask { id, patch } => Self::UpdateTask { id, patch },
            Command::CompleteTask(id) => Self::CompleteTask(id),
            Command::DeferTask { id, to_ms } => Self::DeferTask { id, to_ms },
            Command::DeferTaskTo { id, to } => Self::DeferTaskTo { id, to },
            Command::DeleteTask(id) => Self::DeleteTask(id),
            Command::BulkTasks(items) => {
                Self::BulkTasks(items.into_iter().map(BulkItem::of).collect())
            }
            Command::Restore(id) => Self::Restore(id),
            Command::PromoteToStream { id, stream } => Self::PromoteToStream { id, stream },
            Command::CreateStream(d) => Self::CreateStream(d),
            Command::UpdateStream { id, patch } => Self::UpdateStream { id, patch },
            Command::DeleteStream {
                id,
                disposition: Disposition::Detach,
            } => Self::DeleteStream(id),
            Command::DeleteStream {
                id,
                disposition: Disposition::MoveTo(into),
            } => Self::DeleteStreamInto { id, into },
            Command::MarkTriaged { ids } => Self::MarkTriaged(ids),
            Command::ReturnToInbox { ids } => Self::ReturnToInbox(ids),
            Command::CreateContext(d) => Self::CreateContext(d),
            Command::UpdateContext { id, patch } => Self::UpdateContext { id, patch },
            Command::DeleteContext(id) => Self::DeleteContext(id),
            Command::CreateRoutine(d) => Self::CreateRoutine(d),
            Command::UpdateRoutine { id, patch } => Self::UpdateRoutine { id, patch },
            Command::DeleteRoutine(id) => Self::DeleteRoutine(id),
            Command::CreateBlock(d) => Self::CreateBlock(d),
            Command::DeleteBlock(id) => Self::DeleteBlock(id),
            Command::DetachFile(id) => Self::DetachFile(id),
            Command::MoveTasks { ids, after, before } => Self::MoveTasks { ids, after, before },
            Command::CreateSavedView(d) => Self::CreateSavedView(Cbor(d)),
            Command::UpdateSavedView { id, patch } => Self::UpdateSavedView {
                id,
                patch: Cbor(patch),
            },
            Command::DeleteSavedView(id) => Self::DeleteSavedView(id),
            _ => return None,
        })
    }

    fn into_command(self) -> Command {
        match self {
            Self::CreateTask(d) => Command::CreateTask(d),
            Self::UpdateTask { id, patch } => Command::UpdateTask { id, patch },
            Self::CompleteTask(id) => Command::CompleteTask(id),
            Self::DeferTask { id, to_ms } => Command::DeferTask { id, to_ms },
            Self::DeferTaskTo { id, to } => Command::DeferTaskTo { id, to },
            Self::DeleteTask(id) => Command::DeleteTask(id),
            Self::BulkTasks(items) => {
                Command::BulkTasks(items.into_iter().map(BulkItem::into_item).collect())
            }
            Self::Restore(id) => Command::Restore(id),
            Self::PromoteToStream { id, stream } => Command::PromoteToStream { id, stream },
            Self::CreateStream(d) => Command::CreateStream(d),
            Self::UpdateStream { id, patch } => Command::UpdateStream { id, patch },
            Self::DeleteStream(id) => Command::DeleteStream {
                id,
                disposition: Disposition::Detach,
            },
            Self::DeleteStreamInto { id, into } => Command::DeleteStream {
                id,
                disposition: Disposition::MoveTo(into),
            },
            Self::MarkTriaged(ids) => Command::MarkTriaged { ids },
            Self::ReturnToInbox(ids) => Command::ReturnToInbox { ids },
            Self::CreateContext(d) => Command::CreateContext(d),
            Self::UpdateContext { id, patch } => Command::UpdateContext { id, patch },
            Self::DeleteContext(id) => Command::DeleteContext(id),
            Self::CreateRoutine(d) => Command::CreateRoutine(d),
            Self::UpdateRoutine { id, patch } => Command::UpdateRoutine { id, patch },
            Self::DeleteRoutine(id) => Command::DeleteRoutine(id),
            Self::CreateBlock(d) => Command::CreateBlock(d),
            Self::DeleteBlock(id) => Command::DeleteBlock(id),
            Self::DetachFile(id) => Command::DetachFile(id),
            Self::MoveTasks { ids, after, before } => Command::MoveTasks { ids, after, before },
            Self::CreateSavedView(d) => Command::CreateSavedView(d.0),
            Self::UpdateSavedView { id, patch } => Command::UpdateSavedView { id, patch: patch.0 },
            Self::DeleteSavedView(id) => Command::DeleteSavedView(id),
        }
    }
}

/// One item of a bulk command, with its patch encoded losslessly.
#[derive(Serialize, Deserialize)]
struct BulkItem {
    id: EntityRef,
    action: BulkAction,
}

// Built and dropped once per item a history file holds; boxing the patch
// would buy nothing but a `with` shim for the box.
#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BulkAction {
    Complete,
    DeferTo(SunriseTime),
    Update(#[serde(with = "TaskPatchDef")] TaskPatch),
    Delete,
    Restore,
}

impl BulkItem {
    fn of(item: BulkTaskItem) -> Self {
        Self {
            id: item.id,
            action: match item.action {
                BulkTaskAction::Complete => BulkAction::Complete,
                BulkTaskAction::DeferTo(to) => BulkAction::DeferTo(to),
                BulkTaskAction::Update(patch) => BulkAction::Update(*patch),
                BulkTaskAction::Delete => BulkAction::Delete,
                BulkTaskAction::Restore => BulkAction::Restore,
            },
        }
    }

    fn into_item(self) -> BulkTaskItem {
        BulkTaskItem {
            id: self.id,
            action: match self.action {
                BulkAction::Complete => BulkTaskAction::Complete,
                BulkAction::DeferTo(to) => BulkTaskAction::DeferTo(to),
                BulkAction::Update(patch) => BulkTaskAction::Update(Box::new(patch)),
                BulkAction::Delete => BulkTaskAction::Delete,
                BulkAction::Restore => BulkTaskAction::Restore,
            },
        }
    }
}

/// `Option<Option<T>>` as absent (`None`), `null` (`Some(None)`) or a value
/// (`Some(Some(v))`). Used with `default` and `skip_serializing_if`, which is
/// what makes "absent" a third state.
mod double {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::option_option, clippy::ref_option)]
    pub(super) fn serialize<T: Serialize, S: Serializer>(
        v: &Option<Option<T>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            // Skipped by `skip_serializing_if`; written as null if not.
            None | Some(None) => s.serialize_none(),
            Some(Some(v)) => v.serialize(s),
        }
    }

    #[allow(clippy::option_option)]
    pub(super) fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "TaskPatch")]
#[allow(clippy::option_option)]
struct TaskPatchDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    body: Option<Option<NoteBody>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    stream_id: Option<Option<EntityRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    contexts: Option<Vec<EntityRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<TaskState>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    priority: Option<Option<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    energy: Option<Option<Energy>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    estimated_duration_s: Option<Option<u64>>,
    // The plan and the hard deadline keep the names an older build wrote a
    // persisted history under (ADR-0047 C1).
    #[serde(
        default,
        rename = "scheduled_at",
        skip_serializing_if = "Option::is_none",
        with = "double"
    )]
    planned_at: Option<Option<SunriseTime>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    target_at: Option<Option<SunriseTime>>,
    #[serde(
        default,
        rename = "due_at",
        skip_serializing_if = "Option::is_none",
        with = "double"
    )]
    hard_due_at: Option<Option<SunriseTime>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheduling_constraints: Option<Vec<ScheduleConstraint>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blocked_by: Option<Vec<EntityRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    assignee: Option<Option<EntityRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    reminder_lead_s: Option<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    sort_order: Option<Option<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "StreamPatch")]
#[allow(clippy::option_option)]
struct StreamPatchDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    description: Option<Option<NoteBody>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<StreamColor>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    parent_id: Option<Option<EntityRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    review_cadence: Option<StreamReviewCadence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sort_order: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    paused_until: Option<Option<SunriseTime>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    reminder_lead_s: Option<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    icon: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    default_context: Option<Option<EntityRef>>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ContextPatch")]
#[allow(clippy::option_option)]
struct ContextPatchDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    description: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "RoutinePatch")]
#[allow(clippy::option_option)]
struct RoutinePatchDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    template: Option<TaskTemplate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rrule: Option<RRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
    // A bare instant persisted before #506 reads as an instant `SunriseTime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    starts_at: Option<SunriseTime>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    ends_at: Option<Option<SunriseTime>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheduling_constraints: Option<Vec<ScheduleConstraint>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catchup_policy: Option<RoutineCatchupPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    grace_window_s: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forgiveness_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    paused_until: Option<Option<SunriseTime>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
}

#[cfg(test)]
mod tests;
