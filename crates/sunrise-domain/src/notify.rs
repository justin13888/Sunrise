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

use crate::task::{Task, TaskState};
use crate::time::SunriseTime;
use jiff::civil;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

/// Default lead time for a Block reminder: 15 minutes before it starts, per
/// `docs/08-features/notifications.md` §Lead times.
pub const BLOCK_DEFAULT_LEAD_S: u32 = 15 * 60;

/// How far a queued notification may be pushed past its original time before
/// it is dropped instead: 4 hours, per the spec's quiet-hours rule.
pub const QUIET_HOURS_QUEUE_CAP_S: i64 = 4 * 60 * 60;

/// What a device does with a notification that lands inside quiet hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuietHoursPolicy {
    /// Fire at the next minute outside quiet hours, capped at
    /// [`QUIET_HOURS_QUEUE_CAP_S`]; still quiet at the cap means drop.
    #[default]
    Queue,
    /// Drop it outright.
    Drop,
}

/// A device-local quiet window. `start == end` is an empty window, not a
/// 24-hour one: a user who set both to the same time has configured nothing,
/// and silencing them permanently is the worse reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// Local wall-clock start.
    pub start: civil::Time,
    /// Local wall-clock end. May be earlier than `start`, which wraps midnight.
    pub end: civil::Time,
    /// What to do with a notification inside the window.
    #[serde(default)]
    pub policy: QuietHoursPolicy,
}

impl QuietHours {
    /// Is this wall-clock time inside the window?
    #[must_use]
    pub fn contains(&self, t: civil::Time) -> bool {
        if self.start == self.end {
            return false;
        }
        if self.start < self.end {
            t >= self.start && t < self.end
        } else {
            // Wraps midnight: 22:00..07:00 is "at or after 22:00, or before 07:00".
            t >= self.start || t < self.end
        }
    }
}

/// The per-device half of notification settings.
///
/// Deliberately **not** replicated state. The spec configures quiet hours per
/// device and makes exactly one device primary, so these travel in with the
/// query rather than living on an entity that would sync them to devices they
/// do not describe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReminderSettings {
    /// The device's global lead time in seconds — the floor of the hierarchy.
    pub default_lead_s: u32,
    /// Quiet window; `None` means never quiet.
    pub quiet_hours: Option<QuietHours>,
    /// Whether this device is the account's primary reminder device.
    ///
    /// A non-primary device plans **nothing**: the spec's dedup rule is "the
    /// primary handles reminders; other devices stay silent". The fallback
    /// when the primary goes quiet is a server-side decision (it is the only
    /// party that can see a device stop checking in), so a client that has
    /// been told to take over passes `true` here.
    pub is_primary_device: bool,
}

impl Default for ReminderSettings {
    fn default() -> Self {
        Self {
            // Spec default: fire at the scheduled time.
            default_lead_s: 0,
            quiet_hours: None,
            is_primary_device: true,
        }
    }
}

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

/// Resolve the lead-time hierarchy: per-entity, then per-Stream, then the
/// device's global default. First non-`None` wins
/// (`docs/08-features/notifications.md` §Lead-time hierarchy).
///
/// `Some(0)` is a value, not an absence — it means "fire at the scheduled
/// time", the documented default — so it stops the fallback like any other.
#[must_use]
pub const fn lead_time_s(own: Option<u32>, stream: Option<u32>, global: u32) -> u32 {
    match (own, stream) {
        (Some(v), _) | (None, Some(v)) => v,
        (None, None) => global,
    }
}

