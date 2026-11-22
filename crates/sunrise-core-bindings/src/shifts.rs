//! Context shifts across the seam (ADR-0073): the `shf_` entity mirrored,
//! its draft and edit.

use sunrise_domain::Shift;
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
