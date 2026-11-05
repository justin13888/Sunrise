//! Streams and contexts: the records a task is filed under.

use sunrise_domain::{Context, NoteBody, Stream, StreamColor, StreamReviewCadence};
use sunrise_id::EntityRef;

use super::{task::patch_field, time::legacy_instant};

/// See [`sunrise_domain::Stream`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamItem {
    /// Stream id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<NoteBody>,
    /// Colour.
    pub color: StreamColor,
    /// Optional icon name.
    pub icon: Option<String>,
    /// Parent stream, for nesting.
    pub parent_id: Option<EntityRef>,
    /// Sibling ordering key.
    pub sort_order: String,
    /// Archived.
    pub archived: bool,
    /// Paused.
    pub paused: bool,
    /// Paused until this instant.
    pub paused_until: Option<jiff::Timestamp>,
    /// How often this stream expects a review.
    pub review_cadence: StreamReviewCadence,
    /// Context applied by default to tasks captured into this stream.
    pub default_context: Option<EntityRef>,
    /// Default reminder lead time for this stream's tasks, in seconds.
    pub reminder_lead_s: Option<u32>,
    /// Tombstoned.
    pub deleted: bool,
    /// Where the stream's deletion re-homed what it held (ADR-0046 §4); none
    /// for a live stream and for a delete that detached them.
    #[uniffi(default = None)]
    pub rehomed_to: Option<EntityRef>,
}

impl From<&Stream> for StreamItem {
    fn from(s: &Stream) -> Self {
        let Stream {
            id,
            created_at,
            updated_at,
            name,
            description,
            color,
            icon,
            parent_id,
            sort_order,
            archived,
            paused,
            paused_until,
            review_cadence,
            default_context,
            reminder_lead_s,
            rehomed_to,
            deleted,
            unknown: _,
        } = s;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            name: name.clone(),
            description: description.clone(),
            color: color.clone(),
            icon: icon.clone(),
            parent_id: *parent_id,
            sort_order: sort_order.clone(),
            archived: *archived,
            paused: *paused,
            paused_until: paused_until.as_ref().map(|p| legacy_instant(p, "UTC")),
            review_cadence: review_cadence.clone(),
            default_context: *default_context,
            reminder_lead_s: *reminder_lead_s,
            deleted: *deleted,
            rehomed_to: *rehomed_to,
        }
    }
}

/// See [`sunrise_domain::StreamDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct StreamDraftIn {
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<NoteBody>,
    /// Colour; the core picks one when absent.
    pub color: Option<StreamColor>,
    /// Parent stream.
    pub parent_id: Option<EntityRef>,
    /// Review cadence.
    pub review_cadence: Option<StreamReviewCadence>,
    /// Default reminder lead time for this stream's tasks, in seconds.
    pub reminder_lead_s: Option<u32>,
    /// Optional icon name.
    #[uniffi(default = None)]
    pub icon: Option<String>,
    /// Context applied to Tasks captured into this stream.
    #[uniffi(default = None)]
    pub default_context: Option<EntityRef>,
}

impl From<StreamDraftIn> for sunrise_domain::StreamDraft {
    fn from(d: StreamDraftIn) -> Self {
        Self {
            name: d.name,
            description: d.description,
            color: d.color,
            parent_id: d.parent_id,
            review_cadence: d.review_cadence,
            reminder_lead_s: d.reminder_lead_s,
            icon: d.icon,
            default_context: d.default_context,
        }
    }
}

