---
status: accepted
---

# Planning Views

The views the user uses to *decide* what to work on. Not the editor; the planner.

## Today

The default landing view.

Composition:

1. **Scheduled Blocks** for today, in time order.
2. **Tasks scheduled for today** (`scheduled_at` falls on today in user's tz), grouped by Stream.
3. **Tasks due today** that are not scheduled.
4. **Promoted from Inbox** (manually pulled into Today via drag/keyboard).
5. **Overdue** (folded section by default; opens with a count badge).

Today is **computed**, not edited directly except via promote/demote — and via
capture into Today's own bar, which schedules an otherwise-undated line for now
rather than filing it out of sight in Inbox. That is a promotion in everything
but name: the core has no promote-into-Today command, so `scheduled_at` is the
only lever, and the field it writes is the one this view already selects on.
See `inbox-and-capture.md` §Capture UX patterns, *Today default*.

### Overdue boundary

A task is **overdue** iff `due_at < start_of_today_local`. Tasks whose `due_at` falls within today are "Today" tasks, not overdue, regardless of the wall-clock time within the day.

## Upcoming

A rolling view, span user-toggleable in the view header (chip group: 7 / 14 / 30 days). Selection persists per-device.

- Day-by-day breakdown of scheduled tasks and blocks.
- A separate "no date" lane per Stream so unscheduled but expected items show up.
- Drag tasks across days to reschedule.

## Stream view

Per Stream:

- Header: name, color, paused/archived state, share status.
- Default sort: soonest first — `COALESCE(scheduled_at, due_at)`, then id as a
  deterministic tiebreak so two replicas render one order. Not priority, and not
  a manual order: a Task has no ordering facet to sort by. Manual arrangement of
  a task list is per-device and applied client-side, per
  [`../07-clients/interaction-patterns.md` §Reorder](../07-clients/interaction-patterns.md#reorder).
  `sort_order` belongs to Streams, and orders the sidebar rather than this list.
- **Sub-tabs (mutually exclusive)**: Open / Done / All / Routines. Rendered as full-width tabs at the top of the pane — not filter chips, since chips imply combinable filters and these states are mutually exclusive in v1.
- Filters as chips: contexts, priority, energy, due date.

## Saved views

A user creates a saved view by:

1. Filtering current view to taste.
2. "Save this view as…"

The saved-view **spec** is an ordinary synced entity (entity-level LWW). The **result** is recomputed on each device. Per-device sort and scroll position are local-only.

Examples:

- "Errands today" — context `@errands`, scheduled_at ≤ today, all open streams.
- "Waiting on" — context prefix `waiting-on:`, sorted by created_at desc.
- "This week's deep work" — `@deep-work`, scheduled_at within current week.

## Calendar (week view)

Vertical day columns × hour rows. Time blocks render as colored rectangles tinted by Stream. Tasks not yet scheduled live in a sidebar drawer; drag-onto-grid schedules them.

## Stream of streams

One screen showing every Stream as a card with: count of open tasks, count overdue, next due, recent activity. Useful for the "where am I overall" zoom-out.

## What we explicitly don't ship

- A single "all tasks" mega-view sorted by date. Multi-stream operators get drowned in such a list.
- Kanban boards. Decided non-goal — we are not a project management tool. (A Stream view *can* group by `state`; that's as far as we go.)
- Gantt charts.

## View composition rules

- Every view is built from the same primitives: a query against the local DB + a renderer.
- New views are added via spec changes, not user-built — saved views fill that gap.

## States

Empty / loading / error states for every planning view follow the three-state contract in [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#three-state-view-contract). Per-view empty copy lives in the same file's "Per-view empty-state copy" table.
