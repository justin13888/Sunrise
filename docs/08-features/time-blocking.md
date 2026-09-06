---
status: accepted
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

### Snap, overlap, and reminders

- **Snap granularity.** 15 min default; user-configurable to {5, 10, 15, 30, 60} minutes — see [`../07-clients/interaction-patterns.md`](../07-clients/interaction-patterns.md#drag-and-drop-ux-tokens).
- **Overlap.** Overlapping Blocks coexist (the user might intend to be at two places); UI shades overlaps and offers a "resolve" tool.
- **Reminders.** Per-block override; default inherits the Stream default of 15-min-before. Both fields are nullable to disable.

## Travel-time buffers

*Target state.* A Block would carry `travel_time_before` and `travel_time_after`, surfaced as a half-tone leading/trailing rectangle on the grid. `crates/sunrise-domain/src/block.rs` models neither field. Used for reminders ("leave for the gym in 15 min") and to prevent stacking conflicting Blocks.

## Conflicts

Overlapping Blocks coexist (the user might intend to be at two places). UI shades overlaps and offers a "resolve" tool.

## Recurring blocks

*Target state.* A Block with an `rrule`. `Block` carries no `rrule` field; recurrence lives on `Routine` ([`routines-and-recurrence.md`](../02-domain/routines-and-recurrence.md)), and the two have not been joined.

## External calendar integration

- **Import (read-only Blocks).** *Target state for the Google half* — Google Calendar is deferred out of v1 ([ADR-0020](../11-adr/0020-v1-must-demotions.md) §(b), issue #4) and no toggle exists on any client. Toggle per integration: pull events from Google Calendar. Imported Blocks are tagged `source = import:gcal`. They are read-only (cannot edit or bind tasks). User can convert one to a Sunrise Block (snapshot to a new editable Block; the import remains). One-shot `.ics` import is also supported and yields `source = import:ics` Blocks.
- **Export (push to external).** *Target state.* Per-Stream toggle. When on, Sunrise-created Blocks within that Stream are pushed as events to a designated Google Calendar. Edits propagate. Conflict policy: external is updated on change; if the external event is deleted out-of-band, we drop the binding and surface a notification.

## Why not a native calendar server inside Sunrise

Calendars are a 10-year product; we use them, we don't compete with them.

## CLI experience

There is no *block* subcommand: a Block cannot be created, moved, bound or
deleted from the CLI, so the grid itself is reachable only from the macOS
client. The one exception is bulk movement — `sunrise ical import` writes Blocks
through `Command::ImportBlock`, and `sunrise ical export [today|day|week]`
reads them back out — which is data interchange rather than time-blocking. This
section described the terminal client's day column, removed by
[ADR-0019](../11-adr/0019-swiftui-macos-client.md).

## Interactions with reminders

- Block start fires a 15-min-before reminder (configurable, per Stream defaults).
- Travel-time-before adjusts the reminder time backwards.
- Reminders are local notifications — see [`notifications.md`](./notifications.md).

## Today integration

Today's "Scheduled" section is composed of Blocks for today, sorted by start time, with bound tasks expanded inline.

## States

Empty / loading / error states follow the three-state contract in [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#three-state-view-contract).
