//! Stream entity per `docs/02-domain/streams.md`.

use crate::common::NoteBody;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::{validate_title, ValidationError, MAX_STREAM_NAME_LEN};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Fixed Stream colors per the spec's palette. Currently 8 fixed values.
///
/// An unrecognised color reads as [`StreamColor::Slate`] and is written back
/// verbatim, on the wire and in storage alike.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// A color this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(StreamColor, fallback = Slate, {
    Slate => "slate",
    Rose => "rose",
    Amber => "amber",
    Emerald => "emerald",
    Sky => "sky",
    Indigo => "indigo",
    Violet => "violet",
    Pink => "pink",
});

/// Review cadence for a Stream.
///
/// An unrecognised cadence reads as [`StreamReviewCadence::Weekly`], the
/// documented default, and is written back verbatim. Reading it as `None`
/// would make a Stream silently stop appearing in reviews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamReviewCadence {
    /// Weekly review.
    Weekly,
    /// Every two weeks.
    Biweekly,
    /// Monthly review.
    Monthly,
    /// No review reminders.
    None,
    /// A cadence this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(StreamReviewCadence, fallback = Weekly, {
    Weekly => "weekly",
    Biweekly => "biweekly",
    Monthly => "monthly",
    None => "none",
});

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
    /// Fractional-index sort key: where this Stream sits among its siblings.
    ///
    /// A base-26 string over `A`..=`Z` whose lexicographic order *is* its
    /// numeric order, so a list sorts with `ORDER BY sort_order` and a single
    /// reorder rewrites exactly one row. See [`crate::sort_order`] for the
    /// encoding and [`crate::sort_order::between`] for the arithmetic.
    ///
    /// **Concurrent reorders do not merge.** Under
    /// [ADR-0014](../../../docs/11-adr/0014-entity-level-lww-merge.md) the
    /// whole Stream is one last-writer-wins unit on `(hlc, device_id, seq)`,
    /// and `sort_order` is a field on it like any other — so two devices that
    /// rearrange the same list while apart converge on **one device's whole
    /// Stream row**, not on an interleave of the two arrangements. The
    /// fractional index still earns its place: it keeps one reorder from
    /// rewriting every sibling, which is what makes the losing device's
    /// *other* rows survive. It does not make the two orderings merge, and
    /// nothing here pretends otherwise.
    ///
    /// The empty string means "never ordered" — the column default for a row
    /// that predates the write path — and sorts first, which is where those
    /// rows already were. It is not a valid key: see
    /// [`crate::sort_order::is_valid`].
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
    /// Optional icon name.
    ///
    /// The column and the round-trip have always existed; this field is what
    /// gives them a caller. Without it no command could set an icon, and the
    /// only writer in the tree was a test issuing direct SQL.
    pub icon: Option<String>,
    /// Optional Context applied to Tasks captured into this Stream.
    pub default_context: Option<EntityRef>,
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
    /// New position among siblings, as a fractional index.
    ///
    /// The whole of the reorder write path: a client computes the key with
    /// [`crate::sort_order::between`] from the two rows the dragged one landed
    /// between, and sends it here. Nothing else moves.
    ///
    /// A reorder is an ordinary Stream update, so it is one entity-level LWW
    /// write and merges like one — see [`Stream::sort_order`].
    pub sort_order: Option<String>,
    /// Archive / unarchive.
    pub archived: Option<bool>,
    /// Pause / unpause.
    pub paused: Option<bool>,
    /// New pause expiry.
    pub paused_until: Option<Option<Timestamp>>,
    /// New default reminder lead time; `Some(None)` falls back to the device
    /// default.
    pub reminder_lead_s: Option<Option<u32>>,
    /// New icon; `Some(None)` clears it.
    pub icon: Option<Option<String>>,
    /// New default Context; `Some(None)` clears it.
    pub default_context: Option<Option<EntityRef>>,
}

impl StreamPatch {
    /// Validate.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(n) = &self.name {
            let _ = validate_title(n, "stream.name", MAX_STREAM_NAME_LEN)?;
        }
        // Refused here rather than repaired, because there is no honest
        // repair: a key outside `A`..=`Z` has no defined place in the list,
        // and picking one for the user would move a row they did not drag.
        if let Some(k) = &self.sort_order {
            crate::sort_order::validate(k)?;
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

    #[test]
    fn patch_accepts_a_well_formed_sort_key() {
        let p = StreamPatch {
            sort_order: Some("ABN".into()),
            ..Default::default()
        };
        p.validate().unwrap();
    }

    #[test]
    fn patch_rejects_a_sort_key_it_could_not_have_produced() {
        // The literal every build before the write path hardcoded, the unset
        // sentinel, and a second spelling of a number already in use.
        for bad in ["a0", "", "NA", "N1"] {
            let p = StreamPatch {
                sort_order: Some(bad.into()),
                ..Default::default()
            };
            assert_eq!(
                p.validate(),
                Err(ValidationError::Field {
                    field: crate::sort_order::FIELD,
                    constraint: "sort_order_key",
                }),
                "{bad:?} should not be writable"
            );
        }
    }
}
