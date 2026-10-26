//! Reminder scheduling across the seam: [`sunrise_client_core::reminders`]'s
//! `UniFFI` mirror, and the core's reminder plan and zone flag.
//!
//! The planner, the reconcile and the notification preferences are the
//! client layer's. Every body here converts a record and delegates, so the
//! Apple scheduler and any later client cap, order, word and identify a
//! reminder the same way.
//!
//! The usual path is one call: [`SunriseCore::schedule_reminders`] reads the
//! preferences, plans, applies the Focus filter and reconciles against what
//! the OS holds. The pieces are exported too, for a client that already has
//! the intents in hand.

use sunrise_client_core::reminders as plan;
use sunrise_domain::{ReminderIntent, ReminderKind, SnoozeSpan};
use sunrise_id::EntityRef;

use crate::dto::{NotificationSettings, QuietWindow, Reminder};
use crate::editing::zone;
use crate::{BindingError, SunriseCore};

/// See [`sunrise_client_core::reminders::ReminderCategory`]: which action
/// buttons a reminder carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ReminderCategoryKind {
    /// A task or a routine occurrence: done, snooze, open.
    Task,
    /// A time block: open only.
    Block,
}

impl From<plan::ReminderCategory> for ReminderCategoryKind {
    fn from(c: plan::ReminderCategory) -> Self {
        match c {
            plan::ReminderCategory::Task => Self::Task,
            plan::ReminderCategory::Block => Self::Block,
        }
    }
}

impl From<ReminderCategoryKind> for plan::ReminderCategory {
    fn from(c: ReminderCategoryKind) -> Self {
        match c {
            ReminderCategoryKind::Task => Self::Task,
            ReminderCategoryKind::Block => Self::Block,
        }
    }
}

/// See [`sunrise_client_core::reminders::ClockStyle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ClockStyle {
    /// `6:30 AM`.
    H12,
    /// `06:30`.
    H24,
}

impl From<ClockStyle> for plan::ClockStyle {
    fn from(c: ClockStyle) -> Self {
        match c {
            ClockStyle::H12 => Self::H12,
            ClockStyle::H24 => Self::H24,
        }
    }
}

impl From<plan::ClockStyle> for ClockStyle {
    fn from(c: plan::ClockStyle) -> Self {
        match c {
            plan::ClockStyle::H12 => Self::H12,
            plan::ClockStyle::H24 => Self::H24,
        }
    }
}

/// See [`sunrise_client_core::reminders::PlannedNotification`]: one local
/// notification the OS should hold.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReminderNotification {
    /// Stable across re-plans: `(entity, rule)`, never the fire time.
    pub identifier: String,
    /// Changes when anything shown or fired on changes. Store it with the
    /// request (in its `userInfo`) and hand it back in [`PendingReminder`].
    pub revision: String,
    /// The title.
    pub title: String,
    /// The line under it; empty when the title says everything.
    pub body: String,
    /// Which buttons it carries.
    pub category: ReminderCategoryKind,
    /// The source.
    pub kind: ReminderKind,
    /// What a button acts on and a tap opens.
    pub entity: EntityRef,
    /// The absolute instant to fire at, epoch ms. Arm a trigger that stays at
    /// this instant across a zone change (a calendar trigger whose components
    /// carry their zone); a floating reminder follows the zone by re-planning.
    pub fire_at_ms: i64,
    /// When quiet hours moved it, the instant it was due, epoch ms — what a
    /// localized body says beside "held for quiet hours".
    pub deferred_from_ms: Option<i64>,
}

impl From<plan::PlannedNotification> for ReminderNotification {
    fn from(n: plan::PlannedNotification) -> Self {
        let plan::PlannedNotification {
            identifier,
            revision,
            title,
            body,
            category,
            kind,
            entity,
            fire_at_ms,
            deferred_from_ms,
        } = n;
        Self {
            identifier,
            revision,
            title,
            body,
            category: category.into(),
            kind,
            entity,
            fire_at_ms,
            deferred_from_ms,
        }
    }
}

impl From<ReminderNotification> for plan::PlannedNotification {
    fn from(n: ReminderNotification) -> Self {
        Self {
            identifier: n.identifier,
            revision: n.revision,
            title: n.title,
            body: n.body,
            category: n.category.into(),
            kind: n.kind,
            entity: n.entity,
            fire_at_ms: n.fire_at_ms,
            deferred_from_ms: n.deferred_from_ms,
        }
    }
}

