//! Scheduling constraints per `docs/02-domain/scheduling-constraints.md`.
//!
//! A [`ScheduleConstraint`] is a value type (no id) carried as a list on Task
//! and Routine. It restricts *when* a Task should be scheduled/executed along
//! up to three independent window dimensions: time of day, days of week, and
//! date range. The whole list is a single LWW register.
//!
//! Serde shape matches the CDDL in the spec exactly: `time_of_day` and
//! `date_range` are optional nested maps, `days_of_week` is a list of
//! `"MO".."SU"` tokens (skipped when empty, defaults to empty on decode), and
//! `severity` is a required `"hard"`/`"soft"` string. Civil types serialize
//! via jiff's defaults (`"HH:MM:SS"` / `"YYYY-MM-DD"`), verified to match the
//! spec's stated forms for whole-second times.

use crate::rrule::Weekday;
use crate::validation::ValidationError;
use jiff::civil::{Date, Time};
use jiff::Zoned;
use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Maximum number of constraints carried on a single Task/Routine.
pub const MAX_CONSTRAINTS: usize = 16;

/// Severity of a scheduling constraint.
///
/// A `hard` violation blocks auto-scheduling and fails validation when the
/// user schedules against it; a `soft` violation never blocks and only
/// demotes ranking in planning views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintSeverity {
    /// Blocks scheduling and fails validation.
    Hard,
    /// Only demotes ranking; never blocks.
    Soft,
}

impl ConstraintSeverity {
    /// The stable lowercase wire/storage string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }

    /// Parse from the wire/storage string. An unrecognised value degrades to
    /// [`ConstraintSeverity::Soft`] rather than failing.
    ///
    /// An unknown severity must not HARD-block scheduling. A soft constraint
    /// influences ranking; a hard one rejects the user's command outright.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "hard" => Self::Hard,
            // "soft" and anything this build has never heard of.
            _ => Self::Soft,
        }
    }
}

crate::unknown::lossy_enum!(ConstraintSeverity);

/// Local wall-clock time-of-day window. Half-open `[start, end)`; `start` MUST
/// be strictly `< end` (no midnight wrap in v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeOfDayRange {
    /// Inclusive lower bound (local wall-clock).
    pub start: Time,
    /// Exclusive upper bound (local wall-clock).
    pub end: Time,
}

/// Inclusive civil-date range. Open-ended when `end` is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRange {
    /// Inclusive lower bound.
    pub start: Date,
    /// Inclusive upper bound; open-ended if `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Date>,
}

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

/// Bit index of a weekday within [`WeekdaySet`] (`MO`=0 .. `SU`=6).
const fn weekday_bit(w: Weekday) -> u8 {
    match w {
        Weekday::Mo => 0,
        Weekday::Tu => 1,
        Weekday::We => 2,
        Weekday::Th => 3,
        Weekday::Fr => 4,
        Weekday::Sa => 5,
        Weekday::Su => 6,
    }
}

/// A compact set over the seven weekdays.
///
/// Backed by a 7-bit mask, so membership is O(1) and duplicate tokens are
/// collapsed automatically (the spec's "entries MUST be distinct" rule is
/// free). Serializes as a list of `"MO".."SU"` strings in canonical `MO..SU`
/// order; an **empty** set means "all days" and is skipped on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WeekdaySet(u8);

impl WeekdaySet {
    /// The empty set (== "all days").
    #[must_use]
    pub const fn new() -> Self {
        Self(0)
    }

    /// Build from an iterator of weekdays.
    #[must_use]
    pub fn from_days<I: IntoIterator<Item = Weekday>>(days: I) -> Self {
        let mut s = Self(0);
        for d in days {
            s.insert(d);
        }
        s
    }

    /// Insert a weekday.
    pub fn insert(&mut self, w: Weekday) {
        self.0 |= 1 << weekday_bit(w);
    }

    /// True if `w` is present.
    #[must_use]
    pub const fn contains(self, w: Weekday) -> bool {
        self.0 & (1 << weekday_bit(w)) != 0
    }

    /// True if the set is empty ("all days").
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Number of weekdays in the set.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.0.count_ones()
    }

    /// Iterate the members in canonical `MO..SU` order.
    fn iter(self) -> impl Iterator<Item = Weekday> {
        WEEKDAY_ORDER.into_iter().filter(move |&w| self.contains(w))
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

/// One scheduling constraint. At least one window dimension MUST be populated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConstraint {
    /// Time-of-day window (local wall-clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_of_day: Option<TimeOfDayRange>,
    /// Allowed weekdays; empty == all days.
    #[serde(default, skip_serializing_if = "WeekdaySet::is_empty")]
    pub days_of_week: WeekdaySet,
    /// Allowed date range (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_range: Option<DateRange>,
    /// Severity (required).
    pub severity: ConstraintSeverity,
}

