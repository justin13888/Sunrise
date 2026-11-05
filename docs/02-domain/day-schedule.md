---
status: accepted
---

# Day schedule

When the user usually wakes and sleeps. It is optional but recommended, and it
defines the **planner day**: the unit behind Today, lateness, the briefs, the
wind-down notification and the planner. It is the `day_schedule` preference
([`preferences.md`](./preferences.md)), vault-synced, and the decision is
[ADR-0050](../11-adr/0050-preferences-and-day-schedule.md). Implementation is
tracked in [#338](https://github.com/justin13888/Sunrise/issues/338).

## Shape

```cddl
DaySchedule = {
    ? weekday:   { * Weekday   => DayWindow },     ; defaults per weekday
    ? month_day: { * month-day => DayWindow },     ; defaults per day of month
    ? date:      { * civil-date => DayOverride },  ; per-date overrides
    unknown-fields,
}

DayWindow   = { wake: civil-time, sleep: civil-time, unknown-fields }
DayOverride = DayWindow / { none: true }           ; "no schedule on this date"

month-day = -1 / 1..31                             ; -1 = the last day of the month
; Weekday is defined once, in ../10-cross-cutting/time.md §1.
civil-time = tstr .regexp "([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]"
civil-date = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}"
```

Every map entry is its own register (`day_schedule.weekday.MO`,
`day_schedule.month_day.1`, `day_schedule.date.2026-12-25`), so edits to
different entries on different devices both survive. Clearing an entry removes
that level for that key and nothing else.

## Resolution

```rust
fn window_for(schedule: &DaySchedule, date: civil::Date) -> Option<DayWindow>
```

First match wins:

1. `date[date]`: a `DayWindow`, or `none`, which means **unset** and stops the
   search (a holiday with no schedule does not fall through to the weekday).
2. `month_day[date.day()]`, then `month_day[-1]` if `date` is the last day of
   its month.
3. `weekday[date.weekday()]`.
4. Unset.

It is a pure function of `(schedule, date)`. It reads no zone and no clock.

## Civil, and resolved in the reader's zone

`wake` and `sleep` are civil times with no zone. They resolve in the **reader's**
zone at read time, so the schedule follows the user when they travel: a 07:00
wake is 07:00 in Tokyo while the user is in Tokyo. DST gaps and folds follow
the one rule in [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §3.

**Sleep may cross midnight.** If `sleep ≤ wake` as civil times, sleep falls on
the next civil date. `wake = 07:00, sleep = 01:00` means awake from 07:00 until
01:00 the following morning. `sleep == wake` is rejected at validation.

## Planner day

For a date `d` in zone `z`:

```
nominal_start(d) = wake(d) resolved in z,             if window_for(d) is set
                 = 00:00 of d in z,                   otherwise

end(d)           = sleep(d) resolved in z (next date if sleep ≤ wake),  if set
                 = 00:00 of d+1 in z,                                   otherwise

boundary(d)      = max(nominal_start(d), end(d-1))

planner_day(d)   = [boundary(d), boundary(d+1))
plannable(d)     = [nominal_start(d), end(d))          ; wake to sleep
```

These are the two names every other document uses: the **planner day** is
`planner_day(d)`, and the **plannable window** is `plannable(d)`, wake to sleep.
"Planner day" never means the wake-to-sleep span.

Properties, each pinned by a table test:

- **Total and disjoint.** Every instant belongs to exactly one planner day,
  because the boundaries are monotone: `boundary(d)` is always before civil
  midnight of `d+1` (a wake on `d` is on `d`, and a sleep that crosses midnight
  from `d-1` ends before `d-1`'s wake time on `d`), while `boundary(d+1)` is
  never before it. A planner day runs from its own start to the **next
  day's** start, so time after midnight belongs to the **previous** day until
  the later of that day's sleep and the next day's wake: the night between
  sleep and the next wake is still the day that ended with it. A task
  completed at 00:30 by someone who sleeps at 01:00 lands on the day they were
  living, and so does one opened at 03:00. This is the formula above; an
  earlier draft of this paragraph gave the night to the next day, which the
  formula never did.
- **Unset collapses to civil.** With no schedule at all, every planner day is
  the civil day.
- **A missing date is empty.** In a zone that skips a civil date
  (`Pacific/Apia`, 2011-12-30), that date's planner day has zero length.
- **Plannable is a subset.** The planner
  ([ADR-0048](../11-adr/0048-interactive-planner.md)) places work only inside
  `plannable(d)`; views and lateness use `planner_day(d)`.

`day_bounds(date, zone) -> (Timestamp, Timestamp)` returns `planner_day`, and
`planner_date(instant, zone) -> civil::Date` is its inverse. They replace
`civil_span` (`crates/sunrise-core/src/engine/block.rs#civil_span`) wherever a
planner day is meant: Today, Upcoming, the morning and evening briefs,
lateness and the planner.

## Wind-down

When `window_for(d)` has a `sleep`, the core computes the wind-down instant
`end(d) − notifications.wind_down.lead_s` (default 3600 s). The primary
notification device schedules it as the `wind_down` kind when
`notifications.wind_down.enabled` is on
([`../08-features/notifications.md`](../08-features/notifications.md), which
owns both keys).
With no sleep time for a date, no wind-down fires that night. This is the
trigger #301 asks for.

## Validation

- `wake` and `sleep` MUST be distinct.
- `month_day` keys MUST be in `-1` or `1..31`. A key of 31 simply never matches
  a 30-day month.
- A `date` override for a past date is kept. It still determines that day's
  planner bounds for stats and history.

## In the tree

- **The rules** are `crates/sunrise-domain/src/day_schedule.rs`, pure
  functions of `(schedule, date or instant, zone)`: `DaySchedule::window_for`,
  `day_bounds`, `plannable`, `planner_date`, `planner_day_of` (the planner day
  a stored time falls on: an `all_day` value's own date, else its resolved
  instant's) and `wind_down_at` / `next_wind_down`. Their table tests
  (precedence, a sleep past midnight, the gaps and folds of
  `America/New_York`, `Australia/Lord_Howe` and `Pacific/Chatham`, the
  skipped Apia date, an unset schedule, and contiguity over a week around
  each) are `crates/sunrise-domain/src/day_schedule/tests.rs`.
- **Storage** is the `Preferences` entity's `values` map, one key per entry:
  the key table holds the three maps (`day_schedule.weekday`,
  `day_schedule.month_day`, `day_schedule.date`, `PrefSpec::entries`), each
  entry's value is `{ "wake", "sleep" }` (`PrefType::DayWindow`) or, for a
  date, `{ "none": true }` (`PrefType::DayOverride`). An entry is written and
  cleared with `Command::SetPreference` / `ClearPreference`, vault target, so
  no command, table, migration or merge rule is the schedule's own. Two
  devices' overrides for two dates both survive
  (`crates/sunrise-core/src/engine/tests/preferences.rs#concurrent_day_schedule_overrides_for_different_dates_both_survive`).
  `Query::Preferences` does not list the entries; `Query::DaySchedule {
  at_ms }` returns the schedule and the reader's planner day holding `at_ms`.
- **Readers of the planner day** (`crates/sunrise-core/src/engine/day_schedule.rs#planner_day_at`):
  `Query::Today` (membership by `planner_day_of`), `Query::Upcoming` (each
  day's bounds and membership), `Query::MorningSummary` (`since` and
  `today_start` are planner-day starts), `Query::EndOfDayPlan` (the day and
  the seven planner days after it), `DailyReview`'s Today list (it is
  `Query::Today`), the time-zone change's lateness count, and Today's
  sections (`sunrise_domain::today_section_in`, and
  `sunrise_client_core::vocab::today_section_in` for a client holding the
  schedule).
- **Still civil midnight, on purpose:** the calendar grid (`DayBlocks`,
  `WeekBlocks`, through `civil_span`), because a grid column is a calendar
  date; the search grammar's date operators (`due:today`), which name
  calendar dates; and an `all_day` value's own instant, which
  `SunriseTime::resolve_in` and `due_in` still place at civil midnight. So a
  brief's instant-window filter (`lands_in`) counts an all-day deadline on
  the civil date after a late night into the night's planner day. Making an
  `all_day` value resolve to its planner day's bounds is a change to every
  `SunriseTime` comparison and is not part of #338.
- **Wind-down** is planned by `crates/sunrise-client-core/src/reminders/wind_down.rs`
  beside the reminder schedule (`Schedule::wind_down`), under its own
  identifier `sunrise.wind_down`, re-planned in the reader's zone on every
  call, so a zone change replaces it. The seam exports it as
  `SunriseCore::wind_down_plan`. No Apple client schedules it yet.
- **The CLI** reads and writes it with `sunrise day-schedule`, and
  `sunrise prefs get day_schedule.…` reads one entry
  (`crates/sunrise-e2e/tests/cli_day_schedule.rs` drives both against a
  relay).
