# 0047 — A task has a plan time and two deadlines, lateness is derived at read time, and triage is one bulk command

**Status:** accepted

**Amends** [`../02-domain/tasks.md`](../02-domain/tasks.md): `scheduled_at` and
`due_at` are replaced by `planned_at`, `target_at` and `hard_due_at`, and
`deferred_count` becomes a PN-counter.

**Depends on** [ADR-0044](./0044-per-field-ops.md) (per-field registers and
PN-counters), [ADR-0050](./0050-preferences-and-day-schedule.md) (the day
boundary and `stale_after_days`) and
[`../10-cross-cutting/time.md`](../10-cross-cutting/time.md) (how a time of any
kind is compared). **Used by** [ADR-0048](./0048-interactive-planner.md), which
writes `planned_at` and never the two deadlines.

**Tracked by** [#334](https://github.com/justin13888/Sunrise/issues/334) (fields and lateness) and [#335](https://github.com/justin13888/Sunrise/issues/335) (triage queue and bulk
actions).

## Context

Two fields carry three meanings. A task has `scheduled_at` and `due_at`
(`crates/sunrise-domain/src/task.rs#Task`), and `tasks.md` has said both that
`scheduled_at` is "when the user intends to do it" and that it "**is** the
target deadline". Those are different: "I'll do it Tuesday" is a plan, "I want
it done by Friday" is a goal, and "the filing closes on the 15th" is a fact
about the world. A user who has all three can record two.

The consequences are visible in the tree:

- **A slipped plan is silent.** A task scheduled for an earlier day stays in
  the `Scheduled` section (`crates/sunrise-domain/src/planning.rs#TodaySection`),
  and only `due_at` can make a task overdue
  (`crates/sunrise-domain/src/planning.rs#is_overdue`). No rule notices an
  undated task that nobody has touched in a month.
- **Defer is a different write path.** `defer_task` overwrites `scheduled_at`
  without calling `validate_invariants`, and flattens every time kind to an
  `Instant` because the command takes epoch milliseconds
  (`crates/sunrise-core/src/engine/task.rs#defer_task`).
- **Concurrent defers lose counts.** `deferred_count` is read, incremented and
  written back as part of a full-state op, so two devices deferring the same
  task count one.
- **Every task command takes one id.** The evening brief's "move everything to
  tomorrow" is a client loop of single submits that stops at the first error
  (`apps/apple/Sunrise/Notifications/DailyBrief.swift#moveEverythingToTomorrow`).
- **Completion is always "now"**, and dropping a task keeps no reason.

## Decision

### 1. Three times, three owners

```cddl
planned_at?:   stime,   ; when I intend to work on it. Owned by the planner and defer.
target_at?:    stime,   ; soft deadline: when I want it done.
hard_due_at?:  stime,   ; hard deadline: when it must be done.
```

All three are `SunriseTime` of any kind
([`time.md`](../10-cross-cutting/time.md) §Which kind each field uses). Each is
its own per-field LWW register.

- **Defer moves only `planned_at`.** No command other than an explicit edit of
  a deadline field ever writes `target_at` or `hard_due_at`. The planner
  (ADR-0048) obeys the same rule.
- **Defer keeps the kind it is given.** `DeferTask` takes a `SunriseTime`, not
  epoch milliseconds. Deferring "sometime Tuesday" to "sometime Wednesday"
  writes a `Floating` or `AllDay` value, never an `Instant`.
- **`deferred_count` is a PN-counter** (ADR-0044). Each defer contributes `+1`
  from its device, and two concurrent defers count two.

**Ordering invariants are warnings, never rejections.** `target_at ≤
hard_due_at` and `planned_at ≤ hard_due_at` are *expected*, and neither is
enforced by a command or by merge:

- concurrent per-field edits on two devices can produce any combination, and a
  rule that merge cannot enforce must not be one that a command pretends to;
- "I plan to do it after the deadline, knowingly" is a real decision.

`CommandResult` carries each violated ordering as a warning, and readers
compute the same list for display. What still rejects a write is structural
validation (a malformed value, a zone name that is not an IANA identifier) and
a `hard` **time** constraint (time of day, day of week, date range) that the
write would violate, evaluated in the reader's zone
([`scheduling-constraints.md`](../02-domain/scheduling-constraints.md) §Hard and
soft). That rejection applies to every write path alike: an ordinary submit,
`Command::Triage`, and a planner `plan_commit` (ADR-0048). A `soft` constraint
is a warning, and a Place requirement (`at_place`) never rejects anything
([ADR-0051](./0051-places.md) §4). Neither rejection is an ordering between two
fields that a concurrent edit could break.

