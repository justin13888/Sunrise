//! Places across the seam (`docs/02-domain/places.md`, ADR-0051): the
//! `plc_` entity mirrored, its draft and edit, and presence.
//!
//! The client's region monitor calls [`SunriseCore::on_place_presence`] from
//! its enter, exit and state callbacks. Nothing of presence is written: the
//! core keeps it in memory for the life of the handle, and the views that
//! read it ([`SunriseCore::here_now`], [`SunriseCore::place_requirements`])
//! and the region selection ([`SunriseCore::select_regions`]) are methods
//! that answer now, never a stored query result.

use sunrise_domain::{Place, PlaceDraft, PlaceEval, PlaceId, PlacePatch};
use sunrise_id::EntityRef;

use crate::dto::TaskItem;
use crate::{BindingError, SunriseCore};

/// See [`sunrise_domain::Place`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlaceItem {
    /// Its id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// Display name.
    pub name: String,
    /// Latitude × 10⁷.
    pub lat_e7: i32,
    /// Longitude × 10⁷.
    pub lon_e7: i32,
    /// Region radius in metres.
    pub radius_m: u32,
    /// Display text only; never geocoded.
    pub address_label: Option<String>,
    /// Deleted.
    pub deleted: bool,
}

impl From<&Place> for PlaceItem {
    fn from(p: &Place) -> Self {
        let Place {
            id,
            created_at,
            updated_at,
            name,
            lat_e7,
            lon_e7,
            radius_m,
            address_label,
            deleted,
            unknown: _,
        } = p;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            name: name.clone(),
            lat_e7: *lat_e7,
            lon_e7: *lon_e7,
            radius_m: *radius_m,
            address_label: address_label.clone(),
            deleted: *deleted,
        }
    }
}

impl crate::dto::Mirrored for Place {
    type Dto = PlaceItem;
}

/// A new Place, from an explicit user act. See [`sunrise_domain::PlaceDraft`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlaceDraftIn {
    /// Display name.
    pub name: String,
    /// Latitude × 10⁷.
    pub lat_e7: i32,
    /// Longitude × 10⁷.
    pub lon_e7: i32,
    /// Region radius in metres; 150 when absent.
    #[uniffi(default = None)]
    pub radius_m: Option<u32>,
    /// Display text only.
    #[uniffi(default = None)]
    pub address_label: Option<String>,
}

impl From<PlaceDraftIn> for PlaceDraft {
    fn from(d: PlaceDraftIn) -> Self {
        Self {
            name: d.name,
            lat_e7: d.lat_e7,
            lon_e7: d.lon_e7,
            radius_m: d
                .radius_m
                .unwrap_or(sunrise_domain::place::DEFAULT_RADIUS_M),
            address_label: d.address_label,
        }
    }
}

/// An edit of a Place: each set field is written, and only those.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct PlaceEdit {
    /// A new name.
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// A new latitude × 10⁷.
    #[uniffi(default = None)]
    pub lat_e7: Option<i32>,
    /// A new longitude × 10⁷.
    #[uniffi(default = None)]
    pub lon_e7: Option<i32>,
    /// A new radius in metres.
    #[uniffi(default = None)]
    pub radius_m: Option<u32>,
    /// A new label.
    #[uniffi(default = None)]
    pub address_label: Option<String>,
    /// Clear the label. Ignored when `address_label` is set.
    #[uniffi(default = false)]
    pub clear_address_label: bool,
}

impl From<PlaceEdit> for PlacePatch {
    fn from(e: PlaceEdit) -> Self {
        let address_label = match (e.address_label, e.clear_address_label) {
            (Some(label), _) => Some(Some(label)),
            (None, true) => Some(None),
            (None, false) => None,
        };
        Self {
            name: e.name,
            lat_e7: e.lat_e7,
            lon_e7: e.lon_e7,
            radius_m: e.radius_m,
            address_label,
        }
    }
}

