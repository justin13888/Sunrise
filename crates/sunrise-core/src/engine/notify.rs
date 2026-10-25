//! Notification reads (`docs/08-features/notifications.md`, issue #9).
//!
//! Everything a notification says is computed here, on-device, after decryption.
//! The relay only ever sends a content-less wake-up, so a count or a title
//! reaching the server would be the whole privacy model failing at once. The
//! rules themselves are the pure `sunrise_domain::notify` functions; this
//! module's job is to read the rows and resolve the planner-day bounds.

use super::ids::ms_to_ts;
use super::review::read_live_tasks;
use super::stream::read_stream;
use super::{Engine, EngineError};
use crate::queries::QueryResult;
use std::collections::BTreeMap;
use sunrise_domain::{
    build_end_of_day_plan, build_morning_summary, plan_reminders_reported, ReminderCandidate,
    ReminderKind, ReminderPlan, ReminderSettings, TaskState,
};
use sunrise_storage::Db;

/// The date before `d`.
fn day_before(d: jiff::civil::Date) -> Result<jiff::civil::Date, EngineError> {
    d.yesterday()
        .map_err(|e| EngineError::Invalid(format!("previous day: {e}")))
}

impl Engine {
    pub(super) fn query_morning_summary(
        &self,
        db: &Db,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        // Today's planner day (`docs/02-domain/day-schedule.md`), and "since
        // the previous one": the start of yesterday's, over civil dates so a
        // DST day is still a day. With no day schedule both are civil days.
        let schedule = self.day_schedule(db)?;
        let (today, today_start, today_end) = self.planner_day_at(&schedule, now_ms)?;
        let (prev_start, _) = self.planner_bounds(&schedule, day_before(today)?)?;
        let tasks = read_live_tasks(db.conn())?;
        Ok(QueryResult::MorningSummary(Box::new(
            build_morning_summary(
                &tasks,
                ms_to_ts(prev_start),
                ms_to_ts(today_start),
                ms_to_ts(today_end),
                &self.device_zone(),
            ),
        )))
    }

    pub(super) fn query_end_of_day_plan(
        &self,
        db: &Db,
        now_ms: u64,
    ) -> Result<QueryResult, EngineError> {
        let schedule = self.day_schedule(db)?;
        let (today, day_start, day_end) = self.planner_day_at(&schedule, now_ms)?;
        // "The week ahead": the seven planner days that follow today.
        let last = today
            .checked_add(jiff::Span::new().days(7))
            .map_err(|e| EngineError::Invalid(format!("week ahead: {e}")))?;
        let (_, week_end) = self.planner_bounds(&schedule, last)?;
        let tasks = read_live_tasks(db.conn())?;
        Ok(QueryResult::EndOfDayPlan(Box::new(build_end_of_day_plan(
            &tasks,
            ms_to_ts(day_start),
            ms_to_ts(day_end),
            ms_to_ts(week_end),
            &self.device_zone(),
        ))))
    }

    /// Everything worth scheduling with the OS between `now` and the horizon.
    ///
    /// Two sources today: a Task's `scheduled_at` and a Block's start. A
    /// routine occurrence is already a Task by the time it is due — that is
    /// what materialization produces — so it needs no separate source, and
    /// having one would double every routine reminder.
    pub(super) fn query_reminder_intents(
        &self,
        db: &Db,
        now_ms: u64,
        horizon_ms: u64,
        settings: &ReminderSettings,
    ) -> Result<QueryResult, EngineError> {
        Ok(QueryResult::Reminders(
            self.reminder_plan(db, now_ms, horizon_ms, settings)?
                .intents,
        ))
    }

    /// [`Engine::query_reminder_intents`], with the candidates it could not
    /// schedule because their time has no instant for this reader
    /// (`docs/11-adr/0066-times-survive-travel.md` §3).
    ///
    /// Planned in the reader's zone at the moment of the call, so a zone
    /// change re-plans every reminder by being read again: a floating one
    /// moves to its wall-clock time in the new zone, a zoned one keeps its
    /// instant, and each keeps its [`sunrise_domain::ReminderIntent::identity`].
    pub(crate) fn reminder_plan(
        &self,
        db: &Db,
        now_ms: u64,
        horizon_ms: u64,
        settings: &ReminderSettings,
    ) -> Result<ReminderPlan, EngineError> {
        // A non-primary device is silent, so do not even read the rows.
        if !settings.is_primary_device {
            return Ok(ReminderPlan::default());
        }
        // Each stream's lead, read once. No stream has none: the device
        // default is the fallback.
        let mut stream_leads: BTreeMap<[u8; 16], Option<u32>> = BTreeMap::new();
        let mut lead_of =
            |stream: Option<sunrise_id::EntityRef>| -> Result<Option<u32>, EngineError> {
                let Some(stream) = stream else {
                    return Ok(None);
                };
                Ok(match stream_leads.entry(*stream.bytes()) {
                    std::collections::btree_map::Entry::Occupied(e) => *e.get(),
                    std::collections::btree_map::Entry::Vacant(e) => *e.insert(
                        read_stream(db.conn(), stream.bytes())?.and_then(|s| s.reminder_lead_s),
                    ),
                })
            };
        let mut candidates = Vec::new();

        for t in read_live_tasks(db.conn())? {
            if !matches!(t.state.effective(), TaskState::Todo | TaskState::InProgress) {
                continue;
            }
            let Some(at) = t.scheduled_at.clone() else {
                continue;
            };
            let stream_lead = lead_of(t.stream_id)?;
            candidates.push(ReminderCandidate {
                entity: t.id,
                kind: ReminderKind::Task,
                at,
                title: t.title.clone(),
                own_lead_s: t.reminder_lead_s,
                stream_lead_s: stream_lead,
            });
        }

        // Blocks in the window, plus a day either side: the lead time can pull
        // a block's reminder back before the window's start, and the horizon
        // filter in `plan_reminders` is what actually decides.
        let pad = 86_400_000i64;
        let from = (now_ms as i64).saturating_sub(pad);
        let to = (horizon_ms as i64).saturating_add(pad);
        let QueryResult::Blocks(rows) = self.query_blocks_between(db, from, to)? else {
            return Err(EngineError::Invalid(
                "block read returned non-blocks".into(),
            ));
        };
        for row in rows {
            let stream_lead = lead_of(row.block.stream_id)?;
            candidates.push(ReminderCandidate {
                entity: row.block.id,
                kind: ReminderKind::Block,
                at: row.block.starts_at.clone(),
                // The resolved title, so a notification says what the calendar
                // says rather than re-deriving the shadow-copy rules.
                title: row.title.unwrap_or_default(),
                own_lead_s: None,
                stream_lead_s: stream_lead,
            });
        }

        Ok(plan_reminders_reported(
            &candidates,
            ms_to_ts(now_ms as i64),
            ms_to_ts(horizon_ms as i64),
            &self.device_zone(),
            settings,
        ))
    }
}
