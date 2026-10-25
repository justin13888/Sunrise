//! Lateness for the reader (ADR-0047 §2, #334): the pure
//! `sunrise_domain::lateness` rule, fed with what only the engine knows —
//! the injected clock's now, the reader's zone, the vault's day schedule and
//! the `stale_after_days` preference — and the views built on it.
//!
//! Lateness is computed on every read and never stored, so a zone change, a
//! new day or an edited schedule is re-evaluated by reading again
//! (`docs/10-cross-cutting/time.md` §7). No client computes it: each view
//! carries the state it read here.

use super::ids::ms_to_ts;
use super::query::{sort_by_landing, Unlanded};
use super::task::read_task;
use super::{Engine, EngineError};
use crate::queries::{QueryResult, TaskLateness, TodayRow};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::{
    lateness, today_group, DaySchedule, LatenessState, PrefValue, Task, DEFAULT_STALE_AFTER_DAYS,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::Db;

/// Everything reading a task's lateness needs that is not in the task: who
/// is reading, where, when, and on which days.
#[derive(Debug, Clone)]
pub(crate) struct LatenessReader {
    /// The reader's zone (`Clock::timezone`).
    pub(crate) tz: TimeZone,
    /// The reader's now.
    pub(crate) now: Timestamp,
    /// The planner day holding `now`.
    pub(crate) today: Date,
    /// The vault's day schedule.
    pub(crate) schedule: DaySchedule,
    /// The `stale_after_days` preference.
    pub(crate) stale_after_days: u16,
}

impl LatenessReader {
    /// `task`'s lateness for this reader.
    pub(crate) fn of(&self, task: &Task) -> LatenessState {
        lateness(
            task,
            self.now,
            &self.tz,
            &self.schedule,
            self.stale_after_days,
        )
    }

    /// The saved-view filter's reader: the same zone, now, day schedule and
    /// staleness.
    pub(crate) fn filter_reader(&self) -> sunrise_domain::saved_view::FilterReader {
        sunrise_domain::saved_view::FilterReader {
            tz: self.tz.clone(),
            today: self.today,
            now: self.now,
            schedule: self.schedule.clone(),
            stale_after_days: self.stale_after_days,
        }
    }
}

impl Engine {
    /// The `stale_after_days` preference (`preferences.md`): 14 when unset,
    /// `0` to disable staleness.
    pub(crate) fn stale_after_days(&self, db: &Db) -> Result<u16, EngineError> {
        Ok(match self.resolved_preference(db, "stale_after_days")? {
            Some(PrefValue::Uint(n)) => u16::try_from(n).unwrap_or(u16::MAX),
            _ => DEFAULT_STALE_AFTER_DAYS,
        })
    }

    /// The reader at `now_ms`, in the reader's zone, on the vault's planner
    /// days.
    pub(crate) fn lateness_reader(
        &self,
        db: &Db,
        now_ms: u64,
    ) -> Result<LatenessReader, EngineError> {
        let schedule = self.day_schedule(db)?;
        let (today, _, _) = self.planner_day_at(&schedule, now_ms)?;
        Ok(LatenessReader {
            tz: self.device_zone(),
            now: ms_to_ts(i64::try_from(now_ms).unwrap_or(i64::MAX)),
            today,
            schedule,
            stale_after_days: self.stale_after_days(db)?,
        })
    }

    /// `Query::Lateness`: each named task's lateness for the reader now, in
    /// the order asked. A task this replica does not hold is refused, so a
    /// client never renders "on track" for a task it has not got.
    pub(super) fn query_lateness(
        &self,
        db: &Db,
        ids: &[EntityRef],
    ) -> Result<QueryResult, EngineError> {
        let reader = self.lateness_reader(db, self.clock.now_ms())?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            super::ids::require_kind(*id, EntityKind::Task)?;
            let task = read_task(db.conn(), id.bytes())?
                .ok_or_else(|| EngineError::NotFound(format!("task {id}")))?;
            out.push(TaskLateness {
                task: *id,
                lateness: reader.of(&task),
            });
        }
        Ok(QueryResult::Lateness(out))
    }

    /// `Query::TodayView`: Today as ADR-0047 composes it
    /// (`docs/08-features/planning-views.md` §Today). Each open task Today
    /// holds ([`Self::query_today`]) or that is late, with its group and its
    /// lateness, in group order and, within a group, by when it lands, then
    /// id.
    ///
    /// Late tasks are every open task that is `LateHard` or `LateSoft`,
    /// whatever day its deadline was on; a task planned for a later day with
    /// no deadline today is left out, as Today leaves it out.
    pub(super) fn query_today_view(
        &self,
        db: &Db,
        now_ms: u64,
        contexts: &[EntityRef],
    ) -> Result<QueryResult, EngineError> {
        let reader = self.lateness_reader(db, now_ms)?;
        let QueryResult::Tasks(mut tasks) = self.query_today(db, now_ms, contexts)? else {
            return Err(EngineError::Invalid("Today returned no task list".into()));
        };
        // A late task whose deadline is before today is already in Today's
        // read (it reads every time on or before the day); one whose
        // deadline passed earlier today is too.
        sort_by_landing(&mut tasks, &reader.tz, Unlanded::Last);
        let mut rows: Vec<TodayRow> = tasks
            .into_iter()
            .filter_map(|task| {
                let lateness = reader.of(&task);
                let (group, deadline) =
                    today_group(&task, &lateness, reader.today, &reader.tz, &reader.schedule)?;
                Some(TodayRow {
                    task,
                    group,
                    deadline,
                    lateness,
                })
            })
            .collect();
        // Stable: within a group, the landing order above. Late rows lead
        // with the hard ones, oldest first, as the triage queue orders them.
        rows.sort_by(|a, b| {
            a.group.cmp(&b.group).then_with(|| {
                if a.group == sunrise_domain::TodayGroup::Late {
                    a.lateness
                        .state
                        .cmp(&b.lateness.state)
                        .then_with(|| a.lateness.since.cmp(&b.lateness.since))
                        .then_with(|| a.task.id.cmp(&b.task.id))
                } else {
                    std::cmp::Ordering::Equal
                }
            })
        });
        Ok(QueryResult::TodayView(rows))
    }
}
