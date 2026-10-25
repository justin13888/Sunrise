//! Turning the core's reminder intents into an OS schedule, and keeping the
//! OS's schedule equal to it (`docs/08-features/notifications.md`).
//!
//! The core decides *whether* and *when* a reminder fires
//! ([`sunrise_core::Core::reminder_plan`]). What is left is the same on every
//! client and was, until this module, written in Swift: how many to hand the
//! OS, in what order, what each one says under its title, which identifier
//! each one is held under, and which of them the OS is already holding.
//!
//! # Identity is `(entity, rule)`, never the fire time
//!
//! [`identifier`] is built from [`sunrise_domain::ReminderIntent::identity`]
//! and nothing else (`docs/11-adr/0066-times-survive-travel.md` §5). A
//! reminder that moves — a task rescheduled, or a floating 09:00 re-planned in
//! a new zone — keeps its identifier and gets a new [`PlannedNotification::revision`],
//! and [`reconcile`] answers "replace" rather than "add one, cancel one". An
//! identifier that carried the fire time could not tell a reminder that moved
//! from one that did not, which is how a reminder for a zoned time kept a
//! stale wall-clock trigger after a zone change (#520).
//!
//! # A sync burst re-arms nothing
//!
//! A client follows the change feed and re-plans on every batch, so a sync
//! catch-up runs [`reconcile`] dozens of times in a row. An unchanged plan
//! answers with only [`Reconciliation::kept`], and a client that acts on
//! `add`, `replace` and `cancel` alone touches the OS not at all.
//!
//! The wind-down (`wind_down`) is planned beside them, under an identifier
//! of its own: it belongs to no entity, so it is not one of these. So are
//! the rest of the catalog's kinds (`catalog`, `alerts`): deadlines, briefs,
//! the triage nudge, and the security, sync and zone alerts, each under a
//! namespace of its own so that no reconcile cancels another's.
//!
//! # Focus holds them
//!
//! During an in-app focus session only its own end and security events fire
//! (§Focus suppression); every reminder here that falls in one waits until
//! 30 s after it, under the quiet-hours rule, and says so in its body.
//!
//! Everything here is pure except [`schedule()`] and [`read_day_schedule`],
//! which read the vault.

mod alerts;
mod catalog;
mod ledger;
mod policy;
mod schedule;
mod wind_down;

pub use alerts::{
    alert_identifier, raise_alerts, security_identifier, stall, AlertInputs, Stall, SyncHealth,
    SYNC_STALL_AFTER_MS,
};
pub use catalog::{
    catalog_entries, catalog_identifier, catalog_policy, plan_catalog, reconcile_catalog,
    triage_fingerprint, CatalogChanges, CatalogEntry, CatalogInputs, CatalogNotification,
    CatalogPolicy, ALERT_PREFIX, CATALOG_PREFIX, EVENING_BODY, EVENING_TITLE, MORNING_BODY,
    MORNING_TITLE, TRIAGE_TITLE,
};
pub use ledger::{NotificationLedger, NudgeRecord, LEDGER_VERSION};
pub use policy::{primary_claim, reminder_policy, ReminderPolicy, PRIMARY_DEVICE_KEY};
pub use schedule::{read_catalog_policy, read_policy, schedule, Schedule, ScheduleRequest};
pub use wind_down::{
    read_day_schedule, reconcile_wind_down, wind_down_notification, WindDownChange,
    WindDownNotification, WIND_DOWN_IDENTIFIER, WIND_DOWN_TITLE,
};

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::{ReminderIntent, ReminderKind, SnoozeSpan};
use sunrise_id::EntityRef;

/// Every identifier this module hands out starts with it.
///
/// Load-bearing: [`reconcile`] cancels what it does not want, and without a
/// namespace it would cancel notifications anything else in the app posted.
pub const IDENTIFIER_PREFIX: &str = "sunrise.reminder.";

