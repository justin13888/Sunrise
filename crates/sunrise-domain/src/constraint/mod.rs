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

mod weekday_set;

pub use weekday_set::WeekdaySet;

use crate::rrule::Weekday;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::ValidationError;
use jiff::civil::{Date, Time};
use jiff::Zoned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of constraints carried on a single Task/Routine.
pub const MAX_CONSTRAINTS: usize = 16;

/// Severity of a scheduling constraint.
///
/// A `hard` violation blocks auto-scheduling and fails validation when the
/// user schedules against it; a `soft` violation never blocks and only
/// demotes ranking in planning views.
///
/// An unrecognised severity reads as [`ConstraintSeverity::Soft`] and is
/// written back verbatim. An unknown severity must not HARD-block scheduling:
/// a soft constraint influences ranking; a hard one rejects the user's command
/// outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintSeverity {
    /// Blocks scheduling and fails validation.
    Hard,
    /// Only demotes ranking; never blocks.
    Soft,
    /// A severity this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(ConstraintSeverity, fallback = Soft, {
    Hard => "hard",
    Soft => "soft",
});

/// Local wall-clock time-of-day window. Half-open `[start, end)`.
///
/// A window whose `end` is before its `start` **wraps midnight** and ends on
/// the following civil day (#333, `docs/10-cross-cutting/time.md` §5):
/// `22:00–02:00` holds 23:30 and 01:30. Such a window belongs to the day it
/// starts on, so the part after midnight is checked against the other
/// dimensions (weekday, date range) as the previous day. `start == end` is
/// refused: it names either nothing or everything.
///
/// Not `Copy`: it keeps the fields a newer build adds to it (ADR-0045 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeOfDayRange {
    /// Inclusive lower bound (local wall-clock).
    pub start: Time,
    /// Exclusive upper bound (local wall-clock).
    pub end: Time,
    /// Fields this build does not know, re-emitted verbatim. See
    /// [`crate::unknown`].
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl TimeOfDayRange {
    /// A window with no unknown fields.
    #[must_use]
    pub fn new(start: Time, end: Time) -> Self {
        Self {
            start,
            end,
            unknown: Unknowns::new(),
        }
    }

    /// Whether the window wraps past midnight (`end` before `start`).
    #[must_use]
    pub fn wraps(&self) -> bool {
        self.end < self.start
    }

    /// Where wall-clock `t` falls: `None` outside the window,
    /// `Some(false)` in it on the civil day it started, `Some(true)` in it
    /// after midnight on the day after (only for a window that wraps).
    #[must_use]
    pub fn place(&self, t: Time) -> Option<bool> {
        if self.wraps() {
            if t >= self.start {
                Some(false)
            } else if t < self.end {
                Some(true)
            } else {
                None
            }
        } else {
            (t >= self.start && t < self.end).then_some(false)
        }
    }
}

/// Inclusive civil-date range. Open-ended when `end` is absent.
///
/// Not `Copy`: it keeps the fields a newer build adds to it (ADR-0045 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRange {
    /// Inclusive lower bound.
    pub start: Date,
    /// Inclusive upper bound; open-ended if `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Date>,
    /// Fields this build does not know, re-emitted verbatim. See
    /// [`crate::unknown`].
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl DateRange {
    /// A range with no unknown fields.
    #[must_use]
    pub fn new(start: Date, end: Option<Date>) -> Self {
        Self {
            start,
            end,
            unknown: Unknowns::new(),
        }
    }
}

