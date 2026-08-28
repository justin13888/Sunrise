//! Stream entity per `docs/02-domain/streams.md`.

use crate::common::NoteBody;
use crate::unknown::Unknowns;
use crate::validation::{validate_title, ValidationError, MAX_STREAM_NAME_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Fixed Stream colors per the spec's palette. v1: 8 fixed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamColor {
    /// Default neutral.
    Slate,
    /// Reds.
    Rose,
    /// Oranges.
    Amber,
    /// Greens.
    Emerald,
    /// Blues.
    Sky,
    /// Indigos.
    Indigo,
    /// Purples.
    Violet,
    /// Pinks.
    Pink,
}

impl StreamColor {
    /// Lowercase wire/storage string form (matches the serde representation).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slate => "slate",
            Self::Rose => "rose",
            Self::Amber => "amber",
            Self::Emerald => "emerald",
            Self::Sky => "sky",
            Self::Indigo => "indigo",
            Self::Violet => "violet",
            Self::Pink => "pink",
        }
    }

    /// Parse from the lowercase string form. Unknown strings fall back to
    /// [`StreamColor::Slate`] so a forward-compatible DB never fails to load.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "rose" => Self::Rose,
            "amber" => Self::Amber,
            "emerald" => Self::Emerald,
            "sky" => Self::Sky,
            "indigo" => Self::Indigo,
            "violet" => Self::Violet,
            "pink" => Self::Pink,
            // "slate" and any unknown value → Slate.
            _ => Self::Slate,
        }
    }
}

/// Review cadence for a Stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamReviewCadence {
    /// Weekly review.
    Weekly,
    /// Every two weeks.
    Biweekly,
    /// Monthly review.
    Monthly,
    /// No review reminders.
    None,
}

impl StreamReviewCadence {
    /// The stable lowercase wire/storage string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Weekly => "weekly",
            Self::Biweekly => "biweekly",
            Self::Monthly => "monthly",
            Self::None => "none",
        }
    }

    /// Parse from the wire/storage string. An unrecognised value degrades to
    /// [`StreamReviewCadence::Weekly`] rather than failing.
    ///
    /// The documented default. Degrading to `None` would make a Stream silently
    /// stop appearing in reviews.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "biweekly" => Self::Biweekly,
            "monthly" => Self::Monthly,
            "none" => Self::None,
            // "weekly" and anything this build has never heard of.
            _ => Self::Weekly,
        }
    }
}

crate::unknown::lossy_enum!(StreamReviewCadence);

/// Persisted Stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// Stream id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-update time.
    pub updated_at: Timestamp,
    /// Display name (1..=128 chars after trim).
    pub name: String,
    /// Optional description (rich text).
    #[serde(default)]
    pub description: Option<NoteBody>,
    /// Color from a fixed palette.
    pub color: StreamColor,
    /// Optional icon id.
    ///
    /// Owned, because `Option<&'static str>` could not be deserialized: a
    /// borrowed `'static` string cannot come out of a deserializer, so the
    /// field carried `#[serde(skip_deserializing)]` and ALWAYS read back as
    /// `None`. Setting an icon therefore survived exactly as long as the
    /// process that set it — it went out on the wire and came back gone, on
    /// every device including the one that set it. That is a data-loss bug
    /// wearing a lifetime annotation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Optional parent Stream — one-level nesting only.
    #[serde(default)]
    pub parent_id: Option<EntityRef>,
    /// Fractional-index sort key.
    pub sort_order: String,
    /// Archived state.
    #[serde(default)]
    pub archived: bool,
    /// Paused state.
    #[serde(default)]
    pub paused: bool,
    /// Optional pause expiry.
    #[serde(default)]
    pub paused_until: Option<Timestamp>,
    /// Review cadence preference.
    pub review_cadence: StreamReviewCadence,
    /// Optional default Context applied to new tasks captured into this Stream.
    #[serde(default)]
    pub default_context: Option<EntityRef>,
    /// Default reminder lead time for this Stream's Tasks, in seconds.
    ///
    /// Middle of the lead-time hierarchy in
    /// `docs/08-features/notifications.md`: a Task's own value wins, this is
    /// the Stream default, and the device's global default is the floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder_lead_s: Option<u32>,
    /// Tombstone.
    #[serde(default)]
    pub deleted: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Draft used by the UI when creating a Stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamDraft {
    /// Display name.
    pub name: String,
    /// Optional description.
    pub description: Option<NoteBody>,
    /// Color (defaults to `Slate` if omitted).
    pub color: Option<StreamColor>,
    /// Optional parent stream id (one-level nesting only).
    pub parent_id: Option<EntityRef>,
    /// Optional review cadence (defaults to `Weekly`).
    pub review_cadence: Option<StreamReviewCadence>,
    /// Optional default reminder lead time for this Stream's Tasks, seconds.
    pub reminder_lead_s: Option<u32>,
}

impl StreamDraft {
    /// Validate.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(&self.name, "stream.name", MAX_STREAM_NAME_LEN)?;
        Ok(())
    }
}

/// Patch applied via `Command::UpdateStream`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamPatch {
    /// New name.
    pub name: Option<String>,
    /// New description.
    pub description: Option<Option<NoteBody>>,
    /// New color.
    pub color: Option<StreamColor>,
    /// New parent id.
    pub parent_id: Option<Option<EntityRef>>,
    /// New review cadence.
    pub review_cadence: Option<StreamReviewCadence>,
    /// Archive / unarchive.
    pub archived: Option<bool>,
    /// Pause / unpause.
    pub paused: Option<bool>,
    /// New pause expiry.
    pub paused_until: Option<Option<Timestamp>>,
    /// New default reminder lead time; `Some(None)` falls back to the device
    /// default.
    pub reminder_lead_s: Option<Option<u32>>,
}

impl StreamPatch {
    /// Validate.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(n) = &self.name {
            let _ = validate_title(n, "stream.name", MAX_STREAM_NAME_LEN)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_validates_name() {
        let d = StreamDraft {
            name: "Work: Acme".into(),
            ..Default::default()
        };
        d.validate().unwrap();
    }

    #[test]
    fn draft_rejects_empty_name() {
        let d = StreamDraft {
            name: "   ".into(),
            ..Default::default()
        };
        assert_eq!(d.validate(), Err(ValidationError::InvalidTitle));
    }
}
