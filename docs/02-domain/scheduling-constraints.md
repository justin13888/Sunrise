---
status: accepted
---

# Scheduling Constraints and Dependencies

A **scheduling constraint** is a requirement restricting *when* or *where* a
Task (or a Task materialized from a Routine) should be planned or done. It
captures intents like "only on weekday mornings", "not during the trip
(Aug 3–17)", "any time after 22:00 until 02:00" or "only at the office". Each
constraint carries a `severity`: a `hard` time constraint rejects any write
that would violate it and is flagged when violated without one; a `soft`
constraint is a warning that only demotes ranking. A place requirement never
rejects anything (§Place).
Constraints are a value type on Task and Routine: not a standalone entity, and
they mint no id.

This document also owns the rule that keeps **dependencies** (`blocked_by`)
acyclic across devices (§Dependencies).

> **Amended** by [ADR-0051](../11-adr/0051-places.md) (the `at_place`
> dimension) and [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md)
> (evaluation zone, wrapping windows, re-evaluation on a zone change).
> Implementation is tracked in [#333](https://github.com/justin13888/Sunrise/issues/333) and [#339](https://github.com/justin13888/Sunrise/issues/339).

## Fields

```cddl
; Carried on Task and Routine as an optional list (max 16); the whole list is
; one register. See tasks.md and routines-and-recurrence.md.
SchedulingConstraint = {
    ? time_of_day:  TimeOfDayRange,                            ; local wall clock; may wrap midnight
    ? days_of_week: [* Weekday],                               ; set of weekdays; empty/absent = all days
    ? date_range:   { start: civil-date, ? end: civil-date, unknown-fields }, ; inclusive; open-ended if end absent
    ? at_place:     [+ entity-ref],                            ; plc_ refs; any one satisfies
    severity:       ConstraintSeverity,                        ; required
    unknown-fields,
}

TimeOfDayRange = { start: civil-time, end: civil-time, unknown-fields }
                 ; start < end: same-day window [start, end)
                 ; start > end: wraps midnight: [start, 24:00) ∪ [00:00, end) of the next day
                 ; start = end: invalid

ConstraintSeverity = "hard" / "soft" / tstr         ; unknown values preserved, read as "soft"

; Weekday is defined once, in ../10-cross-cutting/time.md §1.

; Civil (wall-clock) types serialize as jiff civil strings; see
; ../11-adr/0011-datetime-jiff.md.
civil-time = tstr .regexp "([01][0-9]|2[0-3]):[0-5][0-9]:[0-5][0-9]"   ; "HH:MM:SS"
civil-date = tstr .regexp "[0-9]{4}-[0-9]{2}-[0-9]{2}"                 ; "YYYY-MM-DD"
```

A constraint restricts along up to four independent **dimensions**: time of
day, days of week, date range, and place. At least one MUST be present; a
constraint with only a `severity` is meaningless and rejected at validation.

Every nested map carries `unknown-fields`, so a dimension a newer build adds is
preserved by an older one ([#322](https://github.com/justin13888/Sunrise/issues/322)).

## Semantics

### Combination

- Constraints with the **same set of populated dimensions** OR together: any
  one satisfied means the group is satisfied. This expresses "weekday mornings
  *or* weekend afternoons".
- Groups with **different dimensions** AND together: all must hold. "On
  weekdays" AND "after 18:00" AND "before the trip ends" must all be satisfied.
- Within one constraint, the populated dimensions AND together.

### Time of day, including a wrap past midnight

A window with `start > end` wraps: `22:00–02:00` is satisfied from 22:00 until
midnight and from midnight until 02:00. **The wrapped tail belongs to the day
the window started on** for the `days_of_week` and `date_range` dimensions of
the same constraint: "Fridays 22:00–02:00" is satisfied at 01:00 on Saturday,
because that 01:00 is the tail of Friday's window.

### Hard and soft

One rule, on every write path:

- A `hard` **time** constraint (time of day, days of week, date range) is
  **structural**. Any write that would leave the task violating it, evaluated
  in the evaluation zone below, is rejected: by an ordinary submit, by
  `Command::Triage`, and by the planner's `plan_commit` alike.
  `plan_preview` reports such a violation as **Blocking**, for the dragged item
  too, so the drop is shown as refused and the diff is not committable
  ([`../08-features/planner.md`](../08-features/planner.md) §Severity).
- An existing task that comes to violate a `hard` constraint **without** a write
  (the reader's zone changed, or a concurrent edit landed) is **flagged**, in
  views and in the triage queue, and never moved automatically. Generating a
  routine occurrence is not a user write: an occurrence whose key violates a
  `hard` constraint is still generated and flagged
  ([`routines-and-recurrence.md`](./routines-and-recurrence.md) §Scheduling
  constraints on occurrences).
- A `soft` violation never rejects. It is returned as a warning and only
  **demotes ranking** in planning views (see
  [`../08-features/planning-views.md`](../08-features/planning-views.md)) so the
  item sinks rather than disappears.
- `at_place` is never a rejection, at any severity (§Place).

### Evaluation zone

Evaluation follows [`time.md` §5](../10-cross-cutting/time.md#5-evaluating-constraints-and-requirements):

- the **anchor's zone** for a Routine (or recurring Block) whose anchor is
  `zoned`;
- the **reader's zone** for everything else, including every Task.

The candidate moment is converted to civil time in that zone, and each
dimension is tested against the civil value. Window bounds are never converted
to instants, so a DST gap or fold never needs disambiguating: a `02:00–03:00`
window is empty on a spring-forward night, and a `01:00–02:00` window matches
both passes through a fold.

### Re-evaluation

Violations are derived, never stored. They are recomputed on every read and
whenever the core is told the reader's zone changed
([`time.md` §7](../10-cross-cutting/time.md#7-time-zone-changes)). A write-time
check that passed in one zone does not keep a task valid in another.

### Place

`at_place` is satisfied when the device's in-memory presence says the user is
at any listed Place ([`places.md`](./places.md)). It evaluates to one of
`Satisfied`, `Violated` or `Unknown`, and `Unknown` (no permission, not
monitored, deleted or unreadable Place) is never treated as a violation.
Because presence is only known for *now*, `at_place` is never used to place,
move or refuse work: the planner does not read it when choosing positions, and
no write is rejected for it. A planner preview may annotate a slot with it at
`Info` severity and never higher ([ADR-0051](../11-adr/0051-places.md) §4). The
dimension drives the "here now" view, arrival notifications, and the "not
actionable here" flag for a `hard` violation.

## Dependencies

`blocked_by` is an add/remove OR-set on each Task
([ADR-0044](../11-adr/0044-per-field-ops.md)). A device rejects, at submit, an
addition that would close a cycle in its **current** graph. That check cannot
see a concurrent edit: device A adds `A→B` while device B adds `B→A`, both
submits pass, and the merged set holds a cycle. Rejecting at merge is not
possible (the ops are valid, and refusing one depends on arrival order), so the
cycle is **resolved at read time by a pure function of the merged state**.

### The rule

Each live edge `(task, blocker)` in the merged OR-sets has a **stamp**: the
`(hlc, device_id, seq)` of the **earliest** add-op among its surviving add tags.

```
resolve_cycles(edges) -> (dag, suppressed):
    dag := {}
    for edge in edges, sorted by (stamp ascending, task id, blocker id):
        if adding edge to dag closes a cycle:
            suppressed += edge
        else:
            dag += edge
```

- **Deterministic.** The order is total, so every replica holding the same OR-set
  state computes the same DAG, in any delivery order, without coordination.
- **The older edge wins.** An edge that existed first is kept, and the edge that
  closed the cycle is suppressed, which matches what each device's own submit
  check would have done had it seen the other's edge.
- **Derived, not deleted.** A suppressed edge stays in the OR-set. No op is
  written. If a later edit removes an edge that was part of the cycle, the
  suppressed edge revives on every replica, because the function is re-run over
  the new state.
- **Visible.** Each suppressed edge is reported to views as a dependency
  conflict on both tasks, with an action to remove it (a normal OR-set remove).

`blocked`, the planner and every dependency view read the resolved `dag`, never
the raw sets. `resolve_cycles` lives in `sunrise-domain` beside `would_cycle`
(`crates/sunrise-domain/src/deps.rs#would_cycle`), and a property test asserts
that any permutation of the same ops yields the same `(dag, suppressed)`.

A dependency on a task in another stream is permitted. A reader who cannot
decrypt the blocker (a shared stream's recipient) sees the edge by id and a
redacted title, and treats the blocker as open.

## Merge mapping

The **entire list** of `SchedulingConstraint` values on a Task or Routine is one
per-field LWW register on `(hlc, device_id, seq)`
([ADR-0044](../11-adr/0044-per-field-ops.md)). Constraints are always edited as
a unit in the UI; there is no per-constraint identity and no add/remove of
individual entries. Modelling the list as an OR-set would buy nothing (users do
not concurrently edit individual entries) and would complicate convergence.
Unknown keys inside a constraint survive because each nested map carries
`unknown-fields`, not because the list happens to be replaced whole.

## Storage projection

The list is stored as a single canonical-CBOR blob column
`scheduling_constraints BLOB` on the `tasks` and `routines` tables (`NULL` =
empty). It is a **projection**, rebuildable from the op log, and is opaque to
SQL; evaluation happens in Rust. See
[`../04-storage/local-database.md`](../04-storage/local-database.md).

## Validation

- A Task or Routine MAY carry **at most 16** constraints.
- Each constraint MUST populate **at least one** dimension.
- `time_of_day.start` MUST NOT equal `time_of_day.end`. `start > end` is a wrap.
- `date_range.start` MUST be `≤ date_range.end` when `end` is present.
- `days_of_week` entries MUST be distinct; an empty set means "all days".
- `at_place` MUST hold 1..=8 distinct `plc_` refs. A ref to a deleted Place is
  valid and evaluates `Unknown`.
- A write that would leave a Task violating a `hard` time constraint in the
  evaluation zone is rejected, on every write path including `plan_commit`
  (§Hard and soft). `soft` violations pass as warnings and only affect ranking.
  `at_place` never rejects.

## Status in the tree

- `TimeOfDayRange` rejects `start ≥ end`, so no window wraps
  (`crates/sunrise-domain/src/constraint.rs#TimeOfDayRange`).
- Constraints are checked once at write time in the device zone and never
  re-evaluated (`crates/sunrise-core/src/engine/task.rs#check_schedule_constraints`).
- There is no `at_place` dimension, and nested maps have no unknown-field map
  (`crates/sunrise-domain/src/constraint.rs#ScheduleConstraint`).
- A concurrent `blocked_by` cycle persists and leaves both tasks blocked; no
  merge-path code resolves it.

## See also

- [`./tasks.md`](./tasks.md): the `scheduling_constraints` and `blocked_by` fields.
- [`./routines-and-recurrence.md`](./routines-and-recurrence.md): how constraints propagate to occurrences.
- [`./places.md`](./places.md): the Place entity and presence.
- [`../08-features/planning-views.md`](../08-features/planning-views.md): where `soft` violations affect ranking.
- [`../08-features/time-blocking.md`](../08-features/time-blocking.md): scheduling Tasks into Blocks against these windows.
