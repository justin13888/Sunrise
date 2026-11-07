//! Places and on-device presence, per `docs/02-domain/places.md` and
//! ADR-0051 (`docs/11-adr/0051-places.md`).
//!
//! What lives here is the pure half of the design, the part every client and
//! the core share:
//!
//! - [`PlaceId`], the `plc_` reference a place requirement names;
//! - [`Place`], a user-entered point with a radius, and its validation;
//! - [`Presence`], the device's in-memory answer to "is the user at this
//!   place", and [`evaluate_place`], a pure function of a requirement and that
//!   answer;
//! - [`select_regions`], which picks the places a client asks the OS to
//!   monitor under the OS's region cap.
//!
//! Nothing here reads a clock, a location stack or storage, and nothing here
//! can be serialized: [`Presence`] deliberately implements neither `Serialize`
//! nor a `Debug` that prints its entries, so no op, envelope, row or log line
//! can carry a presence value by accident (ADR-0051 §3).

use crate::validation::{validate_title, ValidationError};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use sunrise_id::{Ulid, UlidParseError};
use thiserror::Error;

/// The prefix of a Place reference (`docs/02-domain/identifiers.md`).
pub const PLACE_PREFIX: &str = "plc_";

/// Longest `name`, in chars after trim.
pub const MAX_PLACE_NAME_LEN: usize = 128;

/// Longest `address_label`, in chars.
pub const MAX_ADDRESS_LABEL_LEN: usize = 256;

/// Largest `|lat_e7|`: 90 degrees × 10⁷.
pub const MAX_LAT_E7: i32 = 900_000_000;

/// Largest `|lon_e7|`: 180 degrees × 10⁷.
pub const MAX_LON_E7: i32 = 1_800_000_000;

/// Smallest `radius_m`: below this, OS region monitoring is not reliable.
pub const MIN_RADIUS_M: u32 = 50;

/// Largest `radius_m`: above this, the region no longer means "here".
pub const MAX_RADIUS_M: u32 = 5000;

/// The `radius_m` a new Place gets when the user picks none.
pub const DEFAULT_RADIUS_M: u32 = 150;

/// How many slots below the OS cap [`select_regions`] leaves free, for the
/// client's own re-selection trigger (ADR-0051 §5).
pub const REGION_TRIGGER_SLOTS: usize = 1;

/// A reference to a Place: `plc_` and a ULID.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlaceId(Ulid);

/// Why a string is not a [`PlaceId`].
#[derive(Debug, PartialEq, Eq, Error)]
pub enum PlaceIdError {
    /// The string does not start with `plc_`.
    #[error("not a place reference: expected the `plc_` prefix")]
    Prefix,
    /// The part after the prefix is not a ULID.
    #[error("not a place reference: {0}")]
    Ulid(#[from] UlidParseError),
}

impl PlaceId {
    /// The reference for `ulid`.
    #[must_use]
    pub const fn from_ulid(ulid: Ulid) -> Self {
        Self(ulid)
    }

    /// The ULID this reference names.
    #[must_use]
    pub const fn ulid(self) -> Ulid {
        self.0
    }

    /// Parse `plc_` followed by 26 Crockford base-32 characters.
    pub fn parse(s: &str) -> Result<Self, PlaceIdError> {
        let rest = s.strip_prefix(PLACE_PREFIX).ok_or(PlaceIdError::Prefix)?;
        Ok(Self(Ulid::parse(rest)?))
    }

    /// The `plc_…` string form.
    #[must_use]
    pub fn to_str(self) -> String {
        format!("{PLACE_PREFIX}{}", self.0.to_str())
    }
}

impl fmt::Display for PlaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_str())
    }
}

impl fmt::Debug for PlaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PlaceId({})", self.to_str())
    }
}

impl std::str::FromStr for PlaceId {
    type Err = PlaceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A named location the user entered (`docs/02-domain/places.md` §Fields).
///
/// Coordinates are fixed-point degrees × 10⁷, never floats: a float has
/// several CBOR encodings, and a signed op must encode one value one way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The Place's reference.
    pub id: PlaceId,
    /// Display name, 1..=128 chars after trim.
    pub name: String,
    /// Latitude × 10⁷, in `-MAX_LAT_E7..=MAX_LAT_E7`.
    pub lat_e7: i32,
    /// Longitude × 10⁷, in `-MAX_LON_E7..=MAX_LON_E7`.
    pub lon_e7: i32,
    /// Region radius in metres, in `MIN_RADIUS_M..=MAX_RADIUS_M`.
    pub radius_m: u32,
    /// What the user typed or picked. Display text only: Sunrise never
    /// geocodes a Place.
    pub address_label: Option<String>,
    /// Tombstoned. A requirement may still name it, and evaluates `Unknown`
    /// for it.
    pub deleted: bool,
}