/// How far ahead to schedule: a day.
///
/// Long enough that a laptop asleep overnight wakes with the morning's
/// reminders pending, short enough that the pending list stays well inside
/// [`MAX_SCHEDULED`].
pub const HORIZON_MS: u64 = 24 * 60 * 60 * 1000;

/// The most reminders to leave pending.
///
/// Apple keeps at most 64 scheduled local notifications per app and silently
/// discards the rest, so the cap is applied here, where the *earliest* are
/// kept, rather than by the OS, which drops an arbitrary set. The margin below
/// 64 is for anything else the app schedules.
pub const MAX_SCHEDULED: usize = 56;

/// What the body says when quiet hours moved a reminder.
pub const QUIET_HOURS_NOTE: &str = "Held for quiet hours";

/// What the body says when a focus session held a reminder.
pub const FOCUS_NOTE: &str = "Held for focus";

/// What the body says for a time block.
pub const BLOCK_STARTING: &str = "Time block starting";

/// Which set of action buttons a reminder carries.
///
/// A block cannot be completed or deferred — the commands do not exist — so it
/// gets its own category rather than buttons that would have to fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReminderCategory {
    /// A task, or a routine occurrence (a task by the time it is due): done,
    /// snooze, open.
    Task,
    /// A time block: open only.
    Block,
}

impl ReminderCategory {
    /// The category a reminder of `kind` is delivered in.
    #[must_use]
    pub const fn of(kind: ReminderKind) -> Self {
        match kind {
            ReminderKind::Block => Self::Block,
            ReminderKind::Task | ReminderKind::Routine => Self::Task,
        }
    }

    /// The identifier the category is registered with the OS under.
    #[must_use]
    pub const fn identifier(self) -> &'static str {
        match self {
            Self::Task => "sunrise.reminder.task",
            Self::Block => "sunrise.reminder.block",
        }
    }

    /// The snooze spans the category offers, in button order: the two the
    /// spec names for a task reminder, and none for a block.
    #[must_use]
    pub const fn snooze_spans(self) -> &'static [SnoozeSpan] {
        match self {
            Self::Task => &[SnoozeSpan::OneHour, SnoozeSpan::Tomorrow],
            Self::Block => &[],
        }
    }

    /// Whether the category offers Mark Done.
    #[must_use]
    pub const fn offers_completion(self) -> bool {
        matches!(self, Self::Task)
    }
}

/// How a wall-clock time is written on a notification.
///
/// The caller decides, from the `time_format` preference and, for `"locale"`,
/// the platform's locale, so nothing here reads a locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockStyle {
    /// `6:30 AM`.
    H12,
    /// `06:30`.
    H24,
}

impl ClockStyle {
    /// The style for a `time_format` preference value (`"h12"`, `"h24"` or
    /// `"locale"`), with `locale_uses_24h` answering `"locale"`.
    #[must_use]
    pub fn from_time_format(time_format: Option<&str>, locale_uses_24h: bool) -> Self {
        match time_format {
            Some("h12") => Self::H12,
            Some("h24") => Self::H24,
            _ if locale_uses_24h => Self::H24,
            _ => Self::H12,
        }
    }

    /// `at`, as a wall clock in `tz`.
    #[must_use]
    pub fn format(self, at: Timestamp, tz: &TimeZone) -> String {
        let zoned = at.to_zoned(tz.clone());
        match self {
            Self::H12 => zoned.strftime("%-I:%M %p").to_string(),
            Self::H24 => zoned.strftime("%H:%M").to_string(),
        }
    }
}

