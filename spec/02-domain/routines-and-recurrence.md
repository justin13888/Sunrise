---
status: accepted
---

# Routines and Recurrence

A Routine is a template plus a recurrence rule. It generates Tasks (occurrences) on a schedule. Routines are first-class because multi-stream operators rely heavily on them — gym, journaling, weekly review, paying bills, watering plants.

## Fields

```cddl
Routine = {
    id:                tstr .regexp "rtn_[A-Z0-9]{26}",
    created_at:        tdate,
    updated_at:        tdate,
    template:          TaskTemplate,        ; what each occurrence looks like
    rrule:             text,                ; RFC 5545 RRULE string (extended; see below)
    timezone:          text,                ; IANA tz; rrule is interpreted in this tz
    starts_at:         tdate,
    ends_at?:          tdate,               ; routine sunsets after this
    skip_dates:        [* tdate],           ; explicit skip overrides
    catchup_policy:    CatchupPolicy,
    streak_counter:    pn-counter,
    last_completed_at?: tdate,
    paused:            bool,
    paused_until?:     tdate,
    archived:          bool,
    deleted:           bool,
}

TaskTemplate = {
    title:             text<512>,
    stream_id:         tstr,
    contexts:          [* tstr],
    energy?:           Energy,
    priority?:         1..5,
    estimated_duration?: duration,
    body?:             NoteBody,
}

CatchupPolicy = "skip"        ; missed occurrences are dropped
              / "merge"       ; missed occurrences collapse into one task
              / "queue"       ; each missed occurrence becomes a separate task
```

## Recurrence rule

We use **RFC 5545 RRULE** (the iCalendar standard) as a baseline. Supported parts: `FREQ`, `INTERVAL`, `BYDAY`, `BYMONTHDAY`, `BYMONTH`, `BYSETPOS`, `COUNT`, `UNTIL`, `WKST`. Not supported in v1: `BYYEARDAY`, `BYWEEKNO`.

Sunrise extensions (carried in a parallel field, not in the RRULE string itself):

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

The horizon is a Routine field (LWW-register, user-editable in the Routine settings UI; range 7–730 days, clamped on write). For each occurrence in the horizon that has no existing Task:

1. Compute occurrence datetime in the routine's tz.
2. Apply `skip_dates`.
3. If `catchup_policy = skip` and the occurrence is in the past beyond a grace window, drop it.
4. Otherwise, create a Task with `routine_id` and `routine_occurrence` set.

Generation is **idempotent** — re-running generates nothing if the Task already exists for that `(routine_id, occurrence)` pair. This is critical because generation runs on every device.

## Editing series vs occurrence

Editing a generated Task only touches that occurrence. Editing the Routine prompts: *"Apply to future occurrences only / All future and past unstarted / Just the routine template."*

## Streak counter

Increments on completion of an occurrence within `grace_window` after the scheduled time. The `grace_window` is a per-Routine field (default: 24 hours; range 0–7 days, LWW). Stored as a CRDT PN-counter so concurrent completions on multiple devices do not double-count (the inner-Op `op_id` makes increments idempotent).

Streak resets to 0 on a missed occurrence with one exception: the **forgiveness rule** allows up to one missed occurrence per 30-day rolling window without resetting. The forgiveness rule is enabled by default and toggled per Routine.

## Pausing

Pausing a Routine stops generation but does not delete already-generated occurrences. `paused_until` auto-unpauses.

## Adaptive cadence — convergence rule

Adaptive-cadence routines compute "next due" from the most recent completion. To converge cleanly under concurrent completions on multiple devices, the rule is:

1. The Routine's `last_completed_at` is an LWW-register on `(timestamp, device_id)`.
2. The "next due" date is **derived** from `last_completed_at` and the cadence interval, snapped to the start of the local day in the Routine's `timezone`.
3. Two devices completing within the same local day produce the same snapped result; later concurrent completions LWW.
