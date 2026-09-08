---
status: accepted
---

# Interaction Patterns

Cross-platform user-facing behaviors. Implemented natively per platform; consistent in *what* the user can do.

## Quick capture

| Trigger | Platform |
|---|---|
| Global hotkey (default `Cmd + Shift + N`), menu bar item | macOS |
| `sunrise capture "…"` | CLI |
| Lock-screen widget tap | iOS |
| Quick Settings tile | Android |
| Browser keyboard shortcut (when extension installed) | Web |

Capture syntax (parsed by core):

- `Buy milk` → task in Inbox
- `Buy milk #errands` → task in stream "errands" (creates if missing — confirms first time)
- `Buy milk @home` → task with context "home"
- `Buy milk ^tomorrow 9am` → scheduled
- `Buy milk !2` → priority 2
- `Buy milk ~30m` → estimated 30 min
- Multiple: `Buy milk #errands @home ^tomorrow 9am !2`

Parser is a single deterministic function in core (string in, structured task draft out).

## Mark done

- Click the checkbox, or press `Space` in keyboard mode. `sunrise done <id>` from a script.
- Confetti? No. We don't gamify.

## Defer

- Right-click → "Defer to…" / long-press → menu / keyboard `d` then a date picker.
- The system increments `deferred_count`; weekly review surfaces serial deferrers.

## Promote (Inbox → Stream)

- Drag onto a Stream / `m` then pick stream.
- One-shot inline: type `#stream` in the task title.

## Reorder

- Drag within a list (cross-platform).
- Keyboard: `Alt+↑/↓` to move within parent.

**Streams sync; tasks do not.** The two halves of this gesture land in two
different places, and the difference is in the domain rather than in any one
client:

