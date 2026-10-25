//! The notification catalog across the seam (`docs/08-features/notifications.md`,
//! #347): [`sunrise_client_core::reminders`]'s catalog kinds, alerts and
//! ledger, and the catalog itself as data.
//!
//! Additive to [`crate::reminders`]: nothing there changed shape, and no enum
//! a client switches over gained a case. The catalog's kinds are a new enum,
//! [`NotificationKind`], carried by a new record, [`CatalogNotificationItem`],
//! and [`SunriseCore::schedule_notifications`] returns the old
//! [`ReminderSchedule`] beside them.

use sunrise_client_core::reminders as plan;
use sunrise_domain::{
    Delivery, InterruptionLevel, NotificationAction, NotificationAudience, NotificationKind,
};
use sunrise_id::EntityRef;

use crate::reminders::{ClockStyle, PendingReminder, ReminderSchedule};
use crate::{BindingError, SunriseCore};

/// See [`sunrise_domain::NotificationKind`]: one kind of the catalog, in
/// catalog order.
#[uniffi::remote(Enum)]
pub enum NotificationKind {
    TaskReminder,
    Deadline,
    RoutineDue,
    BlockStart,
    MorningBrief,
    TriageNudge,
    EveningPlan,
    WindDown,
    FocusEnd,
    TimezoneChanged,
    WeeklyReview,
    SyncStalled,
    DevicePaired,
    DeviceRevoked,
    RecoveryCode,
    CalendarReconnect,
    SharedChange,
    PlaceArrival,
}

/// See [`sunrise_domain::InterruptionLevel`].
#[uniffi::remote(Enum)]
pub enum InterruptionLevel {
    Passive,
    Active,
    TimeSensitive,
}

/// See [`sunrise_domain::Delivery`].
#[uniffi::remote(Enum)]
pub enum Delivery {
    Scheduled,
    OnEvent,
}

/// See [`sunrise_domain::NotificationAudience`].
#[uniffi::remote(Enum)]
pub enum NotificationAudience {
    Primary,
    EveryDevice,
    ThisDevice,
}

/// See [`sunrise_domain::NotificationAction`].
#[uniffi::remote(Enum)]
pub enum NotificationAction {
    MarkDone,
    SnoozeOneHour,
    SnoozeTomorrow,
    SnoozeFiveMinutes,
    SnoozeFifteenMinutes,
    Defer,
    Skip,
    StartFocus,
    PlanMyDay,
    OpenTriage,
    MoveOpenToTomorrow,
    Open,
    SeeTomorrow,
    AddFiveMinutes,
    StartBreak,
    ReviewTimes,
    Keep,
    StartReview,
    OpenSyncStatus,
    ReviewDevices,
    ReviewSecurity,
    Reconnect,
    OpenHereNow,
}

/// One row of the catalog, as data: what a Settings screen and a category
/// registration read.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NotificationKindInfo {
    /// The kind.
    pub kind: NotificationKind,
    /// Its name in keys and identifiers: `morning_brief`.
    pub tag: String,
    /// The key that turns it off; absent for a security event.
    pub enabled_key: Option<String>,
    /// Whether it is on before its key is set.
    pub default_enabled: bool,
    /// How loud it is.
    pub level: InterruptionLevel,
    /// Scheduled ahead, or posted on an event.
    pub delivery: Delivery,
    /// Which devices show it.
    pub audience: NotificationAudience,
    /// Its buttons, in order.
    pub actions: Vec<NotificationAction>,
    /// Whether an in-app focus session lets it through.
    pub passes_focus: bool,
    /// Whether quiet hours hold it.
    pub held_by_quiet_hours: bool,
}

/// Every kind of the catalog, in catalog order.
#[uniffi::export]
#[must_use]
pub fn notification_catalog() -> Vec<NotificationKindInfo> {
    NotificationKind::ALL
        .into_iter()
        .map(|kind| NotificationKindInfo {
            kind,
            tag: kind.tag().to_owned(),
            enabled_key: kind.enabled_key().map(str::to_owned),
            default_enabled: kind.default_enabled(),
            level: kind.level(),
            delivery: kind.delivery(),
            audience: kind.audience(),
            actions: kind.actions().to_vec(),
            passes_focus: kind.passes_focus(),
            held_by_quiet_hours: kind.held_by_quiet_hours(),
        })
        .collect()
}