/// Errors from validating a scheduling constraint (or a list of them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConstraintError {
    /// No window dimension populated (`time_of_day`, `days_of_week`, or
    /// `date_range`).
    #[error("scheduling constraint populates no window dimension")]
    Empty,
    /// `time_of_day.start` was not strictly `< time_of_day.end`.
    #[error("time_of_day.start must be < time_of_day.end")]
    TimeRange,
    /// `date_range.start` was `> date_range.end`.
    #[error("date_range.start must be ≤ date_range.end")]
    DateRange,
    /// The list exceeded [`MAX_CONSTRAINTS`] entries.
    #[error("too many scheduling constraints (max {MAX_CONSTRAINTS})")]
    TooMany,
}

impl ConstraintError {
    /// Field-error constraint name for the wire (maps to `ValidationField`).
    const fn constraint_name(self) -> &'static str {
        match self {
            Self::Empty => "no_dimension",
            Self::TimeRange => "time_range",
            Self::DateRange => "date_range",
            Self::TooMany => "max_16",
        }
    }
}

impl From<ConstraintError> for ValidationError {
    fn from(e: ConstraintError) -> Self {
        ValidationError::Field {
            field: "task.scheduling_constraints",
            constraint: e.constraint_name(),
        }
    }
}

impl ScheduleConstraint {
    /// Bitmask of populated dimensions: bit0=time_of_day, bit1=days_of_week
    /// (non-empty), bit2=date_range. This is the constraint's "kind" for the
    /// OR/AND combination semantics. Always non-zero for a valid constraint.
    const fn kind_key(&self) -> u8 {
        let mut k = 0u8;
        if self.time_of_day.is_some() {
            k |= 1;
        }
        if !self.days_of_week.is_empty() {
            k |= 2;
        }
        if self.date_range.is_some() {
            k |= 4;
        }
        k
    }

    /// Validate a single constraint against the domain rules.
    pub fn validate(&self) -> Result<(), ConstraintError> {
        if self.kind_key() == 0 {
            return Err(ConstraintError::Empty);
        }
        if let Some(t) = self.time_of_day {
            if t.start >= t.end {
                return Err(ConstraintError::TimeRange);
            }
        }
        if let Some(d) = self.date_range {
            if let Some(end) = d.end {
                if d.start > end {
                    return Err(ConstraintError::DateRange);
                }
            }
        }
        Ok(())
    }

    /// True if every populated dimension holds at `zdt` (evaluated in that
    /// zoned datetime's local wall-clock terms).
    ///
    /// - `time_of_day`: local time within `[start, end)`.
    /// - `days_of_week`: local weekday in the set (empty set == all days).
    /// - `date_range`: local date within the inclusive range.
    #[must_use]
    pub fn is_satisfied_at(&self, zdt: &Zoned) -> bool {
        if let Some(t) = self.time_of_day {
            let now = zdt.time();
            if now < t.start || now >= t.end {
                return false;
            }
        }
        if !self.days_of_week.is_empty() && !self.days_of_week.contains(weekday_of(zdt)) {
            return false;
        }
        if let Some(d) = self.date_range {
            let day = zdt.date();
            if day < d.start {
                return false;
            }
            if let Some(end) = d.end {
                if day > end {
                    return false;
                }
            }
        }
        true
    }
}

/// Map a zoned datetime's weekday to the domain [`Weekday`].
fn weekday_of(zdt: &Zoned) -> Weekday {
    // `to_monday_one_offset`: 1=Monday .. 7=Sunday.
    match zdt.weekday().to_monday_one_offset() {
        1 => Weekday::Mo,
        2 => Weekday::Tu,
        3 => Weekday::We,
        4 => Weekday::Th,
        5 => Weekday::Fr,
        6 => Weekday::Sa,
        _ => Weekday::Su,
    }
}

/// Validate a whole list: at most [`MAX_CONSTRAINTS`] entries, each valid.
pub fn validate_list(list: &[ScheduleConstraint]) -> Result<(), ConstraintError> {
    if list.len() > MAX_CONSTRAINTS {
        return Err(ConstraintError::TooMany);
    }
    for c in list {
        c.validate()?;
    }
    Ok(())
}

