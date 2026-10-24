---
status: accepted
---

# Planning Views

The views the user uses to *decide* what to work on. Not the editor; the planner.

Every view is a query in the Rust core plus a renderer. Grouping, ordering, day
boundaries, lateness and membership are computed in Rust; a client never
re-derives them. Times follow [`../10-cross-cutting/time.md`](../10-cross-cutting/time.md):
days are planner days in the reader's zone, bounded by the day schedule
([`../02-domain/day-schedule.md`](../02-domain/day-schedule.md) §Planner day)
where one is set and civil days otherwise, and never a rolling 24-hour window. Task times are `planned_at`,
`target_at` and `hard_due_at`, per
[ADR-0047](../11-adr/0047-deadlines-and-lateness.md). Every gesture that moves
work in time goes through the planner ([`planner.md`](./planner.md)).

## Today

The default landing view.

Composition, in order:

1. **Blocks** for today, in time order, with external calendar events among
   them ([ADR-0049](../11-adr/0049-calendar-integrations-per-device-oauth.md)).
2. **Planned for today**: tasks whose `planned_at` falls on today, grouped by
   Stream.
3. **Carried over**: open tasks whose `planned_at` fell on an earlier planner
   day and that are not late. A slipped plan is intent, not lateness
   ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md) §2), so it is carried
   into Today without any write; nothing rolls `planned_at` forward. Each row
   shows the day it was planned for and offers Defer and "Plan my day". A
   carried-over task that is also late appears under **Late** instead.
4. **Due today**: tasks whose `hard_due_at` or `target_at` falls on today and
   that are not planned for today, each marked with which deadline it is.
5. **Late**: open `LateHard` then `LateSoft` tasks, each with its state
   ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md)). Folded by default,
   with a count badge, and a link to **Needs decision**.

Today is **computed**, not edited directly. Pulling a task into Today sets its
`planned_at` to today, and capture into Today's own bar gives an otherwise
undated line `planned_at = today` so the row appears where it was typed
([`inbox-and-capture.md`](./inbox-and-capture.md) §Capture UX patterns, *Today
default*).

