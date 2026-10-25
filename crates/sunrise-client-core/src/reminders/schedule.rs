//! One re-plan against a live vault: preferences, plan, Focus, reconcile.

use std::collections::{BTreeMap, BTreeSet};

use sunrise_core::{Core, CoreError, Query, QueryResult};
use sunrise_domain::{ReminderIntent, UnscheduledReminder};
use sunrise_id::{EntityKind, EntityRef};

use super::{
    plan, read_day_schedule, reconcile, reconcile_wind_down, reminder_policy,
    wind_down_notification, ClockStyle, PendingNotification, PlannedNotification, Reconciliation,
    ReminderPolicy, WindDownChange, WindDownNotification, HORIZON_MS, MAX_SCHEDULED,
};

/// What a client knows that the vault does not.
#[derive(Debug, Clone)]
pub struct ScheduleRequest {
    /// "Now", from the core's clock.
    pub now_ms: u64,
    /// Whether the OS lets this app schedule notifications at all. `false`
    /// plans nothing and withdraws what is held.
    pub authorized: bool,
    /// The Focus filter in force; see [`crate::focus`].
    pub focus_scope: Option<BTreeSet<EntityRef>>,
    /// What the OS is holding, read back.
    pub pending: Vec<PendingNotification>,
    /// How wall-clock times are written in a body.
    pub clock: ClockStyle,
}

/// One re-plan's answer.
#[derive(Debug, Clone)]
pub struct Schedule {
    /// What the OS should hold, earliest first.
    pub desired: Vec<PlannedNotification>,
    /// What to do to make it so.
    pub reconciliation: Reconciliation,
    /// Reminders the core could not place, each with why
    /// (`docs/11-adr/0066-times-survive-travel.md` §3). A settings screen
    /// says so rather than leaving a reminder that never fires unexplained.
    pub unscheduled: Vec<UnscheduledReminder>,
    /// The preferences the plan was made under.
    pub policy: ReminderPolicy,
    /// Whether the reader's zone is a stand-in, so floating reminders were
    /// placed in UTC; the client reports the OS zone and re-plans.
    pub zone_unknown: bool,
    /// The next wind-down this device should hold, if it fires one
    /// ([`super::wind_down_notification`]). Planned in the reader's zone, so
    /// a zone change moves it.
    pub wind_down: Option<WindDownNotification>,
    /// What to do with the wind-down the OS holds. [`Self::reconciliation`]
    /// never names it.
    pub wind_down_change: WindDownChange,
}