/// Apply quiet hours to a fire time, in the reading device's zone.
///
/// Returns the instant to fire at, or `None` to drop. A queued notification
/// moves to the end of the window and no further than
/// [`QUIET_HOURS_QUEUE_CAP_S`]; if the window is longer than the cap, the
/// notification is dropped rather than fired inside it.
#[must_use]
pub fn apply_quiet_hours(at: &Zoned, quiet: Option<&QuietHours>) -> Option<Timestamp> {
    let Some(q) = quiet else {
        return Some(at.timestamp());
    };
    if !q.contains(at.time()) {
        return Some(at.timestamp());
    }
    if q.policy == QuietHoursPolicy::Drop {
        return None;
    }
    // The next occurrence of the window's end, at or after `at`.
    let mut end = at.with().time(q.end).build().ok()?;
    if end <= *at {
        end = end.tomorrow().ok()?.with().time(q.end).build().ok()?;
    }
    let delay = end.timestamp().as_second() - at.timestamp().as_second();
    if delay > QUIET_HOURS_QUEUE_CAP_S {
        return None;
    }
    Some(end.timestamp())
}

/// How far a "not now" pushes something out.
///
/// The three answers `docs/08-features/notifications.md` §Action buttons puts
/// on every reminder, plus the one the defer menu adds. They are an enum
/// rather than a number of milliseconds because two of them are **civil**
/// decisions, not durations: "tomorrow" is a date, and a client adding
/// 86,400,000 ms gets it wrong twice a year — an hour early or an hour late
/// across a DST boundary, on exactly the reminders someone was relying on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnoozeSpan {
    /// One hour from now. A real duration, and the only one of the four
    /// that is.
    OneHour,
    /// The same wall-clock time tomorrow.
    Tomorrow,
    /// The same wall-clock time next week.
    NextWeek,
}

/// When a [`SnoozeSpan`] lands, read in `tz`.
///
/// `Tomorrow` and `NextWeek` keep the time of day and move the *date*, so a
/// 09:00 reminder snoozed on the night the clocks change is still at 09:00.
/// The civil arithmetic is `jiff`'s, which resolves a wall clock that does not
/// exist (the spring-forward gap) forward rather than failing — so a snooze
/// always lands somewhere, and the somewhere is the next real instant.
#[must_use]
pub fn snooze_target(from: Timestamp, span: SnoozeSpan, tz: &TimeZone) -> Timestamp {
    match span {
        SnoozeSpan::OneHour => from + jiff::SignedDuration::from_hours(1),
        SnoozeSpan::Tomorrow => civil_shift(from, 1, tz),
        SnoozeSpan::NextWeek => civil_shift(from, 7, tz),
    }
}

/// `from`, moved `days` civil days on in `tz`, keeping the time of day.
fn civil_shift(from: Timestamp, days: i32, tz: &TimeZone) -> Timestamp {
    let zoned = from.to_zoned(tz.clone());
    zoned.checked_add(jiff::Span::new().days(days)).map_or_else(
        |_| from + jiff::SignedDuration::from_hours(24 * i64::from(days)),
        |z| z.timestamp(),
    )
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
    if !settings.is_primary_device {
        return Vec::new();
    }
    let mut out = Vec::new();
    for c in candidates {
        let global = match c.kind {
            // "Block reminders: 15 min before (default)" — a different floor,
            // not a different hierarchy.
            ReminderKind::Block => BLOCK_DEFAULT_LEAD_S,
            ReminderKind::Task | ReminderKind::Routine => settings.default_lead_s,
        };
        let lead = lead_time_s(c.own_lead_s, c.stream_lead_s, global);
        let happens_at = c.at.to_instant(tz);
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
        out.push(ReminderIntent {
            entity: c.entity,
            kind: c.kind,
            fire_at,
            title: c.title.clone(),
            deferred_from: (fire_at != raw).then_some(raw),
        });
    }
    out.sort_by(|a, b| {
        a.fire_at
            .cmp(&b.fire_at)
            .then_with(|| a.entity.cmp(&b.entity))
    });
    out
}

/// The morning notification's dedicated view
/// (issue #9): what got done, and what still needs a decision.
#[derive(Debug, Clone, Serialize)]
pub struct MorningSummary {
    /// Start of the previous civil day in the device's zone — the "since" the
    /// issue asks for.
    pub since: Timestamp,
    /// Start of today, so a client can split the completed list into
    /// "yesterday" and "already today" without a second query.
    pub today_start: Timestamp,
    /// Completed at or after `since`, newest completion first.
    pub completed: Vec<Task>,
    /// Live open tasks sitting unfiled in the Inbox: the triage queue.
    pub to_triage: Vec<Task>,
    /// Live open tasks scheduled or due inside today, earliest first.
    pub due_today: Vec<Task>,
}

