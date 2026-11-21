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

> **Status: solver and seam built ([#343](https://github.com/justin13888/Sunrise/issues/343)); the Apple calendar uses them ([#344](https://github.com/justin13888/Sunrise/issues/344)).**
> `sunrise_domain::planner::solve` is the solver, `Core::plan_preview`,
> `Core::plan_commit` and `Core::plan_cancel` are the seam, and all three
> cross UniFFI (`crates/sunrise-core-bindings/src/planner.rs`). The Apple
> client's calendar block move and resize send `MoveBlock` and `ResizeBlock`
> (edge `End`) on each snapped position, draw the diff, and commit it on drop
> (`apps/apple/Sunrise/Calendar/CalendarPlanner.swift`); the keyboard path is
> `Keymap.planning`. Where that client stops short of the contract below:
>
> - One event of a repeating block is not planned by the client yet: the
>   core reads a recurring block as its occurrences and plans a drag of one
>   of them (§Recurring blocks), and UniFFI carries it as
>   `MoveBlockOccurrence` and `ResizeBlockOccurrence`, but the Apple drag
>   still asks "which events?" and writes an ordinary edit
>   ([#1224](https://github.com/justin13888/Sunrise/issues/1224)).
> - A task dropped on the grid is sized by a `MoveTask` preview (its
>   estimate, or `default_task_duration_s`) and becomes a bound block; the
>   preview is cancelled, not committed, because the grid draws blocks and
>   not a task's `planned_at` ([#1225](https://github.com/justin13888/Sunrise/issues/1225)).
> - A block's top edge is not draggable, so `ResizeBlock { edge: Start }` is
>   never sent.
> - The undo step is ordered against the command stack in Swift
>   (`UndoJournal`, beside `CoreBridge`), not in `sunrise-client-core`, and a
>   planner commit has no redo.
> - Task → day in Upcoming sends `BulkDefer { to: day, place: false }`, and
>   a task dropped into its no-date lane sends `Unschedule { keep: Nothing }`,
>   previewed while the drag hovers and committed on drop by the Apple
>   client's `UpcomingPlanner` (`planning-views.md` §Upcoming). Triage Defer
>   and "Plan my day" send no intent yet.
>
> What this build does differently from the contract below:
>
> - `plan_preview` takes only the intent: `now` is the core's clock and the
>   zone is the reader's zone every query reads in. The commit recomputes the
>   base at the preview's `now`, so it refuses only when an input row changed.
> - `PlanBase` digests the planner inputs themselves (every field the solve
>   reads, independent of order), not each row's HLC: an edit to a field the
>   planner does not read, such as a title, does not make a diff stale.
> - `solve` returns `Result`: an intent naming an item the inputs do not hold,
>   or the wrong kind, is an error rather than a diff.
> - There are no external events to read yet ([#930](https://github.com/justin13888/Sunrise/issues/930)).
>   A task bound to a block is not placed on its own.
> - `Unschedule` takes tasks only.
> - `AutoPlace` and `BulkDefer { place: true }` place each listed task in the
>   first free slot at or after `max(wake, now, not_before)`, in order, and
>   move nothing else.
> - No Place annotation is produced (it is optional, §Places), and there is no
>   per-gesture day-line cache: each preview gathers its days afresh.
> - `committable` is false only for a Blocking violation the diff introduces
>   or one standing on an item it writes. A hard constraint already broken on
>   an item the gesture leaves alone is reported (§Hard and soft in
>   `scheduling-constraints.md` only flags it) and refuses nothing.
> - The ripple search branches over at most 4 snapped candidate slots per
>   item besides unscheduling it (`BRANCH` in `planner/search.rs`): the best
>   by the objective among those generated outward until four are clean.
> - UniFFI carries `NotCommittable` and `Invalid` as one
>   `BindingError::PlanRefused(String)`, whose message names which; `Stale`
>   and `Unknown` keep their own variants, and `Closed` and `Engine` cross as
>   `BindingError::Core`. A client knows a refused drop before committing it
>   from the diff's `committable`. `PlanError::code` answers a registered
>   `ErrorCode` for every variant: `PLAN_NOT_COMMITTABLE`, `PLAN_INVALID`,
>   `PLAN_CLOSED` and `PLAN_FAILED` besides `PLAN_STALE` and `PLAN_UNKNOWN`.
> - `plan_commit` stores the inverse in `planner_commits` and
>   `planner_commit_fields` (migration 0050), and only for each field whose
>   register its write changed. `Core::plan_undo` and its UniFFI binding are
>   built ([#1158](https://github.com/justin13888/Sunrise/issues/1158)).
>   The newest 64 commits keep their inverse (`PLAN_UNDO_HISTORY`), and an
>   undone commit is forgotten, so a second undo is `PLAN_UNKNOWN`.
>   `PlanUndoSkip::field` is a `PlanField` (`PlannedAt`, `StartsAt`,
>   `EndsAt`, and `OccurrenceStartsAt` and `OccurrenceEndsAt` for the
>   exception a drag of one event wrote), and `restored` counts fields. An item deleted since the commit,
>   or one its restored fields would leave invalid, is skipped whole. In a
>   vault an older device shares, every write is a full-state op that writes
>   every field, so an edit of any field of an item since the commit skips
>   its undo.

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
    /// Drag a block. Its duration is kept. For a recurring block, `occurrence`
    /// names the event dragged and how far the drag reaches, and `to_start`
    /// is that event's; it is `None` for a block with no rule.
    MoveBlock { block: EntityRef, occurrence: Option<PlanOccurrence>, to_start: SunriseTime },
    /// Drag a block's top or bottom edge.
    ResizeBlock { block: EntityRef, occurrence: Option<PlanOccurrence>, edge: Edge, to: SunriseTime },
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

/// One event of a recurring block, by its occurrence key (the key the
/// calendar query gives its row), and how far a drag of it reaches.
pub struct PlanOccurrence { key: String, scope: OccurrenceScope }

pub enum OccurrenceScope {
    /// That event alone: the commit writes its exception.
    This,
    /// Every event: the commit moves the series' anchor by as much as the
    /// event moved.
    All,
}

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
    Invalid(String), // the intent names nothing, or the wrong kind; or, at commit, a block move would strand a recurring block's exception
}
```

`PlanError::code` answers the registry's `ErrorCode` (`PLAN_STALE` and
`PLAN_UNKNOWN` among the six in `crates/sunrise-error/codes.toml`, listed in
[`../10-cross-cutting/error-handling.md`](../10-cross-cutting/error-handling.md)),
and UniFFI carries those two as `BindingError::PlanStale` and
`BindingError::PlanUnknown`.

## Inputs

```rust
pub struct PlanInputs {
    pub now: Timestamp,
    pub zone: TimeZone,                 // the reader's zone; resolves Floating and AllDay
    pub window: Vec<PlannerDay>,        // the planner days the intent touches, resolved
    pub tasks: Vec<PlanTask>,           // open tasks placed in, or targeted at, the window
    pub blocks: Vec<PlanBlock>,         // Sunrise blocks in the window, recurrences expanded: one per occurrence, with its key
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
| Block `kind` | [`../02-domain/time-blocks.md`](../02-domain/time-blocks.md) ([#342](https://github.com/justin13888/Sunrise/issues/342)) | `fixed` never moves; `flexible` may. The planner reads `Block::effective_kind`, so a block with no `kind` is `flexible` when Sunrise created it and `fixed` when it was imported (`time-blocks.md` §Migration default). |
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
  end precedes its start after a resize, or a recurring block whose move
  would leave an exception applying to nothing) or a violation of a `hard` time
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
   are never written. A recurring block is written as §Recurring blocks
   says: a drag of one event writes that occurrence's exception, and a drag
   of every event moves the series anchor. A move of the anchor carries its
   exceptions and occurrence bindings with
   the occurrences they named, through the same path an edit of the whole
   block takes (`time-blocks.md` §Recurring blocks). A move that would leave
   an exception applying to nothing is flagged by the preview, through the
   same check, as a Blocking `Invalid` violation the diff introduces on the
   block, so the diff is not committable and step 3 refuses it. The base
   digests a block's occurrences in the window but not its rule, so a rule
   changed since the preview can still strand one here: that commit is
   refused as `PLAN_INVALID`, and nothing is written.
5. Stores the inverse (each written field's prior value, and the stamp this
   commit wrote) with a commit record in the local database, and returns the
   commit's id.

The staleness check (step 2) is a commit rule. It does not apply to undo.

### Recurring blocks

The planner reads a recurring block as its occurrences, as the calendar query
expands them (`time-blocks.md` §Recurring blocks): each occurrence overlapping
the window is one `PlanBlock` with its occurrence key, at the times its
exception gives it, and a cancelled one is not there. A block intent on a
recurring block names the event dragged, by key, and how far the drag
reaches; one that names no occurrence, a key the rule does not produce, or a
cancelled event is `PLAN_INVALID`, and so is an occurrence named on a block
with no rule. The diff's subject move carries the occurrence key; nothing
else that moves ever has one.

- **This event.** Only that occurrence moves. Every other occurrence in the
  window, of this series or another, is fixed: rippling one would write an
  exception nobody asked for. The commit writes the occurrence's exception
  (its `starts_at` and `ends_at`, over whatever else it overrides), as an
  edit of one occurrence does, and leaves the series alone.
- **All events.** The other occurrences of the series in the window move with
  the dragged one, so none is in its way. The commit moves the anchor's start
  and end by as much as the dragged event's moved, in the anchor's own kind,
  as the calendar's edit of every event does. An event an exception already
  moved on its own would stay where its exception puts it, so a drag of every
  event from it is flagged as a Blocking `Invalid` violation.

"This and following" is not planned yet: it splits the series into a new
block, which the per-field inverse below cannot undo. The client keeps its
edit for that answer.

A drag of one event stores its inverse against the occurrence's entry in the
`exceptions` map (`OccurrenceStartsAt`, `OccurrenceEndsAt`, the prior value
`None` where the exception overrode nothing), and its undo writes the
exception's start and end back. A block's every write is full-state, so it
renews every field's stamp; a planner commit or undo moves every kept
commit's record of a field it renewed and left as it was on to the new stamp,
so dragging a second event of a series keeps the first drag's undo.

### Undo

`plan_undo(commit_id)` is its own path, not a `plan_commit` of an inverse diff.
The inverse is stored with the commit, so the preview cache's eviction never
loses it; it lives as long as the undo history holds the step. Undo applies the
inverse **per field**, in one local transaction:

- a field whose register still holds this commit's write is restored to its
  prior value;
- a field written again since, by any device, is **skipped and reported** in
  `PlanUndoResult::skipped`, and the undo toast says so ("Restored 4 of 5;
  ‘Standup’ was changed since"). The inverse-command undo
  (`crates/sunrise-client-core/src/undo/mod.rs`) has no such rule: it writes
  the prior value back as a new write, whatever has changed since;
- a restored deferral writes `-1` on `deferred_count`;
- a recurring block's restored anchor carries its exceptions and occurrence
  bindings back with it, as the commit carried them; one it would leave
  applying to nothing skips the block whole, like a restore that would leave
  it invalid;
- a drag of one event restores that occurrence's exception's start and end;
  an exception gone or cancelled since skips them, and one the restore would
  leave ending before it starts skips the block whole.

Undos of successive commits on one item restore newest-first. Undoing the
newer commit writes the field back to the older commit's value, and the older
commit then counts the field as its own again, so its undo restores it. This
holds only where the newer commit wrote over the older one's write directly,
and where the newer undo wrote the field back. A write by anyone else in
between, or a newer undo that left the field as it was, keeps the older
commit's field skipped. Undoing the older commit while the newer one's write
stands skips the field.

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
  waits for the user, with a commit and a cancel control on screen beside the
  note that says so: a touch client has no Return or Esc to press.
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
single-block move dropped onto three tasks (the ripple chains through the
day's own tasks), a cross-day move, an `AutoPlace` of 20 tasks, and a drag of
one event of a daily recurring stand-in, over that fixture with five
recurring stand-ins added, and joins the baseline set; the measured figures are in
`performance-budgets.md` §Planner preview.

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
- It never edits an external calendar. External events are fixed planner inputs.
  A user can edit one, and can publish a Block to a calendar, but a planner move
  never writes an external event
  ([ADR-0071](../11-adr/0071-calendar-read-write-through-synced-intents.md)).