impl Place {
    /// Check every rule of `docs/02-domain/places.md` §Validation.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(&self.name, "place.name", MAX_PLACE_NAME_LEN)?;
        validate_coordinates(self.lat_e7, self.lon_e7)?;
        validate_radius(self.radius_m)?;
        if let Some(label) = &self.address_label {
            validate_address_label(label)?;
        }
        Ok(())
    }
}

/// `lat_e7` and `lon_e7` are each in range.
pub fn validate_coordinates(lat_e7: i32, lon_e7: i32) -> Result<(), ValidationError> {
    if !(-MAX_LAT_E7..=MAX_LAT_E7).contains(&lat_e7) {
        return Err(ValidationError::Field {
            field: "place.lat_e7",
            constraint: "range",
        });
    }
    if !(-MAX_LON_E7..=MAX_LON_E7).contains(&lon_e7) {
        return Err(ValidationError::Field {
            field: "place.lon_e7",
            constraint: "range",
        });
    }
    Ok(())
}

/// `radius_m` is in `MIN_RADIUS_M..=MAX_RADIUS_M`.
pub fn validate_radius(radius_m: u32) -> Result<(), ValidationError> {
    if (MIN_RADIUS_M..=MAX_RADIUS_M).contains(&radius_m) {
        Ok(())
    } else {
        Err(ValidationError::Field {
            field: "place.radius_m",
            constraint: "range_50_5000",
        })
    }
}

/// `address_label` is at most [`MAX_ADDRESS_LABEL_LEN`] chars.
pub fn validate_address_label(label: &str) -> Result<(), ValidationError> {
    if label.chars().count() > MAX_ADDRESS_LABEL_LEN {
        return Err(ValidationError::Field {
            field: "place.address_label",
            constraint: "max_length",
        });
    }
    Ok(())
}

/// What the device knows about where the user is: for each monitored place,
/// whether the OS last said the user is inside its region.
///
/// Memory only (ADR-0051 §3). It has no `Serialize`, and its `Debug` prints
/// how many places it knows and never which or whether, so it cannot reach an
/// op, an envelope, a row or a log line without someone writing the code to
/// put it there. A place it has no entry for is unknown.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Presence(BTreeMap<PlaceId, bool>);

impl Presence {
    /// Nothing known.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what the OS said about `place`.
    pub fn set(&mut self, place: PlaceId, present: bool) {
        self.0.insert(place, present);
    }

    /// Stop knowing anything about `place`: its region is no longer
    /// monitored, or the Place was deleted.
    pub fn forget(&mut self, place: PlaceId) {
        self.0.remove(&place);
    }

    /// Stop knowing anything at all: location permission was denied or
    /// revoked.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// `Some(present)` for a place the OS has reported, `None` otherwise.
    #[must_use]
    pub fn get(&self, place: PlaceId) -> Option<bool> {
        self.0.get(&place).copied()
    }

    /// How many places have a known state.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no place has a known state.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Presence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Presence")
            .field("known", &self.0.len())
            .finish_non_exhaustive()
    }
}

/// How a place requirement evaluates now (`docs/02-domain/places.md`
/// §Presence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaceEval {
    /// The user is at one of the required places.
    Satisfied,
    /// Every required place is known, and the user is at none of them.
    Violated,
    /// Not enough is known to say. Never shown as a violation.
    Unknown,
}

/// Evaluate a requirement naming `required` (any one satisfies it) against
/// `presence`.
///
/// `Satisfied` when any required place is present, `Violated` when every one
/// is known absent, `Unknown` otherwise, including for an empty list. A place
/// that is deleted, not monitored, unreadable, or whose permission was denied
/// has no entry in `presence`, so it is unknown: the caller keeps deleted
/// places out of `presence` with [`Presence::forget`].
#[must_use]
pub fn evaluate_place(required: &[PlaceId], presence: &Presence) -> PlaceEval {
    if required.is_empty() {
        return PlaceEval::Unknown;
    }
    let mut all_absent = true;
    for &place in required {
        match presence.get(place) {
            Some(true) => return PlaceEval::Satisfied,
            Some(false) => {}
            None => all_absent = false,
        }
    }
    if all_absent {
        PlaceEval::Violated
    } else {
        PlaceEval::Unknown
    }
}

/// One open task's place requirement, as [`select_regions`] ranks by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPlaceReq {
    /// The places the requirement names; any one satisfies it.
    pub places: Vec<PlaceId>,
    /// Whether the requirement is `hard`.
    pub hard: bool,
}

