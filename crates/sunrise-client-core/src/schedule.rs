//! Where "defer", "snooze", "schedule" and a Today capture put a task, in
//! civil days and in the kind the task's plan already has.
//!
//! The Apple app computed every one of these as an instant: `now + N ×
//! 86,400,000` for a defer, `instant(now)` for a Today capture, and an
//! instant from a date picker for a schedule. Across a DST change the first
//! lands an hour off, and all three pin a plan that should follow the user
//! to wherever the device happened to be (`docs/11-adr/0066-times-survive-travel.md`
//! §4: "a deferral counts in civil days in the reader's zone, never in
//! multiples of 86,400 seconds", and a write keeps the kind the user meant).
//! The civil arithmetic is the domain's [`shift_civil_days`]; what is here is
//! the choice of base and of kind, which is the part two clients would
//! otherwise answer differently.

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use sunrise_core::Command;
use sunrise_domain::{shift_civil_days, SnoozeSpan, SunriseTime};
use sunrise_id::EntityRef;

use crate::civil::{value_at, CivilError, TimeKind};

/// Where a task scheduled at `scheduled_at` lands when deferred to
/// `days` days after today (today being `now`'s date in `reader`).
///
/// The **date** becomes today + `days`; everything else about the plan stays:
///
/// | Plan | Becomes |
/// |---|---|
/// | `floating` 09:00 | `floating`, 09:00 on the new date |
/// | `zoned` 09:00 Berlin | `zoned`, 09:00 Berlin on the new date |
/// | `all_day` | `all_day` on the new date |
/// | `instant` | the same wall-clock time in `reader` on the new date |
/// | none, or a kind this build does not know | `floating`, `now`'s wall-clock time on the new date |
///
/// Counted from today rather than from the plan, so "defer to tomorrow" on a
/// task that was due last week means tomorrow, which is what the action
/// says. A plan with no instant (an unknown kind with no anchor) has no
/// date to keep, so it is replaced, as the Apple defer replaced every plan.
///
/// # Errors
/// [`CivilError::Unresolvable`] when the result falls off the calendar.
pub fn deferred_to(
    scheduled_at: Option<&SunriseTime>,
    days: i32,
    now: Timestamp,
    reader: &TimeZone,
) -> Result<SunriseTime, CivilError> {
    let today = now.to_zoned(reader.clone()).date();
    let plan = scheduled_at
        .filter(|p| !p.is_unknown())
        .and_then(|p| Some((p, p.resolve_in(reader)?)));
    let Some((plan, at)) = plan else {
        let floating = SunriseTime::floating(now.to_zoned(reader.clone()).datetime());
        return Ok(shift_civil_days(&floating, days, reader)?);
    };
    let plan_day = match plan {
        // A date or a wall clock is on its own date, wherever it resolves.
        SunriseTime::AllDay { date } => *date,
        SunriseTime::Floating { civil } | SunriseTime::Zoned { civil, .. } => civil.date(),
        _ => at.to_zoned(reader.clone()).date(),
    };
    let to_today = today
        .since(plan_day)
        .map_err(|_| CivilError::Unresolvable(out_of_range(plan)))?
        .get_days();
    let shift = to_today
        .checked_add(days)
        .ok_or_else(|| CivilError::Unresolvable(out_of_range(plan)))?;
    Ok(shift_civil_days(plan, shift, reader)?)
}

/// [`Command::DeferTaskTo`] for [`deferred_to`]: the defer that bumps the
/// task's deferral count and keeps the kind of its plan.
///
/// # Errors
/// What [`deferred_to`] raises.
pub fn defer_command(
    id: EntityRef,
    scheduled_at: Option<&SunriseTime>,
    days: i32,
    now: Timestamp,
    reader: &TimeZone,
) -> Result<Command, CivilError> {
    Ok(Command::DeferTaskTo {
        id,
        to: deferred_to(scheduled_at, days, now, reader)?,
    })
}

/// Where a snooze of `span` from `now` puts a task scheduled at
/// `scheduled_at`.
///
/// An hour is a real duration, so [`SnoozeSpan::OneHour`] is the instant an
/// hour on. Tomorrow and next week are dates: [`deferred_to`] by one day and
/// by seven, keeping the plan's kind. [`sunrise_domain::snooze_target`] is the
/// same arithmetic for a caller that needs a moment rather than a value to
/// store.
///
/// # Errors
/// What [`deferred_to`] raises.
pub fn snoozed_to(
    scheduled_at: Option<&SunriseTime>,
    span: SnoozeSpan,
    now: Timestamp,
    reader: &TimeZone,
) -> Result<SunriseTime, CivilError> {
    match span {
        SnoozeSpan::OneHour => now
            .checked_add(SignedDuration::from_hours(1))
            .map(SunriseTime::instant)
            .map_err(|_| CivilError::OutOfRange {
                ms: now.as_millisecond(),
            }),
        SnoozeSpan::Tomorrow => deferred_to(scheduled_at, 1, now, reader),
        SnoozeSpan::NextWeek => deferred_to(scheduled_at, 7, now, reader),
    }
}

/// The kind a time picked in a "Schedule…" sheet is written as when the
/// sheet offers no kind of its own: a wall-clock time with no zone named,
/// which `docs/10-cross-cutting/time.md` §1 writes as `floating`.
pub const SCHEDULE_KIND: TimeKind = TimeKind::Floating;

/// A task's new plan from a "Schedule…" sheet: the moment `ms` as `kind`
/// (by default [`SCHEDULE_KIND`]) in `reader`. Not a deferral: picking a
/// date for the first time does not count against the task.
///
/// # Errors
/// [`CivilError::OutOfRange`] for milliseconds no timestamp holds.
pub fn scheduled_at(
    ms: i64,
    kind: Option<TimeKind>,
    reader: &TimeZone,
) -> Result<SunriseTime, CivilError> {
    value_at(ms, kind.unwrap_or(SCHEDULE_KIND), reader)
}

/// The day `days` after today, all day: "schedule for tomorrow" (`1`) or
/// "for next week" (`7`), with no time of day the user did not give.
///
/// # Errors
/// [`CivilError::Unresolvable`] when the date falls off the calendar.
pub fn scheduled_on_day(
    days: i32,
    now: Timestamp,
    reader: &TimeZone,
) -> Result<SunriseTime, CivilError> {
    let today = SunriseTime::all_day(now.to_zoned(reader.clone()).date());
    Ok(shift_civil_days(&today, days, reader)?)
}

/// The plan a task captured into Today gets when the line named no date:
/// today, all day.
///
/// Today lists only tasks that carry a date, so a capture there with none
/// would be written and then vanish from the screen it was typed on. The
/// Apple app dated it `instant(now)`, a moment rather than the day the user
/// was looking at, which then reads as earlier today on a device west of
/// where it was captured.
#[must_use]
pub fn today_capture_plan(now: Timestamp, reader: &TimeZone) -> SunriseTime {
    SunriseTime::all_day(now.to_zoned(reader.clone()).date())
}

/// The domain's out-of-range error for `value`.
fn out_of_range(value: &SunriseTime) -> sunrise_domain::Unresolvable {
    sunrise_domain::Unresolvable::OutOfRange {
        value: value.to_string(),
    }
}

#[cfg(test)]
mod tests;