/// One scheduling constraint. At least one window dimension MUST be populated.
///
/// Not `Copy`: its severity can hold a value this build does not know, kept
/// verbatim (ADR-0045 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Fields this build does not know, re-emitted verbatim. See
    /// [`crate::unknown`]. A newer build's window dimension lands here: it
    /// makes the constraint non-empty, and it restricts nothing this build
    /// evaluates.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Errors from validating a scheduling constraint (or a list of them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ConstraintError {
    /// No window dimension populated (`time_of_day`, `days_of_week`, or
    /// `date_range`).
    #[error("scheduling constraint populates no window dimension")]
    Empty,
    /// `time_of_day.start` equalled `time_of_day.end`. A window whose end is
    /// before its start wraps midnight and is valid.
    #[error("time_of_day.start must differ from time_of_day.end")]
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
    /// OR/AND combination semantics. Zero only for a constraint whose populated
    /// fields are all unknown to this build, which validates and is satisfied
    /// at every moment, so its group of one never produces a violation.
    fn kind_key(&self) -> u8 {
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
    ///
    /// A constraint whose only populated fields are ones this build does not
    /// know is not empty: a newer build's window dimension is a real
    /// restriction there, and refusing it here would refuse the whole list.
    pub fn validate(&self) -> Result<(), ConstraintError> {
        if self.kind_key() == 0 && self.unknown.is_empty() {
            return Err(ConstraintError::Empty);
        }
        if let Some(t) = &self.time_of_day {
            if t.start == t.end {
                return Err(ConstraintError::TimeRange);
            }
        }
        if let Some(d) = &self.date_range {
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
    /// - `time_of_day`: local time within `[start, end)`, or for a window that
    ///   wraps midnight, at or after `start` or before `end`.
    /// - `days_of_week`: local weekday in the set (empty set == all days; a
    ///   set holding a day token this build does not know == all days too,
    ///   see [`WeekdaySet`]).
    /// - `date_range`: local date within the inclusive range.
    ///
    /// The window's day is the day it starts on: in the after-midnight part
    /// of a wrapping window, the weekday and the date are the previous day's.
    /// The moment is converted to civil time first, and no window bound is
    /// ever resolved (time.md §3), so a window a DST gap empties is simply
    /// empty that night.
    ///
    /// A field this build does not know is not evaluated: it cannot say what
    /// the field restricts, and an unknown value must never invent a block.
    #[must_use]
    pub fn is_satisfied_at(&self, zdt: &Zoned) -> bool {
        let mut day = zdt.date();
        if let Some(t) = &self.time_of_day {
            match t.place(zdt.time()) {
                None => return false,
                Some(true) => day = day.yesterday().unwrap_or(day),
                Some(false) => {}
            }
        }
        if !self.days_of_week.admits(&weekday_of_date(day)) {
            return false;
        }
        if let Some(d) = &self.date_range {
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

/// Map a civil date's weekday to the domain [`Weekday`].
fn weekday_of_date(day: Date) -> Weekday {
    // `to_monday_one_offset`: 1=Monday .. 7=Sunday.
    match day.weekday().to_monday_one_offset() {
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
    // kind_key is in 0..=7 for valid constraints (0 for one holding only
    // fields this build does not know), so a u8 mask over the eight possible
    // kinds is enough to record which kind-groups are satisfied.
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
        let c = list[i].clone();
        // An unknown severity reads as its fallback, `soft`: it must never
        // invent a hard block.
        match &c.severity {
            ConstraintSeverity::Hard => hard.push(c),
            ConstraintSeverity::Soft | ConstraintSeverity::Unknown(_) => soft.push(c),
        }
    }
    (hard, soft)
}

/// Pins the Rust wire shape to the CDDL in the spec, beside the types it pins.
#[test]
fn serde_json_shape_matches_cddl() {
    use jiff::civil::{date, time};

    let c = ScheduleConstraint {
        time_of_day: Some(TimeOfDayRange {
            start: time(9, 0, 0, 0),
            end: time(17, 30, 0, 0),
            unknown: Unknowns::new(),
        }),
        days_of_week: WeekdaySet::from_days([Weekday::Mo, Weekday::We, Weekday::Fr]),
        date_range: Some(DateRange {
            start: date(2026, 8, 3),
            end: Some(date(2026, 8, 17)),
            unknown: Unknowns::new(),
        }),
        severity: ConstraintSeverity::Hard,
        unknown: Unknowns::new(),
    };
    let j = serde_json::to_value(&c).unwrap();
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

#[cfg(test)]
mod tests;