/// The two identifier namespaces of the catalog.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NotificationPrefixes {
    /// Scheduled catalog kinds; [`CatalogChangesItem::cancel`] names only these.
    pub catalog: String,
    /// Posted alerts; nothing cancels these.
    pub alert: String,
}

/// The catalog's identifier namespaces.
#[uniffi::export]
#[must_use]
pub fn notification_prefixes() -> NotificationPrefixes {
    NotificationPrefixes {
        catalog: plan::CATALOG_PREFIX.to_owned(),
        alert: plan::ALERT_PREFIX.to_owned(),
    }
}

/// See [`sunrise_client_core::reminders::CatalogNotification`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CatalogNotificationItem {
    /// Stable across re-plans.
    pub identifier: String,
    /// Changes when anything shown or fired on changes; store it with the
    /// request and hand it back in [`PendingReminder`].
    pub revision: String,
    /// Which kind.
    pub kind: NotificationKind,
    /// The title, in English.
    pub title: String,
    /// The line under it, in English.
    pub body: String,
    /// The task or device it is about.
    pub entity: Option<EntityRef>,
    /// A name it carries: a device's nickname, a zone.
    pub subject: Option<String>,
    /// A number it carries: the triage queue's length.
    pub count: Option<u32>,
    /// What a tap opens.
    pub link: String,
    /// Its buttons, in order.
    pub actions: Vec<NotificationAction>,
    /// How loud.
    pub level: InterruptionLevel,
    /// The absolute instant to fire at, epoch ms.
    pub fire_at_ms: i64,
    /// When focus or quiet hours moved it, the instant it was due.
    pub deferred_from_ms: Option<i64>,
}

impl From<plan::CatalogNotification> for CatalogNotificationItem {
    fn from(n: plan::CatalogNotification) -> Self {
        Self {
            identifier: n.identifier,
            revision: n.revision,
            kind: n.kind,
            title: n.title,
            body: n.body,
            entity: n.entity,
            subject: n.subject,
            count: n.count,
            link: n.link,
            actions: n.actions,
            level: n.level,
            fire_at_ms: n.fire_at_ms,
            deferred_from_ms: n.deferred_from_ms,
        }
    }
}

/// See [`sunrise_client_core::reminders::CatalogChanges`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CatalogChangesItem {
    /// Not held yet: schedule each.
    pub add: Vec<CatalogNotificationItem>,
    /// Held with another revision: schedule each again.
    pub replace: Vec<CatalogNotificationItem>,
    /// Held under the catalog prefix and no longer wanted.
    pub cancel: Vec<String>,
    /// Held and right.
    pub kept: Vec<String>,
}

impl From<plan::CatalogChanges> for CatalogChangesItem {
    fn from(c: plan::CatalogChanges) -> Self {
        Self {
            add: c.add.into_iter().map(Into::into).collect(),
            replace: c.replace.into_iter().map(Into::into).collect(),
            cancel: c.cancel,
            kept: c.kept,
        }
    }
}

/// One kind's toggle, resolved for this device.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CatalogToggle {
    /// The kind.
    pub kind: NotificationKind,
    /// Whether it is on. A security event always is.
    pub enabled: bool,
}

/// See [`sunrise_client_core::reminders::CatalogPolicy`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CatalogPolicyItem {
    /// Every kind in catalog order, with whether it is on.
    pub toggles: Vec<CatalogToggle>,
    /// `notifications.deadline.lead_s`.
    pub deadline_lead_s: u32,
    /// `notifications.morning_brief.offset_s`.
    pub morning_offset_s: i32,
    /// `notifications.evening_plan.offset_s`.
    pub evening_offset_s: i32,
}