/// Indices of constraints violated at `zdt` under the spec's combination
/// semantics:
///
/// - Constraints of the **same kind** (same set of populated dimensions) **OR**
///   together — the group is satisfied if any member is satisfied.
/// - Constraints of **different kinds** **AND** together — every kind-group
///   must be satisfied.
///
/// A constraint is a violation iff its kind-group has no satisfied member (in
/// which case every member of that group is individually unsatisfied). The
/// returned indices are in ascending order. Severity is not considered here;
/// the caller filters by [`ConstraintSeverity`].
#[must_use]
pub fn list_violations(list: &[ScheduleConstraint], zdt: &Zoned) -> Vec<usize> {
    // kind_key is in 1..=7 for valid constraints, so a u8 mask over the eight
    // possible kinds is enough to record which kind-groups are satisfied.
    let mut satisfied_kinds: u8 = 0;
    for c in list {
        if c.is_satisfied_at(zdt) {
            satisfied_kinds |= 1 << c.kind_key();
        }
    }
    list.iter()
        .enumerate()
        .filter(|(_, c)| satisfied_kinds & (1 << c.kind_key()) == 0)
        .map(|(i, _)| i)
        .collect()
}

/// Violated constraints at `zdt`, split by severity: `(hard, soft)`.
///
/// This is the shape callers actually need at a scheduling decision: a
/// non-empty `hard` list **blocks** the schedule and fails validation, while
/// `soft` entries never block and only demote ranking in planning views
/// (`docs/02-domain/scheduling-constraints.md` §Hard vs. soft). Combination
/// semantics are [`list_violations`]'s.
#[must_use]
pub fn violations_by_severity(
    list: &[ScheduleConstraint],
    zdt: &Zoned,
) -> (Vec<ScheduleConstraint>, Vec<ScheduleConstraint>) {
    let mut hard = Vec::new();
    let mut soft = Vec::new();
    for i in list_violations(list, zdt) {
        let c = list[i];
        match c.severity {
            ConstraintSeverity::Hard => hard.push(c),
            ConstraintSeverity::Soft => soft.push(c),
        }
    }
    (hard, soft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::{date, time};
    use jiff::tz::{offset, TimeZone};

    fn zoned(y: i16, m: i8, d: i8, hh: i8, mm: i8) -> Zoned {
        date(y, m, d)
            .at(hh, mm, 0, 0)
            .to_zoned(TimeZone::fixed(offset(0)))
            .unwrap()
    }

    fn tod(constraint: TimeOfDayRange, sev: ConstraintSeverity) -> ScheduleConstraint {
        ScheduleConstraint {
            time_of_day: Some(constraint),
            days_of_week: WeekdaySet::new(),
            date_range: None,
            severity: sev,
        }
    }

    #[test]
    fn empty_constraint_rejected() {
        let c = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::new(),
            date_range: None,
            severity: ConstraintSeverity::Hard,
        };
        assert_eq!(c.validate(), Err(ConstraintError::Empty));
    }

    #[test]
    fn time_range_must_be_ordered() {
        let c = tod(
            TimeOfDayRange {
                start: time(18, 0, 0, 0),
                end: time(9, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        assert_eq!(c.validate(), Err(ConstraintError::TimeRange));

        // Equal start/end is also rejected (strictly <).
        let c = tod(
            TimeOfDayRange {
                start: time(9, 0, 0, 0),
                end: time(9, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        assert_eq!(c.validate(), Err(ConstraintError::TimeRange));
    }

    #[test]
    fn date_range_must_be_ordered() {
        let c = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::new(),
            date_range: Some(DateRange {
                start: date(2026, 8, 17),
                end: Some(date(2026, 8, 3)),
            }),
            severity: ConstraintSeverity::Hard,
        };
        assert_eq!(c.validate(), Err(ConstraintError::DateRange));
    }

    #[test]
    fn open_ended_date_range_valid() {
        let c = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::new(),
            date_range: Some(DateRange {
                start: date(2026, 8, 3),
                end: None,
            }),
            severity: ConstraintSeverity::Soft,
        };
        c.validate().unwrap();
    }

    #[test]
    fn days_only_constraint_valid() {
        let c = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::from_days([Weekday::Mo, Weekday::We]),
            date_range: None,
            severity: ConstraintSeverity::Hard,
        };
        c.validate().unwrap();
    }

    #[test]
    fn list_max_16_enforced() {
        let c = tod(
            TimeOfDayRange {
                start: time(9, 0, 0, 0),
                end: time(17, 0, 0, 0),
            },
            ConstraintSeverity::Soft,
        );
        let ok = vec![c; 16];
        assert!(validate_list(&ok).is_ok());
        let too_many = vec![c; 17];
        assert_eq!(validate_list(&too_many), Err(ConstraintError::TooMany));
    }

    #[test]
    fn weekday_set_collapses_duplicates() {
        let s = WeekdaySet::from_days([Weekday::Mo, Weekday::Mo, Weekday::Tu]);
        assert_eq!(s.len(), 2);
        assert!(s.contains(Weekday::Mo));
        assert!(s.contains(Weekday::Tu));
        assert!(!s.contains(Weekday::We));
    }

    #[test]
    fn is_satisfied_time_of_day_half_open() {
        let c = tod(
            TimeOfDayRange {
                start: time(9, 0, 0, 0),
                end: time(17, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        // Aug 3 2026 is a Monday.
        assert!(c.is_satisfied_at(&zoned(2026, 8, 3, 9, 0))); // inclusive start
        assert!(c.is_satisfied_at(&zoned(2026, 8, 3, 16, 59)));
        assert!(!c.is_satisfied_at(&zoned(2026, 8, 3, 17, 0))); // exclusive end
        assert!(!c.is_satisfied_at(&zoned(2026, 8, 3, 8, 59)));
    }

    #[test]
    fn is_satisfied_weekday() {
        let c = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::from_days([Weekday::Mo]),
            date_range: None,
            severity: ConstraintSeverity::Hard,
        };
        assert!(c.is_satisfied_at(&zoned(2026, 8, 3, 12, 0))); // Monday
        assert!(!c.is_satisfied_at(&zoned(2026, 8, 4, 12, 0))); // Tuesday
    }

    #[test]
    fn is_satisfied_date_range_inclusive() {
        let c = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::new(),
            date_range: Some(DateRange {
                start: date(2026, 8, 3),
                end: Some(date(2026, 8, 17)),
            }),
            severity: ConstraintSeverity::Hard,
        };
        assert!(c.is_satisfied_at(&zoned(2026, 8, 3, 0, 0))); // inclusive start
        assert!(c.is_satisfied_at(&zoned(2026, 8, 17, 23, 0))); // inclusive end
        assert!(!c.is_satisfied_at(&zoned(2026, 8, 2, 12, 0)));
        assert!(!c.is_satisfied_at(&zoned(2026, 8, 18, 12, 0)));
    }

    #[test]
    fn is_satisfied_all_dimensions_and() {
        // Weekday mornings: Mon-Fri 09:00-12:00 in early August.
        let c = ScheduleConstraint {
            time_of_day: Some(TimeOfDayRange {
                start: time(9, 0, 0, 0),
                end: time(12, 0, 0, 0),
            }),
            days_of_week: WeekdaySet::from_days([
                Weekday::Mo,
                Weekday::Tu,
                Weekday::We,
                Weekday::Th,
                Weekday::Fr,
            ]),
            date_range: Some(DateRange {
                start: date(2026, 8, 1),
                end: Some(date(2026, 8, 31)),
            }),
            severity: ConstraintSeverity::Hard,
        };
        assert!(c.is_satisfied_at(&zoned(2026, 8, 3, 10, 0))); // Mon 10:00 in range
        assert!(!c.is_satisfied_at(&zoned(2026, 8, 3, 13, 0))); // right day, wrong time
        assert!(!c.is_satisfied_at(&zoned(2026, 8, 8, 10, 0))); // Saturday
    }

    #[test]
    fn list_violations_same_kind_or() {
        // Two time-of-day constraints (same kind) OR together: morning OR
        // evening. At 10:00 the morning one is satisfied, so neither is a
        // violation.
        let morning = tod(
            TimeOfDayRange {
                start: time(6, 0, 0, 0),
                end: time(12, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        let evening = tod(
            TimeOfDayRange {
                start: time(18, 0, 0, 0),
                end: time(22, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        let list = [morning, evening];
        assert!(list_violations(&list, &zoned(2026, 8, 3, 10, 0)).is_empty());
        // At 14:00 neither is satisfied → both violated.
        assert_eq!(
            list_violations(&list, &zoned(2026, 8, 3, 14, 0)),
            vec![0, 1]
        );
    }

    #[test]
    fn list_violations_different_kind_and() {
        // Different kinds AND together: a time-of-day window AND a weekday set.
        let after_six = tod(
            TimeOfDayRange {
                start: time(18, 0, 0, 0),
                end: time(23, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        let weekdays = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::from_days([
                Weekday::Mo,
                Weekday::Tu,
                Weekday::We,
                Weekday::Th,
                Weekday::Fr,
            ]),
            date_range: None,
            severity: ConstraintSeverity::Hard,
        };
        let list = [after_six, weekdays];
        // Mon 19:00: both satisfied → no violations.
        assert!(list_violations(&list, &zoned(2026, 8, 3, 19, 0)).is_empty());
        // Mon 12:00: weekday ok but time fails → only index 0 violated.
        assert_eq!(list_violations(&list, &zoned(2026, 8, 3, 12, 0)), vec![0]);
        // Sat 19:00: time ok but weekday fails → only index 1 violated.
        assert_eq!(list_violations(&list, &zoned(2026, 8, 8, 19, 0)), vec![1]);
    }

    #[test]
    fn list_violations_same_kind_or_within_failing_group() {
        // Same-kind group with a sibling that fails does not "save" the other
        // if the whole group fails: both are reported.
        let a = tod(
            TimeOfDayRange {
                start: time(6, 0, 0, 0),
                end: time(8, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        let b = tod(
            TimeOfDayRange {
                start: time(20, 0, 0, 0),
                end: time(22, 0, 0, 0),
            },
            ConstraintSeverity::Hard,
        );
        assert_eq!(
            list_violations(&[a, b], &zoned(2026, 8, 3, 12, 0)),
            vec![0, 1]
        );
    }

    #[test]
    fn violations_split_by_severity() {
        // A hard weekday rule and a soft evening rule; Saturday 12:00 violates
        // both, and the caller must be able to tell them apart.
        let weekdays = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::from_days([
                Weekday::Mo,
                Weekday::Tu,
                Weekday::We,
                Weekday::Th,
                Weekday::Fr,
            ]),
            date_range: None,
            severity: ConstraintSeverity::Hard,
        };
        let evening = tod(
            TimeOfDayRange {
                start: time(18, 0, 0, 0),
                end: time(22, 0, 0, 0),
            },
            ConstraintSeverity::Soft,
        );
        let list = [weekdays, evening];
        let (hard, soft) = violations_by_severity(&list, &zoned(2026, 8, 8, 12, 0));
        assert_eq!(hard, vec![weekdays]);
        assert_eq!(soft, vec![evening]);

        // Monday 19:00 satisfies both.
        let (hard, soft) = violations_by_severity(&list, &zoned(2026, 8, 3, 19, 0));
        assert!(hard.is_empty() && soft.is_empty());

        // Monday 12:00 breaks only the soft one.
        let (hard, soft) = violations_by_severity(&list, &zoned(2026, 8, 3, 12, 0));
        assert!(hard.is_empty());
        assert_eq!(soft, vec![evening]);
    }

    #[test]
    fn serde_json_shape_matches_cddl() {
        let c = ScheduleConstraint {
            time_of_day: Some(TimeOfDayRange {
                start: time(9, 0, 0, 0),
                end: time(17, 30, 0, 0),
            }),
            days_of_week: WeekdaySet::from_days([Weekday::Mo, Weekday::We, Weekday::Fr]),
            date_range: Some(DateRange {
                start: date(2026, 8, 3),
                end: Some(date(2026, 8, 17)),
            }),
            severity: ConstraintSeverity::Hard,
        };
        let j = serde_json::to_value(c).unwrap();
        assert_eq!(j["time_of_day"]["start"], "09:00:00");
        assert_eq!(j["time_of_day"]["end"], "17:30:00");
        assert_eq!(
            j["days_of_week"],
            serde_json::json!(["MO", "WE", "FR"]),
            "weekday tokens in MO..SU order"
        );
        assert_eq!(j["date_range"]["start"], "2026-08-03");
        assert_eq!(j["date_range"]["end"], "2026-08-17");
        assert_eq!(j["severity"], "hard");

        // Round-trip.
        let back: ScheduleConstraint = serde_json::from_value(j).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn serde_skips_empty_days_and_absent_ranges() {
        let c = ScheduleConstraint {
            time_of_day: None,
            days_of_week: WeekdaySet::from_days([Weekday::Sa, Weekday::Su]),
            date_range: None,
            severity: ConstraintSeverity::Soft,
        };
        let j = serde_json::to_value(c).unwrap();
        assert!(j.get("time_of_day").is_none());
        assert!(j.get("date_range").is_none());
        assert_eq!(j["days_of_week"], serde_json::json!(["SA", "SU"]));
        assert_eq!(j["severity"], "soft");
    }

    #[test]
    fn days_of_week_defaults_empty_on_decode() {
        // Absent days_of_week decodes to the empty set.
        let j = serde_json::json!({
            "time_of_day": { "start": "09:00:00", "end": "17:00:00" },
            "severity": "hard"
        });
        let c: ScheduleConstraint = serde_json::from_value(j).unwrap();
        assert!(c.days_of_week.is_empty());
    }
}
