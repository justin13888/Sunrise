---
status: accepted
---

# Time

The normative rules for every time Sunrise stores, compares or displays. A rule
here wins over any other document that disagrees with it. It builds on
[ADR-0011](../11-adr/0011-datetime-jiff.md) (jiff is the only datetime library)
and [ADR-0017](../11-adr/0017-sunrise-time-representation.md) (the four-kind
`SunriseTime`), and it **amends ADR-0017** in three places, each marked below:
routine anchors are no longer instants, `index_ms` is no longer a comparison
key, and a civil time in a DST gap resolves by one stated rule.

The work that brings the tree in line is tracked in [#336](https://github.com/justin13888/Sunrise/issues/336). Routines follow in
[#331](https://github.com/justin13888/Sunrise/issues/331), constraints in [#333](https://github.com/justin13888/Sunrise/issues/333), deadlines in [#334](https://github.com/justin13888/Sunrise/issues/334), the day schedule in
[#338](https://github.com/justin13888/Sunrise/issues/338), and blocks in [#342](https://github.com/justin13888/Sunrise/issues/342).

## 1. Every stored time is a `SunriseTime`

```cddl
stime = { "kind": "instant",  "at":    timestamp }
      / { "kind": "zoned",    "civil": civil-datetime, "tz": tstr }   ; tz = IANA zone name
      / { "kind": "floating", "civil": civil-datetime }
      / { "kind": "all_day",  "date":  civil-date }
      / { "kind": tstr, * tstr => any }                                ; unknown kind, preserved (C4)
```

| Kind | Means | Resolves in |
|---|---|---|
| `instant` | a fixed point on the timeline | nothing; it is one |
| `zoned` | a wall-clock time in a named place | its own `tz` |
| `floating` | a wall-clock time wherever the reader is | the reader's zone |
| `all_day` | a date, with no time of day | the reader's zone, as a planner day (§4) |

**MUST.** A field that stores a moment or a date is an `stime`. No stored field
is a bare epoch number, a bare `Timestamp` standing for a civil time, or a
civil time with a zone in a sibling field.

**System-stamped moments** (`created_at`, `updated_at`, `triaged_at`,
`late_acknowledged_at`, focus session bounds, op stamps) are the `instant` kind.
On the wire they keep the bare `timestamp` encoding, which ADR-0017's decoder
already reads as `instant`, so no existing byte changes.

**An unknown kind** (a newer build added a fifth) is carried as
`Unknown { kind, raw }`, round-trips byte for byte, never fails the op that
contains it, and sorts by its `index_ms` when it has one ([#322](https://github.com/justin13888/Sunrise/issues/322)).

**Civil patterns are not stored times.** A time-of-day window
(`09:00–17:00`), a weekday set, a date range in a constraint, a wake or sleep
time, and quiet hours are *recurring civil patterns*, not moments. They are
jiff civil types (`civil::Time`, `civil::Date`) with no zone, and they are
always evaluated against a moment that has already been resolved (§5).

A day of the week is written as its RFC 5545 two-letter code. This is the one
definition; every other document that uses `Weekday` refers here:

```cddl
Weekday = "SU" / "MO" / "TU" / "WE" / "TH" / "FR" / "SA" / tstr   ; unknown value preserved (C4)
```

An unknown `Weekday` round-trips byte for byte and is never read as some other
day; each document that uses the type says what an unknown value does there
(for a routine's rule, the routine generates nothing).

### Which kind each field uses

| Field | Allowed kinds | Written by capture as | Why |
|---|---|---|---|
| `Task.planned_at` | all four | `floating` for a time with no zone ("at 9", "tonight"); `all_day` for a date ("friday"); `instant` for a relative offset ("in 2 hours"); `zoned` when a zone is named | a plan follows the user unless they pin it |
| `Task.target_at`, `Task.hard_due_at` | all four | `all_day` for a date, otherwise as above | a deadline "Friday" is a day, not 00:00 UTC Friday |
| `Task.completed_at` | `instant`, `all_day` | `instant` (now); `all_day` only from a backdated "done yesterday" ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)) | a completion is a recorded fact; a date is the honest record when the time is unknown |
| `Block.starts_at`, `Block.ends_at` | all four; both bounds MUST be the same kind, and the same `tz` when `zoned` | per the calendar UI | a 09:00 block and a fixed-instant block are different commitments |
| Routine and recurring-block **anchor** | `zoned` (default), `floating` | `zoned` in the device zone at creation; `floating` when the user picks "follows me" | **amends ADR-0017**, which kept occurrences as instants: an anchor is a wall-clock time, and changing the zone never moves it |
| `Routine.ends_at`, `RRule.until` | `floating`, `all_day` | from the rule editor or an iCal `UNTIL` converted into the anchor's civil frame | compared against occurrence keys in civil space (§6), so it needs no zone |
| `Routine.paused_until`, `Stream.paused_until` | all four | `all_day` for "pause until Monday" | resumes at the start of that planner day |
| `Task.reminder_lead_s` and other durations | not a time; seconds | — | a duration is not a point |

Before [#336](https://github.com/justin13888/Sunrise/issues/336), `Routine.starts_at`, `ends_at`, `paused_until` and `RRule.until`
are bare `Timestamp`s (`crates/sunrise-domain/src/routine.rs#Routine`,
`crates/sunrise-domain/src/rrule.rs#RRule`), and capture writes every phrase as
an instant (`crates/sunrise-domain/src/capture.rs#parse`). Both are defects
against this table.

## 2. Comparisons resolve through the reader's zone, never through `index_ms`

`index_ms` (`crates/sunrise-domain/src/time.rs#index_ms`) anchors `floating`
and `all_day` values in UTC. It is stable and lossless, and it is up to 14 hours
from the true instant in any real zone. It is a **storage key**, and that is all
it is.

**MUST.**

1. **Two `stime` values are compared by resolving both** with
   `to_instant(reader_zone)` (for `all_day`, the start of its planner day, §4)
   and comparing the instants. This is the only comparison a validator, a view,
   lateness, overlap detection, the planner or a sort may use. **One
   exception:** a deadline's due instant, `due()` in
   [ADR-0047](../11-adr/0047-deadlines-and-lateness.md) §Due instant, resolves
   `all_day(d)` to the **end** of its planner day, `boundary(d+1)`. Lateness and
   the planner's deadline rules use `due()`; every other `all_day` comparison
   stays start-of-day.
2. **`index_ms` is used only as a SQL prefilter.** A query that selects by time
   widens its bounds by **48 hours** on each side (14 h for UTC offsets and up
   to 24 h for a planner day that extends past midnight, rounded up) and then
   filters exactly in Rust after resolving. No result is decided by
   `index_ms` alone.
3. **Sorting** a mixed list is by resolved instant, then by kind (`all_day`
   before timed on the same instant), then by id. `Ord for SunriseTime` on the
   index key (**amends ADR-0017**, which defined it there) is retained only as
   an implementation detail of storage ordering and MUST NOT be used for a
   domain decision. A lint-style test greps for `index_ms()` and
   `SunriseTime` comparisons outside `sunrise-storage` and the prefilter
   helpers.

Today `index_ms` still decides the `due ≥ scheduled` invariant
(`crates/sunrise-domain/src/task.rs#validate_invariants`), block overlap
(`crates/sunrise-domain/src/block.rs#overlaps`) and the Today window
(`crates/sunrise-core/src/engine/query.rs#query_today`). Each is a defect against
this rule.

**The reader's zone** is the zone `Clock::timezone` reports
(`crates/sunrise-core/src/config.rs#timezone`) at the moment of the read. It is
always passed in explicitly; no domain function reads an ambient zone.

## 3. One DST rule

A civil date-time that is converted to an instant uses jiff's
**`Disambiguation::Compatible`**, everywhere, with no per-site variation:

- **Gap** (the civil time does not exist, e.g. 02:30 on a spring-forward
  night): the instant is **shifted forward by the length of the gap**. In
  `America/New_York` 02:30 becomes 03:30 EDT. In `Australia/Lord_Howe`, whose
  gap is 30 minutes, 02:10 becomes 02:40. A whole missing date (`Pacific/Apia`,
  2011-12-30) resolves to the same wall-clock time on the next date that
  exists.
- **Fold** (the civil time happens twice, e.g. 01:30 on a fall-back night):
  the **earlier** instant, the one with the pre-transition offset.

This is the rule `routine_gen` already implements and documents
(`crates/sunrise-domain/src/routine_gen.rs#expand`), and it is what
`TimeZone::to_zoned` does inside `to_instant`. It supersedes ADR-0017's sentence
that a civil time in a gap "falls back rather than failing": the resolution is
not a fallback; it is the defined answer, and it does not depend on whether the
zone name resolved. [`recurrence-engine.md`](../08-features/recurrence-engine.md)
§Edge cases applies this rule and states no other.

**Keys never depend on the rule.** An occurrence key, a skip key, a streak key
and a day-schedule lookup are all **civil**, the intended wall-clock value, and
never the resolved instant. So a spring-forward day's 02:30 occurrence has key
`…T02:30` whether it fires at 03:30 or not, and all three keys agree
([`../02-domain/routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)
§Occurrence key).

**Windows never resolve their bounds.** A civil window (a constraint's time of
day, quiet hours, a wake/sleep pair used as a filter) is evaluated by converting
the **moment** to civil time in the evaluating zone (always unambiguous) and
testing the civil value against the window. So a `02:00–03:00` window is simply
empty on a spring-forward night, and a `01:00–02:00` window matches both passes
through the fold. No window bound is ever disambiguated.

**An unresolvable zone name** (a `zoned` value written by a device with newer
tzdata, naming a zone this build does not know) resolves in the reader's zone
and is flagged `zone_unknown` for display. Its stored value is unchanged.

## 4. "Today", days and weeks

- **A planner day** is defined by the day schedule and the reader's zone
  ([`../02-domain/day-schedule.md`](../02-domain/day-schedule.md) §Planner day).
  With no schedule it is the civil day, midnight to midnight. Every instant
  belongs to exactly one planner day.
- **Today** is the planner day that contains `now` in the reader's zone. It is
  not a rolling 24-hour window.
- **An `all_day` value** occupies its planner day: it starts at that day's
  start and is *missed* at that day's end
  ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md) §Due instant).
- **A week** is seven planner days starting on the `week_start` preference
  (default **Sunday**, [`../02-domain/preferences.md`](../02-domain/preferences.md)).
  The core and every client read the preference. No code hard-codes a first
  weekday.
- **Month and year boundaries** are civil, in the reader's zone.

## 5. Evaluating constraints and requirements

Scheduling constraints ([`../02-domain/scheduling-constraints.md`](../02-domain/scheduling-constraints.md))
are evaluated in:

- the **anchor's zone** for a routine or recurring block whose anchor is
  `zoned`;
- the **reader's zone** for everything else.

The evaluation converts the candidate moment to civil (§3, windows never
resolve their bounds) and tests each dimension against the civil value. A
window whose end is before its start wraps midnight and ends on the next civil
day.

## 6. Recurrence is expanded in civil space

A rule is expanded over **civil date-times**, starting from the anchor's civil
value, with no zone involved. The expansion yields occurrence keys. A key is
resolved to an instant only when something needs one (a reminder, a calendar
position, a comparison with `now`), and it resolves in:

- the anchor's `tz` for a `zoned` anchor;
- the reader's zone for a `floating` anchor, so a "09:00 wherever I am" habit
  fires at 09:00 local on every device in every zone.

Because expansion is zone-free, `RRule.until` and `Routine.ends_at` (always
`floating` or `all_day`) and a `floating` `paused_until` are compared against
keys in civil space. An `all_day` `paused_until` of `d` follows §1 instead: the
pause ends at the start of planner day `d`, so a key is paused when it resolves
before `boundary(d)`. Changing a routine's zone changes when occurrences fire but
never which occurrences exist or what their keys are. This closes ADR-0017's
first revisit trigger ("recurring blocks or tasks with a floating anchor").

## 7. Time-zone changes

- **Observed.** Each client observes the OS zone change
  (`NSSystemTimeZoneDidChange` on Apple platforms, the equivalent broadcast
  elsewhere) and also compares the zone on every foreground. On a change it
  calls the one core entry point `on_time_zone_changed(zone)`.
- **Re-evaluated.** The core recomputes everything derived from the reader's
  zone: Today and the planner day, lateness and the triage queue, constraint
  violations on open planned tasks, and the reminder schedule. A `floating`
  09:00 reminder moves to 09:00 in the new zone; a `zoned` one does not.
  Nothing stored is rewritten. A zone change is not an edit.
- **Hard violations surface.** A planned task that now violates a `hard`
  constraint in the new zone is flagged in views and the triage queue. It is
  never moved automatically.
- **Optional notification.** When `notifications.timezone_changed.enabled` is
  true (default **false**,
  [`../08-features/notifications.md`](../08-features/notifications.md)), the device posts one local notification naming the new zone and
  the count of tasks whose time or status changed. When `home_timezone` is set
  and differs from the new zone, views show times in both zones for `zoned`
  values.

## 8. Test matrix

Every row runs against tasks (lateness, Today membership), blocks (overlap,
week view), routines (expansion, key, skip, streak), constraints (including a
wrapping window), and the day schedule (a sleep time past midnight). Clock and
zone are injected; no test reads the host zone.

| Zone | Transitions and properties exercised |
|---|---|
| `UTC` | the identity case; `index_ms` equals the resolved instant for every kind |
| `America/New_York` | spring gap 2026-03-08 02:00→03:00; fall fold 2026-11-01 01:00–02:00 twice |
| `Australia/Lord_Howe` | 30-minute DST: gap 2026-10-04 02:00→02:30, fold 2026-04-05 01:30–02:00 twice |
| `Pacific/Apia` | the date-line move: civil date 2011-12-30 does not exist; an `all_day` value on it, a daily routine across it, and a planner day of zero length |
| `Asia/Kolkata` | a +05:30 offset with no DST; a floating value read here and in `UTC` |
| `Pacific/Kiritimati` / `Pacific/Honolulu` | the +14 and −10 extremes; the 48-hour prefilter still admits every row the exact filter keeps |

Travel scenarios, each asserting that nothing stored changes and every derived
value is recomputed:

| Scenario | Asserts |
|---|---|
| New York → Kolkata mid-day | a `floating` 09:00 task moves to 09:00 IST; a `zoned` New York task stays at its instant; Today is the Kolkata planner day |
| Los Angeles → New York with a `zoned` 09:00 LA routine | occurrences still fire at 09:00 LA (12:00 NY); keys unchanged |
| Los Angeles → New York with a `floating` 09:00 routine | occurrences fire at 09:00 NY; keys unchanged; no occurrence duplicated or lost on the travel day |
| `Pacific/Apia` ↔ `Pacific/Pago_Pago` (same longitude, 24 hours apart) | an `all_day` deadline is late in one zone and on track in the other at the same instant, and both answers are the reader's |
| A zone change on a spring-forward night | a `hard` constraint window that became empty is flagged, and nothing is rewritten |