/// One local notification the OS should be holding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedNotification {
    /// Stable across re-plans; see [`identifier`].
    pub identifier: String,
    /// Changes whenever anything the OS shows or fires on changes: the fire
    /// instant, the title, the body or the category. A client stores it with
    /// the request and hands it back in [`PendingNotification::revision`].
    pub revision: String,
    /// The title: the entity's, computed on-device by the core.
    pub title: String,
    /// The line under the title, in English; empty when the title says
    /// everything. A client rendering its own catalog builds the line from
    /// [`Self::kind`] and [`Self::deferred_from_ms`] instead, which are the
    /// only facts it says.
    pub body: String,
    /// Which action buttons it carries.
    pub category: ReminderCategory,
    /// The source that produced it.
    pub kind: ReminderKind,
    /// The entity a button acts on and a tap opens.
    pub entity: EntityRef,
    /// The absolute instant to fire at, epoch milliseconds, after lead time
    /// and quiet hours. An OS trigger built from it must stay at this instant
    /// when the device's zone changes (#520); a floating reminder follows the
    /// zone by being re-planned, which changes [`Self::revision`].
    pub fire_at_ms: i64,
    /// When quiet hours moved it, the instant it was due, epoch milliseconds.
    pub deferred_from_ms: Option<i64>,
}

/// A request the OS is holding, as the client read it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotification {
    /// The request's identifier.
    pub identifier: String,
    /// The [`PlannedNotification::revision`] it was scheduled with. `None`
    /// for a request that carries none, which is always replaced.
    pub revision: Option<String>,
}

/// The difference between what the OS holds and what it should hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reconciliation {
    /// Not held yet: schedule each.
    pub add: Vec<PlannedNotification>,
    /// Held under the same identifier with another revision: schedule each
    /// again, which replaces the held request.
    pub replace: Vec<PlannedNotification>,
    /// Ours, held, and no longer wanted: withdraw each.
    pub cancel: Vec<String>,
    /// Held and still correct: leave alone.
    pub kept: Vec<String>,
}

impl Reconciliation {
    /// Whether there is nothing to do.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.replace.is_empty() && self.cancel.is_empty()
    }
}

/// The identifier of the reminder `(entity, kind)` names.
///
/// One per `(entity, rule)` — a task and a block on the same minute differ by
/// the rule, a routine and a task by the same — and never the fire time.
#[must_use]
pub fn identifier(entity: EntityRef, kind: ReminderKind) -> String {
    format!("{IDENTIFIER_PREFIX}{}.{entity}", tag(kind))
}

/// The identifier of `intent`, from [`ReminderIntent::identity`].
#[must_use]
pub fn identifier_of(intent: &ReminderIntent) -> String {
    let (entity, kind) = intent.identity();
    identifier(entity, kind)
}

/// The source as it appears in an identifier. Exhaustive, so a new
/// [`ReminderKind`] fails this build rather than colliding with a tag.
const fn tag(kind: ReminderKind) -> &'static str {
    match kind {
        ReminderKind::Task => "task",
        ReminderKind::Block => "block",
        ReminderKind::Routine => "routine",
    }
}

/// What the notification says under its title, with times read in `tz`.
///
/// Empty for an ordinary task reminder: a second line saying "reminder" is
/// noise on a surface that interrupts. Not empty when there is something the
/// user could not otherwise know — that a block is starting, or that quiet
/// hours moved the alert off the time they set (the visible half of
/// `notifications.quiet_hours.policy = "queue"`).
#[must_use]
pub fn body(intent: &ReminderIntent, tz: &TimeZone, clock: ClockStyle) -> String {
    body_noted(intent, tz, clock, QUIET_HOURS_NOTE)
}

/// [`body`], saying `note` for what moved it.
fn body_noted(intent: &ReminderIntent, tz: &TimeZone, clock: ClockStyle, note: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if intent.kind == ReminderKind::Block {
        parts.push(BLOCK_STARTING.to_owned());
    }
    if let Some(from) = intent.deferred_from {
        parts.push(format!("{note} · due {}", clock.format(from, tz)));
    }
    parts.join(" · ")
}

/// One intent, as a notification.
#[must_use]
pub fn notification(
    intent: &ReminderIntent,
    tz: &TimeZone,
    clock: ClockStyle,
) -> PlannedNotification {
    notification_noted(intent, tz, clock, QUIET_HOURS_NOTE)
}

