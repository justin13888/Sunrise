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
  never before it. The night
  between sleep and the next wake belongs to the **next** day, and time after
  midnight but before a late sleep belongs to the **previous** day. A task
  completed at 00:30 by someone who sleeps at 01:00 lands on the day they were
  living.
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
