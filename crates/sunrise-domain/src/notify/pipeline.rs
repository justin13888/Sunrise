//! The reminder pipeline: candidates in, ordered intents out.

use super::policy::{apply_quiet_hours, lead_time_s, ReminderSettings, BLOCK_DEFAULT_LEAD_S};
use crate::time::SunriseTime;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Which of the spec's notification sources an intent came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReminderKind {
    /// A Task's `scheduled_at`, minus the resolved lead time.
    Task,
    /// A Block's start, minus the resolved lead time.
    Block,
    /// A routine occurrence falling due and still undone.
    Routine,
}

/// One thing to schedule with the OS. A client turns each of these into a
/// local notification; nothing about it reaches the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderIntent {
    /// What the notification is about, and what a deep link opens.
    pub entity: EntityRef,
    /// Which source produced it.
    pub kind: ReminderKind,
    /// When to fire, after lead time and quiet hours.
    pub fire_at: Timestamp,
    /// The title to render. Already plaintext: this is computed on-device.
    pub title: String,
    /// The instant it would have fired at, when quiet hours moved it. `None`
    /// when nothing moved it, so a client can say "delayed from 06:15" only
    /// when that is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_from: Option<Timestamp>,
}

impl ReminderIntent {
    /// Which reminder this is: the entity and the rule that produced it
    /// (one per source today), and never its fire time
    /// (`docs/11-adr/0066-times-survive-travel.md` §5).
    ///
    /// A zone change moves a floating reminder's `fire_at` and leaves this
    /// alone, so a client keyed on it replaces the pending notification
    /// rather than keeping one at a stale wall clock beside the new one.
    #[must_use]
    pub const fn identity(&self) -> (EntityRef, ReminderKind) {
        (self.entity, self.kind)
    }
}

/// One candidate the engine hands the planner: a thing that happens at a time,
/// with whatever lead-time overrides apply to it.
#[derive(Debug, Clone)]
pub struct ReminderCandidate {
    /// The entity to remind about.
    pub entity: EntityRef,
    /// Which source it is.
    pub kind: ReminderKind,
    /// When the thing itself happens.
    pub at: SunriseTime,
    /// What to call it.
    pub title: String,
    /// Lead time set on the entity itself; top of the hierarchy.
    pub own_lead_s: Option<u32>,
    /// Lead time set on the owning Stream; the middle level.
    pub stream_lead_s: Option<u32>,
}

/// Turn candidates into intents: resolve each one's lead time, subtract it,
/// drop anything outside `[now, horizon]`, apply quiet hours, and order by
/// fire time.
///
/// A non-primary device returns nothing at all — that is the whole of the
/// spec's multi-device dedup rule, and doing it here rather than in each
/// client is what keeps the three clients from disagreeing about it.
#[must_use]
pub fn plan_reminders(
    candidates: &[ReminderCandidate],
    now: Timestamp,
    horizon: Timestamp,
    tz: &TimeZone,
    settings: &ReminderSettings,
) -> Vec<ReminderIntent> {
    plan_reminders_reported(candidates, now, horizon, tz, settings).intents
}

/// What [`plan_reminders_reported`] planned, and what it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReminderPlan {
    /// What to schedule, as [`plan_reminders`] returns it.
    pub intents: Vec<ReminderIntent>,
    /// Candidates whose time has no instant for this reader, so nothing was
    /// scheduled for them (`docs/11-adr/0066-times-survive-travel.md` §3).
    /// A candidate outside the window or silenced by quiet hours is not here:
    /// those were planned, and the plan was "not now".
    pub unscheduled: Vec<UnscheduledReminder>,
}

/// One candidate [`plan_reminders_reported`] scheduled nothing for, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnscheduledReminder {
    /// What it is about.
    pub entity: EntityRef,
    /// Which source it is.
    pub kind: ReminderKind,
    /// Why its time has no instant.
    pub reason: crate::time::Unresolvable,
}

