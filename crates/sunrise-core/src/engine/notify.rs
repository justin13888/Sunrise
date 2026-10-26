//! Notification reads (`docs/08-features/notifications.md`, issue #9).
//!
//! Everything a notification says is computed here, on-device, after decryption.
//! The relay only ever sends a content-less wake-up, so a count or a title
//! reaching the server would be the whole privacy model failing at once. The
//! rules themselves are the pure `sunrise_domain::notify` functions; this
//! module's job is to read the rows and resolve the planner-day bounds.

use super::ids::{blob16, ms_to_ts};
use super::review::read_live_tasks;
use super::stream::read_stream;
use super::{Engine, EngineError};
use crate::queries::QueryResult;
use rusqlite::OptionalExtension;
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
                &schedule,
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
            &schedule,
        ))))
    }

    /// Everything worth scheduling with the OS between `now` and the horizon.
    ///
    /// Two sources: a Task's `planned_at` and a Block's start. A routine
    /// occurrence is already a Task by the time it is due — that is what
    /// materialization produces — so it needs no separate source, and having
    /// one would double every routine reminder. Its intent is
    /// [`ReminderKind::Routine`] instead of [`ReminderKind::Task`].
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

        // The open tasks as Today shows them (#331): a routine occurrence
        // that lapsed under its catch-up policy has no reminder, and the one
        // a `merge` policy shows for several is titled for all of them.
        let mut open: Vec<_> = read_live_tasks(db.conn())?
            .into_iter()
            .filter(|t| matches!(t.state.effective(), TaskState::Todo | TaskState::InProgress))
            .collect();
        self.apply_catchup(db.conn(), &mut open, now_ms.max(self.clock.now_ms()))?;
        for t in open {
            let Some(at) = t.planned_at.clone() else {
                continue;
            };
            let stream_lead = lead_of(t.stream_id)?;
            candidates.push(ReminderCandidate {
                entity: t.id,
                // A routine occurrence is a task by the time it is due, and
                // its reminder is the routine's (#3), which replaces the
                // task's rather than adding to it.
                kind: if t.routine_id.is_some() {
                    ReminderKind::Routine
                } else {
                    ReminderKind::Task
                },
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

    /// Every open, live, unarchived task with a hard deadline, by id: what
    /// the deadline reminders (#2) are planned from.
    pub(crate) fn deadline_candidates(
        &self,
        db: &Db,
    ) -> Result<Vec<crate::alerts::DeadlineCandidate>, EngineError> {
        Ok(read_live_tasks(db.conn())?
            .into_iter()
            .filter(|t| {
                !t.archived
                    && matches!(t.state.effective(), TaskState::Todo | TaskState::InProgress)
            })
            .filter_map(|t| {
                Some(crate::alerts::DeadlineCandidate {
                    hard_due_at: t.hard_due_at?,
                    task: t.id,
                    title: t.title,
                })
            })
            .collect())
    }

    /// The security events this replica holds from after this device's own
    /// certificate was made (§Security events): another device joining, a
    /// device revoked, a rotation that retired the recovery code. Ordered
    /// by time, then kind, then subject, so two reads agree.
    ///
    /// Events before this device joined are not its news: a device that
    /// pairs into an account learns that history on its first sync, and none
    /// of it is an alert.
    pub(crate) fn security_events(
        &self,
        db: &Db,
    ) -> Result<Vec<crate::alerts::SecurityEvent>, EngineError> {
        use crate::alerts::{SecurityEvent, SecurityEventKind as K};
        use crate::inner_op::{decode_inner_op, InnerOp};
        let conn = db.conn();
        let me = self.keychain.device_id();
        let joined_at: i64 = conn
            .query_row(
                "SELECT created_at_ms FROM devices WHERE device_id = ?1",
                [&me[..]],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let ms = |v: i64| u64::try_from(v).unwrap_or(0);
        let mut out = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT device_id, nickname, created_at_ms FROM devices
             WHERE created_at_ms > ?1 AND device_id != ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![joined_at, &me[..]], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, nickname, at) = row?;
            out.push(SecurityEvent {
                kind: K::DeviceJoined,
                subject: blob16(&id),
                at_ms: ms(at),
                nickname: Some(nickname),
                this_device: false,
            });
        }
        let mut stmt = conn.prepare(
            "SELECT r.device_id, d.nickname, r.cut_ms FROM device_revocations r
             LEFT JOIN devices d ON d.device_id = r.device_id
             WHERE r.cut_ms > ?1",
        )?;
        let rows = stmt.query_map([joined_at], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, nickname, at) = row?;
            let subject = blob16(&id);
            out.push(SecurityEvent {
                kind: K::DeviceRevoked,
                subject,
                at_ms: ms(at),
                nickname,
                this_device: subject == me,
            });
        }
        let mut stmt = conn.prepare(
            "SELECT to_identity_id, hlc_physical_ms, payload FROM identity_transitions
             WHERE hlc_physical_ms > ?1",
        )?;
        let rows = stmt.query_map([joined_at], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        for row in rows {
            let (id, at, payload) = row?;
            // A rotation that kept the code seals a share to its holders;
            // one that did not retired it (ADR-0070 §1). A payload this
            // build cannot read is no news either way.
            let retired = matches!(
                decode_inner_op(&payload),
                Ok(InnerOp::IdentityTransition(p)) if p.identity_share.is_none()
            );
            if retired {
                out.push(SecurityEvent {
                    kind: K::RecoveryCodeRetired,
                    subject: blob16(&id),
                    at_ms: ms(at),
                    nickname: None,
                    this_device: false,
                });
            }
        }
        out.sort_by(|a, b| (a.at_ms, a.kind, a.subject).cmp(&(b.at_ms, b.kind, b.subject)));
        Ok(out)
    }

    /// When the oldest op the relay has not acknowledged was queued, epoch
    /// ms; `None` with nothing waiting. What "sync stalled" (#12) measures.
    pub(crate) fn oldest_unacked_ms(&self, db: &Db) -> Result<Option<u64>, EngineError> {
        let at: Option<i64> = db.conn().query_row(
            "SELECT min(enqueued_at_ms) FROM outbox WHERE acked_at_ms IS NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(at.map(|v| u64::try_from(v).unwrap_or(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::Engine;
    use crate::alerts::SecurityEventKind as K;
    use crate::commands::Command;
    use crate::config::{Clock, SystemRng};
    use crate::keychain::Keychain;
    use rusqlite::params;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use sunrise_crypto::keys::VaultRootKey;
    use sunrise_domain::{SunriseTime, TaskDraft, TaskPatch, TaskState};
    use sunrise_storage::Db;

    const ROOT: [u8; 32] = [0x92; 32];
    const NOW: u64 = 1_700_000_000_000;

    #[derive(Debug)]
    struct Fixed;

    impl Clock for Fixed {
        fn now_ms(&self) -> u64 {
            NOW
        }
    }

    fn vault() -> (Engine, Db) {
        let kc = Arc::new(Keychain::for_test(VaultRootKey::from_bytes(ROOT)));
        let e = Engine::from_clock(Arc::new(Fixed), Arc::new(SystemRng), kc);
        (e, Db::open_memory(&VaultRootKey::from_bytes(ROOT)).unwrap())
    }

    /// Which tasks with a hard deadline plan a deadline reminder: an open
    /// one, todo or in progress; never a done, cancelled, archived or
    /// deleted one, and never one with only a target.
    #[test]
    fn deadline_candidates_are_the_live_open_unarchived_tasks_with_a_hard_deadline() {
        let (e, mut db) = vault();
        let due = SunriseTime::instant(super::ms_to_ts(NOW as i64 + 3_600_000));
        let table: [(&str, TaskPatch, bool); 7] = [
            ("todo", TaskPatch::default(), true),
            (
                "in progress",
                TaskPatch {
                    state: Some(TaskState::InProgress),
                    ..Default::default()
                },
                true,
            ),
            (
                "done",
                TaskPatch {
                    state: Some(TaskState::Done),
                    ..Default::default()
                },
                false,
            ),
            (
                "cancelled",
                TaskPatch {
                    state: Some(TaskState::Cancelled),
                    ..Default::default()
                },
                false,
            ),
            (
                "archived",
                TaskPatch {
                    archived: Some(true),
                    ..Default::default()
                },
                false,
            ),
            ("deleted", TaskPatch::default(), false),
            (
                "target only",
                TaskPatch {
                    hard_due_at: Some(None),
                    target_at: Some(Some(due.clone())),
                    ..Default::default()
                },
                false,
            ),
        ];
        let mut want = BTreeSet::new();
        for (title, patch, planned) in table {
            let id = e
                .apply(
                    &mut db,
                    Command::CreateTask(TaskDraft {
                        title: title.into(),
                        hard_due_at: Some(due.clone()),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .entity;
            e.apply(&mut db, Command::UpdateTask { id, patch }).unwrap();
            if title == "deleted" {
                e.apply(&mut db, Command::DeleteTask(id)).unwrap();
            }
            if planned {
                want.insert((id, title.to_owned()));
            }
        }
        let got: BTreeSet<_> = e
            .deadline_candidates(&db)
            .unwrap()
            .into_iter()
            .map(|c| {
                assert_eq!(c.hard_due_at, due, "{}", c.title);
                (c.task, c.title)
            })
            .collect();
        assert_eq!(got, want);
    }

    /// Security events at one instant are ordered by kind, then subject,
    /// whatever order the rows were written in, so two reads agree; a
    /// rotation whose payload this build cannot read is no event.
    #[test]
    fn security_events_at_one_instant_order_by_kind_then_subject() {
        let (e, db) = vault();
        let at = 1_800_000_000_000i64;
        let conn = db.conn();
        for (device, nickname) in [([0x30u8; 16], "thirty"), ([0x20u8; 16], "twenty")] {
            conn.execute(
                "INSERT INTO devices (device_id, cert_blob, nickname, platform, created_at_ms)
                 VALUES (?1, X'', ?2, 'test', ?3)",
                params![&device[..], nickname, at],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO device_revocations
             (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
             VALUES (?1, ?2, 0, X'', 'Lost', ?2)",
            params![&[0x10u8; 16][..], at],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO device_revocations
             (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
             VALUES (?1, ?2, 0, X'', 'Lost', ?2)",
            params![&[0x40u8; 16][..], at - 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO identity_transitions
             (to_identity_id, from_identity_id, to_id_s_pub, to_id_d_pub, meta_epoch,
              hlc_physical_ms, hlc_logical, emitter_device_id, payload, prev_sig, next_sig)
             VALUES (?1, X'', X'', X'', 0, ?2, 0, X'', X'00ff', X'', X'')",
            params![&[0x05u8; 16][..], at],
        )
        .unwrap();
        let events = e.security_events(&db).unwrap();
        let order: Vec<(u64, K, [u8; 16])> = events
            .iter()
            .map(|ev| (ev.at_ms, ev.kind, ev.subject))
            .collect();
        let at = u64::try_from(at).unwrap();
        assert_eq!(
            order,
            [
                (at - 1, K::DeviceRevoked, [0x40; 16]),
                (at, K::DeviceJoined, [0x20; 16]),
                (at, K::DeviceJoined, [0x30; 16]),
                (at, K::DeviceRevoked, [0x10; 16]),
            ],
            "time, then kind, then subject; the unreadable rotation is no news"
        );
        assert_eq!(events, e.security_events(&db).unwrap());
    }
}