/// The end-of-day notification's dedicated view (issue #9): what is still open
/// today, and what the coming week holds.
#[derive(Debug, Clone, Serialize)]
pub struct EndOfDayPlan {
    /// Start of today in the device's zone.
    pub day_start: Timestamp,
    /// Start of tomorrow — the exclusive end of "today".
    pub day_end: Timestamp,
    /// Exclusive end of the seven civil days that follow today.
    pub week_end: Timestamp,
    /// Still open and landing at or before `day_end`, **overdue included**:
    /// the whole point of the view is deciding what to do about them.
    pub still_open: Vec<Task>,
    /// Open and landing in `[day_end, week_end)`.
    pub week_ahead: Vec<Task>,
    /// Open, live, and carrying neither a scheduled time nor a deadline — the
    /// candidates for "plan the coming week".
    pub unscheduled: Vec<Task>,
}

/// Build the morning summary from every live task, the civil bounds the caller
/// resolved, and the Inbox's id.
#[must_use]
pub fn build_morning_summary(
    tasks: &[Task],
    since: Timestamp,
    today_start: Timestamp,
    today_end: Timestamp,
    inbox: EntityRef,
) -> MorningSummary {
    let mut completed: Vec<Task> = tasks
        .iter()
        .filter(|t| {
            !t.deleted
                && t.state == TaskState::Done
                && t.completed_at
                    .as_ref()
                    .is_some_and(|c| c.index_ms() >= since.as_millisecond())
        })
        .cloned()
        .collect();
    completed.sort_by(|a, b| {
        completion_ms(b)
            .cmp(&completion_ms(a))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut to_triage: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && t.stream_id == inbox)
        .cloned()
        .collect();
    to_triage.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut due_today: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && lands_in(t, i64::MIN, today_end.as_millisecond()))
        .cloned()
        .collect();
    due_today.sort_by(plan_order);

    MorningSummary {
        since,
        today_start,
        completed,
        to_triage,
        due_today,
    }
}

/// Build the end-of-day plan from every live task and the civil bounds the
/// caller resolved.
#[must_use]
pub fn build_end_of_day_plan(
    tasks: &[Task],
    day_start: Timestamp,
    day_end: Timestamp,
    week_end: Timestamp,
) -> EndOfDayPlan {
    let mut still_open: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && lands_in(t, i64::MIN, day_end.as_millisecond()))
        .cloned()
        .collect();
    still_open.sort_by(plan_order);

    let mut week_ahead: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && lands_in(t, day_end.as_millisecond(), week_end.as_millisecond()))
        .cloned()
        .collect();
    week_ahead.sort_by(plan_order);

    let mut unscheduled: Vec<Task> = tasks
        .iter()
        .filter(|t| is_open(t) && t.scheduled_at.is_none() && t.due_at.is_none())
        .cloned()
        .collect();
    unscheduled.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });

    EndOfDayPlan {
        day_start,
        day_end,
        week_end,
        still_open,
        week_ahead,
        unscheduled,
    }
}

/// Live and not finished. `cancelled` counts as finished: it is a decision the
/// user already made, and re-offering it for triage every morning would be
/// nagging rather than summarizing.
fn is_open(t: &Task) -> bool {
    !t.deleted && matches!(t.state, TaskState::Todo | TaskState::InProgress)
}

fn completion_ms(t: &Task) -> i64 {
    t.completed_at
        .as_ref()
        .map_or(i64::MIN, SunriseTime::index_ms)
}

/// The instant a task "lands" on: its scheduled time, or its deadline when it
/// has no scheduled time. `None` for a task with neither.
fn lands_at(t: &Task) -> Option<i64> {
    t.scheduled_at
        .as_ref()
        .or(t.due_at.as_ref())
        .map(SunriseTime::index_ms)
}

