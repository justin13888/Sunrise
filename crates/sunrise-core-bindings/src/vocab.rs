//! The shared vocabulary, exported as functions rather than restated.
//!
//! Everything here is a thin lowering of [`sunrise_domain::phrase`] or
//! [`sunrise_domain::planning`]. None of it computes anything; the point is
//! that a client **cannot** be tempted to compute it.
//!
//! The temptation is real and specific. "Overdue", "tomorrow", "1h30" and
//! "weekdays 09:00–17:00" are each two lines to write in Swift, and each of
//! those two-line versions would disagree with the CLI the first time a
//! deadline landed on a DST boundary or a task was due at 09:00 and read at
//! 17:00. `docs/07-clients/overview.md` says this out loud; these exports are
//! what make following it the path of least resistance.
//!
//! Free functions, not methods: none of them touches the vault, so making them
//! hang off [`crate::SunriseCore`] would imply a dependency on an open vault
//! that does not exist — and a capture preview must render before a vault is
//! even unlocked on a first run.

use sunrise_domain::{RRule, ScheduleConstraint, SunriseTime, TodaySection};

use crate::dto::{Constraint, Recurrence, TimeValue};
use crate::BindingError;

/// See [`sunrise_domain::TodaySection`].
///
/// Declared remotely rather than mirrored so that adding a section upstream
/// fails this crate's build instead of silently producing a value the app
/// cannot name.
#[uniffi::remote(Enum)]
pub enum TodaySection {
    Scheduled,
    Due,
    Overdue,
}

/// `relative_day`'s two answers, as a record.
///
/// `is_past` is not derivable from `text` — "-3d" and "+3d" differ by one
/// character, and a client branching on a string prefix to colour a row is
/// exactly the restatement this module exists to prevent.
#[derive(Debug, Clone, uniffi::Record)]
pub struct RelativeDay {
    /// `yesterday` / `today` / `tomorrow` / `+3d` / `-3d`.
    pub text: String,
    /// Whether the day is behind the caller's `now_ms`.
    pub is_past: bool,
}

/// Which section of Today a row belongs in.
///
/// See [`sunrise_domain::today_section`]: the overdue boundary is
/// `due_at < start_of_today_local`, which is **not** `due_at < now_ms`.
///
/// `tz` is an IANA zone name; an unknown one falls back to UTC, for the same
/// reason [`crate::SunriseCore::capture`] does — showing a row in the wrong
/// section is a smaller failure than showing no rows at all.
#[uniffi::export]
#[must_use]
pub fn today_section(
    scheduled_at: Option<TimeValue>,
    due_at: Option<TimeValue>,
    now_ms: u64,
    tz: String,
) -> TodaySection {
    let zone = zone_or_utc(&tz);
    let scheduled = scheduled_at.map(SunriseTime::from);
    let due = due_at.map(SunriseTime::from);
    sunrise_domain::today_section(scheduled.as_ref(), due.as_ref(), now_ms, &zone)
}

/// `yesterday` / `today` / `tomorrow` / `+3d`, in whole civil days.
///
/// See [`sunrise_domain::relative_day`]. Whole days, not elapsed hours: "due
/// tomorrow" must not read as "today" because it happens to be 23:30 now.
#[uniffi::export]
#[must_use]
pub fn relative_day(at: TimeValue, now_ms: u64, tz: String) -> RelativeDay {
    let zone = zone_or_utc(&tz);
    let at = SunriseTime::from(at).to_instant(&zone);
    let (text, is_past) = sunrise_domain::relative_day(at, now_ms, &zone);
    RelativeDay { text, is_past }
}

/// `1800` → `30m`, `5400` → `1h30`. The compact form, for a list column.
#[uniffi::export]
#[must_use]
pub fn short_duration(secs: u64) -> String {
    sunrise_domain::short_duration(secs)
}

/// `MM:SS`, widening to `H:MM:SS` past an hour. The form for a live timer.
#[uniffi::export]
#[must_use]
pub fn duration_clock(ms: u64) -> String {
    sunrise_domain::fmt_duration_ms(ms)
}

/// The word for an energy facet. `None` reads as "any" — the value that drops
/// energy out of the planner's ranking rather than meaning "no energy".
///
/// Three words, and exported anyway. A client writing its own `switch` here
/// would be the first place "med" quietly became "medium" in one client and
/// not the other.
#[uniffi::export]
#[must_use]
pub fn energy_label(energy: Option<sunrise_domain::Energy>) -> String {
    sunrise_domain::energy_budget_label(energy).to_string()
}

/// One line describing a task's scheduling constraints, empty when it has
/// none.
///
/// The `soft_violations` a [`crate::dto::CommandOutcome`] carries are the same
/// type, so the same call renders "you scheduled this outside its window".
#[uniffi::export]
#[must_use]
pub fn constraint_summary(constraints: Vec<Constraint>) -> String {
    let list: Vec<ScheduleConstraint> = constraints.into_iter().map(Into::into).collect();
    sunrise_domain::constraint_summary(&list)
}

/// Read `every 2 weeks on tue` — or a raw `FREQ=WEEKLY;BYDAY=TU` body — into
/// a [`Recurrence`].
///
/// See [`sunrise_domain::parse_recurrence`]. This is the **only** way a client
/// should turn typed text into a rule. Writing a second parser in Swift would
/// mean `weekends` meaning one thing in the app and another in `sunrise-cli`,
/// and the disagreement would surface as tasks generated on the wrong days —
/// weeks later, silently.
///
/// # Errors
///
/// [`BindingError::BadRecurrence`], carrying the phrase and the domain's own
/// message. Nothing is guessed at: an unreadable cadence is refused rather
/// than rounded to the nearest thing that parses.
#[uniffi::export]
pub fn parse_recurrence(text: String) -> Result<Recurrence, BindingError> {
    sunrise_domain::parse_recurrence(&text)
        .map(|r| Recurrence::from(&r))
        .map_err(|cause| BindingError::BadRecurrence { text, cause })
}

/// Describe a rule in one line: `every 2 weeks on Mo, We`.
///
/// See [`sunrise_domain::rrule_summary`] — the inverse of
/// [`parse_recurrence`], and what [`crate::dto::RoutineListRow::cadence`]
/// already carries for a list row. Exported separately because a *routine
/// editor* has to describe a rule it has parsed but not yet saved, and there
/// is no row to read it off yet.
///
/// Lossy on purpose: it is prose, not a serialization.
#[uniffi::export]
#[must_use]
pub fn recurrence_summary(rule: Recurrence) -> String {
    sunrise_domain::rrule_summary(&RRule::from(rule))
}

/// The synthetic Inbox stream's id.
///
/// `Query::StreamList` puts the Inbox first as an ordinary-looking row, and a
/// client has to tell it apart: it cannot be renamed, recoloured or deleted,
/// and offering those would produce a rejected command and a confused user.
///
/// Exported rather than hardcoded because the alternative is a client
/// comparing against the literal all-zero id — or worse, against the string
/// "Inbox", which is a display name and translatable.
#[uniffi::export]
#[must_use]
pub fn inbox_stream_id() -> sunrise_id::EntityRef {
    sunrise_domain::inbox_stream_ref()
}

/// An IANA zone, or UTC when the name is not one.
fn zone_or_utc(tz: &str) -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get(tz).unwrap_or(jiff::tz::TimeZone::UTC)
}