/// One open task's place requirement, now. See
/// [`sunrise_core::TaskPlaceStatus`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TaskPlaceStatusItem {
    /// The task.
    pub task: EntityRef,
    /// The places it names, any one of which satisfies it.
    pub places: Vec<String>,
    /// Whether any constraint naming a place is `hard`.
    pub hard: bool,
    /// How it evaluates now.
    pub status: PlaceStatus,
    /// A `hard` requirement that evaluates `Violated`: show "not actionable
    /// here". Never blocks a write.
    pub not_actionable_here: bool,
}

impl From<sunrise_core::TaskPlaceStatus> for TaskPlaceStatusItem {
    fn from(s: sunrise_core::TaskPlaceStatus) -> Self {
        Self {
            task: s.task,
            places: s.places.into_iter().map(PlaceId::to_str).collect(),
            hard: s.hard,
            status: s.eval.into(),
            not_actionable_here: s.not_actionable_here,
        }
    }
}

/// A rough position for [`SunriseCore::select_regions`], degrees × 10⁷. Used
/// for that call and kept nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ApproxPosition {
    /// Latitude × 10⁷.
    pub lat_e7: i32,
    /// Longitude × 10⁷.
    pub lon_e7: i32,
}

/// How a place requirement evaluates now. See
/// [`sunrise_domain::PlaceEval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum PlaceStatus {
    /// The user is at one of the required places.
    Satisfied,
    /// Every required place is known, and the user is at none of them.
    Violated,
    /// Not enough is known to say. Never shown as a violation.
    Unknown,
}

impl From<PlaceEval> for PlaceStatus {
    fn from(e: PlaceEval) -> Self {
        match e {
            PlaceEval::Satisfied => Self::Satisfied,
            PlaceEval::Violated => Self::Violated,
            PlaceEval::Unknown => Self::Unknown,
        }
    }
}

/// `place` as a [`PlaceId`], or [`BindingError::BadId`].
fn place_id(place: &str) -> Result<PlaceId, BindingError> {
    PlaceId::parse(place).map_err(|e| BindingError::BadId {
        id: place.to_string(),
        cause: e.to_string(),
    })
}

#[uniffi::export]
impl SunriseCore {
    /// The OS said the user is (`present`) or is not inside `place`'s region.
    /// See [`sunrise_core::Core::on_place_presence`].
    ///
    /// # Errors
    ///
    /// [`BindingError::BadId`] when `place` is not a `plc_` reference.
    pub fn on_place_presence(&self, place: String, present: bool) -> Result<(), BindingError> {
        self.inner.on_place_presence(place_id(&place)?, present);
        Ok(())
    }

    /// The OS stopped monitoring `place`. See
    /// [`sunrise_core::Core::forget_place_presence`].
    ///
    /// # Errors
    ///
    /// [`BindingError::BadId`] when `place` is not a `plc_` reference.
    pub fn forget_place_presence(&self, place: String) -> Result<(), BindingError> {
        self.inner.forget_place_presence(place_id(&place)?);
        Ok(())
    }

    /// Location permission was denied or revoked: every place is unknown.
    pub fn clear_place_presence(&self) {
        self.inner.clear_place_presence();
    }

