//! The notification catalog as data, and the rules every kind shares
//! (`docs/08-features/notifications.md` §Catalog, #347).
//!
//! [`NotificationKind`] is one row of the catalog: its preference key, its
//! default, its interruption level, how it is delivered, which devices show
//! it, and its actions. A client reads them from here rather than writing its
//! own table, so a Settings screen, an Android channel list and the planner
//! cannot disagree about which kinds exist or what they do.
//!
//! The rules are pure functions of their arguments:
//!
//! - [`deadline_fire_at`]: when a hard deadline's reminder fires.
//! - [`focus_hold`]: what an in-app focus session does to a fire time
//!   (§Focus suppression).
//! - [`place`]: a fire time after focus and quiet hours, the one path every
//!   catalog kind takes.

use crate::day_schedule::{brief_at, Brief, DaySchedule};
use crate::notify::QUIET_HOURS_QUEUE_CAP_S;
use crate::notify::{apply_quiet_hours, QuietHours, QuietHoursPolicy, ReminderKind};
use crate::time::SunriseTime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

/// How long after a focus session ends the notifications it held fire:
/// 30 s, per §Focus suppression.
pub const FOCUS_RELEASE_S: i64 = 30;

/// One kind of the catalog, in catalog order (§Catalog's `#` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// #1: a task's plan less its lead time.
    TaskReminder,
    /// #2: a task's hard deadline.
    Deadline,
    /// #3: a routine occurrence's plan less its lead time.
    RoutineDue,
    /// #4: a block's start less its lead time.
    BlockStart,
    /// #5: wake plus its offset.
    MorningBrief,
    /// #6: the triage queue, once a day when it changed.
    TriageNudge,
    /// #7: sleep less its offset.
    EveningPlan,
    /// #8: sleep less its lead.
    WindDown,
    /// #9: a focus session's planned end, and a break's.
    FocusEnd,
    /// #10: the device's zone differs from the one it last observed.
    TimezoneChanged,
    /// #11: the review cadence.
    WeeklyReview,
    /// #12: ops unacknowledged by the relay, or a terminal refusal.
    SyncStalled,
    /// #13: a device joined the account.
    DevicePaired,
    /// #14: a device was revoked.
    DeviceRevoked,
    /// #15: the recovery code was retired.
    RecoveryCode,
    /// #16: a calendar token was rejected.
    CalendarReconnect,
    /// #17: a peer changed a task in a shared stream.
    SharedChange,
    /// #18: a monitored place was entered.
    PlaceArrival,
}

/// How loud a notification is (§Platform mechanisms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionLevel {
    /// Apple `.passive`.
    Passive,
    /// Apple `.active`.
    Active,
    /// Apple `.timeSensitive`.
    TimeSensitive,
}

/// How a kind reaches the OS (§Catalog's Delivery column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The fire time is known ahead and handed to the OS scheduler, and a
    /// re-plan reconciles it.
    Scheduled,
    /// Raised when the device observes the event, posted once.
    OnEvent,
}

/// Which devices show a kind (§Multi-device dedup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAudience {
    /// The primary device only: once per account.
    Primary,
    /// Every device of the account, each once.
    EveryDevice,
    /// The device the event concerns, and no other.
    ThisDevice,
}

