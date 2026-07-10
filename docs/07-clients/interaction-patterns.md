---
status: accepted
---

# Interaction Patterns

Cross-platform user-facing behaviors. Implemented natively per platform; consistent in *what* the user can do.

## Quick capture

| Trigger | Platform |
|---|---|
| Global hotkey (default `Ctrl/Cmd + Shift + N`) | Desktop |
| Lock-screen widget tap | iOS |
| Quick Settings tile | Android |
| Browser keyboard shortcut (when extension installed) | Web |
| `s capture` subcommand or `:` in app | TUI |

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

- Click checkbox / tap checkbox / press `x` in TUI / press `Space` in keyboard mode.
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
- Visual marker in TUI; `V` to enter visual mode (vim-like).

## Undo

- `Cmd/Ctrl+Z` / `u` in TUI.
- **Undoable**: any user-initiated CRUD on entities; explicit user actions in views.
- **Not undoable**: sync receipts (other-device ops), background routine generation, server-initiated ops.
- Time-bound: undo within 5 min is one-tap. > 5 min: confirmation modal `"Undo this change from <Nm ago>?"`.
- Per-device undo stack, 64 entries; not synced.

## Drag-and-drop matrix

| From → To | Desktop | iOS | Android | Web | TUI |
|---|---|---|---|---|---|
| Task → Stream | Yes | Yes | Yes | Yes | Use `m` |
| Task → Calendar block | Yes | Yes | Yes | Yes | Use `s` |
| Calendar block → Task | Yes | Yes | Yes | Yes | N/A |
| File → Task (attach) | Yes | Yes (Files app) | Yes | Yes | Use `attach <path>` |
| Task → Task (reorder) | Yes | Yes | Yes | Yes | `Alt+↑/↓` |

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

| Action | iOS | Android | Web |
|---|---|---|---|
| Complete task | UNNotificationAction `complete`, deep link `sunrise://task/<id>?action=complete` | Notification `Action` with `PendingIntent` carrying the same URI | Web Push `actions[0].action = "complete"`, app handles in `notificationclick` |
| Snooze 1h | `snooze_1h` | same | `actions[1]` |
| Open | tap body | tap body | default action |

The app intercepts `sunrise://` URIs (or the equivalent intent / click) and translates to a CRDT op without opening UI when possible.

## URL scheme

`sunrise://` deep links for:

- `sunrise://entity/<EntityRef>` — open entity in detail.
- `sunrise://capture?text=…&stream=…` — open quick capture pre-filled.
- `sunrise://focus/<TaskId>` — start focus on a task.
- `sunrise://share/<token>` — accept a share invite.

All deep links are validated; unknown shapes are ignored (no shell injection).

## Conflict-of-shortcut handling

Where platform-standard shortcuts disagree (e.g. `Cmd+T` is "new tab" on a browser), the web app yields and uses `Cmd+Shift+T` for new task; the desktop app uses `Cmd+N`. No global "consistent across all platforms" mandate that fights the OS.
