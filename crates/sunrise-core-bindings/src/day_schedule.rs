//! The day schedule across the seam (`docs/02-domain/day-schedule.md`, #338):
//! the schedule and the reader's planner day as `CoreQuery::DaySchedule`
//! answers them, the keys a settings screen writes one entry at a time, the
//! Today section over planner days, and the wind-down plan.
//!
//! Additive only. An entry is written with the existing
//! `CoreCommand::SetPreference` / `ClearPreference`, its key from
//! [`day_schedule_key`] and its value a `PreferenceValue::DayWindow` (or
//! `PreferenceValue::NoSchedule` for a date with none). Nothing here changes
//! a record or an enum case Swift already reads.

use sunrise_client_core::reminders as plan;
use sunrise_domain::{
    DayOverride, DaySchedule, DayScheduleEntry, DayWindow, SunriseTime, TodaySection, Weekday,
};

use crate::dto::TimeValue;
use crate::reminders::PendingReminder;
use crate::{BindingError, SunriseCore};

/// See [`sunrise_domain::DayWindow`]: a wake and a sleep time; a sleep at or
/// before the wake is the next morning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct DayWindowItem {
    /// Civil wake time.
    pub wake: jiff::civil::Time,
    /// Civil sleep time.
    pub sleep: jiff::civil::Time,
}

impl From<DayWindow> for DayWindowItem {
    fn from(w: DayWindow) -> Self {
        Self {
            wake: w.wake,
            sleep: w.sleep,
        }
    }
}

/// One weekday default.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WeekdayWindow {
    /// The weekday.
    pub day: Weekday,
    /// Its window.
    pub window: DayWindowItem,
}

/// One day-of-month default.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MonthDayWindow {
    /// `1`..`31`, or `-1` for the last day of the month.
    pub day: i8,
    /// Its window.
    pub window: DayWindowItem,
}

/// One per-date override.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DateOverrideItem {
    /// The date.
    pub date: jiff::civil::Date,
    /// Its window; absent means "no schedule this date".
    pub window: Option<DayWindowItem>,
}

/// See [`sunrise_domain::DaySchedule`]: every entry, in order.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DayScheduleItem {
    /// Weekday defaults, Monday first; a weekday with none is absent.
    pub weekday: Vec<WeekdayWindow>,
    /// Day-of-month defaults, by day.
    pub month_day: Vec<MonthDayWindow>,
    /// Per-date overrides, by date.
    pub date: Vec<DateOverrideItem>,
}

impl From<&DaySchedule> for DayScheduleItem {
    fn from(s: &DaySchedule) -> Self {
        Self {
            weekday: sunrise_domain::day_schedule::WEEKDAYS
                .iter()
                .zip(s.weekday.iter())
                .filter_map(|(d, w)| {
                    w.map(|w| WeekdayWindow {
                        day: d.clone(),
                        window: w.into(),
                    })
                })
                .collect(),
            month_day: s
                .month_day
                .iter()
                .map(|(day, w)| MonthDayWindow {
                    day: *day,
                    window: (*w).into(),
                })
                .collect(),
            date: s
                .date
                .iter()
                .map(|(date, o)| DateOverrideItem {
                    date: *date,
                    window: match o {
                        DayOverride::Window(w) => Some((*w).into()),
                        DayOverride::None => None,
                    },
                })
                .collect(),
        }
    }
}

impl From<&DayScheduleItem> for DaySchedule {
    fn from(s: &DayScheduleItem) -> Self {
        let mut out = Self::default();
        let window = |w: &DayWindowItem| {
            Some(sunrise_domain::PrefValue::DayWindow {
                wake: w.wake,
                sleep: w.sleep,
            })
        };
        for w in &s.weekday {
            out.set(
                &DayScheduleEntry::Weekday(w.day.clone()),
                window(&w.window).as_ref(),
            );
        }
        for m in &s.month_day {
            if sunrise_domain::day_schedule::is_month_day(m.day) {
                out.set(
                    &DayScheduleEntry::MonthDay(m.day),
                    window(&m.window).as_ref(),
                );
            }
        }
        for d in &s.date {
            let value = d
                .window
                .as_ref()
                .map_or(Some(sunrise_domain::PrefValue::NoSchedule), window);
            out.set(&DayScheduleEntry::Date(d.date), value.as_ref());
        }
        out
    }
}