/// Re-plan this device's reminders against `core` and say what the OS must
/// change.
///
/// Safe to call on every change batch and every zone change: the plan is
/// read again in the reader's zone each time, so a floating reminder moves
/// to its wall clock in the new zone under the same identifier, and an
/// unchanged plan reconciles to nothing.
///
/// Three ways it ends with nothing desired are not failures: the OS refused
/// permission, the master switch is off, or this is not the primary device.
/// Each withdraws whatever is held, which is the point — a device demoted
/// from primary must go quiet, not merely stop adding.
///
/// # Errors
///
/// A failed read of the vault.
pub async fn schedule(core: &Core, req: ScheduleRequest) -> Result<Schedule, CoreError> {
    let policy = read_policy(core).await?;
    let zone = core.reader_zone();

    let (desired, unscheduled) = if req.authorized && policy.enabled {
        let planned = core.reminder_plan(
            req.now_ms,
            req.now_ms.saturating_add(HORIZON_MS),
            &policy.settings,
        )?;
        let intents: Vec<ReminderIntent> = planned
            .intents
            .into_iter()
            .filter(|i| policy.kind_enabled(i.kind))
            .collect();
        let streams = match &req.focus_scope {
            Some(_) => streams_of(core, &intents).await,
            None => BTreeMap::new(),
        };
        let audible = crate::focus::muting(intents, req.focus_scope.as_ref(), &streams);
        (
            plan(&audible, &zone, req.clock, MAX_SCHEDULED),
            planned.unscheduled,
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let wind_down = if req.authorized {
        let schedule = read_day_schedule(core, req.now_ms).await?;
        wind_down_notification(
            &policy,
            &schedule,
            crate::vocab::instant_at(req.now_ms),
            &zone,
        )
    } else {
        None
    };

    Ok(Schedule {
        reconciliation: reconcile(&desired, &req.pending),
        wind_down_change: reconcile_wind_down(wind_down.as_ref(), &req.pending),
        wind_down,
        desired,
        unscheduled,
        policy,
        zone_unknown: core.zone_unknown(),
    })
}

/// This device's [`ReminderPolicy`], from the vault's preferences and the
/// devices it calls revoked.
///
/// # Errors
///
/// A failed read of the vault.
pub async fn read_policy(core: &Core) -> Result<ReminderPolicy, CoreError> {
    let this_device = EntityRef::new(EntityKind::Device, core.device_id());
    let QueryResult::Preferences(prefs) = core.query(Query::Preferences).await? else {
        return Err(unexpected("preferences"));
    };
    let QueryResult::Devices(devices) = core.query(Query::DeviceList).await? else {
        return Err(unexpected("device list"));
    };
    let revoked: Vec<EntityRef> = devices
        .iter()
        .filter(|d| d.revoked)
        .map(|d| EntityRef::new(EntityKind::Device, d.device_id))
        .collect();
    Ok(reminder_policy(&prefs, this_device, &revoked))
}

/// The stream each reminder's entity is filed in: a task's own, a routine's
/// template's, a block's own, and the legacy Inbox reference for none
/// ([`sunrise_domain::stream_key`]). A read that fails leaves the entity out, which
/// [`crate::focus::allows`] lets through. Read only while a Focus filter is
/// on, and at most a day's reminders.
async fn streams_of(core: &Core, intents: &[ReminderIntent]) -> BTreeMap<EntityRef, EntityRef> {
    let mut streams = BTreeMap::new();
    let entities: BTreeSet<EntityRef> = intents.iter().map(|i| i.entity).collect();
    for entity in entities {
        let stream = match core.query(Query::EntityById(entity)).await {
            Ok(QueryResult::Task(t)) => Some(t.stream_id),
            Ok(QueryResult::Routine(r)) => Some(r.template.stream_id),
            Ok(QueryResult::Blocks(rows)) => rows.first().map(|r| r.block.stream_id),
            _ => None,
        }
        // No stream is filed under the legacy Inbox reference, which a
        // Focus filter picks to mean "no stream".
        .map(sunrise_domain::stream_key);
        if let Some(stream) = stream {
            streams.insert(entity, stream);
        }
    }
    streams
}

pub(super) fn unexpected(what: &str) -> CoreError {
    CoreError::Engine(sunrise_core::EngineError::Invalid(format!(
        "{what} read returned another shape"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reminders::{identifier, primary_claim};
    use std::sync::Arc;
    use sunrise_core::{Command, CoreConfig, Unlock};
    use sunrise_domain::{
        PrefTarget, PrefValue, ReminderKind, StreamDraft, SunriseTime, TaskDraft,
    };

    /// 2026-06-01T12:00:00Z.
    const NOW: u64 = 1_780_315_200_000;

    fn now() -> u64 {
        NOW
    }

    async fn vault(dir: &std::path::Path) -> Core {
        Core::open(
            CoreConfig::with_clock(
                dir.to_path_buf(),
                "test",
                Arc::new(crate::host::HostClock::new(now, "America/Toronto")),
                Arc::new(sunrise_core::SystemRng),
            ),
            Unlock::DevicePaired {
                root: sunrise_crypto::keys::VaultRootKey::from_bytes([4; 32]),
                paired: None,
            },
        )
        .await
        .unwrap()
    }

    async fn task_in(core: &Core, name: &str, minutes: i64) -> (EntityRef, EntityRef) {
        let stream = core
            .submit(Command::CreateStream(StreamDraft {
                name: name.into(),
                ..StreamDraft::default()
            }))
            .await
            .unwrap()
            .entity;
        let at = jiff::Timestamp::from_millisecond(i64::try_from(NOW).unwrap() + minutes * 60_000)
            .unwrap();
        let task = core
            .submit(Command::CreateTask(TaskDraft {
                title: format!("{name} task"),
                stream_id: Some(stream),
                scheduled_at: Some(SunriseTime::instant(at)),
                ..TaskDraft::default()
            }))
            .await
            .unwrap()
            .entity;
        (stream, task)
    }

    fn request(pending: Vec<PendingNotification>) -> ScheduleRequest {
        ScheduleRequest {
            now_ms: NOW,
            authorized: true,
            focus_scope: None,
            pending,
            clock: ClockStyle::H24,
        }
    }

    fn held(s: &Schedule) -> Vec<PendingNotification> {
        s.desired
            .iter()
            .map(|n| PendingNotification {
                identifier: n.identifier.clone(),
                revision: Some(n.revision.clone()),
            })
            .collect()
    }

    /// The whole loop against a vault: plan, hold, re-plan to nothing, mute a
    /// stream under a Focus, and go quiet when the switch or the OS says so.
    #[tokio::test]
    async fn a_vaults_reminders_are_planned_muted_and_withdrawn() {
        let dir = tempfile::tempdir().unwrap();
        let core = vault(dir.path()).await;
        let (work, _) = task_in(&core, "Work", 10).await;
        let (_, home_task) = task_in(&core, "Home", 20).await;

        let first = schedule(&core, request(Vec::new())).await.unwrap();
        assert_eq!(first.desired.len(), 2);
        assert_eq!(first.reconciliation.add.len(), 2);
        assert!(first.policy.claim_primary);
        assert!(!first.zone_unknown);

        let again = schedule(&core, request(held(&first))).await.unwrap();
        assert!(
            again.reconciliation.is_empty(),
            "{:?}",
            again.reconciliation
        );

        let focused = schedule(
            &core,
            ScheduleRequest {
                focus_scope: crate::focus::scope([work]),
                ..request(held(&first))
            },
        )
        .await
        .unwrap();
        assert_eq!(focused.desired.len(), 1);
        assert_eq!(
            focused.reconciliation.cancel,
            [identifier(home_task, ReminderKind::Task)],
            "a muted reminder is withdrawn, not merely not added"
        );

        let refused = schedule(
            &core,
            ScheduleRequest {
                authorized: false,
                ..request(held(&first))
            },
        )
        .await
        .unwrap();
        assert!(refused.desired.is_empty());
        assert_eq!(refused.reconciliation.cancel.len(), 2);

        core.submit(Command::SetPreference {
            key: "notifications.enabled".into(),
            value: PrefValue::Bool(false),
            target: PrefTarget::Device,
        })
        .await
        .unwrap();
        let off = schedule(&core, request(held(&first))).await.unwrap();
        assert!(off.desired.is_empty() && !off.policy.enabled);
        assert_eq!(off.reconciliation.cancel.len(), 2);
        core.shutdown().await;
    }

    /// Another device holding the register silences this one.
    #[tokio::test]
    async fn a_device_that_is_not_primary_plans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let core = vault(dir.path()).await;
        task_in(&core, "Work", 10).await;
        let (key, value, target) = primary_claim(EntityRef::new(EntityKind::Device, [9; 16]));
        core.submit(Command::SetPreference {
            key: key.into(),
            value,
            target,
        })
        .await
        .unwrap();
        let s = schedule(&core, request(Vec::new())).await.unwrap();
        assert!(!s.policy.settings.is_primary_device);
        assert!(s.desired.is_empty());
        assert!(
            s.wind_down.is_none(),
            "a device that is not primary is silent"
        );
        core.shutdown().await;
    }

    /// #301's trigger against a vault: once a sleep time is set the plan
    /// carries a wind-down an hour before it, and a zone change re-plans it to
    /// the same wall clock in the new zone under the same identifier.
    #[tokio::test]
    async fn a_sleep_time_plans_a_wind_down_that_follows_the_zone() {
        let dir = tempfile::tempdir().unwrap();
        let core = vault(dir.path()).await;
        let none = schedule(&core, request(Vec::new())).await.unwrap();
        assert!(none.wind_down.is_none(), "no sleep time, no wind-down");
        assert_eq!(none.wind_down_change, WindDownChange::Keep);

        core.submit(Command::SetPreference {
            key: "day_schedule.weekday.MO".into(),
            value: PrefValue::DayWindow {
                wake: jiff::civil::time(7, 0, 0, 0),
                sleep: jiff::civil::time(23, 0, 0, 0),
            },
            target: PrefTarget::Vault,
        })
        .await
        .unwrap();
        // 2026-06-01 is a Monday: 23:00 EDT in Toronto is 03:00Z the next day.
        let toronto = schedule(&core, request(Vec::new())).await.unwrap();
        let w = toronto.wind_down.clone().unwrap();
        assert_eq!(w.fire_at_ms, 1_780_365_600_000, "22:00 EDT");
        assert!(matches!(toronto.wind_down_change, WindDownChange::Add(_)));
        assert!(
            toronto
                .reconciliation
                .add
                .iter()
                .all(|n| n.identifier != w.identifier),
            "the reminder reconcile never names it"
        );

        let held = vec![PendingNotification {
            identifier: w.identifier.clone(),
            revision: Some(w.revision.clone()),
        }];
        let again = schedule(&core, request(held.clone())).await.unwrap();
        assert_eq!(again.wind_down_change, WindDownChange::Keep);
        assert!(
            again.reconciliation.cancel.is_empty(),
            "a held wind-down is not one of the reminders to withdraw"
        );

        core.on_time_zone_changed("America/Vancouver").unwrap();
        let moved = schedule(&core, request(held)).await.unwrap();
        let m = moved.wind_down.clone().unwrap();
        assert_eq!(m.identifier, w.identifier);
        assert_eq!(m.fire_at_ms, 1_780_376_400_000, "22:00 PDT");
        assert!(matches!(moved.wind_down_change, WindDownChange::Replace(_)));

        core.submit(Command::SetPreference {
            key: "notifications.wind_down.enabled".into(),
            value: PrefValue::Bool(false),
            target: PrefTarget::Device,
        })
        .await
        .unwrap();
        let off = schedule(
            &core,
            request(vec![PendingNotification {
                identifier: m.identifier.clone(),
                revision: Some(m.revision),
            }]),
        )
        .await
        .unwrap();
        assert!(off.wind_down.is_none());
        assert_eq!(off.wind_down_change, WindDownChange::Cancel);
        core.shutdown().await;
    }
}
