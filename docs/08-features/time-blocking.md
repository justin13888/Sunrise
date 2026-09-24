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

Calendar integrations are **read-only**
([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)).

- **Fetched events are `ExternalEvent`s, never Blocks.** Events from Google
  Calendar, Microsoft Graph and CalDAV become read-only `ExternalEvent`
  entities: recurring events are stored as the occurrences the provider
  expanded inside the fetch window, each with a deterministic id. The grid draws
  them beside Blocks, the planner treats them as fixed, and no client edits
  them or binds a Task to them. Not built; ranked on the roadmap
  ([`../roadmap.md`](../roadmap.md)) as
  [#4](https://github.com/justin13888/Sunrise/issues/4).
- **Nothing is pushed out.** Sunrise writes to no external calendar, and there
  is no `import:gcal` source.
- **`.ics` files are the one path that writes Blocks.** One-shot `.ics` import
  yields `fixed` Blocks with `source = import:ics`, and `.ics` export writes
  Blocks back out; `external_id` serves only this round trip
  ([`../09-integrations/icalendar.md`](../09-integrations/icalendar.md)).

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
