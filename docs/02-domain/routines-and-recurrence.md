---
status: accepted
---

# Routines and Recurrence

A Routine is a template plus a recurrence rule. It generates Tasks (occurrences) on a schedule. Routines are first-class because multi-stream operators rely heavily on them — gym, journaling, weekly review, paying bills, watering plants.

## Fields

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

```cddl
Routine = {
    id:                tstr .regexp "rtn_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:        timestamp,
    updated_at:        timestamp,
    template:          TaskTemplate,        ; what each occurrence looks like
    rrule:             RRule,               ; a STRUCTURED MAP, not an RFC 5545 string
    timezone:          tstr,                ; IANA tz; rrule is interpreted in this tz
    starts_at:         timestamp,
    ends_at?:          timestamp,           ; routine sunsets after this
    skip_dates:        [* timestamp],       ; DEPRECATED; see below
    skipped_keys?:     [* occurrence-key],  ; the live skip list; omitted when empty
    catchup_policy:    CatchupPolicy,
    streak_counter:    int,
    last_completed_at?: timestamp,
    grace_window_s?:   uint,                ; seconds; absent = the 24h default
    forgiveness_enabled?: bool,             ; default TRUE; only `false` hits the wire
    streak_started_at?: timestamp,          ; anchor of the streak and its 30-day window
    forgivenesses_in_window?: uint,         ; omitted when 0
    streak_keys?:      [* occurrence-key],  ; occurrences already counted; omitted when empty
    paused:            bool,
    paused_until?:     timestamp,
    scheduling_constraints?: [* SchedulingConstraint], ; copied to each materialized task; whole list is one LWW register (max 16); omitted when empty; see scheduling-constraints.md
    archived:          bool,
    deleted:           bool,
    unknown-fields,                         ; see overview.md
}

; A wall-clock occurrence identifier, minute precision, no zone. Resolved
; against the Routine's own `timezone`. Chosen over an instant because a key
; is immune to tzdb drift; see §Skip list below.
occurrence-key = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}"

TaskTemplate = {
    title:             text<512>,
    stream_id:         entity-ref,
    contexts:          [* entity-ref],
    energy?:           Energy,
    priority?:         1..5,
    estimated_duration_s?: uint,            ; SECONDS, matching Task
    body?:             NoteBody,
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
    until?:            timestamp,
    wkst?:             Weekday,
}

Frequency = "DAILY" / "WEEKLY" / "MONTHLY" / "YEARLY"
Weekday   = "SU" / "MO" / "TU" / "WE" / "TH" / "FR" / "SA"

CatchupPolicy = "skip"        ; missed occurrences are dropped
              / "merge"       ; missed occurrences collapse into one task
              / "queue"       ; each missed occurrence becomes a separate task
```

`Frequency` and `Weekday` are the **only** two enums on the wire without an
unknown-value fallback: every other one degrades to a safe default rather than
rejecting the op it arrived in. Recurring on the wrong schedule is worse than
failing the routine, so these reject. See
[`schema-versioning.md`](./schema-versioning.md).

### Skip list: `skipped_keys` supersedes `skip_dates`

Two representations of "skipped" is one more than can be kept in agreement, so
`skip_dates` is **deprecated** and `skipped_keys` is the live field. Stage A of
the removal (stop reading for new skips) is complete — `SkipRoutineOccurrence`
writes only `skipped_keys`. `skip_dates` survives to read pre-existing
payloads and iCal `EXDATE` imports, and is dropped at
`DOC_SCHEMA_FLOOR = 3`, per
[`../04-storage/migrations.md`](../04-storage/migrations.md) §doc-schema
migrations.

An instant has to be re-resolved against the routine's timezone on every read
and silently stops matching its occurrence when that zone's rules change. A
key does not.

### `merge` semantics

When generation runs and finds N ≥ 2 missed occurrences, `merge` produces exactly **one** task:

- `scheduled_at` and `routine_occurrence` = the **most recent** missed
  occurrence's instant. Both are single values, not lists: `Task.routine_occurrence`
  is one instant, which is also what keys generation's idempotency.
- `title` = the template's title with a ` (xN catch-up)` suffix. There is no
  separate `title_template` field; `TaskTemplate.title` is the template.
- The idempotency key is the **latest missed occurrence's** key, so the merged
  task collides with — and therefore replaces rather than duplicates — the
  occurrence it stands in for, and completing it increments the streak by 1,
  not N. Earlier revisions specified a `":merged:" || sorted_dates_hash` key;
  that would have made the merged task a *fourth* distinct id, generating a
  duplicate whenever the policy changed. Nothing has ever emitted it.

N = 1 is not a merge: the single missed occurrence materializes normally.

## Recurrence rule

We use **RFC 5545 RRULE** (the iCalendar standard) as a baseline. Supported parts: `FREQ`, `INTERVAL`, `BYDAY`, `BYMONTHDAY`, `BYMONTH`, `BYSETPOS`, `COUNT`, `UNTIL`, `WKST`. Not supported in v1: `BYYEARDAY`, `BYWEEKNO`.

The parser accepts the RFC 5545 text form and stores the decomposed `RRule`
map above; `RRuleParseError::UnknownPart` rejects anything outside the
supported set rather than silently dropping it.

`EXDATE` and `RDATE` are separate iCal properties, not RRULE parts. EXDATE maps to `Routine.skip_dates` at import; RDATE is not supported in v1 (import drops it with an `int.import.rrule_lossy` warning).

> Both statements are about an importer that is not reachable in v1.
> `crates/sunrise-integrations` implements an iCal VEVENT subset but nothing
> depends on it — see [`../09-integrations/overview.md`](../09-integrations/overview.md).