/// See [`sunrise_core::PlannerDay`]: one planner day for the reader.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlannerDayItem {
    /// The planner day's date.
    pub date: jiff::civil::Date,
    /// Where it starts.
    pub start: jiff::Timestamp,
    /// Where it ends: the next planner day's start.
    pub end: jiff::Timestamp,
    /// The window the date resolved to; absent with no schedule that date.
    pub window: Option<DayWindowItem>,
    /// Wake, resolved; absent with no schedule.
    pub plannable_start: Option<jiff::Timestamp>,
    /// Sleep, resolved; absent with no schedule.
    pub plannable_end: Option<jiff::Timestamp>,
    /// Every entry of the schedule.
    pub schedule: DayScheduleItem,
}

impl From<&sunrise_core::PlannerDay> for PlannerDayItem {
    fn from(d: &sunrise_core::PlannerDay) -> Self {
        Self {
            date: d.date,
            start: d.start,
            end: d.end,
            window: d.window.map(Into::into),
            plannable_start: d.plannable.map(|(a, _)| a),
            plannable_end: d.plannable.map(|(_, b)| b),
            schedule: (&d.schedule).into(),
        }
    }
}

/// Which entry of the schedule a key names.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum DayScheduleEntryItem {
    /// A weekday default.
    Weekday {
        /// The weekday.
        day: Weekday,
    },
    /// A day-of-month default: `1`..`31`, or `-1` for the last day.
    MonthDay {
        /// The day.
        day: i8,
    },
    /// A per-date override.
    Date {
        /// The date.
        date: jiff::civil::Date,
    },
}

/// The preference key of one schedule entry, e.g.
/// `day_schedule.date.2026-12-25`, for `CoreCommand::SetPreference` and
/// `ClearPreference` with the vault as target.
///
/// # Errors
/// A weekday this build does not know, or a day of month outside `-1, 1..31`.
#[uniffi::export]
pub fn day_schedule_key(entry: DayScheduleEntryItem) -> Result<String, BindingError> {
    let entry = match entry {
        DayScheduleEntryItem::Weekday { day } if !day.is_unknown() => {
            DayScheduleEntry::Weekday(day)
        }
        DayScheduleEntryItem::MonthDay { day }
            if sunrise_domain::day_schedule::is_month_day(day) =>
        {
            DayScheduleEntry::MonthDay(day)
        }
        DayScheduleEntryItem::Date { date } => DayScheduleEntry::Date(date),
        other => {
            return Err(BindingError::Core(format!(
                "{other:?} names no day-schedule entry"
            )))
        }
    };
    Ok(entry.key())
}

/// Which section of Today a row belongs in, with "today" and "overdue" read
/// against the planner days of `schedule`
/// ([`sunrise_client_core::vocab::today_section_in`]). With an empty schedule
/// it is `today_section`.
#[uniffi::export]
#[must_use]
pub fn today_section_in_schedule(
    scheduled_at: Option<TimeValue>,
    due_at: Option<TimeValue>,
    now_ms: u64,
    tz: String,
    schedule: DayScheduleItem,
) -> TodaySection {
    let scheduled = scheduled_at.map(SunriseTime::from);
    let due = due_at.map(SunriseTime::from);
    sunrise_client_core::vocab::today_section_in(
        scheduled.as_ref(),
        due.as_ref(),
        now_ms,
        &tz,
        &DaySchedule::from(&schedule),
    )
}

/// See [`sunrise_client_core::reminders::WindDownNotification`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WindDownItem {
    /// Always the same: a moved wind-down replaces the held one.
    pub identifier: String,
    /// Changes whenever the fire instant does.
    pub revision: String,
    /// The planner day whose sleep time it counts back from.
    pub date: jiff::civil::Date,
    /// That sleep time, epoch ms.
    pub sleep_at_ms: i64,
    /// When it fires, epoch ms.
    pub fire_at_ms: i64,
    /// The title, in English.
    pub title: String,
}

