//! [`WeekdaySet`], the value type behind a constraint's `days_of_week`: a set
//! over the seven weekdays and the codec that keeps the day tokens this build
//! does not know.

use crate::rrule::Weekday;
use crate::unknown::UnknownVariant;
use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;

/// Canonical weekday order (`MO`..`SU`) used for the compact set.
const WEEKDAY_ORDER: [Weekday; 7] = [
    Weekday::Mo,
    Weekday::Tu,
    Weekday::We,
    Weekday::Th,
    Weekday::Fr,
    Weekday::Sa,
    Weekday::Su,
];

/// Bit index of a weekday within [`WeekdaySet`] (`MO`=0 .. `SU`=6); `None`
/// for a token this build does not know.
const fn weekday_bit(w: &Weekday) -> Option<u8> {
    Some(match w {
        Weekday::Mo => 0,
        Weekday::Tu => 1,
        Weekday::We => 2,
        Weekday::Th => 3,
        Weekday::Fr => 4,
        Weekday::Sa => 5,
        Weekday::Su => 6,
        Weekday::Unknown(_) => return None,
    })
}

/// A set over the seven weekdays, plus any day tokens this build does not
/// know.
///
/// The known days are a 7-bit mask, so membership is O(1) and duplicate
/// tokens are collapsed automatically (the spec's "entries MUST be distinct"
/// rule is free). Serializes as a list of `"MO".."SU"` strings in canonical
/// `MO..SU` order; an **empty** set means "all days" and is skipped on the
/// wire.
///
/// An unknown token is kept, in arrival order after the known days, and
/// written back verbatim (ADR-0045 §6). It names no day this build can match,
/// and a set that holds one places **no** restriction on the day: reading the
/// set as only its known days could turn a newer peer's "Mondays or a day
/// this build has no name for" into a hard block on every day but Monday, and
/// inventing a block
/// is exactly what an unknown value must never do.
///
/// The unknown tokens come off the wire from a peer, so their count is
/// unbounded. They are indexed by a hash set beside the arrival-order list,
/// which keeps decoding `n` of them linear rather than quadratic.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WeekdaySet {
    mask: u8,
    /// Unknown tokens in arrival order, each once.
    unknown: Vec<UnknownVariant>,
    /// The same tokens as `unknown`, for O(1) membership.
    unknown_index: HashSet<UnknownVariant>,
}

impl WeekdaySet {
    /// The empty set (== "all days").
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from an iterator of weekdays.
    #[must_use]
    pub fn from_days<I: IntoIterator<Item = Weekday>>(days: I) -> Self {
        let mut s = Self::new();
        for d in days {
            s.insert(d);
        }
        s
    }

    /// Insert a weekday. An unknown token is kept once, in arrival order.
    pub fn insert(&mut self, w: Weekday) {
        match (weekday_bit(&w), w) {
            (Some(bit), _) => self.mask |= 1 << bit,
            (None, Weekday::Unknown(raw)) => {
                if self.unknown_index.insert(raw.clone()) {
                    self.unknown.push(raw);
                }
            }
            // `weekday_bit` is `None` only for `Unknown`.
            (None, _) => {}
        }
    }

    /// True if `w` is present. An unknown token is compared by its spelling.
    #[must_use]
    pub fn contains(&self, w: &Weekday) -> bool {
        match (weekday_bit(w), w) {
            (Some(bit), _) => self.mask & (1 << bit) != 0,
            (None, Weekday::Unknown(raw)) => self.unknown_index.contains(raw),
            (None, _) => false,
        }
    }

    /// True if the set is empty ("all days").
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mask == 0 && self.unknown.is_empty()
    }

    /// Number of entries in the set, unknown tokens included.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.mask.count_ones() + u32::try_from(self.unknown.len()).unwrap_or(u32::MAX)
    }

    /// Whether the set restricts which day it is: non-empty, and every token
    /// in it one this build knows. See the type's docs for why an unknown
    /// token lifts the restriction.
    #[must_use]
    pub fn restricts(&self) -> bool {
        self.mask != 0 && self.unknown.is_empty()
    }

    /// Whether the set admits `w`, the weekday of a moment being checked:
    /// true when it places no restriction, or when `w` is in it.
    #[must_use]
    pub fn admits(&self, w: &Weekday) -> bool {
        !self.restricts() || self.contains(w)
    }

    /// Iterate the members: known days in canonical `MO..SU` order, then any
    /// unknown tokens in arrival order.
    pub fn iter(&self) -> impl Iterator<Item = Weekday> + '_ {
        WEEKDAY_ORDER
            .into_iter()
            .filter(move |w| self.contains(w))
            .chain(self.unknown.iter().cloned().map(Weekday::Unknown))
    }
}

impl Serialize for WeekdaySet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.len() as usize))?;
        for w in self.iter() {
            seq.serialize_element(&w)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for WeekdaySet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SetVisitor;
        impl<'de> Visitor<'de> for SetVisitor {
            type Value = WeekdaySet;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a list of weekday tokens")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<WeekdaySet, A::Error> {
                let mut set = WeekdaySet::new();
                while let Some(w) = seq.next_element::<Weekday>()? {
                    set.insert(w);
                }
                Ok(set)
            }
        }
        deserializer.deserialize_seq(SetVisitor)
    }
}