    /// How a requirement naming `places` (any one satisfies it) evaluates
    /// now. See [`sunrise_core::Core::evaluate_place`].
    ///
    /// # Errors
    ///
    /// [`BindingError::BadId`] when an entry is not a `plc_` reference.
    pub fn evaluate_place(&self, places: Vec<String>) -> Result<PlaceStatus, BindingError> {
        let ids = places
            .iter()
            .map(|p| place_id(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(self.inner.evaluate_place(&ids).into())
    }

    /// "Here now": every open task whose place requirement evaluates
    /// `Satisfied`. See [`sunrise_core::Core::here_now`].
    ///
    /// # Errors
    ///
    /// [`BindingError`] after close, or on a storage read error.
    pub fn here_now(&self) -> Result<Vec<TaskItem>, BindingError> {
        Ok(self.inner.here_now()?.iter().map(TaskItem::from).collect())
    }

    /// Every open task that names a place, with how it evaluates now and
    /// whether it is "not actionable here". See
    /// [`sunrise_core::Core::place_requirements`].
    ///
    /// # Errors
    ///
    /// [`BindingError`] after close, or on a storage read error.
    pub fn place_requirements(&self) -> Result<Vec<TaskPlaceStatusItem>, BindingError> {
        Ok(self
            .inner
            .place_requirements()?
            .into_iter()
            .map(TaskPlaceStatusItem::from)
            .collect())
    }

    /// The places to ask the OS to monitor, at most `cap - 1` of them, as
    /// `plc_` references. See [`sunrise_core::Core::select_regions`].
    ///
    /// # Errors
    ///
    /// [`BindingError`] after close, or on a storage read error.
    pub fn select_regions(
        &self,
        approx_position: Option<ApproxPosition>,
        cap: u32,
    ) -> Result<Vec<String>, BindingError> {
        let at = approx_position.map(|p| (p.lat_e7, p.lon_e7));
        let cap = usize::try_from(cap).unwrap_or(usize::MAX);
        Ok(self
            .inner
            .select_regions(at, cap)?
            .into_iter()
            .map(PlaceId::to_str)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_reference_parses_and_anything_else_is_a_bad_id() {
        let ok = "plc_01HZX4T7Q0000000000000000Z";
        assert_eq!(place_id(ok).unwrap().to_str(), ok);
        for bad in ["tsk_01HZX4T7Q0000000000000000Z", "plc_", ""] {
            assert!(
                matches!(place_id(bad), Err(BindingError::BadId { ref id, .. }) if id == bad),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn every_evaluation_crosses_the_seam_as_itself() {
        assert_eq!(
            PlaceStatus::from(PlaceEval::Satisfied),
            PlaceStatus::Satisfied
        );
        assert_eq!(
            PlaceStatus::from(PlaceEval::Violated),
            PlaceStatus::Violated
        );
        assert_eq!(PlaceStatus::from(PlaceEval::Unknown), PlaceStatus::Unknown);
    }

    #[test]
    fn a_draft_defaults_its_radius_and_an_edit_clears_the_label_only_when_asked() {
        let d = PlaceDraft::from(PlaceDraftIn {
            name: "Gym".into(),
            lat_e7: 1,
            lon_e7: 2,
            radius_m: None,
            address_label: None,
        });
        assert_eq!(d.radius_m, sunrise_domain::place::DEFAULT_RADIUS_M);
        let clear = PlacePatch::from(PlaceEdit {
            clear_address_label: true,
            ..PlaceEdit::default()
        });
        assert_eq!(clear.address_label, Some(None));
        let set = PlacePatch::from(PlaceEdit {
            address_label: Some("x".into()),
            clear_address_label: true,
            ..PlaceEdit::default()
        });
        assert_eq!(set.address_label, Some(Some("x".into())));
        assert!(PlacePatch::from(PlaceEdit::default()).is_empty());
    }

    #[test]
    fn a_place_crosses_the_seam_field_for_field() {
        let p = Place {
            id: EntityRef::new(sunrise_id::EntityKind::Place, [7; 16]),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            name: "Gym".into(),
            lat_e7: -3,
            lon_e7: 4,
            radius_m: 200,
            address_label: Some("a".into()),
            deleted: true,
            unknown: sunrise_domain::Unknowns::new(),
        };
        let item = PlaceItem::from(&p);
        assert_eq!(
            (
                item.id,
                item.lat_e7,
                item.lon_e7,
                item.radius_m,
                item.deleted
            ),
            (p.id, -3, 4, 200, true)
        );
        assert_eq!(item.address_label.as_deref(), Some("a"));
    }
}