**Migration.** `scheduled_at → planned_at` and `due_at → hard_due_at`, value
and kind unchanged. No task gets a `target_at`. The old `DueBeforeScheduled`
rejection is retired.

### 2. Lateness is derived, in Rust, at read time

```rust
enum Lateness { OnTrack, LateSoft, LateHard, Stale }

fn lateness(task: &Task, now: Timestamp, zone: &TimeZone,
            schedule: &DaySchedule, stale_after_days: u16) -> LatenessState;

struct LatenessState { state: Lateness, since: Option<Timestamp> }
```

It is never stored, never synced, and never computed by a client. Two devices
in different zones may legitimately disagree about whether an `AllDay`
deadline has passed, because they are in different days. That is the correct
answer for a reader, and it is why the value cannot be a field.

**Due instant.** Each deadline resolves to the instant it is missed:

| Kind | Missed at |
|---|---|
| `Instant` | the instant |
| `Zoned` | the civil time in its own zone |
| `Floating` | the civil time in the reader's zone |
| `AllDay(d)` | `boundary(d+1)`, the **end of planner day `d`**, in the reader's zone ([`day-schedule.md`](../02-domain/day-schedule.md) §Planner day) |

An `AllDay` deadline of Friday is therefore not late at 00:30 on Saturday for
someone whose Friday runs until 01:00, and any slot on Friday meets it. This is
the one place an `AllDay` value resolves to the end of its day rather than the
start ([`time.md`](../10-cross-cutting/time.md) §2).

**States**, evaluated for open tasks only (`todo`, `in_progress`; not
archived, not deleted), first match wins:

| State | Condition | `since` |
|---|---|---|
| `LateHard` | `hard_due_at` is set and `now ≥ due(hard_due_at)` | `due(hard_due_at)` |
| `LateSoft` | `target_at` is set and `now ≥ due(target_at)` | `due(target_at)` |
| `Stale` | neither deadline is set, `stale_after_days > 0`, and the reader's planner date of `now` is at least `stale_after_days` after the planner date of `touched_at` | start of that planner day |
| `OnTrack` | otherwise | — |

`touched_at` is the task's `updated_at`: the latest stamp of any field op
applied to it (ADR-0044). Any edit, including a Keep (§3), resets it.

`stale_after_days` is a Preference, default **14**; `0` disables staleness
([`preferences.md`](../02-domain/preferences.md)).

A slipped `planned_at` is **not** lateness. A plan is intent, not a promise:
it shows in Today as carried over (ADR-0048 owns re-planning it), and it turns
stale on the same clock as any other untouched task.

### 3. The triage queue is a derived query, and Keep is the only thing it remembers

```
in_triage(task) =  open(task)
               AND lateness(task).state ∈ {LateSoft, LateHard, Stale}
               AND NOT (task.late_acknowledged_at ≥ lateness(task).since)
               AND NOT stream_paused(effective_stream(task))
               AND NOT lapsed_occurrence(task)          ; routines-and-recurrence.md §Catch-up
```

`Query::Triage` returns the queue with each task's `LatenessState`, ordered by
`(state: LateHard < LateSoft < Stale, since ascending, id)`, which is total and
the same on every replica holding the same data in the same zone.

**Keep writes one field.** `late_acknowledged_at: timestamp?` is a per-field
LWW register. A task leaves the queue when it is acknowledged after the moment
its current state began, and **returns by itself** at the next threshold,
because a new state has a later `since`:

- a `LateSoft` task kept on Monday reappears as `LateHard` when its hard
  deadline passes;
- a `Stale` task kept today reappears as `Stale` after another
  `stale_after_days` untouched, because the Keep itself was a touch.

There is no suppression state machine to get wrong, and no timer that has to
fire: the rule is one comparison of two instants.

### 4. Four actions, each one command over a selection

```rust
Command::Triage { ids: Vec<TaskId>, action: TriageAction }

enum TriageAction {
    Defer { to: DeferTarget },
    AlreadyDone { completed_at: SunriseTime },   // Instant or AllDay
    Drop { reason: Option<String> },             // text<512>
    Keep,
}

enum DeferTarget {
    At(SunriseTime),   // a chosen time or date, kind preserved
    NextFreeSlot,      // resolved by the planner before any op is written
}
```