/// See [`sunrise_client_core::reminders::PendingNotification`]: a request
/// the OS holds, read back.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PendingReminder {
    /// Its identifier.
    pub identifier: String,
    /// The revision it was scheduled with; absent is always replaced.
    pub revision: Option<String>,
}

impl From<PendingReminder> for plan::PendingNotification {
    fn from(p: PendingReminder) -> Self {
        Self {
            identifier: p.identifier,
            revision: p.revision,
        }
    }
}

/// See [`sunrise_client_core::reminders::Reconciliation`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReminderChanges {
    /// Not held yet: schedule each.
    pub add: Vec<ReminderNotification>,
    /// Held with another revision: schedule each again, replacing it.
    pub replace: Vec<ReminderNotification>,
    /// Ours and no longer wanted: withdraw each identifier.
    pub cancel: Vec<String>,
    /// Held and correct: leave alone.
    pub kept: Vec<String>,
}

impl From<plan::Reconciliation> for ReminderChanges {
    fn from(r: plan::Reconciliation) -> Self {
        Self {
            add: r.add.into_iter().map(Into::into).collect(),
            replace: r.replace.into_iter().map(Into::into).collect(),
            cancel: r.cancel,
            kept: r.kept,
        }
    }
}

/// See [`sunrise_domain::UnscheduledReminder`]: a reminder whose time has no
/// instant for this reader, so nothing was scheduled for it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UnscheduledReminderItem {
    /// What it is about.
    pub entity: EntityRef,
    /// Which source.
    pub kind: ReminderKind,
    /// Why its time has no instant, as a sentence.
    pub reason: String,
}

impl From<&sunrise_domain::UnscheduledReminder> for UnscheduledReminderItem {
    fn from(u: &sunrise_domain::UnscheduledReminder) -> Self {
        Self {
            entity: u.entity,
            kind: u.kind,
            reason: u.reason.to_string(),
        }
    }
}

/// See [`sunrise_core::Core::reminder_plan`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct ReminderPlanReport {
    /// What to schedule, earliest first.
    pub reminders: Vec<Reminder>,
    /// What could not be placed, and why.
    pub unscheduled: Vec<UnscheduledReminderItem>,
}

/// See [`sunrise_client_core::reminders::ReminderPolicy`]: this device's
/// notification preferences, read off the core's `Preferences`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ReminderPolicyItem {
    /// `notifications.enabled`, the master switch on this install.
    pub enabled: bool,
    /// What the planner takes: lead time, quiet window, primary or not.
    pub settings: NotificationSettings,
    /// `notifications.task_reminder.enabled`.
    pub task_enabled: bool,
    /// `notifications.block_start.enabled`.
    pub block_enabled: bool,
    /// `notifications.routine_due.enabled`.
    pub routine_enabled: bool,
    /// The primary register is unset and reminders are on here: call
    /// [`SunriseCore::claim_primary_reminder_device`].
    pub claim_primary: bool,
}

impl From<&plan::ReminderPolicy> for ReminderPolicyItem {
    fn from(p: &plan::ReminderPolicy) -> Self {
        let s = &p.settings;
        Self {
            enabled: p.enabled,
            settings: NotificationSettings {
                default_lead_s: s.default_lead_s,
                quiet_hours: s.quiet_hours.as_ref().map(|q| QuietWindow {
                    start: q.start,
                    end: q.end,
                    policy: q.policy,
                }),
                is_primary_device: s.is_primary_device,
            },
            task_enabled: p.task_enabled,
            block_enabled: p.block_enabled,
            routine_enabled: p.routine_enabled,
            claim_primary: p.claim_primary,
        }
    }
}

/// See [`sunrise_client_core::reminders::Schedule`]: one re-plan's answer.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ReminderSchedule {
    /// What the OS should hold, earliest first.
    pub desired: Vec<ReminderNotification>,
    /// What to change to make it so.
    pub changes: ReminderChanges,
    /// Reminders that could not be placed, each with why.
    pub unscheduled: Vec<UnscheduledReminderItem>,
    /// The preferences the plan was made under.
    pub policy: ReminderPolicyItem,
    /// The reader's zone is a stand-in: report the OS zone
    /// ([`SunriseCore::on_time_zone_changed`]) and schedule again.
    pub zone_unknown: bool,
}

/// What a category offers, for registering it with the OS.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReminderCategoryInfo {
    /// The identifier to register it under.
    pub identifier: String,
    /// The snooze buttons, in order.
    pub snooze_spans: Vec<SnoozeSpan>,
    /// Whether it offers Mark Done.
    pub offers_completion: bool,
}