/// [`plan_reminders`], and the candidates it could not schedule.
///
/// A candidate whose time has no instant in `tz` is never given a stand-in
/// one: the epoch would fire it at once, and a far-future one would drop it
/// without a word. It is reported in [`ReminderPlan::unscheduled`] instead.
#[must_use]
pub fn plan_reminders_reported(
    candidates: &[ReminderCandidate],
    now: Timestamp,
    horizon: Timestamp,
    tz: &TimeZone,
    settings: &ReminderSettings,
) -> ReminderPlan {
    let mut plan = ReminderPlan::default();
    if !settings.is_primary_device {
        return plan;
    }
    for c in candidates {
        let global = match c.kind {
            // "Block reminders: 15 min before (default)" — a different floor,
            // not a different hierarchy.
            ReminderKind::Block => BLOCK_DEFAULT_LEAD_S,
            ReminderKind::Task | ReminderKind::Routine => settings.default_lead_s,
        };
        let lead = lead_time_s(c.own_lead_s, c.stream_lead_s, global);
        let happens_at = match c.at.to_instant(tz) {
            Ok(at) => at,
            Err(reason) => {
                plan.unscheduled.push(UnscheduledReminder {
                    entity: c.entity,
                    kind: c.kind,
                    reason,
                });
                continue;
            }
        };
        let Some(raw) = happens_at.as_second().checked_sub(i64::from(lead)) else {
            continue;
        };
        let Ok(raw) = Timestamp::from_second(raw) else {
            continue;
        };
        // The window is on the RAW fire time, before quiet hours: a
        // notification queued past the horizon is still this window's, and
        // dropping it here would lose it entirely.
        if raw < now || raw > horizon {
            continue;
        }
        let Some(fire_at) =
            apply_quiet_hours(&raw.to_zoned(tz.clone()), settings.quiet_hours.as_ref())
        else {
            continue;
        };
        plan.intents.push(ReminderIntent {
            entity: c.entity,
            kind: c.kind,
            fire_at,
            title: c.title.clone(),
            deferred_from: (fire_at != raw).then_some(raw),
        });
    }
    plan.intents.sort_by(|a, b| {
        a.fire_at
            .cmp(&b.fire_at)
            .then_with(|| a.entity.cmp(&b.entity))
    });
    plan
}

#[cfg(test)]
mod tests {
    use super::super::policy::QuietHoursPolicy;
    use super::super::test_support::{at_local, quiet, ts, DAY_MS};
    use super::*;
    use sunrise_id::EntityKind;

    // ---- planning ----

    fn candidate(n: u8, kind: ReminderKind, at_ms: i64) -> ReminderCandidate {
        ReminderCandidate {
            entity: EntityRef::new(EntityKind::Task, [n; 16]),
            kind,
            at: SunriseTime::instant(ts(at_ms)),
            title: format!("thing {n}"),
            own_lead_s: None,
            stream_lead_s: None,
        }
    }

    #[test]
    fn a_task_reminder_fires_at_its_scheduled_time_by_default() {
        let at = 1_772_000_000_000;
        let out = plan_reminders(
            &[candidate(1, ReminderKind::Task, at)],
            ts(at - DAY_MS),
            ts(at + DAY_MS),
            &TimeZone::UTC,
            &ReminderSettings::default(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].fire_at, ts(at));
        assert!(out[0].deferred_from.is_none());
    }

    #[test]
    fn a_block_reminder_defaults_to_fifteen_minutes_before() {
        let at = 1_772_000_000_000;
        let out = plan_reminders(
            &[candidate(1, ReminderKind::Block, at)],
            ts(at - DAY_MS),
            ts(at + DAY_MS),
            &TimeZone::UTC,
            &ReminderSettings::default(),
        );
        assert_eq!(
            out[0].fire_at,
            ts(at - i64::from(BLOCK_DEFAULT_LEAD_S) * 1_000)
        );
    }

