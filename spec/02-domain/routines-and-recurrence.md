---
status: draft
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

A background job in the core looks ahead by `materialization_horizon` (default: 14 days for daily, 60 days for weekly+, configurable). For each occurrence in the horizon that has no existing Task:

1. Compute occurrence datetime in the routine's tz.
2. Apply `skip_dates`.
3. If `catchup_policy = skip` and the occurrence is in the past beyond a grace window, drop it.
4. Otherwise, create a Task with `routine_id` and `routine_occurrence` set.

Generation is **idempotent** — re-running generates nothing if the Task already exists for that `(routine_id, occurrence)` pair. This is critical because generation runs on every device.

## Editing series vs occurrence

Editing a generated Task only touches that occurrence. Editing the Routine prompts: *"Apply to future occurrences only / All future and past unstarted / Just the routine template."*

## Streak counter

Increments on completion of an occurrence ≤ `grace_window` after the scheduled time. Decrements (or resets to 0, depending on user setting) on skip. Stored as a CRDT PN-counter so concurrent completions on multiple devices do not double-count.

## Pausing

Pausing a Routine stops generation but does not delete already-generated occurrences. `paused_until` auto-unpauses.

## Open questions

> **Open:** Should the streak counter survive a 1-day miss (forgiveness rule)? Default proposal: no streak break for 1 miss/30 days. Configurable per Routine.

> **Open:** "Adaptive cadence" recurrence is a hard fit for a CRDT — two devices completing at slightly different moments produce different "next due" dates. Resolution path: snap "next due" to a coarse grid (e.g. day) and use LWW.
