---
status: draft
---

# Time Blocking

Connecting tasks to the calendar grid. Blocks live alongside tasks; this spec describes the UX and the connector to external calendars.

## Why

Multi-stream operators benefit from saying "from 9 to 11 I'm doing Work A" rather than juggling task lists. A Block makes that intention concrete.

## UX

- Drag a task onto the calendar grid → creates a Block bound to that task with the dragged time range.
- Drag a Block on the grid → moves it; bound tasks travel with it.
- Drag the bottom edge → resize.
- Click an empty grid slot → create an empty Block (later bind a task to it).
- Click a Block → detail pane: bind/unbind tasks, edit notes, set color, set travel buffers.

## Travel-time buffers

A Block can have `travel_time_before` and `travel_time_after`. Surfaced visually as a half-tone leading/trailing rectangle on the grid. Used for reminders ("leave for the gym in 15 min") and to prevent stacking conflicting Blocks.

## Conflicts

Overlapping Blocks coexist (the user might intend to be at two places). UI shades overlaps and offers a "resolve" tool.

## Recurring blocks

Use a Block with `rrule`. UX prompts: "every weekday 9-10 → Work A standup."

## External calendar integration

- **Import (read-only Blocks).** Toggle per integration: pull events from Google Calendar / CalDAV. Imported Blocks are tagged `source = import:gcal/caldav`. They are read-only (cannot edit or bind tasks). User can convert one to a Sunrise Block (snapshot to a new editable Block; the import remains).
- **Export (push to external).** Per-Stream toggle. When on, Sunrise-created Blocks within that Stream are pushed as events to a designated external calendar. Edits propagate. Conflict policy: external is updated on change; if the external system's event is deleted out-of-band, we drop the binding and surface a notification.

## Why not a native CalDAV server inside Sunrise

We considered it. Decision: integrate, don't replace. Calendars are a 10-year product; we use them, we don't compete with them.

## TUI experience

- No grid; instead, a vertical day column with hour ticks.
- `T` (time-block mode) opens a focused day view.
- Schedule via `s` on a task with natural-language input ("9-11 tomorrow").

## Interactions with reminders

- Block start fires a 15-min-before reminder (configurable, per Stream defaults).
- Travel-time-before adjusts the reminder time backwards.
- Reminders are local notifications — see [`notifications.md`](./notifications.md).

## Today integration

Today's "Scheduled" section is composed of Blocks for today, sorted by start time, with bound tasks expanded inline.