/// The planner's fixed numbers.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReminderLimits {
    /// Every reminder identifier starts with it; nothing else is cancelled.
    pub identifier_prefix: String,
    /// How far ahead to schedule, ms.
    pub horizon_ms: u64,
    /// The most reminders left pending.
    pub max_scheduled: u32,
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// What `Query::ReminderIntents` schedules, with the reminders it could
    /// not place and why (`docs/11-adr/0066-times-survive-travel.md` §3).
    /// Planned in the reader's zone at the moment of the call.
    pub fn reminder_plan(
        &self,
        now_ms: u64,
        horizon_ms: u64,
        settings: NotificationSettings,
    ) -> Result<ReminderPlanReport, BindingError> {
        let p = self
            .inner
            .reminder_plan(now_ms, horizon_ms, &settings.into())?;
        Ok(ReminderPlanReport {
            reminders: p.intents.iter().map(Reminder::from).collect(),
            unscheduled: p.unscheduled.iter().map(Into::into).collect(),
        })
    }

    /// Whether the reader's zone is a stand-in, so times read in UTC. A
    /// client that sees it reports the OS zone through
    /// [`SunriseCore::on_time_zone_changed`].
    pub fn zone_unknown(&self) -> bool {
        self.inner.zone_unknown()
    }

    /// Re-plan this device's reminders and say what the OS must change. See
    /// [`sunrise_client_core::reminders::schedule`].
    ///
    /// `authorized` is whether the OS lets the app schedule; `focus_scope`
    /// the streams the Focus filter in force picked, empty for none;
    /// `pending` what the OS holds now. Call it on every change batch, every
    /// zone change and every Focus change: an unchanged plan comes back with
    /// nothing to do.
    pub async fn schedule_reminders(
        &self,
        now_ms: u64,
        authorized: bool,
        focus_scope: Vec<EntityRef>,
        pending: Vec<PendingReminder>,
        clock: ClockStyle,
    ) -> Result<ReminderSchedule, BindingError> {
        let s = plan::schedule(
            &self.inner,
            plan::ScheduleRequest {
                now_ms,
                authorized,
                focus_scope: sunrise_client_core::focus::scope(focus_scope),
                pending: pending.into_iter().map(Into::into).collect(),
                clock: clock.into(),
                // This call returns no alert, so it keeps no ledger:
                // [`SunriseCore::schedule_notifications`] is the one that does.
                ledger: plan::NotificationLedger::new(),
                network_available: true,
            },
        )
        .await?;
        Ok(ReminderSchedule {
            desired: s.desired.into_iter().map(Into::into).collect(),
            changes: s.reconciliation.into(),
            unscheduled: s.unscheduled.iter().map(Into::into).collect(),
            policy: (&s.policy).into(),
            zone_unknown: s.zone_unknown,
        })
    }

    /// This device's notification preferences, as the scheduler reads them —
    /// for a Settings screen.
    pub async fn reminder_policy(&self) -> Result<ReminderPolicyItem, BindingError> {
        Ok((&plan::read_policy(&self.inner).await?).into())
    }

    /// Make this device the account's primary reminder device: write its id
    /// to `notifications.primary_device`. Done when
    /// [`ReminderPolicyItem::claim_primary`] says so, and when the user picks
    /// this device in Settings.
    pub async fn claim_primary_reminder_device(&self) -> Result<(), BindingError> {
        let this = EntityRef::new(sunrise_id::EntityKind::Device, self.inner.device_id());
        let (key, value, target) = plan::primary_claim(this);
        self.inner
            .submit(sunrise_core::Command::SetPreference {
                key: key.to_owned(),
                value,
                target,
            })
            .await?;
        Ok(())
    }
}

impl From<Reminder> for ReminderIntent {
    fn from(r: Reminder) -> Self {
        Self {
            entity: r.entity,
            kind: r.kind,
            fire_at: r.fire_at,
            title: r.title,
            deferred_from: r.deferred_from,
        }
    }
}

/// The identifier the reminder `(entity, kind)` is held under. Never the fire
/// time: a reminder that moves keeps it.
#[uniffi::export]
#[must_use]
pub fn reminder_identifier(entity: EntityRef, kind: ReminderKind) -> String {
    plan::identifier(entity, kind)
}