> **Today's build** composes Scheduled Blocks, tasks scheduled today, tasks due
> today and an Overdue section from `scheduled_at` and `due_at`: every open task
> scheduled or due on or before the reader's planner day
> (`crates/sunrise-core/src/engine/query.rs#query_today`), with overdue meaning
> `due_at` on a planner day before today's
> (`sunrise_domain::today_section_in`). The day is bounded by the day schedule
> ([`../02-domain/day-schedule.md`](../02-domain/day-schedule.md), #338), and
> is the civil day without one. [#334](https://github.com/justin13888/Sunrise/issues/334)
> moves it to the composition above.

## Upcoming

A rolling, day-by-day view of what is coming. [#351](https://github.com/justin13888/Sunrise/issues/351) builds it.

- **Span:** 7, 14 or 30 days, chosen from a chip group in the view header. The
  span is a device-local preference (`views.upcoming.span_days`, default 7).
- **Query:** `Query::Upcoming { now, zone, span_days, contexts }` returns one
  entry per planner day from today, each holding:
  - blocks and external events, in time order;
  - tasks whose `planned_at` falls that day, timed ones in time order, then
    date-only ones by Stream;
  - tasks whose `target_at` or `hard_due_at` falls that day and are not planned
    that day, marked by which deadline it is.
- **Day boundaries** come from the day schedule: a task planned for 00:30 on a
  night whose sleep time is 01:00 belongs to the previous day. Without a
  schedule, civil midnight.
- **No-date lane:** below the days, open tasks with no `planned_at`, grouped by
  Stream with a "No stream" group ([ADR-0046](../11-adr/0046-optional-stream.md)),
  at most 5 per Stream with "N more", ordered deterministically (target date,
  then priority, then id).
- **Rescheduling:** dragging a task onto another day (or into the no-date
  lane) sends a `BulkDefer` or `Unschedule` intent to `plan_preview` and shows
  its ripple live before the drop ([`planner.md`](./planner.md)). Committing a
  `BulkDefer` is a deferral: it writes `planned_at` and `+1` on
  `deferred_count` for each dragged task, the same ops as triage Defer. The keyboard
  equivalent moves the selection a day with ⌥←/⌥→ and previews the same diff.
- **Bindings:** ⌘3 opens Upcoming ([`keyboard.md`](./keyboard.md)).
- **Surfaces:** a sidebar entry on desktop, an entry in Browse on phone and
  tablet, and `sunrise upcoming [7|14|30]` on the CLI.

> **Today's build** has the query and the CLI. `Query::Upcoming { now_ms,
> span_days, contexts }` (`crates/sunrise-core/src/engine/upcoming.rs`) folds
> through the pure `crates/sunrise-domain/src/upcoming.rs#build_upcoming`: days
> are planner days in the reader's zone (`Clock::timezone`, ADR-0066 §5, rather
> than a `zone` argument), each carrying its own `[start, end)` so a DST day
> is 23 or 25 hours, and civil days with no day schedule; a task is on the day
> `sunrise_domain::planner_day_of` puts it, keeping its kind. Until
> [#334](https://github.com/justin13888/Sunrise/issues/334)
> the plan is `scheduled_at` and a day's deadlines are one `due` list over
> `due_at`; and until
> [ADR-0046](../11-adr/0046-optional-stream.md) lands, "No stream" is the
> Inbox, listed last. `span_days: None` reads `views.upcoming.span_days`; any
> span but 7, 14 or 30 is refused. Blocks overlap a day as on the calendar
> grid. The seam carries it as `CoreQuery.upcoming` / `UpcomingReport`, and
> `sunrise upcoming` prints it. Drag rescheduling (`plan_preview`, #343/#344)
> and the Apple surfaces are not built.

## Needs decision (triage)

The triage tray from [ADR-0047](../11-adr/0047-deadlines-and-lateness.md): every
open task that is late (soft or hard) or stale and not yet acknowledged, in the
core's order (`LateHard`, then `LateSoft`, then `Stale`; oldest first; then id).
[#335](https://github.com/justin13888/Sunrise/issues/335) builds it.

- **Where it appears:** a sidebar entry with a count badge (hidden at zero), ⌘4;
  the folded Late section of Today links to it; the morning brief's **Open
  Triage** action opens it ([`notifications.md`](./notifications.md)).
- **Rows** show the task, its lateness state and since when, and its Stream.
- **Four actions**, each on a single row or a multi-selection, each with its key
  hint:
  - **Defer** (`D`): pick a day, or "next free slot" from the planner, previewed
    like any drag. It writes `planned_at` and `+1` on `deferred_count`, whether
    issued as `Command::Triage` or committed from the preview with
    `plan_commit` ([ADR-0047](../11-adr/0047-deadlines-and-lateness.md) §4).
  - **Already done** (`X`, with `Shift+X` to choose when): completes with a backdated
    `completed_at`.
  - **Drop** (`Backspace`): cancels, with an optional reason.
  - **Keep** (`Shift+K`; plain `k` is vim's "up"): acknowledges; the task
    leaves the tray until its next lateness transition.
- **Bulk is one command.** A multi-selection issues one `Command::Triage` over
  all its ids, validated all-or-nothing, and is one undo step.
- **Nothing is automatic.** Nothing leaves the tray except by one of these four
  actions or by the task changing.

## Stream view

Per Stream:

- Header: name, color, paused/archived state, share status.
- Default sort: soonest first — the earliest of `planned_at`, `target_at` and
  `hard_due_at`, then id as a deterministic tiebreak so two replicas render one
  order. A manual order, where the user has arranged the list, is
  `Task.sort_order`, a synced fractional index computed in Rust by
  `Command::MoveTasks` ([#340](https://github.com/justin13888/Sunrise/issues/340)): the placed tasks lead, by key and then by
  id, and the rest follow in the default sort. `Query::StreamTasks`,
  `Query::Inbox` and `Query::ContextTasks` return their lists in that order
  (`crates/sunrise-core/src/engine/query.rs#sort_listing`).
  `Stream.sort_order` orders the sidebar, not this list.
- **Sub-tabs (mutually exclusive)**: Open / Done / All / Routines. Rendered as
  full-width tabs at the top of the pane, not filter chips, since chips imply
  combinable filters and these states are mutually exclusive.
- Filters as chips: contexts, priority, energy, lateness, deadline window.

## Saved views

A user creates a saved view by filtering the current view to taste and choosing
"Save this view as…", or by saving a search with ⌘S ([`search.md`](./search.md)).

A saved view is a **synced `SavedView` entity** (`svw_`) in the vault ([#341](https://github.com/justin13888/Sunrise/issues/341)),
written only by `Patch`, one register per field
([ADR-0044](../11-adr/0044-per-field-ops.md)), so a view saved on one device
exists on every device, and an edit of one field on one device and of
another on a second both survive. The shipped shape
(`crates/sunrise-domain/src/saved_view.rs`):

```rust
pub struct SavedView {
    pub id: EntityRef,
    pub name: String,                  // tracked; not unique
    pub view: ViewKind,                // tracked; lossless (today, inbox, stream, search, calendar, …)
    pub filter: ViewFilter,            // tracked; one register for the whole filter
    pub sort: ViewSort,                // quiet; default, manual (#340), created, updated, title, priority, due, scheduled
    pub sort_order: Option<String>,    // quiet; position in the saved-views list
    pub deleted: bool,                 // tombstone; Restore brings it back
}

pub struct ViewFilter {                // every part optional, ANDed; values inside a part ORed
    pub streams: Vec<EntityRef>,
    pub contexts: Vec<EntityRef>,      // by id, so a rename keeps working
    pub context_names: Vec<String>,    // names an import could not resolve: reported, matched by nothing
    pub priority: Option<PriorityRange>,
    pub energy: Vec<Energy>,
    pub window: Option<DateWindow>,    // over scheduled_at or due_at, in civil days from the reader's today
    pub text: String,                  // a Search view's query; a title match otherwise
    // + any part a newer build added, kept verbatim
}
```

The **result** is recomputed on each device by the core
(`Query::SavedView`, `crates/sunrise-core/src/engine/saved_view.rs#evaluate_saved_view`);
it is never stored. The view's base list is Today, the Inbox, the named
Streams (every task when none), or the search for a Search view; the filter
then narrows it in the reader's zone, and the sort orders it. A view of a
screen with no list (calendar, focus, routines, review, the briefs) returns no
tasks, and the client opens that screen. The result names any context the
filter holds that no longer exists, and says when a filter part or view kind
from a newer build could not be applied (`complete: false`): it then shows
more than that build would, never silently less. A filter part this build
does not know is written back unchanged by every edit
([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md) §6).
Per-device scroll position and collapsed sections stay in the device-local
overlay.

The entity is the feature `saved_view.entity`: no view is written into a
vault until every paired device has advertised it, because a build without
it cannot decode a `Patch` naming `svw_`. Views are found by search under
`kind:view`, by name.

Examples:

- "Errands today" — context `@errands`, Today.
- "Waiting on" — context `waiting-on`, sorted by `created`.
- "This week's deep work" — `@deep-work`, `scheduled` within 0 to 6 days.

The per-device `~/.config/sunrise/views.toml` of earlier builds is imported
once into each vault (`sunrise_client_core::views::plan_import`): context
names resolve to ids, a name that resolves to nothing is reported and kept in
`context_names`, a view the vault already has by name is left alone, and a
marker beside the vault records the import. The CLI runs it the first time
`sunrise views` meets a vault and no longer writes the file.

> **Not built yet.** A "no stream" selector waits for optional streams
> ([#332](https://github.com/justin13888/Sunrise/issues/332)), a lateness
> predicate for lateness states ([#334](https://github.com/justin13888/Sunrise/issues/334)),
> a place predicate for Place ([#339](https://github.com/justin13888/Sunrise/issues/339)),
> and the window over `planned_at`, `target_at` and `hard_due_at` for those
> fields ([#336](https://github.com/justin13888/Sunrise/issues/336)); each
> arrives as a new part of `ViewFilter`, which older builds keep. The full
> search grammar as a filter over every view is [#345](https://github.com/justin13888/Sunrise/issues/345).
> The Apple client still reads and writes `views.toml`; its import
> (`plan_saved_views_import` across the seam) and its switch to the entity
> are its remaining part of #341.

## Calendar (week view)

Vertical day columns × hour rows, spanning each planner day. Blocks render as
coloured rectangles tinted by Stream; external events in their calendar's
colour, visibly read-only. Tasks with no time live in a drawer; dragging one onto
the grid places it, and every drag previews its ripple before the drop
([`planner.md`](./planner.md)).

## Stream of streams

One screen showing every Stream as a card with: count of open tasks, count
late, next deadline, recent activity. Useful for the "where am I overall"
zoom-out.

## What we explicitly don't ship

- A single "all tasks" mega-view sorted by date. Multi-stream operators get
  drowned in such a list.
- Kanban boards. Decided non-goal — we are not a project management tool. (A
  Stream view *can* group by `state`; that's as far as we go.)
- Gantt charts.

## View composition rules

- Every view is built from the same primitives: a query against the local DB,
  evaluated in Rust, and a renderer.
- New views are added via spec changes, not user-built — saved views fill that
  gap.
- Clients of one device class present the same views
  ([`../07-clients/parity-matrix.md`](../07-clients/parity-matrix.md)).

## States

Empty / loading / error states for every planning view follow the three-state
contract in [`../07-clients/shared-ui-system.md`](../07-clients/shared-ui-system.md#three-state-view-contract).
Per-view empty copy lives in the same file's "Per-view empty-state copy" table,
and on desktop each empty state names the key of the action that fills it
([`keyboard.md`](./keyboard.md) §Rules).