Sunrise extensions — **specified, not implemented.** Neither has a field in
`Routine`, and `RRule` has no room for one; both would be a `DOC_SCHEMA_V`
bump:

- **Floating windows.** "Within a 3-day window starting Monday." Useful for non-anchored habits ("3 workouts/week, any 3 days").
- **Adaptive cadence.** "Every X days since last completion" rather than calendar dates.

## Generation

A background job in the core looks ahead by a per-Routine `materialization_horizon`. Defaults by `FREQ`:

| FREQ | Default horizon |
|---|---|
| `DAILY` | 14 days |
| `WEEKLY` | 60 days |
| `MONTHLY` | 180 days |
| `YEARLY` | 540 days |

> **Not a field in v1.** The horizon is a pure function of `FREQ`
> (`materialization_horizon_days` in `crates/sunrise-domain/src/routine.rs`),
> not a stored, user-editable `Routine` field. The paragraph below describes
> the intended editable form; adding it is a `DOC_SCHEMA_V` bump.

The horizon is intended to become a Routine field (user-editable in the Routine settings UI; range 7–730 days, clamped on write). Reducing the horizon **does not** delete already-generated future occurrences (that would discard any user notes/edits on them); increasing the horizon generates new occurrences from `max(now, last_generated_at)` to the new horizon. For each occurrence in the horizon that has no existing Task:

1. Compute occurrence datetime in the routine's tz.
2. Apply the skip list — `skipped_keys` first, then the deprecated `skip_dates`.
3. If `catchup_policy = skip` and the occurrence is in the past beyond a grace window, drop it.
4. Otherwise, create a Task with `routine_id` and `routine_occurrence` set.

Generation is **idempotent** — re-running generates nothing if the Task already exists for that `(routine_id, occurrence)` pair. This is critical because generation runs on every device.

### Scheduling constraints on generated tasks

A Routine's `scheduling_constraints` are **copied verbatim** onto each Task at generation time (evaluated thereafter in the *Task's* device-local tz, per [`scheduling-constraints.md`](./scheduling-constraints.md)). The `rrule` decides *when* occurrences exist; constraints only annotate and validate the *scheduling* of the resulting Tasks. An occurrence that violates a `hard` constraint is still materialized — never silently dropped — but **flagged** so the UI can surface it; the constraint governs scheduling, not existence.

## Editing series vs occurrence

Editing a generated Task only touches that occurrence. Editing the Routine prompts: *"Apply to future occurrences only / All future and past unstarted / Just the routine template."*

## Streak counter

Increments on completion of an occurrence within `grace_window_s` after the
scheduled time. `grace_window_s` is a per-Routine field: absent means the
24-hour default (`DEFAULT_GRACE_WINDOW_S`), and any value is clamped on read to
7 days (`MAX_GRACE_WINDOW_S`).

`streak_counter` is a plain signed integer that merges with the rest of the
Routine row under entity-level LWW
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)). It is **not** a
PN-counter, and entity LWW alone would not stop a double-count — the
idempotency set below is what does.

Idempotency: only the **first** `pending → done` transition for an occurrence
increments the counter. The occurrence's key (`YYYY-MM-DDTHH:MM`, the
`occurrence-key` type above) is appended to `streak_keys`, sorted; the full
idempotency key in prose is this Routine's id joined with the entry.
Subsequent `done → pending → done` transitions on the same occurrence find the
key already present and are no-ops for the streak. Membership is **permanent
(no GC)**, per
[`../08-features/recurrence-engine.md`](../08-features/recurrence-engine.md).

`streak_keys` is a literal sorted list of strings, not a probabilistic
structure: an earlier revision of this spec described an "HLL-flavored set",
and an approximate membership test is the wrong tool here — a false positive
silently drops a real increment, and the exact list costs ~16 bytes per
occurrence.

Two devices completing the same occurrence concurrently both write the same
key, so whichever row wins the LWW carries one copy of it. Two devices
completing *different* occurrences concurrently is where entity LWW bites: one
row wins whole, and the loser's key and increment are dropped from the
projection (they survive in the op log). See ADR-0014 §What we give up.

Streak resets to 0 on a missed occurrence with one exception: the **forgiveness rule** allows up to one missed occurrence per 30-day rolling window without resetting. The forgiveness rule is enabled by default and toggled per Routine.

The 30-day forgiveness window is anchored at **`streak_started_at`**, the timestamp of the first non-failed completion that began the current streak. Sliding behavior:

- When `now - streak_started_at > 30 days`, `streak_started_at` advances to `streak_started_at + 30 days` (re-anchor without resetting the streak).
- Forgiveness is consumed when applied; the counter `forgivenesses_in_window`
  tracks usage and resets to 0 on each anchor advance. The allowance is 1 per
  window (`FORGIVENESS_ALLOWANCE`), and the window is 30 days
  (`FORGIVENESS_WINDOW_S`), both in `crates/sunrise-domain/src/streak.rs`.

## Pausing

Pausing a Routine stops generation but does not delete already-generated occurrences. `paused_until` auto-unpauses.

## Adaptive cadence — convergence rule

> **Not implemented.** Adaptive cadence has no field in `Routine` (see §Recurrence
> rule). This records the rule it must follow when it lands.

Adaptive-cadence routines compute "next due" from the most recent completion. To converge cleanly under concurrent completions on multiple devices, the rule is:

1. The Routine's `last_completed_at` wins by the same key as every other field:
   `(hlc, device_id, seq)` ([ADR-0016](../11-adr/0016-hlc-timestamps.md)).
2. The "next due" date is **derived** from `last_completed_at` and the cadence interval, snapped to the start of the local day in the Routine's `timezone`.
3. Two devices completing within the same local day produce the same snapped result; later concurrent completions LWW.