impl From<&plan::CatalogPolicy> for CatalogPolicyItem {
    fn from(p: &plan::CatalogPolicy) -> Self {
        Self {
            toggles: plan::catalog_entries(p)
                .into_iter()
                .map(|e| CatalogToggle {
                    kind: e.kind,
                    enabled: e.enabled,
                })
                .collect(),
            deadline_lead_s: p.deadline_lead_s,
            morning_offset_s: p.morning_offset_s,
            evening_offset_s: p.evening_offset_s,
        }
    }
}

/// See [`sunrise_client_core::reminders::schedule`]: one re-plan of every
/// kind this build plans.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NotificationSchedule {
    /// The reminders, as [`SunriseCore::schedule_reminders`] returns them.
    pub reminders: ReminderSchedule,
    /// The scheduled catalog kinds the OS should hold, earliest first.
    pub catalog: Vec<CatalogNotificationItem>,
    /// What to change for them.
    pub catalog_changes: CatalogChangesItem,
    /// Alerts to post now, each once, never cancelled.
    pub alerts: Vec<CatalogNotificationItem>,
    /// The catalog preferences the plan was made under.
    pub catalog_policy: CatalogPolicyItem,
    /// The ledger after this plan: keep it on this device once the alerts are
    /// posted, and pass it to the next call.
    pub ledger_json: String,
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Re-plan every kind: the reminders, the deadline, the briefs and the
    /// triage nudge, and the alerts to post. `ledger_json` is what the last
    /// call returned, absent the first time; `network_available` is the OS's
    /// answer, for the sync-stalled rule. The other arguments are
    /// [`SunriseCore::schedule_reminders`]'s.
    #[allow(clippy::too_many_arguments)]
    pub async fn schedule_notifications(
        &self,
        now_ms: u64,
        authorized: bool,
        focus_scope: Vec<EntityRef>,
        pending: Vec<PendingReminder>,
        clock: ClockStyle,
        ledger_json: Option<String>,
        network_available: bool,
    ) -> Result<NotificationSchedule, BindingError> {
        let s = plan::schedule(
            &self.inner,
            plan::ScheduleRequest {
                now_ms,
                authorized,
                focus_scope: sunrise_client_core::focus::scope(focus_scope),
                pending: pending.into_iter().map(Into::into).collect(),
                clock: clock.into(),
                ledger: plan::NotificationLedger::from_json(ledger_json.as_deref()),
                network_available,
            },
        )
        .await?;
        Ok(NotificationSchedule {
            catalog: s.catalog.into_iter().map(Into::into).collect(),
            catalog_changes: s.catalog_changes.into(),
            alerts: s.alerts.into_iter().map(Into::into).collect(),
            catalog_policy: (&s.catalog_policy).into(),
            ledger_json: s.ledger.to_json(),
            reminders: ReminderSchedule {
                desired: s.desired.into_iter().map(Into::into).collect(),
                changes: s.reconciliation.into(),
                unscheduled: s.unscheduled.iter().map(Into::into).collect(),
                policy: (&s.policy).into(),
                zone_unknown: s.zone_unknown,
            },
        })
    }

    /// This device's catalog preferences, for Settings → Notifications.
    pub async fn notification_policy(&self) -> Result<CatalogPolicyItem, BindingError> {
        Ok((&plan::read_catalog_policy(&self.inner).await?).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_crosses_in_catalog_order_with_its_rules() {
        let all = notification_catalog();
        assert_eq!(all.len(), 18);
        assert_eq!(all[0].kind, NotificationKind::TaskReminder);
        let paired = all
            .iter()
            .find(|k| k.kind == NotificationKind::DevicePaired)
            .unwrap();
        assert_eq!(paired.enabled_key, None);
        assert_eq!(paired.audience, NotificationAudience::EveryDevice);
        assert!(paired.passes_focus && !paired.held_by_quiet_hours);
        let tz = all.iter().find(|k| k.tag == "timezone_changed").unwrap();
        assert!(!tz.default_enabled);
        assert_eq!(tz.delivery, Delivery::OnEvent);
        let p = notification_prefixes();
        assert!(p.catalog.starts_with("sunrise.") && p.alert.starts_with("sunrise."));
        assert_ne!(p.catalog, p.alert);
    }
}
