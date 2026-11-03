//! Notification planning: the two dedicated views issue #9 asks for, and the
//! reminder intents a client schedules from.
//!
//! Per `docs/08-features/notifications.md`. Everything here is **pure**: it
//! takes the rows the engine read, the civil bounds the engine resolved, and
//! the device's own settings, and returns what to show and when to fire. No
//! clock, no zone lookup, no storage — so every rule below is unit-testable in
//! isolation, which is the point of putting it here rather than in the engine.
//!
//! Notifications render **locally, after decryption**. The server never sees a
//! title, a count, or a fire time; it only ever sends a content-less wake-up.
//! That is why the planning lives in the core at all.

mod digest;
mod pipeline;
mod policy;
mod snooze;
#[cfg(test)]
mod test_support;

pub use digest::{build_end_of_day_plan, build_morning_summary, EndOfDayPlan, MorningSummary};
pub use pipeline::{
    plan_reminders, plan_reminders_reported, ReminderCandidate, ReminderIntent, ReminderKind,
    ReminderPlan, UnscheduledReminder,
};
pub use policy::{
    apply_quiet_hours, lead_time_s, QuietHours, QuietHoursPolicy, ReminderSettings,
    BLOCK_DEFAULT_LEAD_S, QUIET_HOURS_QUEUE_CAP_S,
};
pub use snooze::{snooze_target, SnoozeSpan};
