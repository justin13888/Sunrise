//! One re-plan against a live vault: preferences, plan, Focus, reconcile,
//! and the rest of the catalog beside them.

use std::collections::{BTreeMap, BTreeSet};

use sunrise_core::{Core, CoreError, DeadlineCandidate, Query, QueryResult};
use sunrise_domain::{
    apply_quiet_hours, focus_hold, FocusWindow, NotificationKind, QuietHoursPolicy, ReminderIntent,
    ResolvedPref, UnscheduledReminder,
};
use sunrise_id::{EntityKind, EntityRef};

use super::{
    catalog_policy, identifier_of, plan_catalog, plan_noted, raise_alerts, read_day_schedule,
    reconcile, reconcile_catalog, reconcile_wind_down, reminder_policy, wind_down_notification,
    AlertInputs, CatalogChanges, CatalogInputs, CatalogNotification, CatalogPolicy, ClockStyle,
    NotificationLedger, PendingNotification, PlannedNotification, Reconciliation, ReminderPolicy,
    SyncHealth, WindDownChange, WindDownNotification, HORIZON_MS, MAX_SCHEDULED,
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
    /// What this device has already shown; see [`super::NotificationLedger`].
    /// The updated one comes back in [`Schedule::ledger`], to be kept.
    pub ledger: NotificationLedger,
    /// Whether the OS says the device has a network, for the stall rule.
    pub network_available: bool,
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
    /// The catalog preferences it was made under.
    pub catalog_policy: CatalogPolicy,
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
    /// The scheduled catalog kinds the OS should hold, earliest first:
    /// deadlines, the morning brief or the triage nudge, the evening plan.
    pub catalog: Vec<CatalogNotification>,
    /// What to do to make it so; names only [`super::CATALOG_PREFIX`].
    pub catalog_changes: CatalogChanges,
    /// On-event alerts to post now, each once: hand each to the OS (at its
    /// `fire_at_ms`, which quiet hours may have moved) and never cancel it.
    pub alerts: Vec<CatalogNotification>,
    /// The ledger after this plan. Keep it once the alerts are posted, and
    /// pass it to the next plan.
    pub ledger: NotificationLedger,
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
/// from primary must go quiet, not merely stop adding. Security alerts are
/// the exception to the third: every device posts them.
///
/// At most [`MAX_SCHEDULED`] reminders and scheduled catalog notifications
/// together are left pending, the earliest kept.
///
/// # Errors
///
/// A failed read of the vault.
pub async fn schedule(core: &Core, req: ScheduleRequest) -> Result<Schedule, CoreError> {
    let prefs = read_prefs(core).await?;
    let policy = policy_of(core, &prefs).await?;
    let catalog_policy = catalog_policy(&prefs);
    let zone = core.reader_zone();
    let now = crate::vocab::instant_at(req.now_ms);
    let horizon = crate::vocab::instant_at(req.now_ms.saturating_add(HORIZON_MS));
    let delivering = req.authorized && policy.enabled;
    let sessions = if delivering {
        focus_windows(core).await?
    } else {
        Vec::new()
    };
    let day = read_day_schedule(core, req.now_ms).await?;
    let mut ledger = req.ledger;

    let (desired, unscheduled, catalog) = if delivering {
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
        let (audible, deadlines) = under_focus_filter(
            core,
            req.focus_scope.as_ref(),
            intents,
            core.deadline_candidates()?,
        )
        .await;
        let (held, focus_held) = hold_for_focus(audible, &sessions, &policy, &zone);
        let triage = triage_queue(core).await?;
        let catalog = plan_catalog(
            &policy,
            &catalog_policy,
            &CatalogInputs {
                now,
                horizon,
                tz: &zone,
                schedule: &day,
                deadlines: &deadlines,
                triage: &triage,
                sessions: &sessions,
                clock: req.clock,
            },
            &mut ledger,
        );
        let reminders = plan_noted(&held, &zone, req.clock, MAX_SCHEDULED, &focus_held);
        let (reminders, catalog) = cap(reminders, catalog, MAX_SCHEDULED);
        (reminders, planned.unscheduled, catalog)
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };
    let wind_down = if req.authorized {
        wind_down_notification(&policy, &day, now, &zone)
    } else {
        None
    };
    let alerts = if req.authorized {
        let events = core.security_events()?;
        let sync = sync_health(core, req.network_available).await?;
        let zone_name = if core.zone_unknown() {
            None
        } else {
            zone.iana_name()
        };
        raise_alerts(
            &policy,
            &catalog_policy,
            &AlertInputs {
                now,
                tz: &zone,
                zone: zone_name,
                events: &events,
                sync: Some(sync),
                sessions: &sessions,
            },
            &mut ledger,
        )
    } else {
        Vec::new()
    };

    Ok(Schedule {
        reconciliation: reconcile(&desired, &req.pending),
        wind_down_change: reconcile_wind_down(wind_down.as_ref(), &req.pending),
        catalog_changes: reconcile_catalog(&catalog, &req.pending),
        wind_down,
        desired,
        unscheduled,
        policy,
        catalog_policy,
        zone_unknown: core.zone_unknown(),
        catalog,
        alerts,
        ledger,
    })
}

/// What the OS Focus filter `scope` leaves audible: the reminders and the
/// deadlines whose entity is filed in a picked stream.
async fn under_focus_filter(
    core: &Core,
    scope: Option<&BTreeSet<EntityRef>>,
    intents: Vec<ReminderIntent>,
    deadlines: Vec<DeadlineCandidate>,
) -> (Vec<ReminderIntent>, Vec<DeadlineCandidate>) {
    if scope.is_none() {
        return (intents, deadlines);
    }
    let mut entities: BTreeSet<EntityRef> = intents.iter().map(|i| i.entity).collect();
    entities.extend(deadlines.iter().map(|d| d.task));
    let streams = streams_of(core, entities).await;
    let deadlines = deadlines
        .into_iter()
        .filter(|d| crate::focus::allows(streams.get(&d.task).copied(), scope))
        .collect();
    (crate::focus::muting(intents, scope, &streams), deadlines)
}

/// The earliest `limit` of both lists together, ties broken on the
/// identifier, split back into the two.
fn cap(
    reminders: Vec<PlannedNotification>,
    catalog: Vec<CatalogNotification>,
    limit: usize,
) -> (Vec<PlannedNotification>, Vec<CatalogNotification>) {
    let mut keys: Vec<(i64, String)> = reminders
        .iter()
        .map(|n| (n.fire_at_ms, n.identifier.clone()))
        .chain(catalog.iter().map(|n| (n.fire_at_ms, n.identifier.clone())))
        .collect();
    if keys.len() <= limit {
        return (reminders, catalog);
    }
    keys.sort_unstable();
    keys.truncate(limit);
    let kept: BTreeSet<(i64, String)> = keys.into_iter().collect();
    let keep = |ms: i64, id: &str| kept.contains(&(ms, id.to_owned()));
    (
        reminders
            .into_iter()
            .filter(|n| keep(n.fire_at_ms, &n.identifier))
            .collect(),
        catalog
            .into_iter()
            .filter(|n| keep(n.fire_at_ms, &n.identifier))
            .collect(),
    )
}

/// The reminders a focus session holds, moved to 30 s after it under the
/// quiet-hours rule and then through quiet hours again, and the identifiers
/// of those it moved. One it drops is left out.
fn hold_for_focus(
    intents: Vec<ReminderIntent>,
    sessions: &[FocusWindow],
    policy: &ReminderPolicy,
    tz: &jiff::tz::TimeZone,
) -> (Vec<ReminderIntent>, BTreeSet<String>) {
    let quiet = policy.settings.quiet_hours.as_ref();
    let rule = quiet.map_or(QuietHoursPolicy::Queue, |q| q.policy);
    let mut held = BTreeSet::new();
    let kept = intents
        .into_iter()
        .filter_map(|mut i| {
            let kind = NotificationKind::of_reminder(i.kind);
            let at = focus_hold(i.fire_at, kind, sessions, rule)?;
            if at == i.fire_at {
                return Some(i);
            }
            let at = apply_quiet_hours(&at.to_zoned(tz.clone()), quiet)?;
            i.deferred_from = Some(i.deferred_from.unwrap_or(i.fire_at));
            i.fire_at = at;
            held.insert(identifier_of(&i));
            Some(i)
        })
        .collect();
    (kept, held)
}

/// The focus sessions running, as the notification rules read them.
async fn focus_windows(core: &Core) -> Result<Vec<FocusWindow>, CoreError> {
    let QueryResult::FocusSessions(rows) = core.query(Query::RunningFocusSessions).await? else {
        return Err(unexpected("running focus sessions"));
    };
    Ok(rows
        .iter()
        .map(|r| {
            let start = r.session.start.started_at;
            FocusWindow {
                start,
                end: r.session.start.planned_ms.map(|ms| {
                    start + jiff::SignedDuration::from_millis(i64::try_from(ms).unwrap_or(0))
                }),
            }
        })
        .collect())
}

/// The triage queue's task ids, in its order.
async fn triage_queue(core: &Core) -> Result<Vec<EntityRef>, CoreError> {
    let QueryResult::Triage(rows) = core.query(Query::Triage).await? else {
        return Err(unexpected("triage"));
    };
    Ok(rows.iter().map(|r| r.task.id).collect())
}

/// Sync on this device, for the stall rule.
async fn sync_health(core: &Core, network_available: bool) -> Result<SyncHealth, CoreError> {
    let QueryResult::SyncStatus(status) = core.query(Query::SyncStatus).await? else {
        return Err(unexpected("sync status"));
    };
    Ok(SyncHealth {
        state: status.state,
        oldest_unacked_ms: core.oldest_unacked_ms()?,
        network_available,
    })
}

async fn read_prefs(core: &Core) -> Result<Vec<ResolvedPref>, CoreError> {
    let QueryResult::Preferences(prefs) = core.query(Query::Preferences).await? else {
        return Err(unexpected("preferences"));
    };
    Ok(prefs)
}

async fn policy_of(core: &Core, prefs: &[ResolvedPref]) -> Result<ReminderPolicy, CoreError> {
    let this_device = EntityRef::new(EntityKind::Device, core.device_id());
    let QueryResult::Devices(devices) = core.query(Query::DeviceList).await? else {
        return Err(unexpected("device list"));
    };
    let revoked: Vec<EntityRef> = devices
        .iter()
        .filter(|d| d.revoked)
        .map(|d| EntityRef::new(EntityKind::Device, d.device_id))
        .collect();
    Ok(reminder_policy(prefs, this_device, &revoked))
}

/// This device's [`ReminderPolicy`], from the vault's preferences and the
/// devices it calls revoked.
///
/// # Errors
///
/// A failed read of the vault.
pub async fn read_policy(core: &Core) -> Result<ReminderPolicy, CoreError> {
    let prefs = read_prefs(core).await?;
    policy_of(core, &prefs).await
}

/// The catalog's preferences on this device: each kind's toggle, the
/// deadline's lead and the briefs' offsets.
///
/// # Errors
///
/// A failed read of the vault.
pub async fn read_catalog_policy(core: &Core) -> Result<CatalogPolicy, CoreError> {
    Ok(catalog_policy(&read_prefs(core).await?))
}

/// The stream each entity is filed in: a task's own, a routine's template's,
/// a block's own, and the legacy Inbox reference for none
/// ([`sunrise_domain::stream_key`]). A read that fails leaves the entity
/// out, which [`crate::focus::allows`] lets through. Read only while a Focus
/// filter is on, for a day's reminders and the open deadlines.
async fn streams_of(core: &Core, entities: BTreeSet<EntityRef>) -> BTreeMap<EntityRef, EntityRef> {
    let mut streams = BTreeMap::new();
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
mod tests;