/// An edit to a stream. Split-optional, like [`TaskEdit`](super::TaskEdit).
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct StreamEdit {
    /// New name.
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// New description.
    #[uniffi(default = None)]
    pub set_description: Option<NoteBody>,
    /// Clear the description.
    #[uniffi(default = false)]
    pub clear_description: bool,
    /// New colour.
    #[uniffi(default = None)]
    pub color: Option<StreamColor>,
    /// New parent.
    #[uniffi(default = None)]
    pub set_parent_id: Option<EntityRef>,
    /// Detach from any parent.
    #[uniffi(default = false)]
    pub clear_parent_id: bool,
    /// New review cadence.
    #[uniffi(default = None)]
    pub review_cadence: Option<StreamReviewCadence>,
    /// New position among siblings.
    ///
    /// Not split-optional like the fields around it, because there is no
    /// "clear it" for a position: a stream is always *somewhere* in the list.
    /// Compute the value with [`crate::vocab::stream_sort_key_between`] rather than
    /// by hand — a key the core does not recognise is rejected, not repaired.
    #[uniffi(default = None)]
    pub sort_order: Option<String>,
    /// Archive or unarchive.
    #[uniffi(default = None)]
    pub archived: Option<bool>,
    /// Pause or resume.
    #[uniffi(default = None)]
    pub paused: Option<bool>,
    /// Pause until this instant.
    #[uniffi(default = None)]
    pub set_paused_until: Option<jiff::Timestamp>,
    /// Clear the pause deadline.
    #[uniffi(default = false)]
    pub clear_paused_until: bool,
    /// New default reminder lead time, in seconds.
    #[uniffi(default = None)]
    pub set_reminder_lead_s: Option<u32>,
    /// Clear it, falling back to the device default.
    #[uniffi(default = false)]
    pub clear_reminder_lead_s: bool,
    /// New icon name.
    #[uniffi(default = None)]
    pub set_icon: Option<String>,
    /// Clear the icon.
    #[uniffi(default = false)]
    pub clear_icon: bool,
    /// New default Context for Tasks captured into this stream.
    #[uniffi(default = None)]
    pub set_default_context: Option<EntityRef>,
    /// Clear the default Context.
    #[uniffi(default = false)]
    pub clear_default_context: bool,
}

impl From<StreamEdit> for sunrise_domain::StreamPatch {
    fn from(e: StreamEdit) -> Self {
        Self {
            name: e.name,
            description: patch_field(e.set_description, e.clear_description),
            color: e.color,
            parent_id: patch_field(e.set_parent_id, e.clear_parent_id),
            review_cadence: e.review_cadence,
            sort_order: e.sort_order,
            archived: e.archived,
            paused: e.paused,
            paused_until: patch_field(e.set_paused_until.map(Into::into), e.clear_paused_until),
            reminder_lead_s: patch_field(e.set_reminder_lead_s, e.clear_reminder_lead_s),
            icon: patch_field(e.set_icon, e.clear_icon),
            default_context: patch_field(e.set_default_context, e.clear_default_context),
        }
    }
}

/// See [`sunrise_domain::Context`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ContextItem {
    /// Context id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Archived contexts stay on their tasks but leave the pickers.
    pub archived: bool,
    /// Tombstoned.
    pub deleted: bool,
}

impl From<&Context> for ContextItem {
    fn from(c: &Context) -> Self {
        let Context {
            id,
            created_at,
            updated_at,
            name,
            description,
            archived,
            deleted,
            unknown: _,
        } = c;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            name: name.clone(),
            description: description.clone(),
            archived: *archived,
            deleted: *deleted,
        }
    }
}

/// See [`sunrise_domain::ContextDraft`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ContextDraftIn {
    /// Display name, without the leading `@`.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
}

impl From<ContextDraftIn> for sunrise_domain::ContextDraft {
    fn from(d: ContextDraftIn) -> Self {
        Self {
            name: d.name,
            description: d.description,
        }
    }
}

/// An edit to a context. Split-optional, like [`TaskEdit`](super::TaskEdit).
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct ContextEdit {
    /// New name.
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// New description.
    #[uniffi(default = None)]
    pub set_description: Option<String>,
    /// Clear the description.
    #[uniffi(default = false)]
    pub clear_description: bool,
    /// Archive or unarchive.
    #[uniffi(default = None)]
    pub archived: Option<bool>,
}

impl From<ContextEdit> for sunrise_domain::ContextPatch {
    fn from(e: ContextEdit) -> Self {
        Self {
            name: e.name,
            description: patch_field(e.set_description, e.clear_description),
            archived: e.archived,
        }
    }
}