    /// `ReminderKind::Routine` is constructed nowhere else in the workspace,
    /// so nothing said which floor it falls back to. It is the device's
    /// `default_lead_s`, like a Task: only a Block gets the 15-minute floor.
    #[test]
    fn a_routine_reminder_falls_back_to_the_device_default_not_the_block_floor() {
        let at = 1_772_000_000_000;
        let settings = ReminderSettings {
            default_lead_s: 300,
            ..ReminderSettings::default()
        };
        assert_ne!(
            settings.default_lead_s, BLOCK_DEFAULT_LEAD_S,
            "the two floors must differ or this test cannot tell them apart"
        );
        let out = plan_reminders(
            &[
                candidate(1, ReminderKind::Routine, at),
                candidate(2, ReminderKind::Block, at),
            ],
            ts(at - DAY_MS),
            ts(at + DAY_MS),
            &TimeZone::UTC,
            &settings,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].fire_at,
            ts(at - i64::from(BLOCK_DEFAULT_LEAD_S) * 1_000),
            "the Block still uses its own 15-minute floor"
        );
        assert_eq!(out[0].kind, ReminderKind::Block);
        assert_eq!(
            out[1].fire_at,
            ts(at - 300_000),
            "the routine uses the device default, five minutes here"
        );
        assert_eq!(out[1].kind, ReminderKind::Routine);
    }

    #[test]
    fn the_hierarchy_beats_the_block_default() {
        let at = 1_772_000_000_000;
        let mut c = candidate(1, ReminderKind::Block, at);
        c.stream_lead_s = Some(60);
        let out = plan_reminders(
            &[c],
            ts(at - DAY_MS),
            ts(at + DAY_MS),
            &TimeZone::UTC,
            &ReminderSettings::default(),
        );
        assert_eq!(out[0].fire_at, ts(at - 60_000));
    }

    #[test]
    fn anything_outside_the_window_is_left_out() {
        let at = 1_772_000_000_000;
        let out = plan_reminders(
            &[
                candidate(1, ReminderKind::Task, at - 10 * DAY_MS),
                candidate(2, ReminderKind::Task, at + 10 * DAY_MS),
                candidate(3, ReminderKind::Task, at + 1_000),
            ],
            ts(at),
            ts(at + DAY_MS),
            &TimeZone::UTC,
            &ReminderSettings::default(),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entity, EntityRef::new(EntityKind::Task, [3u8; 16]));
    }

    /// The spec's dedup rule: the primary device handles reminders and the
    /// others stay silent. Enforced once, here.
    #[test]
    fn a_non_primary_device_plans_nothing() {
        let at = 1_772_000_000_000;
        let settings = ReminderSettings {
            is_primary_device: false,
            ..ReminderSettings::default()
        };
        let out = plan_reminders(
            &[candidate(1, ReminderKind::Task, at)],
            ts(at - DAY_MS),
            ts(at + DAY_MS),
            &TimeZone::UTC,
            &settings,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn a_queued_reminder_reports_what_it_was_deferred_from() {
        // 06:15 UTC on 2026-03-04.
        let raw = at_local(6, 15).timestamp();
        let settings = ReminderSettings {
            quiet_hours: Some(quiet(QuietHoursPolicy::Queue)),
            ..ReminderSettings::default()
        };
        let out = plan_reminders(
            &[candidate(1, ReminderKind::Task, raw.as_millisecond())],
            raw - jiff::SignedDuration::from_hours(1),
            raw + jiff::SignedDuration::from_hours(1),
            &TimeZone::UTC,
            &settings,
        );
        assert_eq!(out[0].fire_at, at_local(7, 0).timestamp());
        assert_eq!(out[0].deferred_from, Some(raw));
    }

    #[test]
    fn intents_come_back_ordered_by_fire_time() {
        let at = 1_772_000_000_000;
        let out = plan_reminders(
            &[
                candidate(1, ReminderKind::Task, at + 3_000),
                candidate(2, ReminderKind::Task, at + 1_000),
                candidate(3, ReminderKind::Task, at + 2_000),
            ],
            ts(at),
            ts(at + DAY_MS),
            &TimeZone::UTC,
            &ReminderSettings::default(),
        );
        let order: Vec<_> = out.iter().map(|i| i.fire_at).collect();
        assert_eq!(order, vec![ts(at + 1_000), ts(at + 2_000), ts(at + 3_000)]);
    }
}