impl From<plan::WindDownNotification> for WindDownItem {
    fn from(n: plan::WindDownNotification) -> Self {
        Self {
            identifier: n.identifier,
            revision: n.revision,
            date: n.date,
            sleep_at_ms: n.sleep_at_ms,
            fire_at_ms: n.fire_at_ms,
            title: plan::WIND_DOWN_TITLE.to_owned(),
        }
    }
}

/// See [`sunrise_client_core::reminders::WindDownChange`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum WindDownChangeItem {
    /// Schedule it; none is held.
    Add {
        /// What to schedule.
        notification: WindDownItem,
    },
    /// Schedule it again under the same identifier; the held one moved.
    Replace {
        /// What to schedule.
        notification: WindDownItem,
    },
    /// Withdraw the held one.
    Cancel,
    /// Nothing to do.
    Keep,
}

impl From<plan::WindDownChange> for WindDownChangeItem {
    fn from(c: plan::WindDownChange) -> Self {
        match c {
            plan::WindDownChange::Add(n) => Self::Add {
                notification: n.into(),
            },
            plan::WindDownChange::Replace(n) => Self::Replace {
                notification: n.into(),
            },
            plan::WindDownChange::Cancel => Self::Cancel,
            plan::WindDownChange::Keep => Self::Keep,
        }
    }
}

/// The wind-down this device should hold, and what to do with the one it
/// holds.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WindDownPlan {
    /// The next wind-down; absent when this device fires none.
    pub desired: Option<WindDownItem>,
    /// What to change.
    pub change: WindDownChangeItem,
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Plan the wind-down notification (`docs/08-features/notifications.md`
    /// #8): `notifications.wind_down.lead_s` before the resolved sleep time,
    /// on the primary device, with wind-down on. Read in the reader's zone at
    /// the call, so call it again on every zone change and change batch, as
    /// with [`SunriseCore::schedule_reminders`]; `pending` is what the OS
    /// holds, the wind-down's identifier among it or not.
    pub async fn wind_down_plan(
        &self,
        now_ms: u64,
        authorized: bool,
        pending: Vec<PendingReminder>,
    ) -> Result<WindDownPlan, BindingError> {
        let pending: Vec<plan::PendingNotification> = pending.into_iter().map(Into::into).collect();
        let desired = if authorized {
            let policy = plan::read_policy(&self.inner).await?;
            let schedule = plan::read_day_schedule(&self.inner, now_ms).await?;
            plan::wind_down_notification(
                &policy,
                &schedule,
                sunrise_client_core::vocab::instant_at(now_ms),
                &self.inner.reader_zone(),
            )
        } else {
            None
        };
        Ok(WindDownPlan {
            change: plan::reconcile_wind_down(desired.as_ref(), &pending).into(),
            desired: desired.map(Into::into),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_schedule_round_trips_through_its_record() {
        let w = DayWindow::new(jiff::civil::time(7, 0, 0, 0), jiff::civil::time(1, 0, 0, 0));
        let mut s = DaySchedule {
            weekday: [w, None, None, None, None, None, w],
            ..DaySchedule::default()
        };
        s.month_day.insert(-1, w.unwrap());
        s.date
            .insert(jiff::civil::date(2026, 12, 25), DayOverride::None);
        s.date.insert(
            jiff::civil::date(2026, 12, 24),
            DayOverride::Window(w.unwrap()),
        );
        let item = DayScheduleItem::from(&s);
        assert_eq!(item.weekday.len(), 2);
        assert_eq!(DaySchedule::from(&item), s);
    }

    #[test]
    fn each_entry_names_its_key_and_a_bad_one_is_refused() {
        assert_eq!(
            day_schedule_key(DayScheduleEntryItem::Weekday { day: Weekday::Mo }).unwrap(),
            "day_schedule.weekday.MO"
        );
        assert_eq!(
            day_schedule_key(DayScheduleEntryItem::MonthDay { day: -1 }).unwrap(),
            "day_schedule.month_day.-1"
        );
        assert_eq!(
            day_schedule_key(DayScheduleEntryItem::Date {
                date: jiff::civil::date(2026, 12, 25)
            })
            .unwrap(),
            "day_schedule.date.2026-12-25"
        );
        assert!(day_schedule_key(DayScheduleEntryItem::MonthDay { day: 0 }).is_err());
    }
}
