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

Deferred clients (iOS, Android, Web) are omitted; see
[`parity-matrix.md`](./parity-matrix.md).

| From → To | macOS | CLI |
|---|---|---|
| Task → Stream | Yes | N/A |
| Task → Calendar block | Yes | N/A |
| Calendar block → Task | Yes | N/A |
| File → Task (attach) | Yes | N/A |
| Task → Task (reorder) | Yes | N/A |

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

macOS is the only column implemented today; the other three are
[deferred clients](./parity-matrix.md) and carry no MUSTs.

The app intercepts `sunrise://` URIs (or the equivalent intent / click) and translates to an op without opening UI when possible.

## URL scheme

`sunrise://` deep links. The scheme is registered by the macOS app through
`project.yml`'s `info:` block, because `CFBundleURLTypes` has no
`INFOPLIST_KEY_` equivalent and the generated plist is gitignored.

| Link | Status | Notes |
|---|---|---|
| `sunrise://morning` | **live** | The morning summary. Also ⌘⌥M. |
| `sunrise://evening` | **live** | End-of-day planning. Also ⌘⌥E. |
| `sunrise://capture?text=…` | **live** | Opens quick capture pre-filled. |
| `sunrise://entity/<EntityRef>` | **live** | Opens *the screen the entity lives on* — a Block routes to the calendar, a Task to Today — not a detail window. There is no standalone entity detail window to open, so a route promising one would be a link that goes nowhere. |
| `sunrise://task/<id>?action=…` | **live** | `complete`, `open`, or a snooze span. This is what a notification action button fires. |
| `sunrise://focus/<TaskId>` | not implemented | Focus is reachable by `F` on a row and from the sidebar; no route exists yet. |
| `sunrise://share/<token>` | not implemented | Sharing is deferred from v1 — [ADR-0020](../11-adr/0020-v1-must-demotions.md). A token route cannot precede a grant model. |

All deep links are validated; unknown shapes are ignored (no shell injection).
A link naming an entity that does not exist resolves to the nearest sensible
screen rather than an error dialog, because a notification tapped after its task
was deleted elsewhere is an ordinary event, not a failure.

## Conflict-of-shortcut handling

Where platform-standard shortcuts disagree (e.g. `Cmd+T` is "new tab" on a browser), the web app yields and uses `Cmd+Shift+T` for new task; the desktop app uses `Cmd+N`. No global "consistent across all platforms" mandate that fights the OS.
