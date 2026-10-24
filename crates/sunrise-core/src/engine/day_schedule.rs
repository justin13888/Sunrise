//! The planner day (`docs/02-domain/day-schedule.md`, ADR-0050 §3, #338):
//! the day schedule read out of the synced `Preferences` entity, and the
//! bounds of the reader's day every planner-day view reads.
//!
//! The schedule is stored one register per entry in the entity's `values`
//! map (`day_schedule.date.2026-12-25`), written by `Command::SetPreference`
//! and `Command::ClearPreference` like every other vault key, so there is no
//! command, table or merge rule of its own. The rules are the pure
//! `sunrise_domain::day_schedule` functions; this module reads the row and
//! the reader's zone.

use super::ids::ms_to_ts;
use super::preferences::read_preferences;
use super::{Engine, EngineError};
use crate::queries::{PlannerDay, QueryResult};
use jiff::civil::Date;
use sunrise_domain::{day_bounds, plannable, planner_date, preferences_ref, DaySchedule};
use sunrise_storage::Db;

impl Engine {
    /// The vault's day schedule. Empty when none is set, which makes every
    /// planner day the civil day.
    pub(crate) fn day_schedule(&self, db: &Db) -> Result<DaySchedule, EngineError> {
        Ok(read_preferences(db.conn(), preferences_ref().bytes())?
            .map(|p| DaySchedule::from_values(&p.values))
            .unwrap_or_default())
    }

    /// The planner day holding `at_ms` for the reader, and its bounds as
    /// epoch milliseconds.
    pub(super) fn planner_day_at(
        &self,
        schedule: &DaySchedule,
        at_ms: u64,
    ) -> Result<(Date, i64, i64), EngineError> {
        let tz = self.device_zone();
        let at = ms_to_ts(i64::try_from(at_ms).unwrap_or(i64::MAX));
        let date = planner_date(schedule, at, &tz).map_err(calendar)?;
        let (start, end) = self.planner_bounds(schedule, date)?;
        Ok((date, start, end))
    }

    /// `day_bounds(date)` in the reader's zone, as epoch milliseconds.
    pub(super) fn planner_bounds(
        &self,
        schedule: &DaySchedule,
        date: Date,
    ) -> Result<(i64, i64), EngineError> {
        let (start, end) = day_bounds(schedule, date, &self.device_zone()).map_err(calendar)?;
        Ok((start.as_millisecond(), end.as_millisecond()))
    }

    /// `Query::DaySchedule`: the schedule, and the planner day holding
    /// `at_ms` in the reader's zone.
    pub(super) fn query_day_schedule(
        &self,
        db: &Db,
        at_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let schedule = self.day_schedule(db)?;
        let (date, start, end) = self.planner_day_at(&schedule, at_ms)?;
        let window = schedule.window_for(date);
        let plannable = plannable(&schedule, date, &self.device_zone()).map_err(calendar)?;
        Ok(QueryResult::DaySchedule(Box::new(PlannerDay {
            date,
            start: ms_to_ts(start),
            end: ms_to_ts(end),
            window,
            plannable,
            schedule,
        })))
    }
}

fn calendar(e: sunrise_domain::OutOfCalendar) -> EngineError {
    EngineError::Invalid(format!("planner day: {e}"))
}
