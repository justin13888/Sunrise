//! Scheduling constraints: when a task may and must happen.

use sunrise_domain::{ConstraintSeverity, DateRange, ScheduleConstraint, TimeOfDayRange, Weekday};

use super::{extra_in, extra_out};

/// See [`sunrise_domain::TimeOfDayRange`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct TimeWindow {
    /// Inclusive start (local wall clock).
    pub start: jiff::civil::Time,
    /// Exclusive end (local wall clock).
    pub end: jiff::civil::Time,
    /// Fields this build does not know, opaque; hand it back unchanged. See
    /// the module docs.
    #[uniffi(default = None)]
    pub extra: Option<Vec<u8>>,
}

/// See [`sunrise_domain::DateRange`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct DateWindow {
    /// Inclusive start.
    pub start: jiff::civil::Date,
    /// Inclusive end; open-ended when absent.
    pub end: Option<jiff::civil::Date>,
    /// Fields this build does not know, opaque; hand it back unchanged. See
    /// the module docs.
    #[uniffi(default = None)]
    pub extra: Option<Vec<u8>>,
}

/// See [`sunrise_domain::ScheduleConstraint`]. `days_of_week` is a list rather
/// than the domain's bit set: UniFFI has sequences, not sets, and an empty list
/// means "every day" exactly as the empty set does.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Constraint {
    /// Time-of-day window.
    pub time_of_day: Option<TimeWindow>,
    /// Allowed weekdays; empty means all of them.
    pub days_of_week: Vec<Weekday>,
    /// Allowed date range.
    pub date_range: Option<DateWindow>,
    /// Whether violating this rejects the write or only demotes it.
    pub severity: ConstraintSeverity,
    /// Fields this build does not know, opaque; hand it back unchanged. See
    /// the module docs.
    #[uniffi(default = None)]
    pub extra: Option<Vec<u8>>,
}

impl From<&ScheduleConstraint> for Constraint {
    fn from(c: &ScheduleConstraint) -> Self {
        let ScheduleConstraint {
            time_of_day,
            days_of_week,
            date_range,
            severity,
            unknown,
        } = c;
        Self {
            time_of_day: time_of_day.as_ref().map(|w| {
                let TimeOfDayRange {
                    start,
                    end,
                    unknown,
                } = w;
                TimeWindow {
                    start: *start,
                    end: *end,
                    extra: extra_out(unknown),
                }
            }),
            // Monday-first, then any day token this build does not know, so a
            // client that hands the list back carries it through.
            days_of_week: days_of_week.iter().collect(),
            date_range: date_range.as_ref().map(|r| {
                let DateRange {
                    start,
                    end,
                    unknown,
                } = r;
                DateWindow {
                    start: *start,
                    end: *end,
                    extra: extra_out(unknown),
                }
            }),
            severity: severity.clone(),
            extra: extra_out(unknown),
        }
    }
}

impl From<Constraint> for ScheduleConstraint {
    fn from(c: Constraint) -> Self {
        Self {
            time_of_day: c.time_of_day.map(|w| TimeOfDayRange {
                start: w.start,
                end: w.end,
                unknown: extra_in(w.extra.as_deref()),
            }),
            days_of_week: sunrise_domain::WeekdaySet::from_days(c.days_of_week),
            date_range: c.date_range.map(|r| DateRange {
                start: r.start,
                end: r.end,
                unknown: extra_in(r.extra.as_deref()),
            }),
            severity: c.severity,
            unknown: extra_in(c.extra.as_deref()),
        }
    }
}
