---
status: accepted
---

# Time Blocks

A Block is a scheduled time range, optionally bound to one or more Tasks. Blocks are the time the user schedules in Sunrise and carry its time-blocking workflows. Events fetched from an external calendar are **not** Blocks: they are read-only `ExternalEvent`s ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)), and see §Calendar integration.

> **Amended** by [ADR-0046](../11-adr/0046-optional-stream.md) (`stream_id` is
> optional), [ADR-0048](../11-adr/0048-interactive-planner.md) (the planner
> moves only `flexible` blocks) and
> [ADR-0044](../11-adr/0044-per-field-ops.md) (per-field merge). Recurrence
> shares the series model of
> [`routines-and-recurrence.md`](./routines-and-recurrence.md), and every time
> rule is in [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md).
> Implementation is tracked in [#342](https://github.com/justin13888/Sunrise/issues/342).

## Fields

`stime` is the four-kinded `SunriseTime` defined in
[`time.md` §1](../10-cross-cutting/time.md#1-every-stored-time-is-a-sunrisetime).
A 09:00 block and a block at a fixed instant are different commitments, and
flying to another timezone must move one and not the other.

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

This is the whole of what a device writes and signs; there is no second, richer Block on the wire:

```cddl
Block = {
    id:           tstr .regexp "blk_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:   timestamp,
    updated_at:   timestamp,
    stream_id?:   entity-ref,                 ; str_ ref; absent = no stream (ADR-0046)
    kind:         BlockKind,                  ; fixed / flexible
    starts_at:    stime,                      ; for a recurring block, the series ANCHOR
    ends_at:      stime,                      ; same kind as starts_at; resolves after it
    title?:       text<256>,                  ; defaults to the bound task's title
    title_track_task: bool,                   ; default false; recompute title from the one bound Task
    tasks:        [* entity-ref],             ; tsk_ refs bound to this Block; OR-set
    rrule?:       RRule,                      ; recurring blocks; see §Recurring blocks
    exceptions?:  { * occurrence-key => BlockException }, ; per-occurrence changes; map of registers
    split_from?:  entity-ref,                 ; blk_ ref of the series this one continues
    location?:    text<128>,                  ; free text; never matched to a Place automatically
    notes?:       NoteBody,
    source?:      BlockSource,
    external_id?: tstr,                       ; the FOREIGN id; .ics export/import only
    deleted:      bool,
    unknown-fields,                           ; see overview.md
}

BlockKind   = "fixed"                         ; never moved by the planner
            / "flexible"                      ; the planner may move it
            / tstr                            ; unknown values preserved, read as "fixed"

BlockSource = "sunrise"                       ; created in Sunrise
            / "import:ics"                    ; imported from a one-shot .ics file
            / tstr                            ; unknown values preserved

BlockException = { cancelled: true }
               / { ? starts_at: stime, ? ends_at: stime, ? title: text<256>,
                   ? location: text<128>, ? notes: NoteBody, unknown-fields }
```

### Fixed and flexible

`kind` says whether the planner may move the block
([ADR-0048](../11-adr/0048-interactive-planner.md)):

- **`fixed`**: a commitment with other people or the world (a meeting, a
  flight). The planner treats it as immovable, and only the user moves it.
  Every block imported from an `.ics` file is `fixed`, and an unknown `kind`
  reads as `fixed`, the safe answer.
- **`flexible`**: time the user reserved for themselves (a focus block). The
  planner may move it within its day to make room. A user-created block is
  `flexible` by default.

A block that has already ended, or is in progress at `now`, is treated as fixed
by the planner whatever its `kind`.

**Migration default for existing rows.** `kind` is additive, so a Block written
before it existed carries no value, and a reader fills one in by the same rule
a new Block gets: an absent `kind` reads as **`flexible`** when `source` is
absent or `sunrise` (a user-created block), and as **`fixed`** when `source` is
`import:ics` (or any other value, the safe answer). Today's tree stores no
`source` either, so a block imported from `.ics` before `source` lands carries
no marker and reads as `flexible`; re-importing the same file recomputes the
same id (§`external_id` is not the dedup key) and rewrites it with
`source = import:ics` and `kind = fixed`.

`rrule`, `location`, `notes`, `source` and `external_id` are what
[ADR-0025](../11-adr/0025-integration-account-entity.md) adds, and each answers
a concrete loss: without `rrule` a recurring calendar event has nowhere to put
its rule, without `location` and `notes` an imported event's `LOCATION` and
`DESCRIPTION` have nowhere to land, and without `source` / `external_id` a
foreign id cannot be round-tripped back out. They are additive fields, so a
build that predates them preserves them through `unknown-fields` rather than
dropping them, and `DOC_SCHEMA_FLOOR` does not move. `kind`, `exceptions` and
`split_from` are additive in the same way.

**Status in the tree.** None of the eight exists yet: `Block` has the base
fields only (`crates/sunrise-domain/src/block.rs#Block`), and `stream_id` is
still required. [#342](https://github.com/justin13888/Sunrise/issues/342) and [#332](https://github.com/justin13888/Sunrise/issues/332) track them.

Two notes against earlier revisions of this spec:

- **`stream_id` is optional** ([ADR-0046](../11-adr/0046-optional-stream.md)).
  A stream-less Block is sealed under the private key domain and drawn in the
  neutral palette colour. An earlier revision made it required; that was the
  Inbox-sentinel model, which ADR-0046 removes.
- **There is no `timezone` field.** `SunriseTime` subsumes it — a `zoned`
  bound carries its own IANA zone, and a `floating` or `all_day` bound
  deliberately carries none. A second, Block-level zone would be a third
  answer to a question the bounds already answer, and the three could
  disagree.

### `external_id` is not the dedup key

**Import dedup is by the Block's own id, and adding `external_id` does not
change that.** The importer hashes `(source, uid)` into the Block id, exactly
as a materialized routine occurrence hashes `(routine, occurrence)` into a
Task's. Re-importing the same file therefore computes the same id and updates
the Block already there, which is what makes re-import idempotent **with no
side table to keep in step with the vault** — see
[`../09-integrations/icalendar.md`](../09-integrations/icalendar.md).

`external_id` exists for the opposite direction: to carry a *foreign* id back
out on `.ics` export and to read it on `.ics` import. It serves nothing else;
calendar-integration events are `ExternalEvent`s with their own id, not Blocks.
Wiring dedup to it would replace an id-derivation that cannot drift with a
lookup that can, and would need an index the schema does not have.
ADR-0025 records this as a consequence precisely so that a later reader does
not "simplify" the importer onto the new field.

### Specified but not modelled

The remaining calendar fields. They are **not** on the wire, and a build that
adds them round-trips through this one without loss, because the forward-compat
`unknown-fields` map preserves them verbatim:

```cddl
; Not yet modelled. Landing these is a DOC_SCHEMA_V bump, not a break.
color?:                BlockColor  ; defaults to stream color
travel_time_before_s?: uint        ; seconds, matching the rest of the domain
travel_time_after_s?:  uint
```

### Recurring blocks

A recurring block reuses the routine **series model**
([`routines-and-recurrence.md`](./routines-and-recurrence.md)), and the
expansion is the same Rust function:

- **Anchor.** `starts_at` is the anchor and MUST be `zoned` or `floating`
  ([`time.md` §6](../10-cross-cutting/time.md#6-recurrence-is-expanded-in-civil-space)).
  The occurrence duration is `ends_at − starts_at` in civil terms, so a
  09:00–11:00 block stays 09:00–11:00 on a DST day.
- **Occurrence keys** are the intended civil start (`YYYY-MM-DDTHH:MM`), as for
  routines, and never depend on how a DST gap resolved.
- **Expanded on read, never stored.** `expand_block(block, window, zone)` yields
  the occurrences that intersect a query window. No per-occurrence row is
  written, so a recurring block costs one entity however long it runs.
- **Exceptions** are a map of registers keyed by occurrence key: `cancelled`
  removes one occurrence (iCal `EXDATE`), and an override changes one
  occurrence's time or fields. Two devices changing two different occurrences
  both keep their change.
- **Edit scope** is *this* (writes an exception), *this and future* (splits the
  series: the old block's `rrule.until` ends before the chosen occurrence and a
  new block with `split_from` starts at it) or *all* (edits the series). The
  split block's id is derived from `(series root, split key)`, so two devices
  splitting at the same occurrence converge on one new block.
- **Binding a task** to a recurring block binds it to one occurrence: the
  binding carries the occurrence key.

### Importer status

`crates/sunrise-integrations` parses `RRULE`, `DESCRIPTION` and `LOCATION` out
of a `VEVENT` and then reports each at the domain boundary as an `ICalNotice`,
because the tree's `Block` has none of the fields above. So **a recurring event
imports as a single occurrence** until [#342](https://github.com/justin13888/Sunrise/issues/342) lands. The importer then maps
`RRULE` to `rrule`, `EXDATE` to `cancelled` exceptions, `RECURRENCE-ID`
overrides to exceptions, `LOCATION` to `location` and `DESCRIPTION` to `notes`,
and `(source, uid)` stays the id derivation. Nothing is silently dropped in
either state.

## Block title

A Block's `title` is a **shadow copy** of the bound task's title at creation/binding time, not a live binding. Subsequent edits to the bound task's title do not propagate to the Block.

- Optional `title_track_task: bool = false`. When true, the Block recomputes its title on read from the bound task's current title. Default false to preserve user-edited Block titles.
- Multi-task Blocks (N ≥ 2) ignore `title_track_task` and require an explicit `title`.

## Symmetry with `Task.blocks`

Binding is **one** op, not two. The `block_tasks` index is the only writer of
the relation, and `Task.blocks` is derived from it on read — the same shape
`Task.blocked_by` already has against `task_blockers`.

Two ops would mean the Block's `tasks` set and the Task's `blocks` set are
separate LWW registers on separate entities. A concurrent edit of the Task on
another device would then win the Task's register and silently drop the
binding, leaving the Block still claiming a Task that no longer claims it back.
Deriving makes "Bound Task's `blocks` field updates symmetrically" true by
construction instead of by repair.

A binding may name a Task this replica has not materialized yet — ops arrive
out of order — so `block_tasks` carries no foreign key on `task_id`. The
binding is a fact; the Task turns up later.

## Why Blocks aren't Tasks

We tried collapsing Block into Task. It caused:

- A "task" with `starts_at` + `ends_at` made the data model murky (two completion semantics: "ran the block" vs. "did the work").
- Calendar imports flooded the task list.

Decision: keep them distinct. A Block can *bind* one-or-more Tasks; completing a bound Task is what matters for productivity, not "completing" a Block.

## Lifecycle

- **Create.** From Today/calendar UI, or by drag-from-Task onto calendar grid.
- **Bind.** Add a Task. Bound Task's `blocks` field updates symmetrically.
- **Complete bound tasks.** When all bound tasks are done before `ends_at`, UI offers to shrink the block.
- **Run / no-show.** No "ran the block" state. We trust the user.
- **Move.** A drag goes through the planner's preview and commit
  ([ADR-0048](../11-adr/0048-interactive-planner.md)), which writes
  `starts_at`/`ends_at` on the dragged block and on any `flexible` block it had
  to move. On a recurring block the drag asks for an edit scope.

## Calendar integration

Calendar integrations are **read-only**
([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md), specified
in [`../09-integrations/overview.md`](../09-integrations/overview.md)), and they
never produce Blocks:

- An event fetched from Google Calendar, Microsoft Graph or CalDAV becomes an
  **`ExternalEvent`**, a separate read-only entity. Recurring events are stored
  as the occurrences the provider expanded inside the fetch window, each with a
  deterministic id, so two devices fetching the same occurrence converge on one
  entity.
- The planner treats every `ExternalEvent` as fixed
  ([ADR-0048](../11-adr/0048-interactive-planner.md)), and the calendar view
  draws it beside the user's Blocks. No client edits it.
- Nothing is pushed to an external calendar. A Block never becomes a provider
  event, and there is no `import:gcal` source.

The only calendar path that writes Blocks is the one-shot `.ics` file import
([`../09-integrations/icalendar.md`](../09-integrations/icalendar.md)), which is
what `source` and `external_id` serve. Not built; ranked on the roadmap
([`../roadmap.md`](../roadmap.md)) as
[#4](https://github.com/justin13888/Sunrise/issues/4).

## Merge mapping

Per [ADR-0044](../11-adr/0044-per-field-ops.md): every scalar field is a
per-field LWW register on `(hlc, device_id, seq)`; `tasks` is an add/remove
OR-set; `exceptions` is a map of registers, one per occurrence key. The tree
still merges the whole Block as one full-state unit until [#319](https://github.com/justin13888/Sunrise/issues/319) lands, so a
concurrent bind on one device and unbind on another resolves by timestamp.

The `block_tasks` projection is the sole writer of the binding relation and
`Task.blocks` is derived from it, so the two never disagree — see §Symmetry
with `Task.blocks`.

## Conflicts

When two devices schedule overlapping Blocks for the same task, both Blocks coexist. UI surfaces the conflict; user resolves manually. We do *not* auto-merge or auto-delete a Block.

Overlap is decided by resolving both blocks' bounds in the reader's zone
([`time.md` §2](../10-cross-cutting/time.md#2-comparisons-resolve-through-the-readers-zone-never-through-index_ms)),
never on the storage index key, and recurring blocks overlap per occurrence.
Today `overlaps` compares index keys
(`crates/sunrise-domain/src/block.rs#overlaps`), tracked in [#336](https://github.com/justin13888/Sunrise/issues/336).

The Calendar view shades the overlap region and shows a "Resolve" overflow menu with three actions:

- **Keep both** — no-op; closes the menu.
- **Merge** — combines the two Blocks into one with the union time range and concatenated tasks. Mechanically: tombstone the two original Blocks and create a new one in a single submit batch.
- **Adjust times** — opens a side-by-side editor for both Blocks.
