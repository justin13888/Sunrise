---
status: accepted
---

# Scheduling Constraints

A **scheduling constraint** is a requirement window restricting *when* a Task (or a Task materialized from a Routine) should be scheduled or executed. It captures user intents like "only on weekday mornings," "not during the trip (Aug 3–17)," or "any time after 6pm." Each constraint carries a `severity`: a `hard` constraint blocks auto-scheduling and fails validation when the user schedules against it; a `soft` constraint only demotes ranking in planning views. Constraints are a value type on Task and Routine — not a standalone entity, and they mint no ID.

## Fields

```cddl
; Carried on Task and Routine as an optional list (max 16); the whole list is
; one LWW register. See tasks.md and routines-and-recurrence.md.
SchedulingConstraint = {
    ? time_of_day:  { start: civil-time, end: civil-time },   ; local wall-clock; start < end (no midnight wrap in v1)
    ? days_of_week: [* Weekday],                              ; set of weekdays; empty/absent = all days
    ? date_range:   { start: civil-date, ? end: civil-date }, ; inclusive; open-ended if end absent
    severity:       ConstraintSeverity,                       ; required
}

ConstraintSeverity = "hard" / "soft"

Weekday = "MO" / "TU" / "WE" / "TH" / "FR" / "SA" / "SU"

; Civil (wall-clock) types serialize as jiff civil strings; see
; ../11-adr/0011-datetime-jiff.md. We use the full seconds form to match
; jiff::civil::Time / jiff::civil::Date serialization.
civil-time = tstr .regexp "([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]"   ; "HH:MM:SS"
civil-date = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}"                 ; "YYYY-MM-DD"
```

Each constraint restricts along up to three independent **window dimensions** — time of day, days of week, and date range. At least one dimension MUST be present; a constraint with only a `severity` is meaningless and rejected at validation.

## Semantics

### Combination

- Multiple constraints of the **same kind** (the same set of populated dimensions) **OR** together — any one satisfied means the group is satisfied. This expresses "weekday mornings *or* weekend afternoons."
- Constraints with **different dimensions** **AND** together — all must hold. "On weekdays" AND "after 6pm" AND "before the trip ends" must all be satisfied simultaneously.

### Hard vs. soft

- A `hard` violation **blocks** auto-scheduling and **fails validation** when the user schedules a Task against it; the UI surfaces which constraint was violated and why.
- A `soft` violation never blocks. It only **demotes ranking** in planning views (see [`../08-features/planning-views.md`](../08-features/planning-views.md)) so the item sinks rather than disappears.

### Evaluation timezone

Window dimensions are evaluated in local wall-clock terms, and the "local" zone depends on the evaluating entity:

- **Routine:** the Routine's own IANA `timezone` (the same zone its `rrule` is interpreted in).
- **Task:** the device-local timezone at evaluation time.

`time_of_day` and `date_range` are civil (zone-less) values; the evaluating zone above is what pins them to instants. `days_of_week` is likewise computed against the evaluating zone's calendar day.

## CRDT mapping

The **entire list** of `SchedulingConstraint` values on a Task or Routine is a **single LWW register** (whole-list replace on `(timestamp, device_id)`; see [`../05-sync/crdt-design.md`](../05-sync/crdt-design.md)). Constraints are always edited as a unit in the UI — there is no per-constraint identity, no add/remove of individual entries. Modeling this as an OR-Set would buy nothing (users never concurrently mutate individual entries) and would complicate convergence; whole-list LWW converges trivially and matches the editing model.

## Storage projection

The list is stored as a single canonical-CBOR blob column `scheduling_constraints BLOB` on the `tasks` and `routines` tables (`NULL` = empty). It is a **projection** — rebuildable from the op log — and is opaque to SQL in v1 (no querying against individual dimensions). See [`../04-storage/local-database.md`](../04-storage/local-database.md).

## Validation

- A Task/Routine MAY carry **at most 16** constraints.
- Each constraint MUST populate **at least one** window dimension (`time_of_day`, `days_of_week`, or `date_range`).
- `time_of_day.start` MUST be strictly `< time_of_day.end`; **no midnight wrap** in v1.
- `date_range.start` MUST be `≤ date_range.end` when `end` is present.
- `days_of_week` entries MUST be distinct `Weekday` tokens; an empty set means "all days."
- Scheduling a Task at an instant that violates any `hard` constraint is rejected at submit time; `soft` violations pass validation and only affect ranking.

## See also

- [`./tasks.md`](./tasks.md) — the `scheduling_constraints` field on Task and deadline semantics.
- [`./routines-and-recurrence.md`](./routines-and-recurrence.md) — how constraints propagate to materialized occurrences.
- [`../08-features/planning-views.md`](../08-features/planning-views.md) — where `soft` violations affect ranking.
- [`../08-features/time-blocking.md`](../08-features/time-blocking.md) — scheduling Tasks into Blocks against these windows.
