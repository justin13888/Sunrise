//! Notifications: reminders, quiet hours and the morning summary.

use sunrise_domain::{
    EndOfDayPlan, MorningSummary, QuietHoursPolicy, ReminderIntent, ReminderKind, ReminderSettings,
};
use sunrise_id::EntityRef;

use super::TaskItem;

/// See [`sunrise_domain::MorningSummary`]: the morning notification's view.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MorningReport {
    /// Start of the previous civil day in the device's zone.
    pub since: jiff::Timestamp,
    /// Start of today, so a client can split `completed` into yesterday and
    /// already-today without a second query.
    pub today_start: jiff::Timestamp,
    /// Completed at or after `since`, newest first.
    pub completed: Vec<TaskItem>,
    /// Open, unfiled Inbox tasks: the triage queue.
    pub to_triage: Vec<TaskItem>,
    /// Open tasks landing today or earlier, earliest first.
    pub due_today: Vec<TaskItem>,
}

impl From<&MorningSummary> for MorningReport {
    fn from(s: &MorningSummary) -> Self {
        let MorningSummary {
            since,
            today_start,
            completed,
            to_triage,
            due_today,
        } = s;
        Self {
            since: *since,
            today_start: *today_start,
            completed: completed.iter().map(TaskItem::from).collect(),
            to_triage: to_triage.iter().map(TaskItem::from).collect(),
            due_today: due_today.iter().map(TaskItem::from).collect(),
        }
    }
}

/// See [`sunrise_domain::EndOfDayPlan`]: the evening notification's view.
#[derive(Debug, Clone, uniffi::Record)]
pub struct EveningReport {
    /// Start of today in the device's zone.
    pub day_start: jiff::Timestamp,
    /// Start of tomorrow.
    pub day_end: jiff::Timestamp,
    /// End of the seven civil days after today.
    pub week_end: jiff::Timestamp,
    /// Still open and landing today or earlier, overdue included.
    pub still_open: Vec<TaskItem>,
    /// Open and landing in the week ahead.
    pub week_ahead: Vec<TaskItem>,
    /// Open with neither a scheduled time nor a deadline.
    pub unscheduled: Vec<TaskItem>,
}

impl From<&EndOfDayPlan> for EveningReport {
    fn from(p: &EndOfDayPlan) -> Self {
        let EndOfDayPlan {
            day_start,
            day_end,
            week_end,
            still_open,
            week_ahead,
            unscheduled,
        } = p;
        Self {
            day_start: *day_start,
            day_end: *day_end,
            week_end: *week_end,
            still_open: still_open.iter().map(TaskItem::from).collect(),
            week_ahead: week_ahead.iter().map(TaskItem::from).collect(),
            unscheduled: unscheduled.iter().map(TaskItem::from).collect(),
        }
    }
}

/// See [`sunrise_domain::ReminderIntent`]: one local notification to schedule.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Reminder {
    /// What it is about, and what a deep link opens.
    pub entity: EntityRef,
    /// Which source produced it.
    pub kind: ReminderKind,
    /// When to fire, after lead time and quiet hours.
    pub fire_at: jiff::Timestamp,
    /// The title to render — already plaintext, computed on-device.
    pub title: String,
    /// What it would have fired at, when quiet hours moved it.
    pub deferred_from: Option<jiff::Timestamp>,
}

impl From<&ReminderIntent> for Reminder {
    fn from(i: &ReminderIntent) -> Self {
        let ReminderIntent {
            entity,
            kind,
            fire_at,
            title,
            deferred_from,
        } = i;
        Self {
            entity: *entity,
            kind: *kind,
            fire_at: *fire_at,
            title: title.clone(),
            deferred_from: *deferred_from,
        }
    }
}

/// See [`sunrise_domain::QuietHours`]. Times are local wall clock; `end`
/// earlier than `start` wraps midnight, and `start == end` silences nothing.
#[derive(Debug, Clone, uniffi::Record)]
pub struct QuietWindow {
    /// Local start.
    pub start: jiff::civil::Time,
    /// Local end.
    pub end: jiff::civil::Time,
    /// Queue to the end of the window, or drop.
    pub policy: QuietHoursPolicy,
}

/// See [`sunrise_domain::ReminderSettings`]: the per-device half, which is
/// never synced and so arrives with the query.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NotificationSettings {
    /// Global lead time in seconds — the floor of the hierarchy.
    pub default_lead_s: u32,
    /// Quiet window; absent means never quiet.
    pub quiet_hours: Option<QuietWindow>,
    /// Whether this device is the account's primary reminder device. A
    /// non-primary device is handed nothing to schedule.
    pub is_primary_device: bool,
}

impl From<NotificationSettings> for ReminderSettings {
    fn from(s: NotificationSettings) -> Self {
        Self {
            default_lead_s: s.default_lead_s,
            quiet_hours: s.quiet_hours.map(|q| sunrise_domain::QuietHours {
                start: q.start,
                end: q.end,
                policy: q.policy,
            }),
            is_primary_device: s.is_primary_device,
        }
    }
}
