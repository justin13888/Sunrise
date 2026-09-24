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
> today and an Overdue section from `scheduled_at` and `due_at` over a rolling
> now + 24 h window (`crates/sunrise-core/src/engine/query.rs#query_today`),
> with overdue meaning `due_at` before the start of today. [#334](https://github.com/justin13888/Sunrise/issues/334) and [#336](https://github.com/justin13888/Sunrise/issues/336)
> move it to the composition above.

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
  order. A manual order, where the user has arranged the list, is a synced
  fractional index computed in Rust ([#340](https://github.com/justin13888/Sunrise/issues/340)), replacing today's per-device
  order ([`../07-clients/interaction-patterns.md` §Reorder](../07-clients/interaction-patterns.md#reorder)).
  `sort_order` on Streams orders the sidebar, not this list.
- **Sub-tabs (mutually exclusive)**: Open / Done / All / Routines. Rendered as
  full-width tabs at the top of the pane, not filter chips, since chips imply
  combinable filters and these states are mutually exclusive.
- Filters as chips: contexts, priority, energy, lateness, deadline window.

## Saved views

A user creates a saved view by filtering the current view to taste and choosing
"Save this view as…", or by saving a search with ⌘S ([`search.md`](./search.md)).

A saved view is a **synced `SavedView` entity** in the vault ([#341](https://github.com/justin13888/Sunrise/issues/341)), merged
per field ([ADR-0044](../11-adr/0044-per-field-ops.md)), so a view saved on one
device exists on every device:

```rust
pub struct SavedView {
    pub id: EntityRef,
    pub name: String,
    pub spec: ViewSpec,
    pub sort_order: FractionalIndex,   // position in the saved-views list
    pub deleted: bool,
}

pub enum ViewSpec {
    /// A primary view with a typed filter.
    Filtered { view: ViewKind, filter: ViewFilter, sort: ViewSort },
    /// A saved search: canonical query text and the grammar it was written in.
    Search { query: String, grammar_v: u16 },
}

pub struct ViewFilter {
    pub streams: Option<StreamSelector>,       // ids (with descendants), or "no stream"
    pub contexts: Vec<EntityRef>,              // by id, so a rename keeps working
    pub priority: Option<RangeInclusive<u8>>,
    pub energy: Vec<Energy>,
    pub lateness: Vec<Lateness>,
    pub place: Option<EntityRef>,
    pub window: Option<DateWindow>,            // over planned_at / target_at / hard_due_at, in reader-civil terms
    pub text: Option<String>,                  // search grammar
}
```

The **result** is recomputed on each device by the core; it is never stored.
Per-device scroll position and collapsed sections stay in the device-local
overlay. An unknown filter kind written by a newer build is preserved on
round-trip ([ADR-0045](../11-adr/0045-schema-identity-and-feature-gating.md)).

Examples:

- "Errands today" — context `@errands`, planned on or before today, all open
  streams.
- "Waiting on" — context `waiting-on`, sorted by `created_at` descending.
- "This week's deep work" — `@deep-work`, planned within the current week.

> **Today's build** keeps saved views as a per-device TOML file,
> `~/.config/sunrise/views.toml`, holding a view, a free-text query and context
> *names* (`crates/sunrise-client-core/src/views.rs#SavedView`). A view saved on
> the Mac does not exist on the iPhone, and renaming a context breaks every view
> naming it. [#341](https://github.com/justin13888/Sunrise/issues/341) imports the file once, resolving names to ids and
> reporting the ones it cannot.

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