/// [`notification`], with `note` saying what moved it.
fn notification_noted(
    intent: &ReminderIntent,
    tz: &TimeZone,
    clock: ClockStyle,
    note: &str,
) -> PlannedNotification {
    let category = ReminderCategory::of(intent.kind);
    let body = body_noted(intent, tz, clock, note);
    let fire_at_ms = intent.fire_at.as_millisecond();
    PlannedNotification {
        identifier: identifier_of(intent),
        revision: revision(fire_at_ms, &intent.title, &body, category),
        title: intent.title.clone(),
        body,
        category,
        kind: intent.kind,
        entity: intent.entity,
        fire_at_ms,
        deferred_from_ms: intent.deferred_from.map(Timestamp::as_millisecond),
    }
}

/// Everything to hand the OS: earliest first, one per identifier, at most
/// `limit`.
///
/// Ties on the fire instant break on the identifier, so the cut at `limit`
/// is the same on every run. Two intents with one identity keep the earlier.
#[must_use]
pub fn plan(
    intents: &[ReminderIntent],
    tz: &TimeZone,
    clock: ClockStyle,
    limit: usize,
) -> Vec<PlannedNotification> {
    plan_noted(
        intents,
        tz,
        clock,
        limit,
        &std::collections::BTreeSet::new(),
    )
}

/// [`plan`], with the reminders a focus session held, by identifier, saying
/// [`FOCUS_NOTE`] instead of [`QUIET_HOURS_NOTE`].
fn plan_noted(
    intents: &[ReminderIntent],
    tz: &TimeZone,
    clock: ClockStyle,
    limit: usize,
    focus_held: &std::collections::BTreeSet<String>,
) -> Vec<PlannedNotification> {
    let mut all: Vec<PlannedNotification> = intents
        .iter()
        .map(|i| {
            let note = if focus_held.contains(&identifier_of(i)) {
                FOCUS_NOTE
            } else {
                QUIET_HOURS_NOTE
            };
            notification_noted(i, tz, clock, note)
        })
        .collect();
    all.sort_by(|a, b| {
        a.fire_at_ms
            .cmp(&b.fire_at_ms)
            .then_with(|| a.identifier.cmp(&b.identifier))
    });
    let mut seen = std::collections::BTreeSet::new();
    all.retain(|n| seen.insert(n.identifier.clone()));
    all.truncate(limit);
    all
}

/// Desired against pending: what to add, replace, cancel and leave.
///
/// Only identifiers under [`IDENTIFIER_PREFIX`] are cancelled. A pending
/// request of ours with no revision — one scheduled before revisions
/// existed, under an identifier that carried its fire time — is not one this
/// plan names, so it is cancelled and its reminder added under its stable
/// identifier.
#[must_use]
pub fn reconcile(
    desired: &[PlannedNotification],
    pending: &[PendingNotification],
) -> Reconciliation {
    use std::collections::{BTreeMap, BTreeSet};
    let ours: BTreeMap<&str, Option<&str>> = pending
        .iter()
        .filter(|p| p.identifier.starts_with(IDENTIFIER_PREFIX))
        .map(|p| (p.identifier.as_str(), p.revision.as_deref()))
        .collect();
    let mut out = Reconciliation::default();
    let mut wanted = BTreeSet::new();
    for d in desired {
        wanted.insert(d.identifier.as_str());
        match ours.get(d.identifier.as_str()) {
            None => out.add.push(d.clone()),
            Some(Some(rev)) if *rev == d.revision => out.kept.push(d.identifier.clone()),
            Some(_) => out.replace.push(d.clone()),
        }
    }
    out.cancel = ours
        .keys()
        .filter(|id| !wanted.contains(*id))
        .map(|id| (*id).to_owned())
        .collect();
    out.kept.sort();
    out
}

/// A fingerprint of what the OS shows and fires on. FNV-1a over the fields,
/// so it is the same on every build and every platform.
fn revision(fire_at_ms: i64, title: &str, body: &str, category: ReminderCategory) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [title, body, category.identifier()] {
        for byte in part.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{fire_at_ms}.{hash:016x}")
}

#[cfg(test)]
mod tests;
