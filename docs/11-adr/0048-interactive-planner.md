# 0048 — The planner is a pure, deterministic Rust solver that previews every drag as a `PlanDiff` and commits exactly what it previewed

**Status:** accepted

**Depends on** [ADR-0044](./0044-per-field-ops.md) (a commit writes per-field
ops), [ADR-0047](./0047-deadlines-and-lateness.md) (`planned_at`, `target_at`,
`hard_due_at`), [ADR-0050](./0050-preferences-and-day-schedule.md) (the day
boundary), [ADR-0051](./0051-places.md) (place requirements) and
[ADR-0049](./0049-calendar-integrations-per-device-oauth.md) (external events).

**Specified in** [`../08-features/planner.md`](../08-features/planner.md).
**Budget in** [`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md)
§Planner preview. **Tracked by** [#343](https://github.com/justin13888/Sunrise/issues/343) (solver and seam) and [#344](https://github.com/justin13888/Sunrise/issues/344)
(drag-with-preview on the clients).

## Context

There is no planner. Nothing in the workspace places a task into time:
`crates/sunrise-domain/src/planning.rs` holds the Today sectioning
(`crates/sunrise-domain/src/planning.rs:66#today_section`) and nothing that
moves anything. The inputs a planner would read all exist and nothing reads
them together: `estimated_duration_s`
(`crates/sunrise-domain/src/task.rs:114#Task`), `blocked_by`
(`crates/sunrise-domain/src/task.rs:140#Task`), `scheduling_constraints`
(`crates/sunrise-domain/src/task.rs:125#Task`, with a per-constraint severity in
`crates/sunrise-domain/src/constraint.rs:199#ScheduleConstraint`) and block
overlap detection (`crates/sunrise-domain/src/block.rs:197#overlaps`). Focus's
ranked queue (`crates/sunrise-core/src/engine/focus.rs:350#query_focus_plan`)
orders tasks and places none of them.

Every calendar drag decides its outcome in Swift today. `BlockDrag`
(`apps/apple/Sunrise/Calendar/BlockLayout.swift:182`) snaps and clamps one
block, the client writes one update, and nothing else on the grid reacts until
the overlap is shaded after the write. That breaks two rules at once: business
logic lives in a client, so a second client would re-implement it and drift;
and the user learns the consequence of a drag only after it has happened.

The product mandate is extreme efficiency for power users and structurally
robust behaviour. For planning that means: a drag shows its full consequence
*while* it is happening, the drop commits exactly what was shown, one undo
takes all of it back, and every device computes the same answer from the same
data.

## Decision

**1. A pure solver in `sunrise-domain`.** `sunrise_domain::planner::solve(inputs,
intent) -> PlanDiff` is a total function of its arguments. It reads no clock, no
database, no locale and no environment; `now` and the reader's zone arrive in
`PlanInputs`. It allocates no hash-ordered collection whose iteration order
could leak into output, uses integer seconds throughout (no floating point), and
bounds its own search by a count of evaluated placements, never by elapsed time.
The same inputs therefore give a byte-identical `PlanDiff` on every device and
every architecture.

**2. Two calls on the core, one on each side of the drop.**

- `plan_preview(intent, now, zone) -> PlanDiff` gathers `PlanInputs` for the
  window the intent touches, calls the solver, remembers the result under its
  `diff_id`, and returns it. It writes nothing.
- `plan_commit(diff_id) -> PlanCommitResult` applies **exactly** the remembered
  diff as ops in one local transaction, or refuses. It never re-solves. If any
  input the diff was computed from has changed since the preview, it refuses with
  `PLAN_STALE` and the client previews again; if the id is unknown (evicted,
  or the process restarted) it refuses with `PLAN_UNKNOWN`. Either way nothing
  is written.

**3. The intent is the user's gesture, and the subject lands where the user put
it.** The solver never overrides the dragged item's target. It moves *other*
items out of the way, reports what the target violates, and leaves the decision
to the user. The one exception is structural: a target that violates a `hard`
time constraint (time of day, day of week, date range) is reported as
**Blocking** and the diff is not committable, because the same rule rejects an
ordinary submit and `plan_commit` is not a way around it
([`scheduling-constraints.md`](../02-domain/scheduling-constraints.md) §Hard and
soft). The drop is shown as refused. A Place requirement is never a placement
input and never more than an `Info` annotation
([ADR-0051](./0051-places.md) §4). `PlanIntent` covers move, resize, bulk defer, unschedule and
auto-place (the full shape is in the feature spec).

**4. The objective is lexicographic.** Among placements of the items it is
allowed to move, the solver picks the one that minimizes, in order:

1. hard violations introduced on items other than the subject;
2. the number of items moved;
3. soft violations introduced;
4. total displacement, in seconds, summed over moved items;
5. the ordered list of moved ids (a stable tie-break on `EntityRef` bytes).

Fixed items are not in the objective at all, because they are not movable:
external calendar events are always fixed, `fixed` blocks are fixed, and so is
anything that has already ended or is in progress at `now`. A position that
would violate a `hard` time constraint is not a candidate for any item.

**5. Minimal and local.** The ripple is confined to the planner days the intent
touches, and work is placed only inside each day's **plannable window**. A
planner day is `[boundary(d), boundary(d+1))` and its plannable window is the
day schedule's wake-to-sleep span inside it
([`day-schedule.md`](../02-domain/day-schedule.md) §Planner day,
[ADR-0050](./0050-preferences-and-day-schedule.md)); with no schedule set, both
are civil midnight to midnight. An item that no longer fits in its
day is **unscheduled onto that day**, keeping its civil date and losing its time
of day, and the diff lists it as such. Nothing ever spills silently into the
next day.

**6. A commit writes positions only.** It writes `planned_at` on tasks and
`starts_at`/`ends_at` on flexible blocks. The one addition is the deferral
counter: committing a `BulkDefer` also writes `+1` on `deferred_count` for each
task it names, so a deferral writes the same ops whether it is committed from a
preview or issued as `Command::Triage` with `Defer`
([ADR-0047](./0047-deadlines-and-lateness.md) §4). A commit never writes
`target_at` or `hard_due_at`: a deadline is a fact about the world, not
something a drag may negotiate.

**7. One commit is one undo step, on its own path.** `plan_commit` stores the
commit's inverse (each written field's prior value) with the commit, outside the
preview cache, so eviction never loses it, and returns a commit id.
`plan_undo(commit_id)` applies the inverse **per field** as one new write. A
field a later edit has changed again is skipped and reported, the rule the
existing inverse-command undo already follows
(`crates/sunrise-client-core/src/undo.rs`). The staleness refusal of §2 applies
to commits only; undo never refuses as stale.