| | Where the order lives | Syncs |
|---|---|---|
| Streams | `Stream.sort_order`, a fractional index ([`../02-domain/streams.md` §Sort order](../02-domain/streams.md#sort-order)) | Yes |
| Tasks | Per device — `UserDefaults` on macOS (`ListOrderStore`) | No |

A Task has **no ordering facet at all**: not on `Task`, not on `TaskEdit`, and
none in [`../02-domain/tasks.md`](../02-domain/tasks.md). There is nothing to
write, so there is nothing to sync, and a hand-arranged task list is a fact
about the machine it was arranged on — kept beside the other device facts, lost
with the device, and never mistaken for the user's data. Giving Tasks their own
`sort_order` is a `DOC_SCHEMA_V` bump and is not in v1.

A per-device task order also has to answer a question the synced one does not:
what happens to a row it has never seen. Rows the order knows come first, in
the order it remembers; everything else follows in the order the core returned
it, so a task arriving by sync or capture never looks like somebody rewrote the
arrangement. Lists the core itself ranks — Today, sectioned by urgency, and
Search, ranked per keystroke — decline the gesture rather than accepting a drop
they cannot honour.

## Schedule

- Drag onto Today / drag onto a calendar block / open detail and edit.
- Keyboard: `s` to open scheduler, then natural-language input ("tomorrow 9am").

## Focus mode

- Pick a task → "Focus" button or `f`.
- Renders task full-screen with optional Pomodoro timer.
- One key to: complete (`x`), defer (`d`), capture-aside (`a`), exit (`Esc`).
- Breaks (5 min default after 25 min focus) are surfaced as a soft suggestion.

## Multi-select

- Cmd/Shift-click on desktop and web.
- Long-press + tap-to-extend on mobile.
- `V` enters visual (range) selection when vim mode is on.

## Undo

- `Cmd+Z`, or `u` when vim mode is on. Implemented by `sunrise-client-core::undo`, which builds the **inverse command** from the rows the client is holding — undo is a new write that converges, not a rollback.
- **Undoable**: any user-initiated CRUD on entities; explicit user actions in views.
- **Not undoable**: sync receipts (other-device ops), background routine generation, server-initiated ops.
- Time-bound: undo within 5 min is one-tap. > 5 min: confirmation modal `"Undo this change from <Nm ago>?"`.
- Per-device undo stack, 64 entries; not synced.

## Drag-and-drop matrix

Android and Web are omitted; see [`parity-matrix.md`](./parity-matrix.md).
The iOS column is not a second implementation: every modifier below sits in a
shared file with no platform fork. Three things still differ. The gesture is a
long-press drag where the Mac has a click-drag. One cell splits outright —
*Task → Calendar block* is a **Yes** on macOS and a **No** on iOS — which is
not about the gesture at all, but about which surfaces the shell can put in
front of a user at once. And one is narrowed rather than lost: *File → Task* is
a plain **Yes** on the Mac and iPad-only on iOS, because dragging a file in
from another app needs two apps on screen. The gesture is the whole of the
first difference; the other two are set out below.

| From → To | macOS | iOS | CLI |
|---|---|---|---|
| Task → Stream | Yes | Yes | N/A |
| Task → Context | Yes | Yes | N/A |
| Task → Calendar block | Yes | **No** — see below | N/A |
| Calendar block → Task | **No** — see below | **No** — see below | N/A |
| Calendar block → Calendar (move / resize) | Yes | Yes | N/A |
| File → Task (attach) | Yes | Yes *(iPad)* | N/A |
| Task → Task (reorder) | Yes | Yes | N/A |
| Stream → Stream (reorder) | Yes | Yes | N/A |

**Calendar block → Task is No in both columns, and what is missing is the two
modifiers, not a layout that could hold them.** A Block is not a drag
source: `BlockChip` (`CalendarView.swift:358-457`) carries a tap
(`:407`), a move gesture (`:408`) and a context menu (`:410`), and no
`.draggable` — the only `.draggable` in the whole tree is the task row's
(`TaskRowView.swift:74`). A task row is not a drop target for one either:
`TaskListView.swift:192` is a `dropDestination` that reorders and does nothing
else, returning `model.reorder(moved, before: task.id)` and declining outright
in Today and in Search. The write the cell would perform exists and is tested —
`TaskListModel.bind(_:to:)` (`TaskListModel.swift:179`) issuing the same
`Command::BindTask` (`crates/sunrise-core/src/commands.rs:157`) the grid's own
drop issues — so building it is UI work with no core work behind it. Until then
a Block is bound to a Task from the grid side, by dropping the task onto it.

**The iOS cell for *Task → Calendar block* is No: both ends ship, and no screen
in the shell presents them together.** The drag source (`TaskRowView.swift:74`)
and the grid's drop target (`CalendarView.swift:221`, into `accept(items:at:)`
at `:312`) are shared, unguarded and compiled into `SunriseiOS` — this cell
fails on reach, not on code. `TaskRowView` renders only inside `TaskListView`
(`:172`) and `DailyBriefBody` (`DailyBriefView.swift:82`); the grid renders on
iOS only at `VaultTabs.swift:72`, its own tab, and `:228`, a pushed destination
that replaces the list on the same stack. The tab is not the boundary and it
would be wrong to say it is: `pushed(destination:)` (`VaultTabs.swift:212`) is
attached to the Today and the Browse stacks alike (`:136`, `:144`), so the grid
can be pushed onto the very stack a task list is on — it just arrives *instead
of* the list, not beside it. Three things the tree establishes: no iOS screen
shows a task row and the grid together, no `Tab` carries a `dropDestination`,
and nothing configures spring-loading. Nothing in the tree shows a path that
completes the gesture, and that is what the **No** records.

**One path this file cannot settle, and it is the one that would overturn the
verdict.** On iOS and iPadOS a drag session survives navigation — an item held
under one finger stays held while a second finger taps a tab — and that needs
neither spring-loading nor a second scene. If a task row held that way reaches
the Calendar tab and lands on the grid's `dropDestination`, the cell is a
**Yes** and the paragraph above is wrong about the consequence, though not
about any of its three facts. Reading the source cannot decide it: the question
is what UIKit delivers to a drop target across a tab change at runtime, not
what the tree declares, and nobody has run it. It is tracked as
[#72](https://github.com/justin13888/Sunrise/issues/72). The verdict stays **No** on the
evidence that exists — a completable path has to be shown, not merely left
open — but it is the cheapest of these cells to overturn, and it takes a
simulator rather than another grep.

**The other way out would be a second window, and that one the tree does
close.** It is also what separates this cell from *File → Task*. A second
Sunrise window would put a list beside the grid, but this app cannot vend one:
`iOS/SunriseiOSApp.swift:23-24` declares a single `WindowGroup`, and multiple
windows on iPadOS are gated on `UIApplicationSupportsMultipleScenes` inside
`UIApplicationSceneManifest`, which nothing here sets. The `SunriseiOS` target
has no checked-in plist at all — its Info.plist is generated
(`project.yml:192-203`, `GENERATE_INFOPLIST_FILE: YES` at `:181`) from three
`properties` (`CFBundleURLTypes` and the two version keys) and three
`INFOPLIST_KEY_` settings (`UILaunchScreen_Generation`, and the two
`UISupportedInterfaceOrientations`, `:182-190`). Neither key appears in any of
them, or anywhere in `apps/apple`; absent, `UIApplicationSupportsMultipleScenes`
takes its default of `NO`, so iPadOS grants the app one scene and there is no
second window to drag into. *File → Task* is qualified to the iPad for the
complementary reason: it needs a second **app** — Files beside Sunrise in Split
View — which asks nothing of this app's own scene support. One window each is
exactly what Split View hands out.

Every file named above is under `Sunrise/`, so every modifier this section
names compiles into the Mac app and the iOS app alike — what differs between
the columns is which of them a user can bring together on one screen, not
which of them exist.
`TaskRowView` is `.draggable`, and it is accepted by the sidebar's stream and
context rows (`BrowseSidebar`) and by other task rows (`TaskListView`, which
declines the drop in Today and in Search because the core ranks those lists).
The attachments pane takes files (`AttachmentsView`), and a block moves and
resizes within the grid by its own gestures rather than by a drop. The grid's
own `dropDestination` serves exactly one cell — *Task → Calendar block*, since
`accept(items:at:)` takes `tsk_` payloads only — so it is evidence for that
cell and for no other. Stream reorder is `ForEach.onMove` writing
`Stream.sort_order` through the core, so it syncs; task reorder is per-device,
per [§Reorder](#reorder) above.

### Drag-and-drop UX tokens

- Snap grid (Calendar): 15-min increments by default; user-configurable {5, 10, 15, 30, 60} minutes.
- Ghost opacity: 0.65.
- Drop-target visual: 2 px solid `accent` border + 8% `accent` background tint.

## Notification interactions

- "Mark done" action button on every reminder push.
- "Defer 1 hour" action.
- "Snooze until tomorrow" action.

These run via local OS APIs (deep links into the app for desktop; native action handlers for iOS / Android; Web Push action buttons for web).

### Action wiring (per platform)

| Action | macOS | iOS | Android | Web |
|---|---|---|---|---|
| Complete task | `UNNotificationAction` `complete`, deep link `sunrise://task/<id>?action=complete` | same | Notification `Action` with `PendingIntent` carrying the same URI | Web Push `actions[0].action = "complete"`, app handles in `notificationclick` |
| Snooze | one action per span the domain offers (`Query::ReminderIntents` carries the targets; the client does no date arithmetic of its own) | `snooze_1h` | same | `actions[1]` |
| Open | tap body | tap body | tap body | default action |

**macOS and iOS are both implemented today**, and not as two implementations:
the categories, the task category's three buttons and the response delegate
are one shared file
(`apps/apple/Sunrise/Notifications/NotificationCenterClient.swift:60-105`)
compiled into both products. The iOS column carries SHOULDs rather than MUSTs
([ADR-0028](../11-adr/0028-ios-is-a-v1-client.md)). Android and Web remain
[deferred clients](./parity-matrix.md) and carry no MUSTs at all.

The app intercepts `sunrise://` URIs (or the equivalent intent / click) and translates to an op without opening UI when possible.

## URL scheme

`sunrise://` deep links. The scheme is registered by **both** Apple apps, each
through its own `info:` block in `project.yml` (`:124-140` for macOS,
`:192-203` for iOS), because `CFBundleURLTypes` has no `INFOPLIST_KEY_`
equivalent and the generated plists are gitignored. The parser
(`Sunrise/Notifications/DeepLink.swift`) is shared; the destination it produces
is resolved to a sidebar selection on macOS and to a tab plus a stack on iOS
(`iOS/TabRoute.swift:83-116`). A link may also name an *entity* as well as a
screen, and then the screen is only half of it: `DeepLink.reveal`
(`DeepLink.swift`) carries the id through, and the shell reads it out of the
vault — a Task opens its editor, a Block moves the grid onto the day it is on
(`CalendarModel.reveal(_:)`). One row below is unparsed on both platforms.

| Link | Status | Notes |
|---|---|---|
| `sunrise://morning` | **live** | The morning summary. Also ⌘⌥M. |
| `sunrise://evening` | **live** | End-of-day planning. Also ⌘⌥E. |
| `sunrise://capture?text=…` | **live** | Opens quick capture pre-filled. |
| `sunrise://entity/<EntityRef>` | **live** | Opens the screen the entity lives on **and the entity on it** — a Block routes to the calendar, moved onto the block's own day; a Task routes to Today and opens its editor, which shows the task whether or not Today happens to hold it. This is the link a block reminder carries and the one "Copy permalink" writes ([`../02-domain/identifiers.md`](../02-domain/identifiers.md)). An id this vault does not hold opens the screen and reveals nothing. |
| `sunrise://task/<id>?action=…` | **live** | `complete`, `open`, or a snooze span. This is what a notification action button fires. |
| `sunrise://focus/<TaskId>` | **live** | Starts a one-pomodoro session on that task and opens Focus — the same write `F` on a row makes. It declines while another session is running rather than opening a second one the screen cannot show; you land on Focus either way, so what is running is the first thing you see. Also reachable by `F` on a row and from the sidebar. |
| `sunrise://share/<token>` | **specified, not built** | See below. |

All deep links are validated; unknown shapes are ignored (no shell injection).
A link naming an entity that does not exist resolves to the nearest sensible
screen rather than an error dialog, because a notification tapped after its task
was deleted elsewhere is an ordinary event, not a failure.

### `sunrise://share/<token>` — specified, not built

> **Specified, not built.** The parser refuses this shape, and nothing in
> either app writes one. Sharing is deferred from v1 by
> [ADR-0020](../11-adr/0020-v1-must-demotions.md): a token names a grant, there
> is no grant model to name, and a route that accepted the token and then had
> nowhere to take it would be the one failure this section rules out — a link
> that goes somewhere plausible and wrong. The shape is kept because it is
> still the design; it will be built with the sharing model ADR-0020 defers,
> and is tracked by [#133](https://github.com/justin13888/Sunrise/issues/133),
> which is closed by whatever change introduces that model rather than on its
> own.

## Conflict-of-shortcut handling

Where platform-standard shortcuts disagree (e.g. `Cmd+T` is "new tab" on a browser), the web app yields and uses `Cmd+Shift+T` for new task; the desktop app uses `Cmd+N`. No global "consistent across all platforms" mandate that fights the OS.
