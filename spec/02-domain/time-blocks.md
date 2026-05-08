---
status: draft
---

# Time Blocks

A Block is a scheduled time range, optionally bound to one or more Tasks. Blocks are how Sunrise integrates with calendars and supports time-blocking workflows.

## Fields

```cddl
Block = {
    id:           tstr .regexp "blk_[A-Z0-9]{26}",
    created_at:   tdate,
    updated_at:   tdate,
    title?:       text<256>,                  ; defaults to bound task's title
    starts_at:    tdate,
    ends_at:      tdate,                      ; > starts_at
    timezone:     text,                       ; IANA tz
    tasks:        [* tstr],                   ; bound task IDs
    stream_id?:   tstr,                       ; for tinting / filtering
    color?:       BlockColor,                 ; defaults to stream color
    location?:    text<128>,
    notes?:       NoteBody,
    travel_time_before?: duration,            ; surfaced as a leading buffer
    travel_time_after?:  duration,
    source:       BlockSource,
    external_id?: text,                       ; for round-tripping with Google/CalDAV
    rrule?:       text,                       ; for recurring blocks (rare; usually use Routine)
    deleted:      bool,
}

BlockSource = "sunrise"                       ; created in Sunrise
            / "import:gcal"                   ; imported from Google
            / "import:caldav"
            / "import:ics"
```

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
