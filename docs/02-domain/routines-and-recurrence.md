---
status: accepted
---

# Routines and Recurrence

A Routine is a template plus a recurrence rule. It generates Tasks (occurrences) on a schedule. Routines are first-class because multi-stream operators rely heavily on them — gym, journaling, weekly review, paying bills, watering plants.

> **Amended** by [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md)
> (civil anchors, one DST rule, expansion in civil space),
> [ADR-0044](../11-adr/0044-per-field-ops.md) (skip and streak sets merge as
> OR-sets), [ADR-0046](../11-adr/0046-optional-stream.md) (the template's
> stream is optional) and [ADR-0047](../11-adr/0047-deadlines-and-lateness.md)
> (occurrences are planned with `planned_at`; nothing is changed
> automatically). §Status in the tree lists what the code does today.

## Fields

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types), and `stime`
in [`time.md` §1](../10-cross-cutting/time.md#1-every-stored-time-is-a-sunrisetime).

```cddl
Routine = {
    id:                tstr .regexp "rtn_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:        timestamp,
    updated_at:        timestamp,
    template:          TaskTemplate,        ; what each occurrence looks like
    rrule:             RRule,               ; a STRUCTURED MAP, not an RFC 5545 string
    anchor:            stime,               ; "zoned" (default) or "floating": the first occurrence's wall clock
    ends_at?:          stime,               ; "floating" or "all_day"; no occurrence after it
    split_from?:       entity-ref,          ; rtn_ ref of the root of the series this one continues (§Edit scope)
    skipped_keys:      [* occurrence-key],  ; OR-set; the skip list
    streak_keys:       [* occurrence-key],  ; OR-set; occurrences completed within the grace window
    catchup_policy:    CatchupPolicy,
    grace_window_s?:   uint,                ; seconds; absent = the 24h default
    forgiveness_enabled?: bool,             ; default TRUE; only `false` hits the wire
    paused:            bool,
    paused_until?:     stime,               ; the pause ends here; see §Pausing
    scheduling_constraints?: [* SchedulingConstraint], ; copied to each occurrence; one register (max 16)
    archived:          bool,
    deleted:           bool,
    unknown-fields,                         ; see overview.md
}

; The intended civil start of an occurrence, minute precision, no zone.
; It is the single identity of an occurrence: task id, skip, streak and merge
; all use it. See §Occurrence key.
occurrence-key = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}"

TaskTemplate = {
    title:             text<512>,
    stream_id?:        entity-ref,          ; absent = no stream (ADR-0046)
    contexts:          [* entity-ref],
    energy?:           Energy,
    priority?:         1..5,
    estimated_duration_s?: uint,            ; SECONDS, matching Task
    body?:             NoteBody,
    target_offset_s?:  uint,                ; if set, each occurrence's target_at = occurrence + offset
    hard_due_offset_s?: uint,               ; if set, each occurrence's hard_due_at = occurrence + offset
    unknown-fields,
}

; The RRULE is parsed at the edge and stored decomposed. Storing the string
; would put an unnormalized value under a signature: `FREQ=DAILY;INTERVAL=1`
; and `INTERVAL=1;FREQ=DAILY` mean the same rule and encode to different
; bytes, so two devices expressing one intent would disagree byte-for-byte.
; The part names below are the RFC 5545 ones, snake_cased.
RRule = {
    freq:              Frequency,           ; required
    interval:          uint,                ; required on the wire; 1 when unspecified
    by_day?:           [* Weekday],
    by_month_day?:     [* int],             ; negative counts from the month's end
    by_month?:         [* 1..12],
    by_set_pos?:       [* int],
    count?:            uint,
    until?:            stime,               ; "floating" or "all_day", in the anchor's civil frame; inclusive
    wkst?:             Weekday,
    unknown-fields,
}

Frequency = "DAILY" / "WEEKLY" / "MONTHLY" / "YEARLY" / tstr
; Weekday is defined once, in ../10-cross-cutting/time.md §1.

CatchupPolicy = "skip"        ; past open occurrences lapse
              / "merge"       ; past open occurrences collapse into the latest
              / "queue"       ; every past open occurrence stays
              / tstr          ; unknown: preserved; generates as "skip", shows as "queue" (below)
```

**Removed from the previous shape.** `timezone` and `starts_at` are replaced by
`anchor` (a zoned anchor carries its own zone). `skip_dates` is gone: its
entries migrate into `skipped_keys` (below). `streak_counter`,
`last_completed_at`, `streak_started_at` and `forgivenesses_in_window` are no
longer stored: they are derived from `streak_keys` (§Streak), because a stored
counter beside the set it counts is two values that can disagree.

**Unknown enum values are preserved, and a rule that cannot be read generates
nothing.** An unknown `Frequency` or `Weekday` round-trips byte for byte
([#321](https://github.com/justin13888/Sunrise/issues/321)). Recurring on the wrong schedule is worse than not recurring, so a
routine whose rule contains an unknown value generates no occurrences on this
build and is flagged: its summary reads "unrecognised schedule (…)" with the
rule's RFC 5545 text in the parentheses
(`crates/sunrise-domain/src/rrule.rs#rrule_summary`), and clients see the
`.unknown` case on the rule's `Frequency` or `Weekday`. The op that carried it
is never rejected.

**An unknown `CatchupPolicy` generates as `skip` and shows as `queue`.** It
round-trips byte for byte. At generation (§Generation, step 3) it writes
nothing for a past key: this build must not author a backlog of tasks under a
policy it cannot read, and a build that can read it generates what that policy
asks for, which then syncs here. At read time (§Catch-up for occurrences
already materialized) it lapses nothing: an occurrence a newer build chose to
generate is shown as an ordinary task, not hidden.

## Occurrence key

**One key per occurrence, used everywhere.** The key is the occurrence's
**intended civil start**, `YYYY-MM-DDTHH:MM`, as produced by expanding the rule
in civil space ([`time.md` §6](../10-cross-cutting/time.md#6-recurrence-is-expanded-in-civil-space)).
It is never derived from the resolved instant. So on a spring-forward night a
02:30 occurrence has key `…T02:30` while it fires at 03:30, and:

- the materialized task's id is `occurrence_task_id(series_root, key)`;
- a skip adds `key` to `skipped_keys`;
- an on-time completion adds `key` to `streak_keys`;
- the task records the occurrence's instant in `routine_occurrence`, and the key
  is derived from the task id.

A key is resolved to an instant only when something needs an instant: in the
anchor's `tz` for a `zoned` anchor, and in the reader's zone for a `floating`
anchor, by the DST rule in [`time.md` §3](../10-cross-cutting/time.md#3-one-dst-rule).

**`series_root`** is the id of the first routine in a chain of splits
(§Edit scope): a routine with no `split_from` is its own root, and a split
routine's `split_from` names the root directly
(`crates/sunrise-domain/src/routine_gen/edit.rs#series_root`). Keying task ids
on the root is what lets a split series adopt occurrences its predecessor
already materialized instead of duplicating them, and naming the root rather
than the routine split means no reader walks a chain whose links may not have
arrived.

A task's `routine_id` names the routine that owns its key: the one that
generated it, or the continuation that adopted it at a split. The streak and
the skip of an occurrence go to that routine.

### Anchors are civil

The anchor is a wall-clock time, not an instant. Changing a routine's zone
changes the anchor's `tz` and nothing else, so a 09:00 Los Angeles routine
moved to New York is a 09:00 New York routine. A `floating` anchor ("09:00
wherever I am") resolves each occurrence in the reader's zone, so it fires at
09:00 local on every device, and its keys, task ids and streak are identical
across devices because they are civil.

## Skip list

`skipped_keys` is an **OR-set** of occurrence keys
([ADR-0044](../11-adr/0044-per-field-ops.md)):

- `Command::SkipRoutineOccurrence { routine, key }` adds the key.
- `Command::UnskipRoutineOccurrence { routine, key }` removes it, so a skip is
  no longer permanent. Two devices skipping and unskipping concurrently resolve
  add-wins.
- A skipped key generates no task. If the occurrence was already materialized
  and is untouched, skipping it also tombstones that task in the same command;
  a touched one is left alone and shown as skipped.

**Migration.** The deprecated `skip_dates` instants are converted to keys by
expanding the rule over each instant's civil day in the routine's zone and
taking the occurrence whose task id matches; an entry that matches no
occurrence is dropped with a `core.routine.skip_date_unmatched` log line. iCal
`EXDATE` values map to keys the same way.

## Recurrence rule

We use **RFC 5545 RRULE** as the baseline. Supported parts: `FREQ`, `INTERVAL`,
`BYDAY`, `BYMONTHDAY`, `BYMONTH`, `BYSETPOS`, `COUNT`, `UNTIL`, `WKST`.
`BYYEARDAY` and `BYWEEKNO` are not supported, and `RRuleParseError::UnknownPart`
rejects them at the edge rather than silently dropping them.

- **Expansion is civil.** The rule is expanded over civil date-times from the
  anchor's civil value, with no zone. `until`, `count` and `ends_at` are
  applied to keys in civil space: an `all_day` `until` includes every
  occurrence on that date.
- **An iCal `UNTIL` in UTC** is converted at import into the anchor's civil
  frame (resolved in the anchor's zone), because a rule's end is a property of
  the series' wall clock.
- `EXDATE` maps to `skipped_keys`. `RDATE` is not supported, and import reports
  it with an `int.import.rrule_lossy` warning.

The iCal importer imports a `VEVENT` as a **Block**, never as a Routine; a
recurring event becomes a recurring Block ([`time-blocks.md`](./time-blocks.md)
§Recurring blocks), which shares this expander.

Sunrise extensions, specified and **not** modelled (each would add a field):

- **Floating windows.** "Within a 3-day window starting Monday." Useful for
  non-anchored habits ("3 workouts a week, any 3 days").
- **Adaptive cadence.** "Every X days since last completion" rather than
  calendar dates (§Adaptive cadence).

## Generation

A job in the core materializes occurrences up to a look-ahead horizon, a pure
function of `FREQ` (`materialization_horizon_days` in
`crates/sunrise-domain/src/routine.rs`):

| FREQ | Horizon |
|---|---|
| `DAILY` | 14 days |
| `WEEKLY` | 60 days |
| `MONTHLY` | 180 days |
| `YEARLY` | 540 days |

For each key in `[watermark, now + horizon]`, in the series that owns the key
(§Edit scope):

1. Skip it if it is in `skipped_keys`, or the routine is paused at that key
   (§Pausing), or the effective stream of the template is paused.
2. Skip it if a task with `occurrence_task_id(series_root, key)` exists, live
   or tombstoned. **Generation is idempotent**, which is critical because it
   runs on every device.
3. If the key is already in the past beyond the grace window (a device that was
   offline, or a routine created with a past anchor), apply the catch-up policy
   at generation: `skip` generates nothing; `merge` generates only the latest
   such key; `queue` generates every one.
4. Otherwise create the Task from the template, with `routine_id =
   series_root`, `routine_occurrence` = the key's instant, `planned_at` = the key in the
   anchor's kind (`zoned` with the anchor's `tz`, or `floating`), `target_at`
   and `hard_due_at` from the template's offsets if set, and the template's
   `stream_id` resolved through `effective_stream`
   ([ADR-0046](../11-adr/0046-optional-stream.md) §4).

Tasks are never generated into a deleted stream: a template naming a deleted
stream generates into whatever the tombstone re-homes it to, or into no stream.

### Catch-up for occurrences already materialized

The watermark advances past occurrences once they are generated, so the rule
above cannot reach an occurrence generated in advance and then left open. The
catch-up policy therefore **also** applies to open occurrences at read time, as
a derived state that writes nothing (nothing changes a task automatically,
[ADR-0047](../11-adr/0047-deadlines-and-lateness.md) §4):

An occurrence is **open** while it is untouched (§Template propagation) and
neither started, done nor cancelled, and **past** when its plan's resolved
instant plus the grace window is before `now`
(`crates/sunrise-core/src/engine/routine.rs#catchup_now`). For the routine's
past open occurrences, oldest first
(`crates/sunrise-domain/src/routine_gen/edit.rs#catchup_view`):

| Policy | Views show |
|---|---|
| `skip` | none of them. Each is **lapsed**: hidden from Today, lists and the triage queue, counted as missed for the streak, and listed under the routine's "Lapsed" with a bulk Drop. |
| `merge` | only the latest, titled with a ` (xN catch-up)` suffix computed at read time (the suffix generation's `merge` writes too); the others are lapsed. |
| `queue` | all of them, as ordinary tasks. |

A lapsed occurrence that the user edits, completes or drops is no longer open
and leaves the lapsed set by the same predicate. Today (and so the Today
view), the stream lists, the context lists and the triage queue
([ADR-0047](../11-adr/0047-deadlines-and-lateness.md) §3) apply it; `Query::LapsedOccurrences`
lists the lapsed set for the routine's "Lapsed".

### Scheduling constraints on occurrences

A Routine's `scheduling_constraints` are **copied** onto each generated Task.
The `rrule` decides *when* occurrences exist; constraints annotate and validate
the planning of the resulting Tasks, evaluated in the anchor's zone for a
`zoned` anchor ([`scheduling-constraints.md`](./scheduling-constraints.md)
§Evaluation zone). An occurrence that violates a `hard` constraint is still
generated, never silently dropped, and carries the **persisted** flag
`Task.hard_constraint_violated` (#331):

- Generation sets it, by evaluating the occurrence's plan against its copied
  constraints (`crates/sunrise-core/src/engine/routine.rs#build_routine_task`).
- A template or constraint propagation (§Template propagation) and a split's
  adoption re-evaluate it on the untouched occurrences they write.
- A plan the user chooses clears it: a user's plan that breaks a `hard`
  constraint is refused, so after any re-plan the occurrence breaks none.
- It is one quiet register, absent on the wire when false. A build before
  `DOC_SCHEMA_V` 22 keeps it as an unknown field, so it needs no feature.

It is stored, not derived, because it records what generation did: a reader
can tell "the routine made this occurrence against its own rule" from "the
user planned this" without re-deriving the generation. The read-time check of
a plan against constraints (`Engine::hard_constraint_violations`, which a zone
change re-runs) is separate and still derived.

## Edit scope

Editing a generated Task touches only that task. Editing the Routine takes an
explicit scope:

```rust
enum EditScope {
    This { key: OccurrenceKey },           // one occurrence
    ThisAndFuture { key: OccurrenceKey },  // split the series at `key`
    All,                                   // the whole series
}
```

- **This** writes the change onto that occurrence's Task only (materializing it
  first if it is inside the horizon but not yet generated). A time change moves
  its `planned_at`; the occurrence keeps its key.
- **This and future** splits the series. In one command, written as one
  group ([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) §6):
  1. the old routine's `rrule.until` is set to the civil start of the last
     occurrence before `key` (or, if there is none, the old routine is
     archived);
  2. a new routine is created with `split_from` = the series root, `anchor` =
     `key` in the anchor's kind (with the edit applied), the edited template
     and rule, a `count` reduced by the occurrences the old routine keeps, and
     the old routine's `skipped_keys` and `streak_keys` at or after `key`;
  3. the new routine's id is derived from `(series_root, key)`
     (`split_routine_id`), so two devices that split the same series at the
     same occurrence create **one** routine and merge its fields, and a second
     split at the same key edits that routine;
  4. of the old routine's occurrences at or after `key`, each untouched open
     one the new routine has is **adopted**: same task id (both are keyed on
     the root), `routine_id` = the new routine, the new template written onto
     it. Each untouched one the new routine does not have (its time or rule
     moved) is tombstoned. A touched one is left as it is.
  The key must be one the series produces; any other is refused. A split
  carries `split_from`, which the feature `routine.series` gates: it is
  refused while a paired device has not advertised it, since that device
  would key the new routine's occurrences on its own id and generate them a
  second time. The new routine's streak starts at the split.
- **All** edits the routine itself.

`Command::UpdateRoutine { id, scope, patch }` carries the scope; an absent
scope is **All**. **This** takes only the template, the anchor's time and the
constraint list (`RoutinePatch::is_occurrence_edit`), and returns the task;
**This and future** returns the new routine.

**Which series owns a key.** Keys are identified against the series root, so a
split can leave two routines whose ranges overlap after a concurrent edit (two
devices splitting at different keys). For each key, the owning routine is the
live routine in the root's chain with the **latest anchor at or before the
key**, ties broken by id. Only the owner's template and rule apply to that key,
so every replica generates each key once, from one template.

### Template propagation

A template edit with scope *This and future* or *All* propagates to occurrences
that already exist:

- It reaches every **open** occurrence in scope (for *All*, past open ones
  included) and never a completed or cancelled one.
- It is **per occurrence**: it reaches an occurrence only while the
  occurrence is **untouched**, which is to have no version but the routine
  engine's own: its generated create, and any propagation, adoption or re-pin
  since, all `"origin": "generated"`
  ([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) §1, §3;
  `UNTOUCHED` in `crates/sunrise-core/src/engine/routine.rs`). An occurrence
  the user edited in any field keeps every field as it is. A propagation is
  written as a generated `Patch` of the fields it changes, so it yields to a
  concurrent user edit and leaves the occurrence untouched for the next one.
  In a vault that still writes full-state ops (ADR-0065 D9) a propagation is a
  full-state `task.update`, which the fold reads as a user's write: an
  occurrence is reached once there, and is touched after.
- The fields it writes are the template's (`title`, `body`, `stream_id`,
  `contexts`, `priority`, `energy`, `estimated_duration_s`) and the
  constraint list, and it re-evaluates the hard-constraint flag.
- A rule or anchor change re-keys nothing. An untouched open occurrence whose
  key is no longer produced by the new rule is tombstoned; a touched one stays
  as an ordinary task, keeps its `routine_id`, and is shown as "no longer in
  the series".
- Propagation writes are made by the device that issued the edit, in the same
  transaction as the edit.

## Streak

A streak is **derived**, never stored. Its only stored input is `streak_keys`,
an OR-set of the occurrence keys completed within the grace window:

- An occurrence's key is added on its **first** transition to `done` whose
  `completed_at` is within `grace_window_s` after the occurrence's resolved
  start (absent: the 24-hour default `DEFAULT_GRACE_WINDOW_S`; any value is
  clamped to 7 days, `MAX_GRACE_WINDOW_S`). A backdated `completed_at`
  ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)) counts by its own
  value, so a late click does not break a streak the work did not break. An
  `all_day` `completed_at` counts if that date is the occurrence's planner
  date.
- A later `done → todo → done` finds the key present and adds nothing.
- Two devices completing different occurrences concurrently both add their
  keys, and the OR-set keeps both. That is the fix for the previous
  whole-row merge, which lost one.

```rust
fn streak(routine: &Routine, now: Timestamp, zone: &TimeZone) -> StreakState
struct StreakState { current: u32, started_at: Option<OccurrenceKey>, last_completed: Option<OccurrenceKey>, forgiveness_used: u32 }
```

`streak` walks the series' expected keys (the rule's expansion across the
split chain, minus `skipped_keys` and paused keys) up to the last key whose
grace window has closed at `now`, newest first, and counts consecutive keys in
`streak_keys`. A key not in `streak_keys` ends the streak, unless forgiveness
covers it:

- **Forgiveness** (enabled by default, per routine) excuses at most
  `FORGIVENESS_ALLOWANCE` (1) missed key in any `FORGIVENESS_WINDOW_S`
  (30 days) window of the walk, both constants in
  `crates/sunrise-domain/src/streak.rs`.
- Because the walk is a pure function of the keys, the forgiveness window no
  longer needs a stored anchor or a stored usage count, and two devices can
  never disagree about how much forgiveness remains.

A streak is deliberately not a PN-counter: a streak resets, and a counter that
must be reset by subtracting its current value is not convergent under
concurrent completions. A set of keys is.

**What the code does today** ([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md)
§7, D17). The streak fields are still stored, and two ways write them:

- A full-state routine op carries them, computed by the completion rule in
  `crates/sunrise-domain/src/streak.rs` (`record_occurrence_completion`),
  which anchors the forgiveness window at `streak_started_at`. Until a
  `Patch` writes a key, a routine shows exactly these.
- In a vault that requires `core.field_ops`, an on-time completion is a
  `Patch` adding its key, in one group with the completion, and a late one
  writes nothing. Once a `Patch` has
  written `streak_keys` or `skipped_keys`, `project` derives the four fields
  from the keys (`Routine::derived_streak`), oldest occurrence first up to the
  newest key: a keyed occurrence counts, a skipped one is neutral, and any
  other spends the forgiveness allowance or ends the run, the window anchored
  as above. A read (`Query::Routine(s)`, the weekly review, the streak export)
  then applies as misses the occurrences after the newest key whose grace
  window has closed (`Routine::streak_as_of`).

The walk newest-first over expected keys, with no stored anchor, described
above, is the target; the read-time miss rule is the part of it that ships.

## Pausing

A routine is paused at key `k` when `paused` is true and either `paused_until`
is absent or `k` is before `paused_until`, compared by the rule in
[`time.md`](../10-cross-cutting/time.md) §1 and §6: in the anchor's civil frame
for a `floating` value, by resolved instant for `instant` and `zoned` ones, and
for an `all_day` value of `d`, against the start of planner day `d` (a key is
paused when it resolves before `boundary(d)`). Paused keys are not generated and are not expected by the
streak. Nothing writes `paused = false` when the pause ends: the comparison
simply stops holding. Already-generated occurrences are not deleted by a
pause.

## Adaptive cadence — convergence rule

> **Not modelled.** Adaptive cadence has no field (see §Recurrence rule). This
> records the rule it must follow when it lands.

Adaptive-cadence routines compute "next due" from the most recent completion.
To converge under concurrent completions on multiple devices:

1. The most recent completion is **derived** as the latest key in
   `streak_keys` (or among completed occurrences), not stored.
2. The next occurrence is derived from it and the cadence interval, snapped to
   the start of the planner day in the anchor's zone.
3. Two devices completing within the same planner day produce the same snapped
   result.

## Merge mapping

| Field | Merge |
|---|---|
| scalars (`anchor`, `ends_at`, `catchup_policy`, `grace_window_s`, `forgiveness_enabled`, `paused`, `paused_until`, `archived`, …) | per-field LWW register |
| `template` fields, `rrule` | one register each for `template.*` field and for `rrule` as a whole (a rule is edited as a unit) |
| `scheduling_constraints` | one register for the list |
| `skipped_keys`, `streak_keys` | add/remove OR-sets |
| streak, lapsed, catch-up title | derived; not stored |

## Status in the tree

The shape above is the design of record. A legacy vault merges a Routine as one
last-writer-wins unit; a `core.field_ops` vault merges it per field. What the
tree stores differently, or does not do yet, is below:

- The anchor is still `starts_at` beside a `timezone` string rather than one
  `anchor` field, but it is a `SunriseTime` (#506): `floating` by default,
  `zoned` when pinned, expanded in civil space, and a zone change keeps its
  wall clock (`crates/sunrise-domain/src/routine.rs#rezone`,
  `crates/sunrise-domain/src/routine_gen.rs#expand`). A bare instant written
  before `DOC_SCHEMA_V` 15 reads as zoned in `timezone`
  ([`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) §1).
- `paused_until` is honoured as §Pausing says
  (`crates/sunrise-domain/src/routine_gen/series.rs#is_paused_at`).
- Catch-up applies at generation to keys first scanned in the past, and at
  read time to occurrences generated earlier and left open
  (`crates/sunrise-core/src/engine/routine.rs#catchup_now`, #331), as
  §Catch-up for occurrences already materialized says.
- The streak key, the skip and the task id share the intended key: the streak
  recovers it from the task id
  (`crates/sunrise-domain/src/routine_gen/series.rs#occurrence_key_of`).
- In a field-ops vault a completion adds its key to the `streak_keys`
  OR-set, so concurrent completions of different occurrences both count
  ([ADR-0065](../11-adr/0065-copy-on-write-entity-versions.md) D17); §Streak
  says what is still stored.
- Edit scopes, the split and template propagation are as §Edit scope says
  (`Command::UpdateRoutine`'s `scope`, `crates/sunrise-domain/src/routine_gen/edit.rs`),
  and an occurrence that breaks a `hard` constraint carries
  `Task.hard_constraint_violated`. A split routine's streak starts at the
  split rather than walking the chain (§Streak), and undo does not reverse a
  split or an edit of one occurrence.
- There is no un-skip, and `skip_dates` is still read.
- Unknown enum values already follow §Fields: an unknown `Frequency` or
  `Weekday` no longer fails the op, the rule is kept and generates nothing, and
  its flag is the rule's summary rather than a field of its own
  (`crates/sunrise-domain/src/rrule.rs#rrule_summary`). Every rule is also
  stored as CBOR in `routines.rrule_cbor` and read from there first, because
  the RFC 5545 text cannot carry a raw token containing `;`, `,` or `=`.