fn lands_in(t: &Task, from_ms: i64, to_ms: i64) -> bool {
    lands_at(t).is_some_and(|ms| ms >= from_ms && ms < to_ms)
}

/// Earliest first, then most urgent, then by id so two devices agree.
fn plan_order(a: &Task, b: &Task) -> std::cmp::Ordering {
    lands_at(a)
        .unwrap_or(i64::MAX)
        .cmp(&lands_at(b).unwrap_or(i64::MAX))
        .then_with(|| {
            a.priority
                .unwrap_or(u8::MAX)
                .cmp(&b.priority.unwrap_or(u8::MAX))
        })
        .then_with(|| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::inbox_stream_ref;
    use crate::unknown::Unknowns;
    use std::collections::BTreeSet;
    use sunrise_id::EntityKind;

    const DAY_MS: i64 = 86_400_000;

    fn ts(ms: i64) -> Timestamp {
        Timestamp::from_millisecond(ms).unwrap()
    }

    fn task(n: u8) -> Task {
        Task {
            id: EntityRef::new(EntityKind::Task, [n; 16]),
            created_at: ts(0),
            updated_at: ts(0),
            title: format!("task {n}"),
            body: None,
            stream_id: EntityRef::new(EntityKind::Stream, [9u8; 16]),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            reminder_lead_s: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    // ---- lead-time hierarchy ----

    #[test]
    fn the_first_non_null_level_wins() {
        assert_eq!(lead_time_s(Some(60), Some(300), 900), 60);
        assert_eq!(lead_time_s(None, Some(300), 900), 300);
        assert_eq!(lead_time_s(None, None, 900), 900);
    }

    /// `Some(0)` is the documented default ("fire at the scheduled time"), not
    /// an absence, so it must stop the fallback.
    #[test]
    fn zero_is_a_value_not_an_absence() {
        assert_eq!(lead_time_s(Some(0), Some(300), 900), 0);
        assert_eq!(lead_time_s(None, Some(0), 900), 0);
    }

    // ---- quiet hours ----

    fn quiet(policy: QuietHoursPolicy) -> QuietHours {
        QuietHours {
            start: civil::time(22, 0, 0, 0),
            end: civil::time(7, 0, 0, 0),
            policy,
        }
    }

    fn at_local(hour: i8, minute: i8) -> Zoned {
        civil::date(2026, 3, 4)
            .at(hour, minute, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap()
    }

    #[test]
    fn a_wrapping_window_covers_both_sides_of_midnight() {
        let q = quiet(QuietHoursPolicy::Queue);
        assert!(q.contains(civil::time(23, 0, 0, 0)));
        assert!(q.contains(civil::time(3, 0, 0, 0)));
        assert!(!q.contains(civil::time(12, 0, 0, 0)));
        // Half-open: the end instant is already allowed.
        assert!(!q.contains(civil::time(7, 0, 0, 0)));
        assert!(q.contains(civil::time(22, 0, 0, 0)));
    }

    #[test]
    fn an_empty_window_silences_nothing() {
        let q = QuietHours {
            start: civil::time(22, 0, 0, 0),
            end: civil::time(22, 0, 0, 0),
            policy: QuietHoursPolicy::Drop,
        };
        assert!(!q.contains(civil::time(22, 0, 0, 0)));
        let at = at_local(22, 0);
        assert_eq!(apply_quiet_hours(&at, Some(&q)), Some(at.timestamp()));
    }

    #[test]
    fn a_notification_outside_the_window_is_untouched() {
        let at = at_local(12, 0);
        assert_eq!(
            apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Queue))),
            Some(at.timestamp())
        );
    }

    #[test]
    fn queue_moves_it_to_the_end_of_the_window() {
        // 06:15 is inside 22:00..07:00, so it queues to 07:00 — 45 minutes,
        // well inside the four-hour cap.
        let at = at_local(6, 15);
        let fired = apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Queue))).unwrap();
        assert_eq!(fired, at_local(7, 0).timestamp());
    }

    #[test]
    fn queue_drops_it_when_the_wait_exceeds_the_cap() {
        // 22:30 would have to wait 8.5 hours for 07:00.
        let at = at_local(22, 30);
        assert_eq!(
            apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Queue))),
            None
        );
    }

    /// The cap is on the **wait**, not on the window.
    ///
    /// Worth stating, because the constant's own doc reads as if a long quiet
    /// window drops everything inside it. It does not: the 22:00..07:00 window
    /// here is nine hours long, and a notification landing three hours before
    /// its end still fires. What is measured is `end - at`, from the
    /// notification's own time — so the same window queues a 06:15 and drops a
    /// 22:30.
    ///
    /// Pinned at the boundary in both directions, because `>` and `>=` are one
    /// character apart and a whole class of reminders sits on the line.
    #[test]
    fn the_queue_cap_is_measured_from_the_notification_and_is_inclusive() {
        let q = quiet(QuietHoursPolicy::Queue);

        // 03:00 → 07:00 is exactly four hours: the cap itself, which fires.
        let at_the_cap = at_local(3, 0);
        assert_eq!(
            at_local(7, 0).timestamp().as_second() - at_the_cap.timestamp().as_second(),
            QUIET_HOURS_QUEUE_CAP_S,
            "the fixture must sit exactly on the cap or it pins nothing"
        );
        assert_eq!(
            apply_quiet_hours(&at_the_cap, Some(&q)),
            Some(at_local(7, 0).timestamp()),
            "a wait of exactly the cap is not longer than the cap"
        );

        // One second earlier is one second over, and is dropped.
        let over_the_cap = civil::date(2026, 3, 4)
            .at(2, 59, 59, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        assert_eq!(
            at_local(7, 0).timestamp().as_second() - over_the_cap.timestamp().as_second(),
            QUIET_HOURS_QUEUE_CAP_S + 1
        );
        assert_eq!(
            apply_quiet_hours(&over_the_cap, Some(&q)),
            None,
            "one second past the cap is dropped, not fired late"
        );
    }

    #[test]
    fn drop_policy_drops_it_outright() {
        let at = at_local(6, 15);
        assert_eq!(
            apply_quiet_hours(&at, Some(&quiet(QuietHoursPolicy::Drop))),
            None
        );
    }

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

    // ---- the two views ----

    #[test]
    fn the_morning_summary_splits_done_from_to_triage() {
        let day = 1_772_064_000_000i64; // a civil midnight, UTC
        let since = day - DAY_MS;

        let mut done_yesterday = task(1);
        done_yesterday.state = TaskState::Done;
        done_yesterday.completed_at = Some(SunriseTime::instant(ts(since + 3_600_000)));

        let mut done_last_week = task(2);
        done_last_week.state = TaskState::Done;
        done_last_week.completed_at = Some(SunriseTime::instant(ts(since - 5 * DAY_MS)));

        let mut inbox_item = task(3);
        inbox_item.stream_id = inbox_stream_ref();

        let mut due_today = task(4);
        due_today.due_at = Some(SunriseTime::instant(ts(day + 3_600_000)));

        let filed_and_later = {
            let mut t = task(5);
            t.scheduled_at = Some(SunriseTime::instant(ts(day + 10 * DAY_MS)));
            t
        };

        let s = build_morning_summary(
            &[
                done_yesterday.clone(),
                done_last_week,
                inbox_item.clone(),
                due_today.clone(),
                filed_and_later,
            ],
            ts(since),
            ts(day),
            ts(day + DAY_MS),
            inbox_stream_ref(),
        );
        assert_eq!(
            s.completed.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![done_yesterday.id],
            "only completions since the previous calendar date"
        );
        assert_eq!(
            s.to_triage.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![inbox_item.id],
            "triage is the unfiled Inbox"
        );
        assert_eq!(
            s.due_today.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![due_today.id]
        );
    }

    /// An overdue task belongs on both views: the morning one so it is not
    /// forgotten, the end-of-day one so a decision gets made about it.
    #[test]
    fn an_overdue_task_shows_up_as_still_open() {
        let day = 1_772_064_000_000i64;
        let mut overdue = task(1);
        overdue.due_at = Some(SunriseTime::instant(ts(day - 3 * DAY_MS)));

        let plan = build_end_of_day_plan(
            &[overdue.clone()],
            ts(day),
            ts(day + DAY_MS),
            ts(day + 8 * DAY_MS),
        );
        assert_eq!(
            plan.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![overdue.id]
        );

        let summary = build_morning_summary(
            &[overdue.clone()],
            ts(day - DAY_MS),
            ts(day),
            ts(day + DAY_MS),
            inbox_stream_ref(),
        );
        assert_eq!(
            summary.due_today.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![overdue.id]
        );
    }

    #[test]
    fn the_end_of_day_plan_separates_today_the_week_and_the_unscheduled() {
        let day = 1_772_064_000_000i64;
        let mut today = task(1);
        today.scheduled_at = Some(SunriseTime::instant(ts(day + 3_600_000)));
        let mut later_this_week = task(2);
        later_this_week.scheduled_at = Some(SunriseTime::instant(ts(day + 3 * DAY_MS)));
        let mut next_month = task(3);
        next_month.scheduled_at = Some(SunriseTime::instant(ts(day + 40 * DAY_MS)));
        let floating = task(4);
        let mut done = task(5);
        done.state = TaskState::Done;

        let plan = build_end_of_day_plan(
            &[
                today.clone(),
                later_this_week.clone(),
                next_month,
                floating.clone(),
                done,
            ],
            ts(day),
            ts(day + DAY_MS),
            ts(day + 8 * DAY_MS),
        );
        assert_eq!(
            plan.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![today.id]
        );
        assert_eq!(
            plan.week_ahead.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![later_this_week.id]
        );
        assert_eq!(
            plan.unscheduled.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![floating.id],
            "a done task is not a planning candidate"
        );
    }

    /// `plan_order` is three keys deep — "earliest first, then most urgent,
    /// then by id so two devices agree" — and every other test of the two
    /// views produces a single-element list, which pins none of them.
    ///
    /// This fixture ties deliberately: four tasks, three of them landing on
    /// the same instant, two of those also sharing a priority. So the expected
    /// order can only come out right if all three keys are applied, in order.
    fn tied_fixture(landing_ms: i64) -> [Task; 4] {
        // Earliest, and *least* urgent — so it can only lead on key one.
        let mut earliest = task(9);
        earliest.scheduled_at = Some(SunriseTime::instant(ts(landing_ms - 1)));
        earliest.priority = None;

        // The three below all land together. `urgent` can only lead on key
        // two; `tie_a` and `tie_b` are separated by key three alone.
        let mut tie_b = task(2);
        tie_b.scheduled_at = Some(SunriseTime::instant(ts(landing_ms)));
        tie_b.priority = Some(5);

        let mut urgent = task(3);
        urgent.scheduled_at = Some(SunriseTime::instant(ts(landing_ms)));
        urgent.priority = Some(1);

        let mut tie_a = task(1);
        tie_a.scheduled_at = Some(SunriseTime::instant(ts(landing_ms)));
        tie_a.priority = Some(5);

        [earliest, tie_b, urgent, tie_a]
    }

    /// The order the three keys, applied in order, must produce: the earliest
    /// (id 9) despite having no priority at all, then the urgent one, then the
    /// two that tie on both keys, in id order.
    fn expected_plan_order(tasks: &[Task; 4]) -> Vec<EntityRef> {
        vec![tasks[0].id, tasks[2].id, tasks[3].id, tasks[1].id]
    }

    #[test]
    fn the_end_of_day_plan_orders_by_landing_then_urgency_then_id() {
        let day = 1_772_064_000_000i64;
        let today = tied_fixture(day + 3_600_000);
        let next_week = tied_fixture(day + 3 * DAY_MS);

        let mut tasks = today.to_vec();
        tasks.extend(next_week.iter().map(|t| {
            // Distinct ids across the two buckets, same shape within each.
            let mut t = t.clone();
            t.id = EntityRef::new(EntityKind::Task, [t.id.bytes()[0] + 10; 16]);
            t
        }));
        let plan = build_end_of_day_plan(&tasks, ts(day), ts(day + DAY_MS), ts(day + 8 * DAY_MS));

        assert_eq!(
            plan.still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            expected_plan_order(&today),
            "earliest first, then most urgent, then by id"
        );
        let expected_week: Vec<EntityRef> = expected_plan_order(&next_week)
            .into_iter()
            .map(|id| EntityRef::new(EntityKind::Task, [id.bytes()[0] + 10; 16]))
            .collect();
        assert_eq!(
            plan.week_ahead.iter().map(|t| t.id).collect::<Vec<_>>(),
            expected_week,
            "the week-ahead bucket sorts by the same three keys"
        );
    }

    #[test]
    fn the_morning_summary_orders_due_today_by_landing_then_urgency_then_id() {
        let day = 1_772_064_000_000i64;
        let tasks = tied_fixture(day + 3_600_000);
        let s = build_morning_summary(
            &tasks,
            ts(day - DAY_MS),
            ts(day),
            ts(day + DAY_MS),
            inbox_stream_ref(),
        );
        assert_eq!(
            s.due_today.iter().map(|t| t.id).collect::<Vec<_>>(),
            expected_plan_order(&tasks),
            "the morning view uses the same comparator as the evening one"
        );
    }

    #[test]
    fn a_cancelled_task_is_not_offered_for_triage() {
        let day = 1_772_064_000_000i64;
        let mut cancelled = task(1);
        cancelled.stream_id = inbox_stream_ref();
        cancelled.state = TaskState::Cancelled;
        let s = build_morning_summary(
            &[cancelled],
            ts(day - DAY_MS),
            ts(day),
            ts(day + DAY_MS),
            inbox_stream_ref(),
        );
        assert!(s.to_triage.is_empty());
    }
}