/// Everything to hand the OS for `reminders`: earliest first, one per
/// identifier, at most `limit`, worded with times read in `tz`.
///
/// # Errors
///
/// [`BindingError::BadTime`] for a zone the tzdb does not know — a body that
/// says "due 06:30" in a stand-in zone would be wrong without saying so.
#[uniffi::export]
pub fn plan_reminder_notifications(
    reminders: Vec<Reminder>,
    tz: String,
    clock: ClockStyle,
    limit: u32,
) -> Result<Vec<ReminderNotification>, BindingError> {
    let zone = zone(&tz)?;
    let intents: Vec<ReminderIntent> = reminders.into_iter().map(Into::into).collect();
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    Ok(plan::plan(&intents, &zone, clock.into(), limit)
        .into_iter()
        .map(Into::into)
        .collect())
}

/// Desired against held: what to add, replace, cancel and leave. Only
/// identifiers under the reminder prefix are ever cancelled.
#[uniffi::export]
#[must_use]
pub fn reconcile_reminders(
    desired: Vec<ReminderNotification>,
    pending: Vec<PendingReminder>,
) -> ReminderChanges {
    let desired: Vec<plan::PlannedNotification> = desired.into_iter().map(Into::into).collect();
    let pending: Vec<plan::PendingNotification> = pending.into_iter().map(Into::into).collect();
    plan::reconcile(&desired, &pending).into()
}

/// What `category` offers, for registering it with the OS.
#[uniffi::export]
#[must_use]
pub fn reminder_category_info(category: ReminderCategoryKind) -> ReminderCategoryInfo {
    let c = plan::ReminderCategory::from(category);
    ReminderCategoryInfo {
        identifier: c.identifier().to_owned(),
        snooze_spans: c.snooze_spans().to_vec(),
        offers_completion: c.offers_completion(),
    }
}

/// The planner's fixed numbers.
#[uniffi::export]
#[must_use]
pub fn reminder_limits() -> ReminderLimits {
    ReminderLimits {
        identifier_prefix: plan::IDENTIFIER_PREFIX.to_owned(),
        horizon_ms: plan::HORIZON_MS,
        max_scheduled: u32::try_from(plan::MAX_SCHEDULED).unwrap_or(u32::MAX),
    }
}

/// The clock style for a `time_format` preference value, with
/// `locale_uses_24h` answering `"locale"`.
#[uniffi::export]
#[must_use]
pub fn clock_style(time_format: Option<String>, locale_uses_24h: bool) -> ClockStyle {
    plan::ClockStyle::from_time_format(time_format.as_deref(), locale_uses_24h).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn reminder(n: u8, ms: i64) -> Reminder {
        Reminder {
            entity: EntityRef::new(EntityKind::Task, [n; 16]),
            kind: ReminderKind::Task,
            fire_at: jiff::Timestamp::from_millisecond(ms).unwrap(),
            title: format!("task {n}"),
            deferred_from: None,
        }
    }

    /// Plan, hold, re-plan: the second pass across the seam does nothing.
    #[test]
    fn a_plan_round_trips_the_seam_and_reconciles_to_nothing() {
        let planned = plan_reminder_notifications(
            vec![reminder(2, 2_000), reminder(1, 1_000)],
            "UTC".into(),
            ClockStyle::H24,
            56,
        )
        .unwrap();
        assert_eq!(planned[0].fire_at_ms, 1_000);
        let first = reconcile_reminders(planned.clone(), vec![]);
        assert_eq!(first.add.len(), 2);
        let held = planned
            .iter()
            .map(|n| PendingReminder {
                identifier: n.identifier.clone(),
                revision: Some(n.revision.clone()),
            })
            .collect();
        let second = reconcile_reminders(planned.clone(), held);
        assert!(second.add.is_empty() && second.replace.is_empty() && second.cancel.is_empty());
        assert_eq!(
            planned[0].identifier,
            reminder_identifier(
                EntityRef::new(EntityKind::Task, [1; 16]),
                ReminderKind::Task
            )
        );
    }

    #[test]
    fn an_unknown_zone_is_refused_rather_than_read_as_utc() {
        let err = plan_reminder_notifications(vec![], "Not/AZone".into(), ClockStyle::H24, 1)
            .unwrap_err();
        assert!(matches!(err, BindingError::BadTime { .. }), "{err}");
    }

    #[test]
    fn the_categories_and_limits_are_the_client_layers() {
        let task = reminder_category_info(ReminderCategoryKind::Task);
        assert_eq!(task.identifier, "sunrise.reminder.task");
        assert_eq!(
            task.snooze_spans,
            [SnoozeSpan::OneHour, SnoozeSpan::Tomorrow]
        );
        assert!(!reminder_category_info(ReminderCategoryKind::Block).offers_completion);
        assert_eq!(reminder_limits().max_scheduled, 56);
        assert_eq!(clock_style(Some("h24".into()), false), ClockStyle::H24);
    }
}