/// A button a notification carries, besides tap-to-open (§Catalog's
/// Actions column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationAction {
    /// Complete the task.
    MarkDone,
    /// Snooze for one hour.
    SnoozeOneHour,
    /// Snooze until the same time tomorrow.
    SnoozeTomorrow,
    /// Snooze for five minutes.
    SnoozeFiveMinutes,
    /// Snooze for fifteen minutes.
    SnoozeFifteenMinutes,
    /// Pick a new plan (a deferral, ADR-0047 §4).
    Defer,
    /// Skip this routine occurrence.
    Skip,
    /// Start a focus session on the block.
    StartFocus,
    /// Open the planner on today.
    PlanMyDay,
    /// Open the triage queue.
    OpenTriage,
    /// Defer every open task of today to tomorrow.
    MoveOpenToTomorrow,
    /// Open the view.
    Open,
    /// Open tomorrow.
    SeeTomorrow,
    /// Add five minutes to the session.
    AddFiveMinutes,
    /// Start the break, or end the session.
    StartBreak,
    /// Open Upcoming.
    ReviewTimes,
    /// Dismiss: keep the times as they are.
    Keep,
    /// Start the weekly review.
    StartReview,
    /// Open the sync status.
    OpenSyncStatus,
    /// Open the device list.
    ReviewDevices,
    /// Open the security settings.
    ReviewSecurity,
    /// Reconnect the calendar.
    Reconnect,
    /// Open "Here now".
    OpenHereNow,
}

use NotificationAction as A;

impl NotificationKind {
    /// Every kind, in catalog order.
    pub const ALL: [Self; 18] = [
        Self::TaskReminder,
        Self::Deadline,
        Self::RoutineDue,
        Self::BlockStart,
        Self::MorningBrief,
        Self::TriageNudge,
        Self::EveningPlan,
        Self::WindDown,
        Self::FocusEnd,
        Self::TimezoneChanged,
        Self::WeeklyReview,
        Self::SyncStalled,
        Self::DevicePaired,
        Self::DeviceRevoked,
        Self::RecoveryCode,
        Self::CalendarReconnect,
        Self::SharedChange,
        Self::PlaceArrival,
    ];

