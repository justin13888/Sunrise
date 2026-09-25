---
status: accepted
---

# Planner

The interactive planner: every gesture that moves work in time is previewed by
the Rust core as a `PlanDiff`, rendered live while the gesture is in progress,
and committed exactly as previewed when it ends.
[ADR-0048](../11-adr/0048-interactive-planner.md) is the decision and its
reasoning; this page is the contract. [#343](https://github.com/justin13888/Sunrise/issues/343) builds the solver and the seam,
and [#344](https://github.com/justin13888/Sunrise/issues/344) builds the client surfaces.

> **Status: not built.** No solver, no `plan_preview` and no `PlanDiff` exist
> in the tree. Calendar drags are resolved in Swift by `BlockDrag`
> (`apps/apple/Sunrise/Calendar/BlockLayout.swift:182`), which this page
> retires.

## Principles

1. **The core decides, the client draws.** A client never computes a position,
   a conflict or a violation. It sends an intent and renders the diff it gets
   back.
2. **You get what you saw.** A drop commits the diff that was on screen, or
   nothing. It never commits a re-solved variant.
3. **The subject lands where the user put it.** The solver moves other items out
   of the way and reports what the target violates. It never overrides the
   user's drop.
4. **Minimal surprise.** The fewest items move, by the smallest distance, inside
   the day the gesture touched. Fixed things never move.
5. **Deterministic.** Same inputs, byte-identical diff, on every device.
6. **Nothing is automatic.** The planner acts on a gesture. "Plan my day" is a
   gesture too.

## API

### Solver (pure, `sunrise-domain`)

```rust
/// Pure and total. No clock, no I/O, no locale, no hash-ordered iteration,
/// no floating point. Bounded by `PlanInputs::search_budget` evaluations.
pub fn solve(inputs: &PlanInputs, intent: &PlanIntent) -> PlanDiff;
```

### Core seam (`sunrise-core`, exposed over UniFFI)

```rust
impl Core {
    /// Gathers PlanInputs for the window the intent touches, runs `solve`,
    /// keeps the result in a bounded in-memory cache under `diff.id`,
    /// and returns it. Writes nothing.
    pub fn plan_preview(&self, intent: PlanIntent, now: Timestamp, zone: TimeZone)
        -> Result<PlanDiff, PlanError>;

    /// Applies exactly the cached diff, in one local transaction, as per-field
    /// ops. Refuses (writing nothing) if the diff is unknown or its base changed.
    /// Stores the commit's inverse with the commit, outside the preview cache.
    pub fn plan_commit(&self, diff_id: PlanDiffId) -> Result<PlanCommitResult, PlanError>;

    /// Undoes one commit from its stored inverse, per field. Never refuses as
    /// stale: a field changed since the commit is skipped and reported.
    pub fn plan_undo(&self, commit: PlanCommitId) -> Result<PlanUndoResult, PlanError>;

    /// Drops a cached preview. Optional: the cache evicts on its own.
    pub fn plan_cancel(&self, diff_id: PlanDiffId);
}
```

The preview cache holds the **8** most recent diffs per core, least recently
previewed evicted first, and is cleared when the vault closes. It holds only
diffs, never inputs, and never a commit's inverse (§Undo).

### `PlanIntent`

```rust
pub enum PlanIntent {
    /// Drag a task onto the grid, or along it. `to` is the new start;
    /// the duration is the task's estimate, or `default_task_duration_s`.
    MoveTask { task: EntityRef, to: SlotTarget },
    /// Drag a block. Its duration is kept.
    MoveBlock { block: EntityRef, to_start: SunriseTime },
    /// Drag a block's top or bottom edge.
    ResizeBlock { block: EntityRef, edge: Edge, to: SunriseTime },
    /// Move many tasks to a day (Upcoming drag, triage Defer, "Move to tomorrow").
    /// `place` = false keeps them date-only on that day; true slots them into
    /// free time as AutoPlace would.
    BulkDefer { tasks: Vec<EntityRef>, to: CivilDate, place: bool },
    /// Take items off the grid.
    Unschedule { items: Vec<EntityRef>, keep: UnscheduleKeep },
    /// Put tasks into the first free slots of a day, in the given order
    /// ("Plan my day", "Next free slot").
    AutoPlace { tasks: Vec<EntityRef>, day: CivilDate, not_before: Option<SunriseTime> },
}

pub enum SlotTarget {
    /// A time on the grid.
    At(SunriseTime),
    /// A day, with no time: the task becomes date-only on that day.
    Day(CivilDate),
}

pub enum Edge { Start, End }

pub enum UnscheduleKeep {
    /// Keep the civil date; drop the time of day. The item moves to the
    /// day's no-time lane.
    Day,
    /// Clear `planned_at` entirely (tasks only; a block cannot be unplanned).
    Nothing,
}
```

`SunriseTime` keeps its kind through the planner
([`../10-cross-cutting/time.md`](../10-cross-cutting/time.md)): a drop onto a
zoned grid writes a `Zoned` time in the grid's zone, and a drop onto a day
writes `AllDay`. The solver never flattens a kind to `Instant`.

### `PlanDiff`

```rust
pub struct PlanDiff {
    /// BLAKE3 over the canonical encoding of (base, intent, moves, unscheduled,
    /// violations), truncated to 16 bytes. Equal diffs have equal ids on every
    /// device.
    pub id: PlanDiffId,
    /// Digest of every PlanInputs row the solve read (entity id + row HLC),
    /// plus `now` truncated to the snap and the zone. `plan_commit` recomputes
    /// it and refuses on mismatch.
    pub base: PlanBase,
    pub intent: PlanIntent,
    /// Every item whose position changes, the subject first, then by id.
    pub moves: Vec<PlanMove>,
    /// Items that no longer fit in their day and are moved to its no-time lane.
    pub unscheduled: Vec<PlanUnscheduled>,
    /// Every violation present *after* the diff on any item in the window,
    /// ordered by (severity desc, item id, rule).
    pub violations: Vec<PlanViolation>,
    /// False when any violation is `Blocking`, the subject's included; the
    /// client renders the drop as refused and `plan_commit` would refuse it.
    pub committable: bool,
    /// True when the search budget ran out before the space was exhausted.
    /// The diff is still valid and deterministic; it may not be minimal.
    pub truncated: bool,
}

pub struct PlanMove {
    pub item: EntityRef,           // Task or Block
    pub role: MoveRole,            // Subject | Rippled
    pub before: PlanPosition,
    pub after: PlanPosition,
}

pub enum PlanPosition {
    Slot { start: SunriseTime, end: SunriseTime },
    Day(CivilDate),
    Unplanned,
}

pub struct PlanUnscheduled {
    pub item: EntityRef,
    pub before: PlanPosition,
    pub day: CivilDate,
    pub reason: UnscheduleReason,  // NoRoomInDay | WouldViolateHard
}

pub struct PlanViolation {
    pub item: EntityRef,
    pub rule: PlanRule,
    pub severity: Severity,
    /// True when this diff introduces it; false when it was already there.
    pub introduced: bool,
    /// The other party, where there is one (the event overlapped, the blocker).
    pub with: Option<EntityRef>,
}

pub enum PlanRule {
    OverlapsFixed,          // overlaps an external event or a fixed block
    Overlaps,               // overlaps a flexible item
    AfterHardDeadline,      // slot ends after due(hard_due_at), ADR-0047 §Due instant
    AfterTarget,            // slot ends after due(target_at)
    BeforeBlocker,          // starts before a blocked_by task ends
    OutsideTimeOfDay,       // ScheduleConstraint time_of_day
    WrongDayOfWeek,         // ScheduleConstraint days_of_week
    OutsideDateRange,       // ScheduleConstraint date_range
    PlaceUnknown,           // annotation only, always Info: requires a Place; the slot says nothing about location
    PlaceMismatch,          // annotation only, always Info: the slot is inside a block at a different Place
    OutsideDay,             // outside the day's plannable window (wake to sleep)
    InPast,                 // the slot starts before `now`
    Invalid(DomainViolation), // the write would fail the entity's invariants
}

pub enum Severity { Info, Soft, Hard, Blocking }

pub struct PlanCommitResult {
    /// Names the commit for `plan_undo`. Its inverse is stored with it.
    pub commit: PlanCommitId,
    pub moved: u32,
}

pub struct PlanUndoResult {
    pub restored: u32,
    /// Fields written again since the commit, left as they are.
    pub skipped: Vec<PlanUndoSkip>,
}

pub struct PlanUndoSkip { pub item: EntityRef, pub field: FieldName }

pub enum PlanError {
    Stale,        // PLAN_STALE (commit only): an input changed since the preview; preview again
    Unknown,      // PLAN_UNKNOWN: a diff evicted or the vault reopened, or no such commit
    NotCommittable, // the diff carries a Blocking violation
    Invalid(String), // the intent names nothing, or the wrong kind
}
```

The error codes `PLAN_STALE` and `PLAN_UNKNOWN` join the error-code registry
([`../10-cross-cutting/error-handling.md`](../10-cross-cutting/error-handling.md))
when [#343](https://github.com/justin13888/Sunrise/issues/343) lands.

## Inputs

```rust
pub struct PlanInputs {
    pub now: Timestamp,
    pub zone: TimeZone,                 // the reader's zone; resolves Floating and AllDay
    pub window: Vec<PlannerDay>,        // the planner days the intent touches, resolved
    pub tasks: Vec<PlanTask>,           // open tasks placed in, or targeted at, the window
    pub blocks: Vec<PlanBlock>,         // Sunrise blocks in the window, recurrences expanded
    pub events: Vec<PlanEvent>,         // external calendar events in the window
    pub snap_s: u32,                    // preference `planner.snap_s`; default 900 (15 min)
    pub default_task_duration_s: u32,   // preference `planner.default_task_duration_s`; default 1800
    pub min_gap_s: u32,                 // preference `planner.min_gap_s`; default 0
    pub search_budget: u32,             // constant; see §Solver
}

/// `day` is the planner day [boundary(d), boundary(d+1)); `plannable` is its
/// wake-to-sleep plannable window, the only span the solver places work in
/// (day-schedule.md §Planner day).
pub struct PlannerDay {
    pub date: CivilDate,
    pub day: (Timestamp, Timestamp),
    pub plannable: (Timestamp, Timestamp),
}
```

| Input | Source | Treated as |
|---|---|---|
| Task `planned_at`, estimate | [ADR-0047](../11-adr/0047-deadlines-and-lateness.md), `Task.estimated_duration_s` | Movable, unless it has ended or is in progress at `now` |
| Task `target_at` | ADR-0047 | Soft deadline (`AfterTarget`, Soft), missed at `due(target_at)` |
| Task `hard_due_at` | ADR-0047 | Hard deadline (`AfterHardDeadline`, Hard), missed at `due(hard_due_at)` |
| Task `blocked_by` | [`../02-domain/tasks.md`](../02-domain/tasks.md) | Ordering: a task starts no earlier than each timed blocker's end (`BeforeBlocker`, Hard). An untimed blocker gives `Info`. |
| Task `scheduling_constraints` | [`../02-domain/scheduling-constraints.md`](../02-domain/scheduling-constraints.md) | A `hard` time dimension (time of day, day of week, date range) is **Blocking**; a `soft` one is Soft |
| Task Place requirement | [ADR-0051](../11-adr/0051-places.md) | Not a placement input: an `Info` annotation only (§Places below) |
| Block `kind` | [`../02-domain/time-blocks.md`](../02-domain/time-blocks.md) ([#342](https://github.com/justin13888/Sunrise/issues/342)) | `fixed` never moves; `flexible` may. Until the field exists, every block is `fixed`. |
| Tasks bound to a block | `Block.tasks` | Move with their block; they are not placed independently |
| External events | [ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md) | Always fixed. An event whose busy treatment is `free` is drawn and never blocks. |
| Day schedule | [ADR-0050](../11-adr/0050-preferences-and-day-schedule.md), [`../02-domain/day-schedule.md`](../02-domain/day-schedule.md) | Each planner day's **plannable window** `plannable(d)`, wake to sleep; civil midnight to midnight when unset |
| `now`, zone | The client, per call | Past and in-progress items are fixed; `InPast` for a subject dropped into the past |

A task with no estimate is placed with `default_task_duration_s`, and the
client shows the assumed duration on its ghost.

A deadline is compared at its due instant (ADR-0047 §Due instant). An `AllDay`
deadline on `d` is due at `boundary(d+1)`, the end of planner day `d`, so a slot
anywhere on its due date meets it and is not flagged.

### Places

The planner cannot know where the user will be, so the solver **never places,
moves or refuses work by Place** ([ADR-0051](../11-adr/0051-places.md) §4). A
Place requirement is not a solver input: it is not in the objective, it never
unschedules an item, and it never makes a diff uncommittable.

After the solve, the preview MAY annotate a slot for a task that requires a
Place, at `Info` severity and never higher:

- `PlaceMismatch` when the slot lies inside a block or external event whose
  `location` names a *different* Place (an exact match on a Place's name after
  case folding and whitespace trimming);
- `PlaceUnknown` otherwise, because on-device presence is a fact about now, not
  about a future slot.

The name match serves this annotation and nothing else.

### Severity

- **Blocking**: the write would be rejected by an ordinary submit. That is
  either a failure of the entity's own invariants (for example a block whose
  end precedes its start after a resize) or a violation of a `hard` time
  constraint (`OutsideTimeOfDay`, `WrongDayOfWeek`, `OutsideDateRange`), which
  is structural ([`../02-domain/scheduling-constraints.md`](../02-domain/scheduling-constraints.md)
  §Hard and soft). It holds for the subject too: the drop is shown as refused,
  and `plan_commit` refuses it. The solver never moves a rippled item to a
  Blocking position; it unschedules the item instead.
- **Hard**: a hard deadline, a dependency, or an overlap with a fixed item.
  Commit is allowed for the subject, because the user's drop is deliberate. The
  solver never *introduces* one on a rippled item when a placement without one
  exists within budget.
- **Soft**: a soft constraint, a target date, the plannable window's bounds for
  the subject, or an overlap between flexible items.
- **Info**: something the user should see and nobody can decide yet (an
  unplanned blocker, a Place annotation).

## Solver

1. **Apply the intent** to the subject. The subject's new position is fixed for
   the rest of the solve.
2. **Build the day line** for each touched day: fixed intervals (events, fixed
   blocks, past and in-progress items, the subject) and movable items, all
   snapped to `snap_s`.
3. **Collect the conflict set**: movable items that overlap a fixed interval,
   that overlap each other, or whose rules the subject's move now breaks (a
   dependent that must now start later).
4. **Search.** For each item in the conflict set, in order of (earliest current
   start, id), candidate positions are generated nearest-first: the nearest free
   snapped slot after, then before, alternating outward within the day. A
   bounded depth-first search assigns candidates and scores each complete
   assignment by the lexicographic objective in
   [ADR-0048](../11-adr/0048-interactive-planner.md) §Decision 4. Items pushed
   into by a candidate join the conflict set, so a ripple chains.
5. **Stop** when the space is exhausted or `search_budget` (a count of
   evaluated assignments, **4096**) is spent. Keep the best assignment found;
   set `truncated` if the budget ran out.
6. **Unschedule** what does not fit: an item with no candidate in its day that
   avoids a new Hard or Blocking violation moves to the day's no-time lane
   (`PlanUnscheduled`), rather than into another day.
7. **Report** every violation in the window after the diff, marking the ones the
   diff introduced.

`AutoPlace` and `BulkDefer { place: true }` run the same search with the listed
tasks as the conflict set, in the listed order, and with no subject.

**Incremental.** Only the planner days the intent touches are loaded and
solved. A move within one day loads one day; a move across days loads two. The
core keeps each day's line from the previous preview of the same gesture and
rebuilds it only when an input row's HLC changed, so a drag that moves a block
by one snap re-solves one day's movable set and no more.

## Commit

`plan_commit(diff_id)`:

1. Looks the diff up in the cache; `PLAN_UNKNOWN` if absent.
2. Recomputes `PlanBase` from the current rows; `PLAN_STALE` on any difference.
3. Refuses `NotCommittable` if any violation is Blocking.
4. In one local transaction, writes one per-field op per changed field
   ([ADR-0044](../11-adr/0044-per-field-ops.md)): `planned_at` for tasks,
   `starts_at` and `ends_at` for blocks. A `BulkDefer` is a deferral: each task
   it names also gets `+1` on its `deferred_count` PN-counter, the same ops
   `Command::Triage` with `Defer` writes
   ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md) §4). Rippled items get
   positions only. Nothing else is written, and `target_at` and `hard_due_at`
   are never written.
5. Stores the inverse (each written field's prior value, and the stamp this
   commit wrote) with a commit record in the local database, and returns the
   commit's id.

The staleness check (step 2) is a commit rule. It does not apply to undo.

### Undo

`plan_undo(commit_id)` is its own path, not a `plan_commit` of an inverse diff.
The inverse is stored with the commit, so the preview cache's eviction never
loses it; it lives as long as the undo history holds the step. Undo applies the
inverse **per field**, in one local transaction:

- a field whose register still holds this commit's write is restored to its
  prior value;
- a field written again since, by any device, is **skipped and reported** in
  `PlanUndoResult::skipped`, and the undo toast says so ("Restored 4 of 5;
  ‘Standup’ was changed since"), which is the inverse-command undo's existing
  rule (`crates/sunrise-client-core/src/undo.rs`);
- a restored deferral writes `-1` on `deferred_count`.

Undo never refuses as stale and never re-solves.

## Client contract

Every client that has a planning surface MUST implement all of this. Clients of
one device class reach parity: the macOS behaviour below is the desktop
contract for every desktop client, and the phone/tablet contract differs only
where a touch gesture replaces a pointer.

- **During a drag**, on each change of snapped position, the client calls
  `plan_preview` off the main thread. At most one call is in flight: a newer
  position replaces a queued one, and a result for a superseded position is
  dropped. A slow preview never blocks the drag itself; the ghosts lag, the
  pointer does not.
- **Ghosts.** Each `PlanMove` renders a ghost at `after` and a faint outline at
  `before`. The subject's ghost is the dragged item. Items in `unscheduled`
  render in the day's no-time lane with a marker.
- **Conflict colouring.** Violations tint the item they name: Hard and Blocking
  in the danger colour, Soft in the warning colour, Info as an outline badge.
  Colours come from the design tokens
  ([`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md)),
  never from a local palette, and each tint is paired with an icon so colour is
  not the only signal ([`../10-cross-cutting/accessibility.md`](../10-cross-cutting/accessibility.md)).
- **A summary line** under the pointer names the consequence in words:
  "Moves 3 · 1 unscheduled · misses hard deadline". It is also announced to
  VoiceOver when it changes.
- **Drop** commits the diff on screen. `PLAN_STALE` or `PLAN_UNKNOWN` triggers
  one immediate re-preview at the same position and a second commit only if
  the new diff has the same moves; otherwise the new diff is shown and the drop
  waits for the user.
- **Cancel** (Esc, or dropping back at the origin) commits nothing.
- **Undo** is one step, labelled by the gesture ("Undo move ‘Standup’ (moved
  3)"), and calls `plan_undo` with the commit's id (§Undo).
- **Keyboard parity.** Every drag has a keyboard path that previews the same
  diff: ⌥↑/⌥↓ nudge by one snap, ⇧⌥↑/⇧⌥↓ resize, ⌥←/⌥→ move a day, Return
  commits and Esc cancels. The bindings come from the keymap and show their
  hints wherever the action appears ([`keyboard.md`](./keyboard.md)).

Surfaces that send intents: calendar block move and resize, task onto the grid,
block onto a task list (unbind), task onto a day in Upcoming
([`planning-views.md`](./planning-views.md)), triage Defer, and "Plan my day".

## Budget and bench

`plan_preview` is ≤ 16 ms p95 and ≤ 33 ms p99 on Baseline A, 2× on Baseline B,
for the typical week defined in
[`../10-cross-cutting/performance-budgets.md`](../10-cross-cutting/performance-budgets.md)
§Planner preview (200 open tasks, 60 blocks, 40 external events, 20
dependencies). The bench `plan_preview` in `sunrise-bench` measures a
single-block move with a three-item ripple, a cross-day move, and an
`AutoPlace` of 20 tasks over that fixture, and joins the baseline set.

## Tests

- **Determinism** (property): the same `PlanInputs` and intent give a
  byte-identical `PlanDiff`, including across shuffled input order.
- **Fixed never moves** (property): no external event, fixed block, past or
  in-progress item appears in `moves` as `Rippled`.
- **No-op**: an intent that puts the subject where it already is yields an empty
  `moves`.
- **Idempotence**: preview, commit, then the same preview again yields a diff
  with no moves.
- **No gratuitous hard violation** (property, within budget): when a placement
  with no introduced Hard violation exists, the diff introduces none.
- **Stale**: a write to any input row between preview and commit refuses with
  `PLAN_STALE` and writes nothing.
- **Undo**: after the preview cache has evicted every diff, `plan_undo` still
  restores a commit; a field edited since the commit is skipped and reported,
  and the other fields are restored.
- **Hard constraint and Place**: dropping the subject outside a `hard`
  time-of-day window yields a Blocking violation and `committable = false`; a
  Place requirement never yields more than `Info`.
- **Fixtures**: a dependency chain, a hard time-of-day constraint, a sleep time
  past midnight, a hard deadline, a DST transition day, and a floating task read
  in two zones.

## What the planner does not do

- It never moves anything without a gesture.
- It never moves work into another day to make room.
- It never writes a deadline.
- It never places, moves or refuses work by Place.
- It never edits an external calendar: external events are read-only
  ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)).