**8. A budget.** `plan_preview` is ≤ 16 ms p95 and ≤ 33 ms p99 on Baseline A for
a typical week, 2× on Baseline B, defined and gated in
[`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md)
§Planner preview. A preview over budget is a solver bug, not a reason to
throttle the drag.

## Alternatives considered

| Option | Why not |
|---|---|
| **Client-side placement (today's `BlockDrag`)** | Each client re-implements the rules and they drift. It cannot see constraints, dependencies or deadlines without re-implementing the domain, and the "core logic in Rust, clients only render" rule forbids it. |
| **Commit re-solves on drop** | The drop would apply something the user never saw whenever the base moved under the drag. Refusing and re-previewing costs one extra round trip on a rare race, and it keeps "you get what you saw" unconditional. |
| **Commit carries the whole diff from the client** | The client would become the authority for what is written, and a stale or tampered diff could write anything. The core keeps the diff and the client passes only its id. |
| **An ILP / CP-SAT solver** | Optimal, but not deterministic across library versions and platforms. It is a large dependency in a security-frozen workspace, and it does not fit a 16 ms frame on a phone. The lexicographic objective over a bounded local search is exact for the instances a single drag produces (one or two days, tens of movable items) and degrades by reporting, not by guessing. |
| **Time-boxed search ("best found in 10 ms")** | The answer would depend on CPU speed and load, so two devices would disagree. The bound is a count of evaluated placements. |
| **Ripple across days** | Moving Tuesday's work into Wednesday because Tuesday filled up is the surprising edit users complain about in auto-schedulers. Unscheduling onto the same day is visible, reversible and local. |
| **Auto-plan continuously in the background** | Nothing in Sunrise changes a task automatically ([ADR-0047](./0047-deadlines-and-lateness.md) §4). The planner acts only on a gesture, and "Plan my day" is itself a gesture (the `AutoPlace` intent). |

## Consequences

- `BlockDrag`'s arithmetic moves into the solver. The Apple client stops
  computing positions and renders `PlanDiff`s instead ([#344](https://github.com/justin13888/Sunrise/issues/344)).
- Blocks need a `fixed | flexible` kind, which
  [#342](https://github.com/justin13888/Sunrise/issues/342) adds ([`time-blocks.md`](../02-domain/time-blocks.md)). Until it lands, every block is
  treated as `fixed` by the solver, so the planner can move tasks and nothing
  else.
- The solver's input set is the design's list of what "a plan" depends on. A
  new field that should influence placement is a change to `PlanInputs` and to
  its digest, which is what makes `PLAN_STALE` correct.
- A commit touches rows in several Streams. It is atomic in the local
  transaction; on the wire it is one op per changed field, and a peer may apply
  them across two sync batches. Each field converges under per-field LWW
  ([ADR-0044](./0044-per-field-ops.md)), so a peer can briefly show a partial
  ripple and never a wrong final state.
- A Criterion bench, `plan_preview` in `sunrise-bench`, joins the baseline
  set, and the determinism property test (same inputs, byte-identical diff)
  is a gate, not a nightly.

## What would force revisiting this

1. **A typical week that no longer fits the budget** once recurring blocks and
   external events are both expanded. The answer is a better incremental index,
   not a time-boxed search.
2. **Users asking for cross-day ripple.** It would be a new intent (`Rebalance`)
   with its own preview, never a change to the default.
3. **Collaborative planning over a shared Stream** ([#133](https://github.com/justin13888/Sunrise/issues/133)), where two people's
   gestures race. The stale-refusal rule holds; the UI for "someone else moved
   this" does not exist yet.