    /// The kind's name: the `<kind>` of its `notifications.<kind>.*` keys,
    /// and its name in an identifier, a CLI table and JSON.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::TaskReminder => "task_reminder",
            Self::Deadline => "deadline",
            Self::RoutineDue => "routine_due",
            Self::BlockStart => "block_start",
            Self::MorningBrief => "morning_brief",
            Self::TriageNudge => "triage_nudge",
            Self::EveningPlan => "evening_plan",
            Self::WindDown => "wind_down",
            Self::FocusEnd => "focus_end",
            Self::TimezoneChanged => "timezone_changed",
            Self::WeeklyReview => "weekly_review",
            Self::SyncStalled => "sync_stalled",
            Self::DevicePaired => "device_paired",
            Self::DeviceRevoked => "device_revoked",
            Self::RecoveryCode => "recovery_code",
            Self::CalendarReconnect => "calendar_reconnect",
            Self::SharedChange => "shared_change",
            Self::PlaceArrival => "place_arrival",
        }
    }

    /// The kind [`Self::tag`] names.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.tag() == tag)
    }

    /// The catalog kind a planner source is.
    #[must_use]
    pub const fn of_reminder(kind: ReminderKind) -> Self {
        match kind {
            ReminderKind::Task => Self::TaskReminder,
            ReminderKind::Block => Self::BlockStart,
            ReminderKind::Routine => Self::RoutineDue,
        }
    }

    /// A security event: always on, every device, never held (§Security
    /// events).
    #[must_use]
    pub const fn is_security(self) -> bool {
        matches!(
            self,
            Self::DevicePaired | Self::DeviceRevoked | Self::RecoveryCode
        )
    }

    /// The key that turns it on and off, or `None` for a security event,
    /// which cannot be turned off.
    #[must_use]
    pub const fn enabled_key(self) -> Option<&'static str> {
        Some(match self {
            Self::TaskReminder => "notifications.task_reminder.enabled",
            Self::Deadline => "notifications.deadline.enabled",
            Self::RoutineDue => "notifications.routine_due.enabled",
            Self::BlockStart => "notifications.block_start.enabled",
            Self::MorningBrief => "notifications.morning_brief.enabled",
            Self::TriageNudge => "notifications.triage_nudge.enabled",
            Self::EveningPlan => "notifications.evening_plan.enabled",
            Self::WindDown => "notifications.wind_down.enabled",
            Self::FocusEnd => "notifications.focus_end.enabled",
            Self::TimezoneChanged => "notifications.timezone_changed.enabled",
            Self::WeeklyReview => "notifications.weekly_review.enabled",
            Self::SyncStalled => "notifications.sync_stalled.enabled",
            Self::CalendarReconnect => "notifications.calendar_reconnect.enabled",
            Self::SharedChange => "notifications.shared_change.enabled",
            Self::PlaceArrival => "notifications.place_arrival.enabled",
            Self::DevicePaired | Self::DeviceRevoked | Self::RecoveryCode => return None,
        })
    }

    /// Whether it is on before anyone sets its key: off for the time-zone
    /// notice and place arrival, on for everything else.
    #[must_use]
    pub const fn default_enabled(self) -> bool {
        !matches!(self, Self::TimezoneChanged | Self::PlaceArrival)
    }

    /// How loud it is.
    #[must_use]
    pub const fn level(self) -> InterruptionLevel {
        match self {
            Self::TriageNudge | Self::TimezoneChanged | Self::SharedChange => {
                InterruptionLevel::Passive
            }
            Self::BlockStart
            | Self::WindDown
            | Self::FocusEnd
            | Self::DevicePaired
            | Self::DeviceRevoked
            | Self::RecoveryCode => InterruptionLevel::TimeSensitive,
            _ => InterruptionLevel::Active,
        }
    }

    /// How it reaches the OS.
    #[must_use]
    pub const fn delivery(self) -> Delivery {
        match self {
            Self::TimezoneChanged
            | Self::SyncStalled
            | Self::DevicePaired
            | Self::DeviceRevoked
            | Self::RecoveryCode
            | Self::CalendarReconnect
            | Self::SharedChange
            | Self::PlaceArrival => Delivery::OnEvent,
            _ => Delivery::Scheduled,
        }
    }

    /// Which devices show it.
    #[must_use]
    pub const fn audience(self) -> NotificationAudience {
        match self {
            _ if self.is_security() => NotificationAudience::EveryDevice,
            Self::SyncStalled | Self::PlaceArrival => NotificationAudience::ThisDevice,
            _ => NotificationAudience::Primary,
        }
    }

    /// Whether an in-app focus session lets it through: its own end and
    /// security events (§Focus suppression).
    #[must_use]
    pub const fn passes_focus(self) -> bool {
        matches!(self, Self::FocusEnd) || self.is_security()
    }

    /// Whether quiet hours hold it. Security events ignore them; a wind-down
    /// and a focus end fire where the user put them (§Quiet hours).
    #[must_use]
    pub const fn held_by_quiet_hours(self) -> bool {
        !(self.is_security() || matches!(self, Self::WindDown | Self::FocusEnd))
    }

    /// Its buttons, in order. The triage nudge folded into a morning brief
    /// adds [`NotificationAction::OpenTriage`] to the brief's.
    #[must_use]
    pub const fn actions(self) -> &'static [NotificationAction] {
        match self {
            Self::TaskReminder => &[A::MarkDone, A::SnoozeOneHour, A::SnoozeTomorrow],
            Self::Deadline => &[A::MarkDone, A::Defer, A::SnoozeOneHour],
            Self::RoutineDue => &[A::MarkDone, A::Skip, A::SnoozeOneHour],
            Self::BlockStart => &[A::StartFocus, A::SnoozeFiveMinutes],
            Self::MorningBrief => &[A::PlanMyDay],
            Self::TriageNudge => &[A::OpenTriage],
            Self::EveningPlan => &[A::MoveOpenToTomorrow, A::Open],
            Self::WindDown => &[A::SeeTomorrow, A::SnoozeFifteenMinutes],
            Self::FocusEnd => &[A::AddFiveMinutes, A::StartBreak],
            Self::TimezoneChanged => &[A::ReviewTimes, A::Keep],
            Self::WeeklyReview => &[A::StartReview, A::SnoozeTomorrow],
            Self::SyncStalled => &[A::OpenSyncStatus],
            Self::DevicePaired | Self::DeviceRevoked => &[A::ReviewDevices],
            Self::RecoveryCode => &[A::ReviewSecurity],
            Self::CalendarReconnect => &[A::Reconnect],
            Self::SharedChange => &[A::Open],
            Self::PlaceArrival => &[A::OpenHereNow, A::MarkDone],
        }
    }
}

