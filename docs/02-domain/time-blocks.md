---
status: accepted
---

# Time Blocks

A Block is a scheduled time range, optionally bound to one or more Tasks. Blocks are how Sunrise integrates with calendars and supports time-blocking workflows.

## Fields

`stime` is the four-kinded `SunriseTime` defined in
[`tasks.md`](./tasks.md) §`stime`. A 09:00 block and a block at a fixed instant
are different commitments, and flying to another timezone must move one and not
the other.

```cddl
Block = {
    id:           tstr .regexp "blk_[A-Z0-9]{26}",
    created_at:   tdate,
    updated_at:   tdate,
    title?:       text<256>,                  ; defaults to bound task's title
    starts_at:    stime,                      ; see tasks.md §stime
    ends_at:      stime,                      ; resolves after starts_at
    timezone:     text,                       ; IANA tz
    title_track_task: bool .default false,    ; recompute title from the one bound Task
    tasks:        [* tstr],                   ; bound task IDs
    stream_id?:   tstr,                       ; for tinting / filtering
    color?:       BlockColor,                 ; defaults to stream color
    location?:    text<128>,
    notes?:       NoteBody,
    travel_time_before?: duration,            ; surfaced as a leading buffer
    travel_time_after?:  duration,
    source:       BlockSource,
    external_id?: text,                       ; for round-tripping with Google Calendar
    rrule?:       text,                       ; for recurring blocks (rare; usually use Routine)
    deleted:      bool,
}

BlockSource = "sunrise"                       ; created in Sunrise
            / "import:gcal"                   ; imported from Google Calendar
            / "import:ics"                    ; imported from a one-shot .ics file
```

## Block title

A Block's `title` is a **shadow copy** of the bound task's title at creation/binding time, not a live binding. Subsequent edits to the bound task's title do not propagate to the Block.

- Optional `title_track_task: bool = false`. When true, the Block recomputes its title on read from the bound task's current title. CRDT field; default false to preserve user-edited Block titles.
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

## v1 scope

`Block` in v1 carries `id`, `created_at`, `updated_at`, `stream_id`,
`starts_at`, `ends_at`, `title`, `title_track_task`, `tasks` and `deleted`.
`timezone` is subsumed by `SunriseTime` (a zoned bound carries its own zone).
`color`, `location`, `notes`, the travel-time buffers, `source`, `external_id`
and `rrule` are specified above and not yet modelled: they land with the
calendar-integration slice, and the forward-compat `unknown` map means a build
that adds them can round-trip through this one without loss.

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
- **Move.** Drag updates `starts_at`/`ends_at`. Travel-time buffers are recalculated.

## Calendar integration

Blocks are the **bidirectional bridge** with external calendars. See [`../09-integrations/google-calendar.md`](../09-integrations/google-calendar.md):

- A Sunrise-created Block can be pushed to Google as an event (opt-in per Stream or per Block).
- A Google event can be imported as a read-only Block (`source = import:gcal`). Sunrise *will not* mutate imported blocks; the user must "convert to Sunrise block" to edit.

This split prevents accidental write-amplification into the user's primary calendar.

## CRDT mapping

- Map of LWW-register fields.
- `tasks`: observed-remove set.

## Conflicts

When two devices schedule overlapping Blocks for the same task, both Blocks coexist. UI surfaces the conflict; user resolves manually. We do *not* auto-merge or auto-delete a Block.

The Calendar view shades the overlap region and shows a "Resolve" overflow menu with three actions:

- **Keep both** — no-op; closes the menu.
- **Merge** — combines the two Blocks into one with the union time range and concatenated tasks. CRDT-wise: tombstone the two original Blocks and create a new one in a single submit batch.
- **Adjust times** — opens a side-by-side editor for both Blocks.
