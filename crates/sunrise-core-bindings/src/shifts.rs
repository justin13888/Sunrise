//! Context shifts across the seam (ADR-0073): the `shf_` entity mirrored,
//! its draft and edit, and the "While you were away" summary.

use sunrise_domain::{AwaySummary, Shift, ShiftDraft, ShiftPatch};
use sunrise_id::EntityRef;

/// See [`sunrise_domain::Shift`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ShiftItem {
    /// Its id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// What the user calls it ("Travel: Tokyo").
    pub name: String,
    /// When it starts.
    pub starts_at: jiff::Timestamp,
    /// When it ends, exclusive; none while it runs until ended.
    pub ends_at: Option<jiff::Timestamp>,
    /// The destination's IANA zone.
    pub zone: Option<String>,
    /// The destination place (`plc_`).
    pub place: Option<EntityRef>,
    /// The streams it keeps primary, sorted.
    pub primary_streams: Vec<EntityRef>,
    /// The contexts it keeps primary, sorted.
    pub primary_contexts: Vec<EntityRef>,
    /// Deleted.
    pub deleted: bool,
}

impl From<&Shift> for ShiftItem {
    fn from(s: &Shift) -> Self {
        let Shift {
            id,
            created_at,
            updated_at,
            name,
            starts_at,
            ends_at,
            zone,
            place,
            primary_streams,
            primary_contexts,
            deleted,
            unknown: _,
        } = s;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            name: name.clone(),
            starts_at: *starts_at,
            ends_at: *ends_at,
            zone: zone.clone(),
            place: *place,
            primary_streams: primary_streams.iter().copied().collect(),
            primary_contexts: primary_contexts.iter().copied().collect(),
            deleted: *deleted,
        }
    }
}

impl crate::dto::Mirrored for Shift {
    type Dto = ShiftItem;
}

/// A new shift. See [`sunrise_domain::ShiftDraft`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ShiftDraftIn {
    /// What the user calls it.
    pub name: String,
    /// When it starts; now when absent.
    #[uniffi(default = None)]
    pub starts_at: Option<jiff::Timestamp>,
    /// When it ends; it runs until ended when absent.
    #[uniffi(default = None)]
    pub ends_at: Option<jiff::Timestamp>,
    /// The destination's IANA zone.
    #[uniffi(default = None)]
    pub zone: Option<String>,
    /// The destination place (`plc_`).
    #[uniffi(default = None)]
    pub place: Option<EntityRef>,
    /// The streams it keeps primary.
    #[uniffi(default = [])]
    pub primary_streams: Vec<EntityRef>,
    /// The contexts it keeps primary.
    #[uniffi(default = [])]
    pub primary_contexts: Vec<EntityRef>,
}

impl From<ShiftDraftIn> for ShiftDraft {
    fn from(d: ShiftDraftIn) -> Self {
        Self {
            name: d.name,
            starts_at: d.starts_at,
            ends_at: d.ends_at,
            zone: d.zone,
            place: d.place,
            primary_streams: d.primary_streams.into_iter().collect(),
            primary_contexts: d.primary_contexts.into_iter().collect(),
        }
    }
}

/// An edit of a shift: each set field is written, and only those. A
/// `clear_*` flag is ignored when its field is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct ShiftEdit {
    /// A new name.
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// A new start.
    #[uniffi(default = None)]
    pub starts_at: Option<jiff::Timestamp>,
    /// A new end.
    #[uniffi(default = None)]
    pub ends_at: Option<jiff::Timestamp>,
    /// Clear the end: the shift runs until ended.
    #[uniffi(default = false)]
    pub clear_ends_at: bool,
    /// A new zone.
    #[uniffi(default = None)]
    pub zone: Option<String>,
    /// Clear the zone.
    #[uniffi(default = false)]
    pub clear_zone: bool,
    /// A new place.
    #[uniffi(default = None)]
    pub place: Option<EntityRef>,
    /// Clear the place.
    #[uniffi(default = false)]
    pub clear_place: bool,
    /// Replace the streams it keeps primary.
    #[uniffi(default = None)]
    pub primary_streams: Option<Vec<EntityRef>>,
    /// Replace the contexts it keeps primary.
    #[uniffi(default = None)]
    pub primary_contexts: Option<Vec<EntityRef>>,
}

/// A field an edit sets (`Some(Some)`), clears (`Some(None)`) or leaves.
#[allow(clippy::option_option)]
fn set_or_clear<T>(value: Option<T>, clear: bool) -> Option<Option<T>> {
    match (value, clear) {
        (Some(v), _) => Some(Some(v)),
        (None, true) => Some(None),
        (None, false) => None,
    }
}

impl From<ShiftEdit> for ShiftPatch {
    fn from(e: ShiftEdit) -> Self {
        Self {
            name: e.name,
            starts_at: e.starts_at,
            ends_at: set_or_clear(e.ends_at, e.clear_ends_at),
            zone: set_or_clear(e.zone, e.clear_zone),
            place: set_or_clear(e.place, e.clear_place),
            primary_streams: e.primary_streams.map(|v| v.into_iter().collect()),
            primary_contexts: e.primary_contexts.map(|v| v.into_iter().collect()),
        }
    }
}

/// One routine occurrence missed during a shift. See
/// [`sunrise_domain::MissedOccurrence`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MissedOccurrenceItem {
    /// The routine.
    pub routine: EntityRef,
    /// The occurrence's key, `YYYY-MM-DDTHH:MM`.
    pub key: String,
    /// When it was due.
    pub at: jiff::Timestamp,
}

/// "While you were away". See [`sunrise_domain::AwaySummary`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AwaySummaryItem {
    /// The window's start; none when the shift had not started.
    pub window_start: Option<jiff::Timestamp>,
    /// The window's end: the shift's end, or now while it runs.
    pub window_end: Option<jiff::Timestamp>,
    /// Routine occurrences missed, by instant.
    pub missed: Vec<MissedOccurrenceItem>,
    /// Open tasks whose hard deadline passed, by deadline.
    pub deadlines_passed: Vec<EntityRef>,
    /// Tasks captured during the shift, by creation.
    pub captured: Vec<EntityRef>,
}

impl From<AwaySummary> for AwaySummaryItem {
    fn from(s: AwaySummary) -> Self {
        Self {
            window_start: s.window.map(|w| w.0),
            window_end: s.window.map(|w| w.1),
            missed: s
                .missed
                .into_iter()
                .map(|m| MissedOccurrenceItem {
                    routine: m.routine,
                    key: m.key,
                    at: m.at,
                })
                .collect(),
            deadlines_passed: s.deadlines_passed,
            captured: s.captured,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    #[test]
    fn an_edit_sets_clears_or_leaves_each_field() {
        let place = EntityRef::new(EntityKind::Place, [1; 16]);
        let p = ShiftPatch::from(ShiftEdit {
            name: Some("Tokyo".into()),
            clear_ends_at: true,
            zone: Some("Asia/Tokyo".into()),
            clear_zone: true,
            place: Some(place),
            ..ShiftEdit::default()
        });
        assert_eq!(p.name.as_deref(), Some("Tokyo"));
        assert_eq!(p.ends_at, Some(None));
        assert_eq!(p.zone, Some(Some("Asia/Tokyo".into())));
        assert_eq!(p.place, Some(Some(place)));
        assert_eq!(p.starts_at, None);
        assert!(p.primary_streams.is_none());
        assert!(ShiftPatch::from(ShiftEdit::default()).is_empty());
    }
}