#[cfg(test)]
mod snooze_tests {
    use super::*;

    fn ts(iso: &str) -> Timestamp {
        iso.parse().expect("timestamp")
    }

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").expect("zone")
    }

    #[test]
    fn an_hour_is_an_hour() {
        assert_eq!(
            snooze_target(ts("2026-03-04T14:00:00Z"), SnoozeSpan::OneHour, &ny()),
            ts("2026-03-04T15:00:00Z")
        );
    }

    /// The reason these are civil and not durations. New York springs forward
    /// at 02:00 on 2026-03-08, so 09:00 Saturday to 09:00 Sunday is 23 hours,
    /// not 24 — and a client adding 86,400,000 ms would fire at 10:00.
    #[test]
    fn tomorrow_keeps_the_wall_clock_across_a_spring_forward() {
        let saturday_nine = ts("2026-03-07T14:00:00Z"); // 09:00 EST
        let tomorrow = snooze_target(saturday_nine, SnoozeSpan::Tomorrow, &ny());
        assert_eq!(tomorrow, ts("2026-03-08T13:00:00Z")); // 09:00 EDT
        assert_eq!(
            tomorrow.to_zoned(ny()).time(),
            saturday_nine.to_zoned(ny()).time(),
            "same wall clock, 23 real hours later"
        );
    }

    #[test]
    fn tomorrow_keeps_the_wall_clock_across_a_fall_back() {
        let saturday_nine = ts("2026-10-31T13:00:00Z"); // 09:00 EDT
        let tomorrow = snooze_target(saturday_nine, SnoozeSpan::Tomorrow, &ny());
        assert_eq!(tomorrow, ts("2026-11-01T14:00:00Z")); // 09:00 EST
    }

    #[test]
    fn next_week_is_the_same_weekday() {
        let now = ts("2026-03-04T14:00:00Z");
        let next = snooze_target(now, SnoozeSpan::NextWeek, &ny());
        assert_eq!(
            next.to_zoned(ny()).date().weekday(),
            now.to_zoned(ny()).date().weekday()
        );
        assert_eq!(next.to_zoned(ny()).time(), now.to_zoned(ny()).time());
    }
}
