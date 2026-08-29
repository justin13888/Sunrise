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

Shared types are defined in
[`overview.md` §Common CDDL types](./overview.md#common-cddl-types).

This is the **v1 shape** — what a device actually writes and signs today:

```cddl
Block = {
    id:           tstr .regexp "blk_[0-9A-HJKMNP-TV-Z]{26}",
    created_at:   timestamp,
    updated_at:   timestamp,
    stream_id:    entity-ref,                 ; str_ ref; REQUIRED, not optional
    starts_at:    stime,                      ; see tasks.md §stime
    ends_at:      stime,                      ; resolves after starts_at
    title?:       text<256>,                  ; defaults to the bound task's title
    title_track_task: bool,                   ; default false; recompute title from the one bound Task
    tasks:        [* entity-ref],             ; tsk_ refs bound to this Block
    deleted:      bool,
    unknown-fields,                           ; see overview.md
}
```

Two corrections against earlier revisions of this spec:

- **`stream_id` is required.** A Block always has an owning Stream; there is
  no untinted Block. It was specified as optional and has never been written
  that way.
- **There is no `timezone` field.** `SunriseTime` subsumes it — a `zoned`
  bound carries its own IANA zone, and a `floating` or `all_day` bound
  deliberately carries none. A second, Block-level zone would be a third
  answer to a question the bounds already answer, and the three could
  disagree.

### Specified but not modelled

These are the calendar-integration slice. They are **not** on the wire today,
and a build that adds them round-trips through this one without loss, because
the forward-compat `unknown-fields` map preserves them verbatim:

```cddl
; Not yet modelled. Landing these is a DOC_SCHEMA_V bump, not a break.
color?:              BlockColor    ; defaults to stream color
location?:           text<128>
notes?:              NoteBody
travel_time_before_s?: uint        ; seconds, matching the rest of the domain
travel_time_after_s?:  uint
source?:             BlockSource
external_id?:        tstr          ; for round-tripping with Google Calendar
rrule?:              RRule         ; recurring blocks (rare; usually use a Routine)

BlockSource = "sunrise"                       ; created in Sunrise
            / "import:gcal"                   ; imported from Google Calendar
            / "import:ics"                    ; imported from a one-shot .ics file
```

`source` and `external_id` are still unmodelled, but the reason has changed:
there **is** an importer now (`sunrise ical import`, and `import_ical` on the
seam), and it works without them. Rather than add two columns, it hashes
`(source, uid)` into the Block's **own id**, exactly as a materialized routine
occurrence hashes `(routine, occurrence)` into a Task's. Re-importing the same
file therefore computes the same id and updates the Block already there, giving
the dedup rule in [`../09-integrations/icalendar.md`](../09-integrations/icalendar.md)
with no side table to keep in step with the vault. The fields would still be
needed to round-trip a *foreign* id back out, which is why they stay listed
here.

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
- **Move.** Drag updates `starts_at`/`ends_at`. Travel-time buffers are recalculated.

## Calendar integration

Blocks are the **bidirectional bridge** with external calendars. See [`../09-integrations/google-calendar.md`](../09-integrations/google-calendar.md):

- A Sunrise-created Block can be pushed to Google as an event (opt-in per Stream or per Block).
- A Google event can be imported as a read-only Block (`source = import:gcal`). Sunrise *will not* mutate imported blocks; the user must "convert to Sunrise block" to edit.

This split prevents accidental write-amplification into the user's primary calendar.

## Merge mapping

The whole Block is one last-writer-wins unit on `(hlc, device_id, seq)`
([ADR-0014](../11-adr/0014-entity-level-lww-merge.md)). `tasks` is an
observed-remove set in the target state only; today a concurrent bind on one
device and unbind on another resolves by timestamp.

The `block_tasks` projection is the sole writer of the binding relation and
`Task.blocks` is derived from it, so the two never disagree — see §Symmetry
with `Task.blocks`.

## Conflicts

When two devices schedule overlapping Blocks for the same task, both Blocks coexist. UI surfaces the conflict; user resolves manually. We do *not* auto-merge or auto-delete a Block.

The Calendar view shades the overlap region and shows a "Resolve" overflow menu with three actions:

- **Keep both** — no-op; closes the menu.
- **Merge** — combines the two Blocks into one with the union time range and concatenated tasks. Mechanically: tombstone the two original Blocks and create a new one in a single submit batch.
- **Adjust times** — opens a side-by-side editor for both Blocks.
