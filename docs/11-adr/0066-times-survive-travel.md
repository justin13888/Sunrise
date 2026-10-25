# 0066 — Routines follow the traveller by default, and no stored time is ever re-derived from a zone

**Status:** accepted

**Amends** [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §1, in
the "Which kind each field uses" table: the default kind of a routine or
recurring-block anchor. It adds four rules that time.md implies but does not
state (§2–§5 below). The rest of time.md stands and still wins over any other
document.

**Builds on** [ADR-0017](./0017-sunrise-time-representation.md),
[ADR-0011](./0011-datetime-jiff.md) and
[ADR-0065](./0065-copy-on-write-entity-versions.md). Time fields are *tracked*
there, so a concurrent edit of a time by two devices in different zones is
kept and surfaced, never silently lost.

**Tracked by**
[#506](https://github.com/justin13888/Sunrise/issues/506) (routine times are
bare instants),
[#331](https://github.com/justin13888/Sunrise/issues/331) (routine anchors,
pauses, DST streak keys),
[#334](https://github.com/justin13888/Sunrise/issues/334) (defer),
[#333](https://github.com/justin13888/Sunrise/issues/333) (constraints after a
zone change) and
[#520](https://github.com/justin13888/Sunrise/issues/520) (reminders after a
zone change).

## Context

Sunrise must be robust for someone who changes time zones often: a weekly
flight, a remote worker who follows the sun, a parent who splits weeks between
two cities. For them, "robust" means three things:

- what they wrote down keeps meaning what they meant;
- nothing moves because they moved, unless it was meant to follow them;
- two of their own devices, briefly in different zones, never disagree about
  what is stored.

The tree breaks each of these:

- **Routine anchors are instants.** A routine created as "daily at 08:00" in
  Los Angeles fires at 05:00 in New York and 01:00 in Berlin
  (`crates/sunrise-domain/src/routine.rs#Routine`).
- **A zoned value's wall-clock time is re-derived from its instant on every
  read.** `SunriseTime::from_parts` (`crates/sunrise-domain/src/time.rs`)
  converts the stored `index_ms` back into the zone. Two consequences follow.
  A tzdata rule change silently moves a stored 09:00. And an unknown zone name
  rebuilds the civil time in UTC, which the next write then re-emits as fact.
- **Failures fall back silently.** An unknown zone becomes UTC
  (`crates/sunrise-core/src/engine/block.rs#device_zone`), and a failed
  conversion becomes the Unix epoch (`SunriseTime::to_instant`). Neither is a
  time anyone meant.
- **Some writes turn a civil time into an instant.** Defer writes an instant
  (`crates/sunrise-core/src/engine/task.rs#defer_task`), and so does a
  materialized occurrence (`crates/sunrise-core/src/engine/routine.rs`). So a
  task the user meant as "tomorrow" stops following them.
- **The CLI reads the host zone itself** (`jiff::tz::TimeZone::system()` in
  `crates/sunrise-cli/src/main.rs`), bypassing the core's injected `Clock`.

## Decision

### 1. A routine follows the traveller unless it is pinned

The default anchor kind for a routine, and for a recurring block, is
**`floating`**. "Daily at 08:00" means 08:00 wherever the user is.

An anchor is **`zoned`** in two cases only:

- the user pins a zone ("08:00 New York time");
- the anchor was imported with an explicit zone (an iCal `TZID`).

This amends time.md §1, which defaulted anchors to `zoned` in the device's
zone at creation. That default is right for a meeting and wrong for a habit,
and habits are what routines are for. Meetings arrive from calendars with a
`TZID`, so they still pin.

The consequences, all under time.md §6:

- **Materialized occurrences inherit the anchor's kind.** An occurrence of a
  floating routine is planned `floating` at its civil key; an occurrence of a
  zoned one is planned `zoned` in the anchor's zone. An occurrence is never an
  instant.
- **Changing a routine's zone keeps its wall-clock time.** Re-pinning
  "08:00 Los Angeles" to New York gives "08:00 New York", not
  "11:00 New York". Keys do not change, so no occurrence is duplicated or
  lost.
- **`ends_at`, `RRule.until` and `paused_until` are `SunriseTime`s** of the
  kinds time.md §1 lists (#506). `paused_until` is honoured, and streak, skip
  and task keys are all the intended civil key (#331).

### 2. A stored wall-clock time is stored, not recomputed

A `zoned` value persists its **civil date-time and its zone name**, beside its
`index_ms`. On read it is rebuilt from those two and nothing else.

- `index_ms` stays a storage key (time.md §2). If tzdata changes the zone's
  rules, `index_ms` may be stale. That is harmless, because no comparison
  reads it without resolving.
- A `zoned` value whose zone name this build cannot resolve keeps its civil
  time and name byte for byte. It reads with `zone_unknown = true` and
  resolves in the reader's zone for display (time.md §3). It is never
  rewritten as UTC.
- The storage change is a `STORAGE_V` migration. It back-fills each `zoned`
  row's civil time from its current `index_ms` and zone. This is the last
  time that derivation runs.

### 3. No time falls back silently

- A conversion that cannot produce an instant returns an error to the caller,
  never the epoch. Every caller decides what that means for its reader:
  - a sort places the value last;
  - a reminder is not scheduled and the plan reports why;
  - a view shows the stored civil text.
- A reported or system zone that cannot be resolved is refused at the entry
  point (`Core::on_time_zone_changed` returns a typed error). The core keeps
  the last zone it could resolve. Only a vault that has never had a zone uses
  UTC, and it reports `zone_unknown`.

### 4. Every write keeps the kind the user meant

- `DeferTask` carries a `SunriseTime`, and defer goes through the same
  validation as every other edit (#334). Deferring a floating "tomorrow 09:00"
  plan by a day gives "the day after, 09:00", floating. Deferring an all-day
  value moves the date. A deferral counts in civil days in the reader's zone,
  never in multiples of 86,400 seconds.
- An editor writes only the fields it changed, and a time it did not change
  keeps its kind. The core provides the draft-to-patch function
  (`sunrise-client-core`), so no client re-derives kinds itself (#515).

### 5. The device's zone comes from one place

The reader's zone is the core's `Clock::timezone`.

- Every client reports it through `Core::on_time_zone_changed`. That includes
  the CLI, which reads `SUNRISE_TZ`, then `--tz`, then the host zone, in that
  order, and reports the result.
- `jiff::tz::TimeZone::system()` is called in exactly one place, the core's
  `SystemClock` (`crates/sunrise-core/src/config.rs`). The determinism gate
  enforces this.

On a zone change, the core re-plans reminders. A reminder's identity is
`(entity, rule)`, never its fire time. So a client replaces each pending
notification whose fire instant moved, and never keeps one at a stale wall
clock (#520). Hard scheduling-constraint windows are re-evaluated in the new
zone and flagged, never moved (#333).

## Alternatives considered

| Option | Why rejected |
|---|---|
| **Keep `zoned` as the routine default** | A traveller's habits drift by their offset every trip, and the fix is a per-routine setting they have to discover. The cost falls on exactly the user this record is for. |
| **Ask "follow me or stay put?" on every zone change** | This interrupts every trip, and the answer is almost always the same per routine. A per-routine pin set once says it better. |
| **Store only `index_ms` and re-derive civil time (status quo)** | Ties the meaning of stored data to the tzdata of whichever build last read it. A tzdata update would be an edit nobody made. |
| **Fall back to UTC or the epoch so reads never fail** | Turns an honest "unknown" into a confident wrong answer, which the next write then persists. |

## Consequences

- The traveller test harness (time.md §8's travel scenarios, extended with an
  offline second device and a scripted itinerary across DST changes) becomes
  the acceptance test for this record. It runs in `sunrise-core` and through
  the CLI against a real relay.
- Existing routines keep their current behaviour: the migration reads each
  existing instant anchor as `zoned` in the routine's stored `timezone`. Only
  new routines default to `floating`. A user switches an existing one with a
  single edit.
- The CLI gains `--tz`, `SUNRISE_TZ` and `sunrise tz`.

## What would force revisiting this

1. **Evidence that most routines are meetings rather than habits.** That would
   come from telemetry on how often pins are set. It would flip the default
   back.
2. **A calendar provider that sends floating recurring events without a
   `TZID` but means a fixed zone.** The importer would then need a per-source
   default.

## Delivered (2026-10)

[#535](https://github.com/justin13888/Sunrise/pull/535) delivered §3, §4's
defer and §5. The routine half (#506, with the routine parts of #331 and the
constraint parts of #333) delivers §1, §2 and the rest of §5:

- **§1.** The routine anchor, end, `UNTIL`, pause and last completion, and a
  stream's pause, are `SunriseTime`s (`DOC_SCHEMA_V` 15). A routine with no
  zone is `floating`; a zone pins it. Expansion is in civil space, and a
  floating routine's occurrences resolve in the reader's zone. An occurrence
  is planned with the anchor's kind. Changing a routine's zone keeps every
  wall-clock time it holds (`Routine::rezone`) and re-pins the untouched
  occurrences already planned. A pause with an expiry generates again once
  it runs out. The streak key is the intended key, recovered from the task
  id (`Routine::occurrence_key_of`), so the task id, a skip and the streak
  agree on a DST-gap day (#331).
- **The forward-compatibility rule.** A bare instant in one of these fields
  reads as `zoned` at its wall clock in the routine's `timezone`, or
  `floating` at its wall clock in UTC when `timezone` does not resolve; a
  pause and a last completion stay instants (time.md §1). An instant is
  still written bare. A routine that holds a time an older build cannot read
  is written only into a vault that requires `routine.civil_times`; until
  every paired device advertises it, a routine every build can read is
  written in the old form, and a new routine with no zone is pinned to its
  zone rather than refused.
- **§2.** Every `SunriseTime` column has a `*_civil` sidecar (migration 0041,
  `STORAGE_V` 41), and a stored value is rebuilt from it and its zone name
  alone. The migration does not derive civil times in SQL, which cannot read
  tzdata: `FOLD_V` 3 rebuilds every row from the op log at the next open,
  and the ops carry each civil value as written.
- **§5, constraints.** A wrapping window (`22:00–02:00`) is valid and belongs
  to the day it starts on; a zone change lists the open planned tasks that
  now break a hard window (`TimeZoneChange::hard_violations`).
- **Since ADR-0065's eighth pull request**, concurrent defers both count
  and concurrent completions of two occurrences of one routine keep both
  streak keys, in a vault that requires `core.field_ops` (ADR-0065 §7; its
  seventh pull request did so between devices that all advertised
  `routine.civil_times`, clarification D16, which the eighth retired): a
  defer is then a `Patch` that adds one to `deferred_count`, and a streak
  advance one that adds its key, from which the streak is derived. In a vault
  still in legacy mode, both stay the full-state ops of before. Edit scopes, template propagation, catch-up of
  occurrences left open, and the hard-constraint flag at generation remain
  #331's.