/// When a hard deadline's reminder fires for a reader in `tz`: `lead_s`
/// before a timed one, and the morning-brief time of its date for a
/// date-only one (`brief_offset_s` after wake, or 08:00 with no schedule).
/// `None` for a value with no instant, which is never reminded of.
///
/// Only `hard_due_at` reaches this: a target date passing is the triage
/// nudge's, never a notification of its own (§Catalog, "Not notifications").
#[must_use]
pub fn deadline_fire_at(
    schedule: &DaySchedule,
    hard_due: &SunriseTime,
    tz: &TimeZone,
    lead_s: u32,
    brief_offset_s: i32,
) -> Option<Timestamp> {
    match hard_due {
        SunriseTime::AllDay { date } => {
            brief_at(schedule, *date, tz, Brief::Morning, brief_offset_s)
                .ok()
                .flatten()
        }
        timed => timed
            .resolve_in(tz)?
            .checked_sub(SignedDuration::from_secs(i64::from(lead_s)))
            .ok(),
    }
}

/// An in-app focus session, as the notification rules see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusWindow {
    /// When it started.
    pub start: Timestamp,
    /// When it is planned to end; `None` for an open-ended one.
    pub end: Option<Timestamp>,
}

/// What `sessions` do to a notification of `kind` due at `at`, under the
/// quiet-hours `policy`: `Some(at)` untouched, `Some(later)` held until 30 s
/// after the session it falls in ends, `None` dropped.
///
/// A held notification follows the quiet-hours rule (§Focus suppression):
/// dropped under `drop`, and dropped when the wait from `at` would pass
/// [`QUIET_HOURS_QUEUE_CAP_S`]. An open-ended session has no end to wait
/// for, so what falls in it is dropped, as a queued notification still
/// inside quiet hours at its cap is.
#[must_use]
pub fn focus_hold(
    at: Timestamp,
    kind: NotificationKind,
    sessions: &[FocusWindow],
    policy: QuietHoursPolicy,
) -> Option<Timestamp> {
    if kind.passes_focus() {
        return Some(at);
    }
    let release = |end: Timestamp| end + SignedDuration::from_secs(FOCUS_RELEASE_S);
    let Some(session) = sessions
        .iter()
        .find(|s| at >= s.start && s.end.is_none_or(|end| at < release(end)))
    else {
        return Some(at);
    };
    let until = release(session.end?);
    let wait = until.as_second() - at.as_second();
    (policy == QuietHoursPolicy::Queue && wait <= QUIET_HOURS_QUEUE_CAP_S).then_some(until)
}

/// Where a notification of `kind` due at `raw` lands: through the focus
/// sessions, then quiet hours in `tz` when the kind is held by them. `None`
/// when either drops it.
#[must_use]
pub fn place(
    raw: Timestamp,
    kind: NotificationKind,
    sessions: &[FocusWindow],
    quiet: Option<&QuietHours>,
    tz: &TimeZone,
) -> Option<Timestamp> {
    let policy = quiet.map_or(QuietHoursPolicy::Queue, |q| q.policy);
    let held = focus_hold(raw, kind, sessions, policy)?;
    if !kind.held_by_quiet_hours() {
        return Some(held);
    }
    apply_quiet_hours(&held.to_zoned(tz.clone()), quiet)
}

#[cfg(test)]
mod tests;