| Action | Writes, per task |
|---|---|
| **Defer** | `planned_at`, and `+1` on `deferred_count`. A planner `BulkDefer` committed with `plan_commit` writes exactly these ops for each task it names (ADR-0048 §6). |
| **Already done** | `state = done`, `completed_at` = the given value |
| **Drop** | `state = cancelled`, `cancel_reason` = the given text or absent |
| **Keep** | `late_acknowledged_at = now` |

- **Bulk is atomic.** The command validates every id and the action against
  every task before writing anything, and applies all of it in one transaction
  or none of it. A single deleted or unknown id rejects the whole command.
  Each task still gets its own per-field ops, so the merge is exactly as if each
  had been edited alone.
- **The same command serves every surface.** The triage list, a multi-select in
  any view, the evening brief's "move everything to tomorrow" and the CLI all
  issue `Command::Triage`. `DeferTask`, `CompleteTask` and a cancel through
  `UpdateTask` remain as the single-id forms and are implemented as
  `Triage` with one id.
- **`NextFreeSlot` is resolved before the ops exist.** The core asks the
  planner (ADR-0048, the `AutoPlace` intent over the selection) for a concrete
  `planned_at` per task and writes that. An op never carries "next free slot",
  so every replica applies the same value. Until the planner lands, the
  resolution is the start of the next planner day.
- **Already done is validated.** `completed_at` MUST NOT be after `now` and
  MUST NOT be before `created_at`. An `AllDay` value is allowed: "I did it
  yesterday" with no time is a date, and stats bucket it by that date. The
  routine streak reads the backdated value (it counts the occurrence as done on
  time if the date or instant falls inside the grace window), so a late click
  does not break a streak the work did not break.
- **`cancel_reason`** is meaningful only while `state = cancelled`. Any
  transition out of `cancelled` writes `cancel_reason = null` in the same
  command, and a reader ignores a reason on a task that is not cancelled (the
  two registers can disagree after a concurrent edit, and the read rule makes
  that harmless).
- **Nothing is automatic.** No background job, timer or sync handler ever
  defers, completes, drops or acknowledges a task. A test asserts that no code
  path other than a user command writes `state`, `planned_at`, `completed_at`
  or `late_acknowledged_at`.

## Alternatives considered

| Option | Why not |
|---|---|
| **Keep two fields and add a `soft: bool` to `due_at`** | Two deadlines are routinely both present ("aim for Friday, must by the 15th"). A flag lets a task hold one of them. |
| **Store lateness, refreshed by a background job** | A stored state is stale between refreshes, differs by device zone, and turns every refresh into a synced write on every device. Derivation costs a comparison per row on read. |
| **Auto-roll a missed plan to today** | Moves data without a command, which §4 forbids ("Nothing is automatic"), and hides how often a task has slipped. Carrying over in the view shows the same thing without writing. |
| **Reject `target_at > hard_due_at` at the command** | A concurrent edit on two devices produces it anyway, so every reader must handle it regardless. A command-only rejection is flakiness with extra steps. |
| **Keep = snooze until a chosen date** | That is Defer. Keep means "this is fine as it is", and the next threshold is the right time to ask again. |
| **Bulk as a client loop of single commands** | Partial failure leaves half a selection changed, and every client reimplements the loop. |

## Consequences

- `Task` gains `planned_at`, `target_at`, `hard_due_at`, `late_acknowledged_at`
  and `cancel_reason`, and loses `scheduled_at` and `due_at` after the
  migration. `reminder_lead_s` counts back from `planned_at`.
- `is_overdue` is replaced by `lateness`. The Today view shows `LateHard` and
  `LateSoft` tasks with their state, and the triage queue is a view of its own
  with multi-select and key hints on each action
  ([`../08-features/keyboard.md`](../08-features/keyboard.md)).
- `Command::Triage` is the one bulk write for tasks. The evening brief's Swift
  loop is deleted.
- Clients never compute lateness, staleness or queue membership. They render
  `LatenessState` from the core.
- Stats and streaks read `completed_at` as written, so a backdated completion
  lands on the day the work was done.
- A new feature id, `task.deadlines_v2`, is declared in `vault_requires`
  (ADR-0045), because an older build cannot write a Task without
  `scheduled_at`/`due_at` semantics and would otherwise overwrite the new
  fields.

## What would force revisiting this

1. **Deadlines that recur independently of routines** (a deadline "every
   quarter" on a single long-lived task). The fields are single values by
   design, and a recurring deadline is a routine.
2. **Shared tasks across identities.** Two people reading one `Floating`
   deadline in two zones get two answers. That is right for each of them, but a
   team view would need to pick a zone, which no field here records.
