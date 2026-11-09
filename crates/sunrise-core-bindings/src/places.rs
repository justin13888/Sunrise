//! Presence at Places across the seam (`docs/02-domain/places.md` §Presence,
//! ADR-0051 §3).
//!
//! The client's region monitor calls [`SunriseCore::on_place_presence`] from
//! its enter, exit and state callbacks. Nothing here is written: the core
//! keeps presence in memory for the life of the handle.

use sunrise_domain::{PlaceEval, PlaceId};

use crate::{BindingError, SunriseCore};

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
}