/// Pick the places a client asks the OS to monitor, at most
/// `cap - REGION_TRIGGER_SLOTS` of them (ADR-0051 §5).
///
/// Only live places that an open task requires are candidates. They are
/// ranked by, in order:
///
/// 1. required by an open task's `hard` requirement first;
/// 2. the number of open tasks requiring the place, descending;
/// 3. distance from `approx_position` (`(lat_e7, lon_e7)`), ascending, when
///    one is given;
/// 4. the place id, ascending.
///
/// Pure: the same arguments give the same answer on every platform, because
/// the distance is computed with additions and multiplications only (see
/// [`distance_key`]). `approx_position` is used for this call and kept
/// nowhere.
#[must_use]
pub fn select_regions(
    places: &[Place],
    open_tasks: &[TaskPlaceReq],
    approx_position: Option<(i32, i32)>,
    cap: usize,
) -> Vec<PlaceId> {
    let room = cap.saturating_sub(REGION_TRIGGER_SLOTS);
    if room == 0 {
        return Vec::new();
    }
    // Per required place: whether any open task requires it hard, and how
    // many distinct open tasks require it.
    let mut demand: BTreeMap<PlaceId, (bool, usize)> = BTreeMap::new();
    for task in open_tasks {
        let distinct: BTreeSet<PlaceId> = task.places.iter().copied().collect();
        for place in distinct {
            let entry = demand.entry(place).or_insert((false, 0));
            entry.0 |= task.hard;
            entry.1 += 1;
        }
    }
    let mut seen = BTreeSet::new();
    let mut ranked: Vec<(bool, usize, Option<f64>, PlaceId)> = places
        .iter()
        .filter(|p| !p.deleted && seen.insert(p.id))
        .filter_map(|p| {
            let &(hard, count) = demand.get(&p.id)?;
            let distance = approx_position.map(|at| distance_key(at, (p.lat_e7, p.lon_e7)));
            Some((hard, count, distance, p.id))
        })
        .collect();
    ranked.sort_by(|a, b| {
        Reverse(a.0)
            .cmp(&Reverse(b.0))
            .then_with(|| Reverse(a.1).cmp(&Reverse(b.1)))
            .then_with(|| match (a.2, b.2) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => Ordering::Equal,
            })
            .then_with(|| a.3.cmp(&b.3))
    });
    ranked.into_iter().take(room).map(|r| r.3).collect()
}

/// A value that orders pairs of points by their distance apart: the squared
/// equirectangular distance, in (degrees × 10⁷)².
///
/// Longitude wraps at the antimeridian, and is scaled by the cosine of the
/// mean latitude. The cosine is a fixed polynomial ([`cos_lat`]) rather than
/// the platform's `cos`, whose last bit may differ between targets and could
/// swap two nearly equidistant places on one device and not another.
#[must_use]
pub fn distance_key(a: (i32, i32), b: (i32, i32)) -> f64 {
    const FULL_TURN_E7: i64 = 3_600_000_000;
    let dlat = f64::from(b.0) - f64::from(a.0);
    let mut dlon = i64::from(b.1) - i64::from(a.1);
    if dlon > i64::from(MAX_LON_E7) {
        dlon -= FULL_TURN_E7;
    } else if dlon < -i64::from(MAX_LON_E7) {
        dlon += FULL_TURN_E7;
    }
    let mean_lat_e7 = f64::midpoint(f64::from(a.0), f64::from(b.0));
    // `dlon` is at most 1.8e9 in magnitude, which an f64 holds exactly.
    #[allow(clippy::cast_precision_loss)]
    let dlon_scaled = dlon as f64 * cos_lat(mean_lat_e7);
    dlat * dlat + dlon_scaled * dlon_scaled
}

/// `cos` of a latitude in degrees × 10⁷, for `|lat| ≤ 90°`, by its Taylor
/// series to the x¹² term: within 3·10⁻⁷ of the true value, from additions
/// and multiplications alone.
fn cos_lat(lat_e7: f64) -> f64 {
    let x = lat_e7 * 1e-7 * std::f64::consts::PI / 180.0;
    let x2 = x * x;
    // Horner form of Σ (-1)^k x^(2k) / (2k)! for k = 0..=6.
    let mut sum = 1.0 / 479_001_600.0; // 1/12!
    for denominator in [
        -3_628_800.0, // 10!
        40_320.0,     // 8!
        -720.0,       // 6!
        24.0,         // 4!
        -2.0,         // 2!
        1.0,          // 0!
    ] {
        sum = sum * x2 + 1.0 / denominator;
    }
    sum
}

#[cfg(test)]
mod tests;
