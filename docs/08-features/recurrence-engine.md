---
status: accepted
---

# Recurrence Engine

Materializes Routines into Tasks. Lives in the core; runs on every device with
idempotent generation.

**The model of record is
[`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md).**
It owns the Routine shape, the occurrence key, the skip list, generation and
its horizon, catch-up, edit scope, the streak and pausing. This page covers
only what the engine adds on top: the RRULE subset shared with the calendar
integrations, when generation runs, and how the engine is tested. Where the
two disagree, the domain page wins.

## Inputs

- The Routine: its template, structured `rrule`, civil `anchor` (a `zoned` or
  `floating` wall-clock time, never an instant), `ends_at`, `skipped_keys` and
  pause state ([`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
  §Fields; [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §6;
  [#331](https://github.com/justin13888/Sunrise/issues/331)).
- Current time and the reader's zone, passed in by the caller; the engine reads
  no clock.
- The tasks already materialized for the series (to dedup by task id).

## RRULE subset

The engine supports the following RFC 5545 RRULE features. The same subset is reused by Google Calendar and iCalendar import/export — see [`../09-integrations/`](../09-integrations/) and reference this section.

| Property | Supported |
|---|---|
| `FREQ` | DAILY, WEEKLY, MONTHLY, YEARLY |
| `INTERVAL` | yes |
| `COUNT` | yes |
| `UNTIL` | yes; a UTC `UNTIL` is converted at import into the anchor's civil frame |
| `BYDAY` | yes (e.g. `MO,WE,FR`, `1MO`, `-1FR`) |
| `BYMONTHDAY` | yes |
| `BYMONTH` | yes |
| `BYSETPOS` | yes |
| `BYHOUR`, `BYMINUTE`, `BYSECOND` | no |
| `BYWEEKNO`, `BYYEARDAY` | no |
| `WKST` | yes (default `MO`) |
| `EXDATE` | yes — maps to `Routine.skipped_keys` at import |
| `RDATE` | no — dropped at import with an `int.import.rrule_lossy` warning |
| `RSCALE` | no |

`EXDATE` and `RDATE` are separate iCal properties, not RRULE parts. How an
`EXDATE` becomes an occurrence key, and how an unsupported part is refused, are
in [`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
§Recurrence rule and §Skip list.

Parsing uses a hand-written parser in `sunrise-domain` (`crates/sunrise-domain/src/rrule.rs`), not a third-party crate: the supported subset is deliberately narrow, it adds zero unvetted transitive dependencies to a security-frozen workspace, and it lets us emit an exact error taxonomy rather than remap a library's errors. See [`../01-architecture/dependencies.md`](../01-architecture/dependencies.md).

## Output and algorithm

Zero or more new Tasks, each with the deterministic id
`occurrence_task_id(series_root, key)`, `routine_occurrence_key = key`, and
`planned_at` = the key in the anchor's kind. The per-key steps (skip, pause,
dedup against live or tombstoned tasks, catch-up, template fields) and the
look-ahead horizon, which is a fixed function of `FREQ`, are specified in
[`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
§Generation. Read-time catch-up of occurrences already materialized is in the
same page's §Catch-up for occurrences already materialized.

Because the id is a pure function of the series root and the civil key, any
device generating the same occurrence emits the same task id, and the merge
layer dedups it.

## Edge cases

- **DST transitions.** The rule is expanded in civil space with no zone
  ([`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §6). A key is
  resolved to an instant only when something needs one, in the anchor's `tz`
  for a `zoned` anchor and in the reader's zone for a `floating` one, with
  jiff's `Disambiguation::Compatible`, the one rule in
  [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §3:
  - **Gap** (the wall-clock time does not exist on a spring-forward night): the occurrence moves **forward by the length of the gap**. A 02:30 routine in `America/New_York` fires at 03:30 on that night; a 02:10 routine in `Australia/Lord_Howe`, whose gap is 30 minutes, fires at 02:40. A 09:00 routine is never in a gap in any zone that shifts at night, and fires at 09:00.
  - **Fold** (the wall-clock time happens twice on a fall-back night): the occurrence fires once, at the **earlier** instant.
  - **Keys stay civil.** The occurrence key, skip key and streak key are the intended wall-clock value (`…T02:30`), never the resolved instant, so all three agree across a transition ([`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md) §Occurrence key).
- **Zone changes.** See [`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
  §Anchors are civil: travelling never moves a `zoned` anchor, and changing a
  routine's zone changes when occurrences fire, never which occurrences exist
  or what their keys are.
- **Routine deletion.** Stops generation. Existing occurrences remain unless explicitly deleted by the user.
- **Adaptive cadence** is not modelled; the convergence rule it must follow is
  in [`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
  §Adaptive cadence — convergence rule.
- **Streaks** are derived from `streak_keys` and never stored
  ([`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
  §Streak).

## Generation timing

- On every app launch.
- *Target state:* on a periodic core timer (every 6 hours when running). No such timer exists in `crates/sunrise-core`; generation is driven by launch, edit and post-sync only.
- On a Routine edit.
- After bulk sync application (since new ops may have come from another device that already generated some occurrences).

## Tests

- Property tests: random RRULEs across DST transitions; assert generation is deterministic and idempotent.
- Multi-device convergence test: two devices with different clocks generate the same Routine's occurrences; assert no duplicates after merge.
